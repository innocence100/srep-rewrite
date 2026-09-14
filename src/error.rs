use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, Error>;

/// Stable error identity from the approved SREP-NG specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidConfiguration,
    UnsupportedVersion,
    UnsupportedLegacySplitIndex,
    UnknownChecksum,
    CorruptHeader,
    TruncatedArchive,
    CorruptRecord,
    CorruptIndex,
    InvalidMatch,
    ChecksumMismatch,
    OutputLimitExceeded,
    MemoryBudgetExceeded,
    TempBudgetExceeded,
    NonSeekableNeedsSpool,
    TemporaryStorageFailure,
    InputIo,
    OutputIo,
    AtomicPublish,
}

impl ErrorKind {
    pub const fn code(self) -> i32 {
        match self {
            Self::InvalidConfiguration => 2,
            Self::UnsupportedVersion => 3,
            Self::UnsupportedLegacySplitIndex => 4,
            Self::UnknownChecksum => 5,
            Self::CorruptHeader => 6,
            Self::TruncatedArchive => 7,
            Self::CorruptRecord => 8,
            Self::CorruptIndex => 9,
            Self::InvalidMatch => 10,
            Self::ChecksumMismatch => 11,
            Self::OutputLimitExceeded => 12,
            Self::MemoryBudgetExceeded => 13,
            Self::TempBudgetExceeded => 14,
            Self::NonSeekableNeedsSpool => 15,
            Self::TemporaryStorageFailure => 16,
            Self::InputIo => 17,
            Self::OutputIo => 18,
            Self::AtomicPublish => 19,
        }
    }

    pub const fn message_id(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "SREP_E_INVALID_CONFIG",
            Self::UnsupportedVersion => "SREP_E_UNSUPPORTED_VERSION",
            Self::UnsupportedLegacySplitIndex => "SREP_E_SPLIT_INDEX",
            Self::UnknownChecksum => "SREP_E_UNKNOWN_CHECKSUM",
            Self::CorruptHeader => "SREP_E_CORRUPT_HEADER",
            Self::TruncatedArchive => "SREP_E_TRUNCATED",
            Self::CorruptRecord => "SREP_E_CORRUPT_RECORD",
            Self::CorruptIndex => "SREP_E_CORRUPT_INDEX",
            Self::InvalidMatch => "SREP_E_INVALID_MATCH",
            Self::ChecksumMismatch => "SREP_E_CHECKSUM",
            Self::OutputLimitExceeded => "SREP_E_OUTPUT_LIMIT",
            Self::MemoryBudgetExceeded => "SREP_E_MEMORY_LIMIT",
            Self::TempBudgetExceeded => "SREP_E_TEMP_LIMIT",
            Self::NonSeekableNeedsSpool => "SREP_E_NEEDS_SPOOL",
            Self::TemporaryStorageFailure => "SREP_E_TEMP_STORAGE",
            Self::InputIo => "SREP_E_INPUT_IO",
            Self::OutputIo => "SREP_E_OUTPUT_IO",
            Self::AtomicPublish => "SREP_E_ATOMIC_PUBLISH",
        }
    }

    pub const fn summary(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid configuration",
            Self::UnsupportedVersion => "unsupported archive version",
            Self::UnsupportedLegacySplitIndex => "external legacy index is unsupported",
            Self::UnknownChecksum => "unknown checksum",
            Self::CorruptHeader => "corrupt archive header",
            Self::TruncatedArchive => "truncated archive",
            Self::CorruptRecord => "corrupt archive record",
            Self::CorruptIndex => "corrupt embedded index",
            Self::InvalidMatch => "invalid match",
            Self::ChecksumMismatch => "checksum mismatch",
            Self::OutputLimitExceeded => "output limit exceeded",
            Self::MemoryBudgetExceeded => "memory budget exceeded",
            Self::TempBudgetExceeded => "temporary budget exceeded",
            Self::NonSeekableNeedsSpool => "seekable input or spool required",
            Self::TemporaryStorageFailure => "temporary storage failure",
            Self::InputIo => "input I/O failure",
            Self::OutputIo => "output I/O failure",
            Self::AtomicPublish => "atomic publication failed",
        }
    }
}

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    context: Option<String>,
    source: Option<io::Error>,
}

