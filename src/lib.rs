//! Core library for the `downget` command-line downloader.
//!
//! The public API intentionally stays small: the binary parses the CLI and
//! delegates complete commands to [`app::App`].  HTTP URLs are never returned
//! in public errors, which keeps callers from accidentally logging secrets.

pub mod app;
pub mod cli;
pub mod lease;
pub mod model;
pub mod part_file;
pub mod retry;
pub mod source;
pub mod store;
pub mod transfer;
pub mod ui;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    User(String),
    #[error("erro de I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("erro de estado local: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("falha de rede; verifique a fonte e tente novamente")]
    Network,
    #[error("erro interno: {0}")]
    Internal(String),
    #[error("interrompido; o Job foi pausado com segurança")]
    Interrupted,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Keeps the SIGINT-specific exit contract testable without relying on a
/// platform-specific process-group signal in every test environment.
pub fn exit_code(error: &Error) -> i32 {
    if matches!(error, Error::Interrupted) {
        130
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn interrupted_jobs_have_posix_sigint_exit_status() {
        // `App::execute` maps observed Ctrl+C to this domain error.
        assert_eq!(super::exit_code(&super::Error::Interrupted), 130);
    }
}
