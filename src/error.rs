use std::fmt;
use std::io;
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
    /// A file was found but could not be read.
    Read { path: PathBuf, source: io::Error },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotADirectory(path) => write!(f, "{} is not a directory", path.display()),
            Error::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::NotADirectory(_) => None,
            Error::Read { source, .. } => Some(source),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
