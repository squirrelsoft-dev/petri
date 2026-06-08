use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, PetriError>;

#[derive(Debug)]
pub enum PetriError {
    Cli(String),
    MissingArgument {
        flag: &'static str,
    },
    InvalidArgument {
        flag: &'static str,
        value: String,
        message: String,
    },
    InvalidInstanceId(String),
    InvalidConfig(String),
    BackendUnavailable {
        backend: String,
        operation: &'static str,
    },
    Backend {
        backend: String,
        message: String,
    },
    /// A transport-level failure talking to the guest (connect/read/write/flush/
    /// decode). Transient by nature: a momentary hiccup should not brick a live
    /// instance, so dispatch returns it to `Ready` rather than `Failed`.
    Transport {
        message: String,
    },
    /// The guest helper was reached and answered with a structured error. The VM
    /// is alive, so the failure is recoverable — the dispatch itself failed, not
    /// the instance.
    Guest {
        message: String,
    },
    LifecycleTransition {
        operation: &'static str,
        from: &'static str,
        to: &'static str,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for PetriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cli(message) => write!(f, "{message}"),
            Self::MissingArgument { flag } => write!(f, "{flag} requires a value"),
            Self::InvalidArgument {
                flag,
                value,
                message,
            } => {
                write!(f, "invalid {flag} '{value}': {message}")
            }
            Self::InvalidInstanceId(value) => write!(f, "invalid instance id '{value}'"),
            Self::InvalidConfig(message) => write!(f, "invalid instance config: {message}"),
            Self::BackendUnavailable { backend, operation } => {
                write!(f, "backend '{backend}' does not implement {operation} yet")
            }
            Self::Backend { backend, message } => write!(f, "backend '{backend}': {message}"),
            Self::Transport { message } => write!(f, "guest transport error: {message}"),
            Self::Guest { message } => write!(f, "guest error: {message}"),
            Self::LifecycleTransition {
                operation,
                from,
                to,
            } => write!(
                f,
                "invalid lifecycle transition during {operation}: {from} -> {to}"
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl PetriError {
    /// Whether a dispatch failure leaves the guest VM presumed-alive, so the
    /// instance can safely return to `Ready` instead of being bricked to
    /// `Failed`. Transport hiccups, lower-level I/O on the control socket, and
    /// guest-reported errors all qualify; `Failed` is reserved for genuinely
    /// unrecoverable states (e.g. confirmed guest/VM death).
    pub fn dispatch_recoverable(&self) -> bool {
        matches!(
            self,
            Self::Transport { .. } | Self::Guest { .. } | Self::Io { .. }
        )
    }
}

impl std::error::Error for PetriError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_and_guest_errors_are_recoverable() {
        assert!(
            PetriError::Transport {
                message: "flaky read".to_string()
            }
            .dispatch_recoverable()
        );
        assert!(
            PetriError::Guest {
                message: "tool rejected".to_string()
            }
            .dispatch_recoverable()
        );
        assert!(
            PetriError::Io {
                path: PathBuf::from("/sock"),
                source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
            }
            .dispatch_recoverable()
        );
    }

    #[test]
    fn unrecoverable_errors_are_not_dispatch_recoverable() {
        assert!(
            !PetriError::Backend {
                backend: "macos".to_string(),
                message: "boom".to_string(),
            }
            .dispatch_recoverable()
        );
        assert!(!PetriError::InvalidConfig("bad".to_string()).dispatch_recoverable());
    }
}
