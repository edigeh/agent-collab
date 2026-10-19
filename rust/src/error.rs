//! Error kinds mirroring the Python exception classes the CLI distinguishes.

use std::fmt;

#[derive(Debug, Clone)]
pub enum Error {
    /// `state.Rejected`: a command cannot be accepted against the observed state.
    Rejected(String),
    /// `JournalError` and its plain subclasses.
    Journal(String),
    /// `JournalBusy`.
    Busy(String),
    /// `JournalCorrupt`.
    Corrupt(String),
    /// `cli.CliError` (argument errors).
    Cli(String),
    /// `OSError`.
    Os(String),
    /// Other `ValueError`s.
    Value(String),
    /// An exception the Python CLI does not catch (it exits with a traceback).
    Crash(String),
}

impl Error {
    pub fn message(&self) -> String {
        match self {
            Error::Rejected(m) | Error::Journal(m) | Error::Busy(m) | Error::Corrupt(m) | Error::Cli(m) | Error::Os(m) | Error::Value(m) | Error::Crash(m) => m.clone(),
        }
    }
    /// `isinstance(exc, JournalError)`.
    pub fn is_journal(&self) -> bool {
        matches!(self, Error::Journal(_) | Error::Busy(_) | Error::Corrupt(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Os(crate::pyfmt::os_error_text(&e))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn reject<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Rejected(msg.into()))
}

pub fn require(test: bool, msg: impl Into<String>) -> Result<()> {
    if test {
        Ok(())
    } else {
        Err(Error::Rejected(msg.into()))
    }
}
