//! What can go wrong.
//!
//! Every variant here is a STARTUP or INITIALIZATION failure — nothing in this
//! crate answers a request yet, so nothing maps to `gwk_domain::KernelErrorCode`.
//! That mapping arrives with the request path, not before it.

/// A kernel failure.
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    /// A required environment variable is missing, malformed, or forbidden in
    /// this process.
    #[error("configuration: {0}")]
    Config(String),

    /// The target database is not one this binary may initialize or serve.
    #[error("schema: {0}")]
    Schema(String),

    /// The credential holds privileges the kernel refuses to run with, or
    /// lacks ones it needs.
    #[error("privilege: {0}")]
    Privilege(String),

    /// This process does not hold write authority for the store.
    #[error("writer: {0}")]
    Writer(String),

    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, KernelError>;
