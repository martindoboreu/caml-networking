use std::fmt;

/// Reasons a datagram is rejected by [`parse_packet`](crate::parse_packet).
///
/// A packet is accepted or rejected as a unit: if any message is invalid, no
/// message from that packet is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Datagram is shorter than the packet header.
    TooShort {
        /// Datagram length in bytes.
        len: usize,
    },
    /// First two bytes are not [`MAGIC`](crate::MAGIC).
    BadMagic {
        /// Bytes found.
        found: [u8; 2],
    },
    /// Version byte is not [`VERSION`](crate::VERSION).
    UnsupportedVersion {
        /// Version found.
        found: u8,
    },
    /// Header flags or reserved bytes are non-zero.
    ReservedNonZero,
    /// Datagram length disagrees with `msg_count`.
    LengthMismatch {
        /// Length implied by the header.
        expected: usize,
        /// Actual datagram length.
        actual: usize,
    },
    /// A message's length field is not [`MESSAGE_LEN`](crate::MESSAGE_LEN).
    BadMessageLength {
        /// Zero-based index of the message in the packet.
        index: u16,
        /// Length field found.
        length: u16,
    },
    /// A message type is not defined in this protocol version.
    UnknownMessageType {
        /// Zero-based index of the message in the packet.
        index: u16,
        /// Type byte found.
        msg_type: u8,
    },
    /// A message's flags byte is non-zero.
    MessageFlagsNonZero {
        /// Zero-based index of the message in the packet.
        index: u16,
    },
}

/// Field-free classification of [`DecodeError`], usable as a metrics index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DecodeErrorKind {
    /// See [`DecodeError::TooShort`].
    TooShort,
    /// See [`DecodeError::BadMagic`].
    BadMagic,
    /// See [`DecodeError::UnsupportedVersion`].
    UnsupportedVersion,
    /// See [`DecodeError::ReservedNonZero`].
    ReservedNonZero,
    /// See [`DecodeError::LengthMismatch`].
    LengthMismatch,
    /// See [`DecodeError::BadMessageLength`].
    BadMessageLength,
    /// See [`DecodeError::UnknownMessageType`].
    UnknownMessageType,
    /// See [`DecodeError::MessageFlagsNonZero`].
    MessageFlagsNonZero,
}

impl DecodeErrorKind {
    /// Number of variants.
    pub const COUNT: usize = 8;

    /// All variants, in index order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::TooShort,
        Self::BadMagic,
        Self::UnsupportedVersion,
        Self::ReservedNonZero,
        Self::LengthMismatch,
        Self::BadMessageLength,
        Self::UnknownMessageType,
        Self::MessageFlagsNonZero,
    ];

    /// Dense index in `0..COUNT`.
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Stable snake_case name for logs and reports.
    pub const fn name(self) -> &'static str {
        match self {
            Self::TooShort => "too_short",
            Self::BadMagic => "bad_magic",
            Self::UnsupportedVersion => "unsupported_version",
            Self::ReservedNonZero => "reserved_non_zero",
            Self::LengthMismatch => "length_mismatch",
            Self::BadMessageLength => "bad_message_length",
            Self::UnknownMessageType => "unknown_message_type",
            Self::MessageFlagsNonZero => "message_flags_non_zero",
        }
    }
}

impl DecodeError {
    /// Classification of this error.
    pub const fn kind(&self) -> DecodeErrorKind {
        match self {
            Self::TooShort { .. } => DecodeErrorKind::TooShort,
            Self::BadMagic { .. } => DecodeErrorKind::BadMagic,
            Self::UnsupportedVersion { .. } => DecodeErrorKind::UnsupportedVersion,
            Self::ReservedNonZero => DecodeErrorKind::ReservedNonZero,
            Self::LengthMismatch { .. } => DecodeErrorKind::LengthMismatch,
            Self::BadMessageLength { .. } => DecodeErrorKind::BadMessageLength,
            Self::UnknownMessageType { .. } => DecodeErrorKind::UnknownMessageType,
            Self::MessageFlagsNonZero { .. } => DecodeErrorKind::MessageFlagsNonZero,
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { len } => write!(f, "datagram too short ({len} bytes)"),
            Self::BadMagic { found } => write!(f, "bad magic {found:02x?}"),
            Self::UnsupportedVersion { found } => write!(f, "unsupported version {found}"),
            Self::ReservedNonZero => write!(f, "header flags/reserved bytes are non-zero"),
            Self::LengthMismatch { expected, actual } => {
                write!(
                    f,
                    "length mismatch: header implies {expected} bytes, got {actual}"
                )
            }
            Self::BadMessageLength { index, length } => {
                write!(f, "message {index}: bad length {length}")
            }
            Self::UnknownMessageType { index, msg_type } => {
                write!(f, "message {index}: unknown type {msg_type}")
            }
            Self::MessageFlagsNonZero { index } => write!(f, "message {index}: non-zero flags"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Reasons [`PacketWriter`](crate::PacketWriter) cannot encode a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// Output buffer cannot hold the next item.
    BufferFull {
        /// Bytes required.
        needed: usize,
        /// Bytes available.
        available: usize,
    },
    /// The packet already holds `u16::MAX` messages.
    TooManyMessages,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferFull { needed, available } => {
                write!(f, "buffer full: need {needed} bytes, have {available}")
            }
            Self::TooManyMessages => write!(f, "packet message count would exceed u16::MAX"),
        }
    }
}

impl std::error::Error for EncodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_indices_are_dense_and_ordered() {
        for (i, kind) in DecodeErrorKind::ALL.iter().enumerate() {
            assert_eq!(kind.index(), i);
        }
    }
}
