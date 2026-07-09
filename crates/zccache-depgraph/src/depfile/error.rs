//! Error type for depfile parsing.

/// Errors that can occur while parsing a `.d` file.
#[derive(Debug)]
pub enum DepfileError {
    /// I/O error reading the file.
    Io(std::io::Error),
    /// The depfile content is malformed (empty or missing colon separator).
    Malformed(String),
}

impl std::fmt::Display for DepfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DepfileError::Io(e) => write!(f, "depfile I/O error: {e}"),
            DepfileError::Malformed(msg) => write!(f, "malformed depfile: {msg}"),
        }
    }
}

impl std::error::Error for DepfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DepfileError::Io(e) => Some(e),
            DepfileError::Malformed(_) => None,
        }
    }
}

impl From<std::io::Error> for DepfileError {
    fn from(e: std::io::Error) -> Self {
        DepfileError::Io(e)
    }
}