impl Error {
    pub fn new(kind: ErrorKind, context: impl Into<String>) -> Self {
        Self {
            kind,
            context: Some(context.into()),
            source: None,
        }
    }

    pub fn with_source(kind: ErrorKind, context: impl Into<String>, source: io::Error) -> Self {
        Self {
            kind,
            context: Some(context.into()),
            source: Some(source),
        }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn code(&self) -> i32 {
        self.kind.code()
    }

    pub fn message_id(&self) -> &'static str {
        self.kind.message_id()
    }

    pub fn summary(&self) -> &'static str {
        self.kind.summary()
    }

    pub fn context(&self) -> Option<&str> {
        self.context.as_deref()
    }

    pub fn invalid_config(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidConfiguration, context)
    }

    pub fn unsupported_version(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::UnsupportedVersion, context)
    }

    pub fn unknown_checksum(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::UnknownChecksum, context)
    }

    pub fn unsupported_legacy_split_index(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::UnsupportedLegacySplitIndex, context)
    }

    pub fn corrupt_header(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::CorruptHeader, context)
    }

    pub fn truncated(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::TruncatedArchive, context)
    }

    pub fn corrupt_record(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::CorruptRecord, context)
    }

    pub fn corrupt_index(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::CorruptIndex, context)
    }

    pub fn invalid_match(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidMatch, context)
    }

    pub fn checksum_mismatch(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::ChecksumMismatch, context)
    }

    pub fn output_limit(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::OutputLimitExceeded, context)
    }

    pub fn memory_limit(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::MemoryBudgetExceeded, context)
    }

    pub fn temp_limit(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::TempBudgetExceeded, context)
    }

    pub fn needs_spool(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::NonSeekableNeedsSpool, context)
    }

    pub fn temp_storage_context(context: impl Into<String>) -> Self {
        Self::new(ErrorKind::TemporaryStorageFailure, context)
    }

    pub fn input_io(source: io::Error) -> Self {
        let context = source.to_string();
        Self {
            kind: ErrorKind::InputIo,
            context: Some(context),
            source: Some(source),
        }
    }

    pub fn output_io(source: io::Error) -> Self {
        let context = source.to_string();
        Self {
            kind: ErrorKind::OutputIo,
            context: Some(context),
            source: Some(source),
        }
    }

    pub fn temp_storage(source: io::Error) -> Self {
        let context = source.to_string();
        Self {
            kind: ErrorKind::TemporaryStorageFailure,
            context: Some(context),
            source: Some(source),
        }
    }

    pub fn atomic_publish(source: io::Error) -> Self {
        let context = source.to_string();
        Self {
            kind: ErrorKind::AtomicPublish,
            context: Some(context),
            source: Some(source),
        }
    }

    pub fn map_eof(error: io::Error, context: impl Into<String>) -> Self {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            Self::truncated(context)
        } else {
            Self::input_io(error)
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self
            .context
            .as_deref()
            .filter(|context| !context.is_empty())
        {
            Some(context) => write!(f, "{}: {}: {}", self.message_id(), self.summary(), context),
            None => write!(f, "{}: {}", self.message_id(), self.summary()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_identity_is_independent_of_context() {
        let a = Error::corrupt_header("flags");
        let b = Error::corrupt_header("reserved");
        assert_eq!(a.code(), b.code());
        assert_eq!(a.message_id(), "SREP_E_CORRUPT_HEADER");
        assert_eq!(a.summary(), "corrupt archive header");
        assert_eq!(a.code(), 6);
    }

    #[test]
    fn display_uses_specified_shape() {
        let error = Error::unsupported_version("experimental SREP-NG v1");
        assert_eq!(
            error.to_string(),
            "SREP_E_UNSUPPORTED_VERSION: unsupported archive version: experimental SREP-NG v1"
        );
    }
}
