use crate::{Address, Error, Result, compression, shard_path};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) const SHARDING_LEVELS: usize = 2;

const EXTENSION: &str = "tar.zst";
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
        let mut path = self.root.join(shard_path(address, SHARDING_LEVELS));
        path.set_extension(EXTENSION);
        path
    }

    pub fn store_at(&self, address: &Address, files: &[&Path]) -> Result<()> {
        self.write_entry(address, |out| compression::write_files(files, out))
            .map(|_| ())
    }

    pub fn retrieve(&self, address: &Address, destinations: &[&Path]) -> Result<()> {
        compression::read_files(self.open_raw(address)?, destinations)
    }

    pub fn open_raw(&self, address: &Address) -> Result<File> {
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

    pub fn store_raw<R: Read>(&self, address: &Address, mut blob: R) -> Result<bool> {
        self.write_entry(address, |out| {
            std::io::copy(&mut blob, out)?;
            Ok(())
        })
    }

    fn write_entry(
        &self,
        address: &Address,
        write: impl FnOnce(&mut File) -> Result<()>,
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

        let result = (|| -> Result<()> {
            let mut file = File::create_new(&tmp)?;
            write(&mut file)?;
            file.flush()?;
            std::fs::rename(&tmp, &dest)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map(|_| !existed)
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
    use tempdir::TempDir;

    fn files(dir: &Path, contents: &[&str]) -> Vec<PathBuf> {
        contents
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let p = dir.join(format!("in{i}"));
                std::fs::write(&p, c).unwrap();
                p
            })
            .collect()
    }

    fn refs(paths: &[PathBuf]) -> Vec<&Path> {
        paths.iter().map(PathBuf::as_path).collect()
    }

    #[test]
    fn store_and_retrieve_several_files() {
        let dir = TempDir::new("lager_test").unwrap();
        let lager = Lager::new(dir.path()).unwrap();
        let address = Address::from([1u8; ADDRESS_SIZE]);
        let inputs = files(dir.path(), &["object", "", "header"]);
        lager.store_at(&address, &refs(&inputs)).unwrap();

        let outs: Vec<PathBuf> = (0..3).map(|i| dir.path().join(format!("out{i}"))).collect();
        lager.retrieve(&address, &refs(&outs)).unwrap();
        for (i, expected) in ["object", "", "header"].iter().enumerate() {
            assert_eq!(std::fs::read_to_string(&outs[i]).unwrap(), *expected);
        }
    }

    #[test]
    fn retrieving_a_different_number_of_files_is_corrupt() {
        let dir = TempDir::new("lager_test").unwrap();
        let lager = Lager::new(dir.path()).unwrap();
        let address = Address::from([2u8; ADDRESS_SIZE]);
        lager
            .store_at(&address, &refs(&files(dir.path(), &["a", "b"])))
            .unwrap();

        let one = [dir.path().join("out")];
        assert!(matches!(
            lager.retrieve(&address, &refs(&one)),
            Err(Error::Corrupt(_))
        ));
        let three: Vec<PathBuf> = (0..3).map(|i| dir.path().join(format!("o{i}"))).collect();
        assert!(lager.retrieve(&address, &refs(&three)).is_err());
    }

    #[test]
    fn retrieving_an_absent_entry_is_not_found() {
        let dir = TempDir::new("lager_test").unwrap();
        let lager = Lager::new(dir.path()).unwrap();
        let address = Address::from([3u8; ADDRESS_SIZE]);
        let result = lager.retrieve(&address, &[&dir.path().join("out")]);
        assert!(matches!(result, Err(Error::NotFound { .. })));
    }

    #[test]
    fn raw_entries_move_between_stores() {
        let dir = TempDir::new("lager_raw").unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (la, lb) = (Lager::new(&a).unwrap(), Lager::new(&b).unwrap());
        let address = Address::from([7u8; ADDRESS_SIZE]);
        la.store_at(&address, &refs(&files(dir.path(), &["payload"])))
            .unwrap();

        assert!(
            lb.store_raw(&address, la.open_raw(&address).unwrap())
                .unwrap()
        );
        assert!(
            !lb.store_raw(&address, la.open_raw(&address).unwrap())
                .unwrap()
        );
        let out = dir.path().join("out");
        lb.retrieve(&address, &[&out]).unwrap();
        assert_eq!(std::fs::read(out).unwrap(), b"payload");
    }

    #[test]
    fn concurrent_raw_stores_of_same_address_do_not_collide() {
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
                        .store_raw(&address, payload.as_slice())
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let mut got = Vec::new();
        Lager::new(&root)
            .unwrap()
            .open_raw(&address)
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got.len(), 4096);
        assert!(got.iter().all(|b| *b == got[0]), "blob mixes two writes");
        let leftovers = walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(TEMP_SUFFIX))
            .count();
        assert_eq!(leftovers, 0, "temp files left behind");
    }
}
