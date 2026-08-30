use std::fmt;
use std::path::PathBuf;

/// Something that stopped envwire from doing its job.
///
/// A disagreement between a project's env sources is not one of these: that is a
/// finding, the answer the tool exists to give, and it leaves through the report.
/// This type is only for when envwire cannot look in the first place.
#[derive(Debug)]
pub enum Error {
    /// The path handed to envwire is not a directory it can inspect.
    NotADirectory(PathBuf),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotADirectory(path) => write!(f, "{} is not a directory", path.display()),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
