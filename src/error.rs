use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml parse: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("config: {0}")]
    Config(String),

    #[error("path expression: {0}")]
    Path(String),

    #[error("git: {0}")]
    Git(#[from] git2::Error),
}
