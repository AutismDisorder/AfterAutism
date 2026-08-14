// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Shared error model: `Error` (with `ErrorKind` classification) and
//! `Result<T>`.

pub use crate::core::gate::NetworkGateError;

/// Stable machine-readable classification for engine errors.
/// Consumers match on this instead of error text. It is intentionally a
/// small closed set: new kinds are additive, and each maps to a recovery
/// posture (see `recovery_hint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The operation requires egress the network gate denied.
    GateClosed,
    /// The requested entity does not exist.
    NotFound,
    /// The input was structurally invalid (parse / schema / corrupt data).
    Invalid,
    /// The operation is not supported by this adapter / corpus / build.
    Unsupported,
    /// A resource limit (memory, disk, size, quota) was exceeded.
    Quota,
    /// An I/O failure (disk, network, permission).
    Io,
    /// The data failed an integrity check (checksum, header, signature).
    Corrupt,
    /// The corpus/format is from a newer version than this engine reads.
    FutureVersion,
    /// The operation was cancelled by the caller.
    Cancelled,
    /// A concurrent access conflict (lock, busy, write contention).
    Contention,
    /// Any other failure. Always prefer a specific kind when one fits.
    Other,
}

impl ErrorKind {
    /// A short human hint for the recovery posture.
    #[must_use]
    pub fn recovery_hint(self) -> &'static str {
        match self {
            Self::GateClosed => "open the network gate or retry offline",
            Self::NotFound => "check the identifier or re-ingest the source",
            Self::Invalid => "validate the input and retry",
            Self::Unsupported => "use a different adapter or feature tier",
            Self::Quota => "raise the limit or free resources",
            Self::Io => "check permissions, disk, and network",
            Self::Corrupt => "restore from backup or re-index",
            Self::FutureVersion => "upgrade the engine",
            Self::Cancelled => "retry if desired",
            Self::Contention => "retry after the concurrent operation finishes",
            Self::Other => "inspect the error detail",
        }
    }
}

/// Top-level error type for `afterautism-core`. Individual crates can wrap
/// via `#[from] Error` or compose in their own enums.
#[derive(Debug)]
pub enum Error {
    /// Caller attempted outbound I/O through `NetworkGate` while the
    /// policy was offline. The exact text is owned by `NetworkGateError`.
    NetworkGate(NetworkGateError),

    /// A plain classified error with optional context.
    Classified {
        /// The error category.
        kind: ErrorKind,
        /// Human-readable description.
        message: String,
        /// Attached context lines (`source=…, corpus=…`), if any.
        context: Option<String>,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NetworkGate(e) => write!(f, "{e}"),
            Self::Classified {
                message, context, ..
            } => match context {
                Some(c) => write!(f, "{message} ({c})"),
                None => write!(f, "{message}"),
            },
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NetworkGate(e) => Some(e),
            Self::Classified { .. } => None,
        }
    }
}

impl Error {
    /// Classify an error with a message and optional context.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::Classified {
            kind,
            message: message.into(),
            context: None,
        }
    }

    /// The stable classification of this error.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::NetworkGate(_) => ErrorKind::GateClosed,
            Self::Classified { kind, .. } => *kind,
        }
    }

    /// Attach context to the error, returning a new error.
    /// ```text
    /// err.with_context("source", "file:///a/b.csv")
    /// ```
    #[must_use]
    pub fn with_context(mut self, key: &str, value: impl std::fmt::Display) -> Self {
        let kv = format!("{key}={value}");
        match &mut self {
            Self::Classified { context, .. } => match context {
                Some(c) => {
                    c.push_str(", ");
                    c.push_str(&kv);
                }
                None => *context = Some(kv),
            },
            other @ Self::NetworkGate(_) => {
                let base = other.to_string();
                *other = Self::Classified {
                    kind: other.kind(),
                    message: base,
                    context: Some(kv),
                };
            }
        }
        self
    }

    /// True when the error is a classified resource/validation failure
    /// (not a gate or integrity issue) — useful for retry heuristics.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind(),
            ErrorKind::Io | ErrorKind::Quota | ErrorKind::Contention
        )
    }
}

impl From<NetworkGateError> for Error {
    fn from(err: NetworkGateError) -> Self {
        Self::NetworkGate(err)
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Self::new(kind, "")
    }
}

/// Convenience result alias for core operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_classifies_gate() {
        let e = Error::from(NetworkGateError::OfflineEnforced);
        assert_eq!(e.kind(), ErrorKind::GateClosed);
    }

    #[test]
    fn classified_kind_and_hint() {
        let e = Error::new(ErrorKind::NotFound, "no such node");
        assert_eq!(e.kind(), ErrorKind::NotFound);
        assert_eq!(
            ErrorKind::NotFound.recovery_hint(),
            "check the identifier or re-ingest the source"
        );
    }

    #[test]
    fn context_chaining_appends() {
        let e = Error::new(ErrorKind::Io, "read failed")
            .with_context("source", "a.csv")
            .with_context("corpus", "live.db");
        assert_eq!(e.to_string(), "read failed (source=a.csv, corpus=live.db)");
    }

    #[test]
    fn context_attaches_to_gate_error_too() {
        let e = Error::from(NetworkGateError::OfflineEnforced).with_context("key", "http://x");
        assert_eq!(e.kind(), ErrorKind::GateClosed);
        assert!(e.to_string().contains("key=http://x"));
    }

    #[test]
    fn retryable_classification() {
        assert!(Error::new(ErrorKind::Io, "e").is_retryable());
        assert!(!Error::new(ErrorKind::NotFound, "e").is_retryable());
        assert!(!Error::new(ErrorKind::Corrupt, "e").is_retryable());
    }

    #[test]
    fn serde_roundtrip_kind() {
        let k = ErrorKind::FutureVersion;
        let s = serde_json::to_string(&k).expect("serialize");
        assert_eq!(s, "\"future_version\"");
        let back: ErrorKind = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, k);
    }
}
