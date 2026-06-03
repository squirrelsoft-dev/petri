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
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
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
