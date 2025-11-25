use crate::{compression, shard_path, Address};
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

pub struct Lager {
    root: PathBuf,
}

impl Lager {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Lager {
            root: path.as_ref().to_path_buf().canonicalize()?,
        })
    }

    pub fn store_at(&self, address: &Address, source: &Path) -> Result<()> {
        const SHARDING_LEVELS: u32 = 2;

        let mut dest = self.root.join(shard_path(address, SHARDING_LEVELS));

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if source.is_file() {
            dest.set_extension("zst");
            compression::write_file(source, File::create(dest)?)?;
        } else if source.is_dir() {
            dest.set_extension("tar.zst");
            compression::write_dir(source, File::create(dest)?)?;
        } else {
            return Err(Error::NoSuchFile {
                path: source.to_path_buf(),
            });
        }

        Ok(())
    }

    pub fn retrieve(&self, address: &Address, destination: &Path) -> Result<()> {
        let mut source = self.root.join(shard_path(address, 2));

        source.set_extension("zst");
        if source.exists() {
            compression::read_file(File::open(source)?, destination)?;
            return Ok(());
        }

        source.set_extension("tar.zst");
        if source.exists() {
            compression::read_dir(destination, File::open(source)?)?;
            Ok(())
        } else {
            Err(Error::NoSuchFile { path: source })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::TempDir;

    #[test]
    fn test_lager_from_path() {
        let dir = TempDir::new("lager_test").unwrap();

        let _lager = Lager::new(dir);
    }

    #[test]
    fn test_store_retrieve_file() {
        let dir = TempDir::new("lager_test").unwrap();
        let lager = Lager::new(dir.path()).unwrap();

        let temp_file_path = dir.path().join("temp_file.txt");
        std::fs::write(&temp_file_path, b"Hello, World!").unwrap();

        let address = Address::from([0u8; 32]);

        lager.store_at(&address, &temp_file_path).unwrap();

        let retrieve_path = dir.path().join("retrieved_file.txt");
        lager.retrieve(&address, &retrieve_path).unwrap();

        let content = std::fs::read(&retrieve_path).unwrap();
        assert_eq!(content, b"Hello, World!");
    }

    #[test]
    fn test_store_retrieve_directory() {
        let dir = TempDir::new("lager_test").unwrap();
        let lager = Lager::new(dir.path()).unwrap();

        let temp_dir_path = dir.path().join("temp_dir");
        std::fs::create_dir(&temp_dir_path).unwrap();
        std::fs::write(temp_dir_path.join("file1.txt"), b"File 1").unwrap();
        std::fs::write(temp_dir_path.join("file2.txt"), b"File 2").unwrap();

        let address = Address::from([1u8; 32]);

        lager.store_at(&address, &temp_dir_path).unwrap();

        let retrieve_path = dir.path().join("retrieved_dir");
        std::fs::create_dir(&retrieve_path).unwrap();
        lager.retrieve(&address, &retrieve_path).unwrap();

        let content1 = std::fs::read(retrieve_path.join("file1.txt")).unwrap();
        let content2 = std::fs::read(retrieve_path.join("file2.txt")).unwrap();
        assert_eq!(content1, b"File 1");
        assert_eq!(content2, b"File 2");
    }
}
