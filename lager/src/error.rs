use crate::Address;

use std::io;
use std::path::PathBuf;
use thiserror::Error;
use walkdir;

fn format_io_error_with_message(msg: &Option<String>, source: &io::Error) -> String {
    match msg {
        Some(m) => format!("{}: {}", m, source),
        None => format!("{}", source),
    }
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("{}", .msg)]
    Runtime {
        msg: String,
        #[source]
        source: Option<Box<dyn std::error::Error>>,
    },
    #[error("Address not found: {}", .address)]
    NotFound { address: Address },
    #[error("No such file or directory: {}", .path.display())]
    NoSuchFile { path: PathBuf },
    #[error("{}", format_io_error_with_message(.msg, .source))]
    Io {
        msg: Option<String>,
        #[source]
        source: io::Error,
    },
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Error::Io {
            msg: None,
            source: io::Error::new(io::ErrorKind::Other, source),
        }
    }
}

impl From<walkdir::Error> for Error {
    fn from(source: walkdir::Error) -> Self {
        if let Some(io_error) = source.into_io_error() {
            Error::Io {
                msg: Some("An error occurred while walking the directory tree".to_string()),
                source: io_error,
            }
        } else {
            Error::Runtime {
                msg: "An error occurred while walking the directory tree".to_string(),
                source: None,
            }
        }
    }
}
