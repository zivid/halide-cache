use crate::Address;

use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("{}", .msg)]
    Runtime { msg: String },
    #[error("Invalid address")]
    InvalidAddress(#[from] hex::FromHexError),
    #[error("Address not found: {}", .address)]
    NotFound { address: Address },
    #[error("No such file or directory: {}", .path.display())]
    NoSuchFile { path: PathBuf },
    #[error("Io")]
    Io(#[from] std::io::Error),
    #[error("WalkDir")]
    WalkDir(#[from] walkdir::Error),
}
