use crate::Address;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("{}", .msg)]
    Runtime { msg: String },
    #[error("Invalid address")]
    InvalidAddress(#[from] hex::FromHexError),
    #[error("Address not found: {}", .address)]
    NotFound { address: Address },
    #[error("Corrupt entry: {0}")]
    Corrupt(&'static str),
    #[error("Io")]
    Io(#[from] std::io::Error),
    #[error("WalkDir")]
    WalkDir(#[from] walkdir::Error),
}
