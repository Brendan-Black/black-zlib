use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The compressed data or image is damaged, cut short or not what it claims to be.
    Corrupt(String),
    /// Decoding would produce more than the caller allowed.
    TooLarge { limit: usize },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn corrupt(msg: impl Into<String>) -> Self {
        Error::Corrupt(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Corrupt(msg) => write!(f, "corrupt data: {msg}"),
            Error::TooLarge { limit } => write!(f, "the data decodes to more than the {limit}-byte limit"),
        }
    }
}

impl std::error::Error for Error {}
