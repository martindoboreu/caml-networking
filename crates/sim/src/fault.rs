use std::num::NonZeroU64;

/// A deliberate impairment applied to one packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Do not send the packet.
    Drop,
    /// Overwrite the magic bytes so the receiver rejects it as malformed.
    Corrupt,
    /// Send the packet twice back to back.
    Duplicate,
    /// Hold the packet and send it after the next one.
    Reorder,
}

/// Periodic fault schedule keyed by 1-based packet index.
///
/// If several rules match the same index, the first in the order
/// drop > corrupt > duplicate > reorder wins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaultPlan {
    /// Drop every Nth packet.
    pub drop_every: Option<NonZeroU64>,
    /// Corrupt every Nth packet.
    pub corrupt_every: Option<NonZeroU64>,
    /// Duplicate every Nth packet.
    pub duplicate_every: Option<NonZeroU64>,
    /// Reorder every Nth packet with its successor.
    pub reorder_every: Option<NonZeroU64>,
}

impl FaultPlan {
    /// Whether no faults are configured.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Fault scheduled for packet `index` (1-based), if any.
    pub fn fault_for(&self, index: u64) -> Option<Fault> {
        let hits = |every: Option<NonZeroU64>| every.is_some_and(|n| index.is_multiple_of(n.get()));
        if hits(self.drop_every) {
            Some(Fault::Drop)
        } else if hits(self.corrupt_every) {
            Some(Fault::Corrupt)
        } else if hits(self.duplicate_every) {
            Some(Fault::Duplicate)
        } else if hits(self.reorder_every) {
            Some(Fault::Reorder)
        } else {
            None
        }
    }
}

/// How many of each fault were applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaultCounts {
    /// Packets dropped.
    pub dropped: u64,
    /// Packets corrupted.
    pub corrupted: u64,
    /// Packets duplicated.
    pub duplicated: u64,
    /// Packets reordered.
    pub reordered: u64,
}

/// Applies a [`FaultPlan`] between packet generation and transmission.
#[derive(Debug)]
pub struct FaultInjector {
    plan: FaultPlan,
    held: Vec<u8>,
    holding: bool,
    counts: FaultCounts,
}

impl FaultInjector {
    /// Creates an injector. `max_packet_len` sizes the reorder hold buffer.
    pub fn new(plan: FaultPlan, max_packet_len: usize) -> Self {
        Self {
            plan,
            held: Vec::with_capacity(max_packet_len),
            holding: false,
            counts: FaultCounts::default(),
        }
    }

    /// Faults applied so far.
    pub fn counts(&self) -> FaultCounts {
        self.counts
    }

    /// Processes packet number `index` (1-based), calling `send` zero or more
    /// times. `packet` may be modified in place.
    pub fn process<E>(
        &mut self,
        index: u64,
        packet: &mut [u8],
        mut send: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        match self.plan.fault_for(index) {
            Some(Fault::Drop) => {
                self.counts.dropped += 1;
            }
            Some(Fault::Corrupt) => {
                self.counts.corrupted += 1;
                if let Some(magic) = packet.first_mut() {
                    *magic ^= 0xFF;
                }
                send(packet)?;
            }
            Some(Fault::Duplicate) => {
                self.counts.duplicated += 1;
                send(packet)?;
                send(packet)?;
            }
            Some(Fault::Reorder) => {
                self.counts.reordered += 1;
                self.flush(&mut send)?;
                self.held.clear();
                self.held.extend_from_slice(packet);
                self.holding = true;
                return Ok(());
            }
            None => send(packet)?,
        }
        self.flush(&mut send)
    }

    /// Sends any packet still held for reordering. Call once after the last
    /// packet.
    pub fn finish<E>(&mut self, mut send: impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        self.flush(&mut send)
    }

    fn flush<E>(&mut self, send: &mut impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        if self.holding {
            self.holding = false;
            send(&self.held)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(n: u64) -> Option<NonZeroU64> {
        NonZeroU64::new(n)
    }

    /// Runs packets `1..=n` (each a single byte equal to its index) through
    /// the injector and returns what was sent.
    fn run(plan: FaultPlan, n: u8) -> (Vec<u8>, FaultCounts) {
        let mut inj = FaultInjector::new(plan, 1);
        let mut sent = Vec::new();
        for i in 1..=n {
            let mut pkt = [i];
            inj.process(u64::from(i), &mut pkt, |p| {
                sent.push(p[0]);
                Ok::<(), ()>(())
            })
            .unwrap();
        }
        inj.finish(|p| {
            sent.push(p[0]);
            Ok::<(), ()>(())
        })
        .unwrap();
        (sent, inj.counts())
    }

    #[test]
    fn no_faults_is_identity() {
        let (sent, counts) = run(FaultPlan::default(), 5);
        assert_eq!(sent, [1, 2, 3, 4, 5]);
        assert_eq!(counts, FaultCounts::default());
    }

    #[test]
    fn each_fault_kind() {
        let plan = FaultPlan {
            drop_every: nz(7),
            corrupt_every: nz(5),
            duplicate_every: nz(3),
            reorder_every: nz(4),
        };
        let (sent, counts) = run(plan, 9);
        // 3: dup, 4: held until after 5, 5: corrupt (first byte inverted),
        // 6: dup, 7: drop, 8: held until after 9, 9: dup.
        assert_eq!(sent, [1, 2, 3, 3, 5 ^ 0xFF, 4, 6, 6, 9, 9, 8]);
        assert_eq!(
            counts,
            FaultCounts {
                dropped: 1,
                corrupted: 1,
                duplicated: 3,
                reordered: 2
            }
        );
    }

    #[test]
    fn trailing_reorder_is_flushed_by_finish() {
        let plan = FaultPlan {
            reorder_every: nz(3),
            ..FaultPlan::default()
        };
        assert_eq!(run(plan, 3).0, [1, 2, 3]);
    }

    #[test]
    fn priority_order() {
        let plan = FaultPlan {
            drop_every: nz(2),
            corrupt_every: nz(2),
            duplicate_every: nz(2),
            reorder_every: nz(2),
        };
        assert_eq!(plan.fault_for(2), Some(Fault::Drop));
        assert_eq!(plan.fault_for(3), None);
        assert!(FaultPlan::default().is_empty());
    }
}
