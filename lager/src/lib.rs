mod compression;
mod lager;

pub use lager::Lager;

use std::io;
use std::path::PathBuf;

use hex;
use thiserror::Error;

fn format_io_error_with_message(msg: &Option<String>, source: &io::Error) -> String {
    match msg {
        Some(m) => format!("{}: {}", m, source),
        None => format!("{}", source),
    }
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("{}", .msg)]
    Runtime { msg: String },
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

pub type Result<T> = std::result::Result<T, Error>;

pub struct Address([u8; 32]);

impl From<[u8; 32]> for Address {
    fn from(bytes: [u8; 32]) -> Self {
        Address(bytes)
    }
}

impl Into<String> for &Address {
    fn into(self) -> String {
        hex::encode(self.0)
    }
}

fn shard_path(address: &Address, levels: u32) -> PathBuf {
    let address_str: String = address.into();
    let mut path = PathBuf::new();
    for i in 0..levels {
        let start = (i * 2) as usize;
        let end = start + 2;
        path.push(&address_str[start..end]);
    }
    path.push(address_str);
    path
}
