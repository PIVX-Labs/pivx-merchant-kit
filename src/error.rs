//! Crate-wide error type. Each layer (config, storage, sync, api, webhooks)
//! gets its own variant so callers can match on category without parsing
//! strings. Anything from a third-party crate goes through `#[from]`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml parse: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("invoice: {0}")]
    Invoice(String),
}

pub type Result<T> = std::result::Result<T, Error>;
