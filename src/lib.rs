//! Reproducible, vendored R source context for coding agents.

pub mod cli;
pub mod config;
pub mod digest;
pub mod fetch;
pub mod hosttools;
pub mod lock;
pub mod manifest;
pub mod resolve;
pub mod rlib;
pub mod spec;
pub mod vendor;

/// Errors surfaced by the library and their stable CLI exit classes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Spec(String),
    #[error("{0}")]
    Fetch(String),
    #[error("{0}")]
    Verification(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Map an error to the public exit-code contract in SPEC.md section 6.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) | Self::Spec(_) => 2,
            Self::Fetch(_) => 3,
            Self::Verification(_) => 4,
            Self::Io(_) => 1,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn exit_codes_follow_the_cli_contract() {
        assert_eq!(Error::Config(String::new()).exit_code(), 2);
        assert_eq!(Error::Spec(String::new()).exit_code(), 2);
        assert_eq!(Error::Fetch(String::new()).exit_code(), 3);
        assert_eq!(Error::Verification(String::new()).exit_code(), 4);
    }
}
