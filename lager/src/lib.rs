mod compression;
mod error;
mod lager;
mod lru;

pub use crate::error::Error;
pub use crate::lager::Lager;
pub use crate::lru::LRU;

use std::fmt::Display;

use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

const ADDRESS_SIZE: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Address([u8; ADDRESS_SIZE]);

impl Address {
    pub(crate) fn from_hex(hex: &str) -> Result<Self> {
        let bytes = hex::decode(hex)?;
        if bytes.len() != ADDRESS_SIZE {
            return Err(Error::Runtime {
                msg: format!(
                    "Invalid address length ({}). Only 64 byte addresses are supported.",
                    bytes.len()
                ),
            });
        }
        let mut array = [0u8; ADDRESS_SIZE];
        array.copy_from_slice(&bytes);
        Ok(Address(array))
    }
}

impl std::convert::TryFrom<&[u8]> for Address {
    type Error = Error;
    fn try_from(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != ADDRESS_SIZE {
            return Err(Error::Runtime {
                msg: format!(
                    "Invalid address length ({}). Only 64 byte addresses are supported.",
                    bytes.len()
                ),
            });
        }
        let mut array = [0u8; ADDRESS_SIZE];
        array.copy_from_slice(bytes);
        Ok(Address(array))
    }
}
impl From<[u8; ADDRESS_SIZE]> for Address {
    fn from(bytes: [u8; ADDRESS_SIZE]) -> Self {
        Address(bytes)
    }
}

impl From<&Address> for String {
    fn from(address: &Address) -> Self {
        hex::encode(address.0)
    }
}

impl Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let address_str: String = self.into();
        write!(f, "{}", address_str)
    }
}

fn shard_path(address: &Address, levels: usize) -> PathBuf {
    let address_str: String = address.into();
    let mut path = PathBuf::new();
    for i in 0..levels {
        let start = i * 2;
        let end = start + 2;
        path.push(&address_str[start..end]);
    }
    path.push(address_str);
    path
}
