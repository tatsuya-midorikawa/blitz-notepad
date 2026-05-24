use std::io;
use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, BlitzError>;

#[derive(Debug, Error)]
pub enum BlitzError {
    #[error("I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("I/O error: {0}")]
    PlainIo(#[from] io::Error),

    #[error("invalid edit range {start}..{end} for document length {len}")]
    InvalidRange {
        start: usize,
        end: usize,
        len: usize,
    },

    #[error("offset {offset} is not a UTF-8 character boundary")]
    InvalidCharBoundary { offset: usize },

    #[error("cannot save an untitled document without a target path")]
    MissingSavePath,

    #[error("encoding error: {0}")]
    Encoding(String),

    #[error("settings error: {0}")]
    Settings(String),

    #[error("window error: {0}")]
    Window(String),
}

pub fn io_path(path: impl Into<PathBuf>, source: io::Error) -> BlitzError {
    BlitzError::Io {
        path: path.into(),
        source,
    }
}
