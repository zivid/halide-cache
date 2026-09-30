use crate::{Address, Error, Result, shard_path};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) const SHARDING_LEVELS: usize = 2;

pub(crate) const TEMP_SUFFIX: &str = ".tmp";
static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn touch(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_modified(SystemTime::now())
}

fn is_not_found(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

#[derive(Clone, Debug)]
pub struct Lager {
    root: PathBuf,
}

impl Lager {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Lager {
            root: path.as_ref().to_path_buf().canonicalize()?,
        })
    }

    fn path(&self, address: &Address) -> PathBuf {
        self.root.join(shard_path(address, SHARDING_LEVELS))
    }

    pub fn store_at(
        &self,
        address: &Address,
        write: impl FnOnce(&mut dyn std::io::Write) -> std::io::Result<()>,
    ) -> Result<bool> {
        let dest = self.path(address);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let existed = dest.exists();

        let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut tmp_name = dest.file_name().unwrap().to_os_string();
        tmp_name.push(format!("{TEMP_SUFFIX}-{}-{seq}", std::process::id()));
        let tmp = dest.with_file_name(tmp_name);

        let result = (|| -> std::io::Result<()> {
            let mut file = File::create_new(&tmp)?;
            write(&mut file)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &dest)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(result.map(|_| !existed)?)
    }

    pub fn retrieve(&self, address: &Address) -> Result<File> {
        let path = self.path(address);
        let file = File::open(&path).map_err(|e| {
            if is_not_found(&e) {
                Error::NotFound { address: *address }
            } else {
                e.into()
            }
        })?;
        if let Err(e) = touch(&path)
            && !is_not_found(&e)
        {
            return Err(e.into());
        }
        Ok(file)
    }

    pub(crate) fn remove_if_unused_since(
        &self,
        address: &Address,
        last_used: SystemTime,
    ) -> Result<bool> {
        let path = self.path(address);
        match std::fs::metadata(&path) {
            Ok(m) if m.modified()? > last_used => return Ok(false),
            Ok(_) => {}
            Err(e) if is_not_found(&e) => return Ok(true),
            Err(e) => return Err(e.into()),
        }
        self.remove(address).map(|_| true)
    }

    pub fn remove(&self, address: &Address) -> Result<()> {
        match std::fs::remove_file(self.path(address)) {
            Err(e) if !is_not_found(&e) => Err(e.into()),
            _ => Ok(()),
        }
    }

    pub(crate) fn dir(&self) -> PathBuf {
        self.root.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ADDRESS_SIZE;
    use std::io::Read;
    use tempdir::TempDir;

    fn read(lager: &Lager, address: &Address) -> Vec<u8> {
        let mut content = Vec::new();
        lager
            .retrieve(address)
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        content
    }

    fn temp_files(root: &Path) -> usize {
        walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(TEMP_SUFFIX))
            .count()
    }

    #[test]
    fn store_and_retrieve() {
        let dir = TempDir::new("lager_test").unwrap();
        let lager = Lager::new(dir.path()).unwrap();
        let address = Address::from([1u8; ADDRESS_SIZE]);

        assert!(
            lager
                .store_at(&address, |out| out.write_all(b"one"))
                .unwrap()
        );
        assert_eq!(read(&lager, &address), b"one");
        assert!(
            !lager
                .store_at(&address, |out| out.write_all(b"two"))
                .unwrap()
        );
        assert_eq!(read(&lager, &address), b"two");
    }

    #[test]
    fn retrieving_an_absent_entry_is_not_found() {
        let dir = TempDir::new("lager_test").unwrap();
        let lager = Lager::new(dir.path()).unwrap();
        let address = Address::from([3u8; ADDRESS_SIZE]);
        let result = lager.retrieve(&address);
        assert!(matches!(result, Err(Error::NotFound { .. })));
    }

    #[test]
    fn a_failed_write_stores_nothing() {
        let dir = TempDir::new("lager_test").unwrap();
        let lager = Lager::new(dir.path()).unwrap();
        let address = Address::from([5u8; ADDRESS_SIZE]);

        let result = lager.store_at(&address, |out| {
            out.write_all(b"partial")?;
            Err(std::io::Error::other("source failed"))
        });
        assert!(result.is_err());
        assert!(matches!(
            lager.retrieve(&address),
            Err(Error::NotFound { .. })
        ));
        assert_eq!(temp_files(dir.path()), 0, "temp files left behind");
    }

    #[test]
    fn concurrent_stores_of_same_address_do_not_collide() {
        let dir = TempDir::new("lager_race").unwrap();
        let root = dir.path().join("l");
        std::fs::create_dir_all(&root).unwrap();
        let address = Address::from([9u8; ADDRESS_SIZE]);

        let handles: Vec<_> = (0..8u8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let payload = vec![i; 4096];
                    Lager::new(&root)
                        .unwrap()
                        .store_at(&address, |out| out.write_all(&payload))
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let got = read(&Lager::new(&root).unwrap(), &address);
        assert_eq!(got.len(), 4096);
        assert!(got.iter().all(|b| *b == got[0]), "entry mixes two writes");
        assert_eq!(temp_files(&root), 0, "temp files left behind");
    }
}
