#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Memory(String),
    #[error("{0}")]
    Invariant(String),
    #[error("emulator: {0}")]
    Engine(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<unicorn_engine::uc_error> for Error {
    fn from(value: unicorn_engine::uc_error) -> Self {
        Self::Engine(format!("{value:?}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
