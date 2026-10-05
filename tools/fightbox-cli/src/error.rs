//! CLI error type and process exit-code mapping.
//!
//! Every failure surfaces a specific, machine-readable message and exits
//! nonzero. The CLI never prints a misleading success and never silences a
//! backend rejection.

use std::process::ExitCode;

/// A failure produced while parsing a fixture/asset or running a Phase A command.
#[derive(Debug)]
pub struct CliError {
    message: String,
}

impl CliError {
    /// Build a new error from any displayable cause.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Concatenate a context prefix with a cause, separated by `: `.
    #[must_use]
    #[allow(dead_code)]
    pub fn with(context: impl Into<String>, cause: impl std::fmt::Display) -> Self {
        Self::new(format!("{}: {cause}", context.into()))
    }

    /// The single-line message printed to stderr.
    #[must_use]
    #[allow(dead_code)]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// Allow `?` to convert a `Result<T, String>` (used by the asset/scene/calibrate
/// validation helpers) into a CLI error.
impl From<String> for CliError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

/// Shorthand for the common fallible result type used across the CLI.
pub type Result<T> = std::result::Result<T, CliError>;

/// Convert a `serde_json` error into a CLI error tagged with context.
#[allow(dead_code)]
pub fn json_context(context: &str, error: serde_json::Error) -> CliError {
    CliError::new(format!("{context}: {error}"))
}

/// Entry point helper: run a fallible command, print any error to stderr, and
/// map the outcome to a process exit code.
pub fn report(result: Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("fightbox: {error}");
            ExitCode::from(1)
        }
    }
}
