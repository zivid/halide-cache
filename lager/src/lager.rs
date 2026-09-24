use crate::{Address, compression, shard_path};
use crate::{Error, Result};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) const SHARDING_LEVELS: usize = 2;

const FILE_EXTENSION: &str = "zst";
const DIR_EXTENSION: &str = "tar.zst";
/// Suffix of in-progress writes; ignored by lookups and by the LRU scan.
pub(crate) const TEMP_SUFFIX: &str = ".tmp";
static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What a stored blob decompresses to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// A zstd compressed single file.
    File,
    /// A zstd compressed tar archive of a directory.
    Dir,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Dir => "dir",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "file" => Some(Kind::File),
            "dir" => Some(Kind::Dir),
            _ => None,
        }
    }

    fn extension(&self) -> &'static str {
        match self {
            Kind::File => FILE_EXTENSION,
            Kind::Dir => DIR_EXTENSION,
        }
    }
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

    pub fn store_at(&self, address: &Address, source: &Path) -> Result<()> {
        let mut dest = self.root.join(shard_path(address, SHARDING_LEVELS));

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let kind = if source.is_file() {
            Kind::File
        } else if source.is_dir() {
            Kind::Dir
        } else {
            return Err(Error::NoSuchFile {
                path: source.to_path_buf(),
            });
        };
        dest.set_extension(kind.extension());
        match kind {
            Kind::File => compression::write_file(source, File::create(dest)?)?,
            Kind::Dir => compression::write_dir(source, File::create(dest)?)?,
        }
        self.remove_other_kind(address, kind)
    }

    pub fn retrieve(&self, address: &Address, destination: &Path) -> Result<()> {
        let (file, kind) = self.open_raw(address)?;
        match kind {
            Kind::File => compression::read_file(file, destination)?,
            Kind::Dir => compression::read_dir(destination, file)?,
        }
        Ok(())
    }

    /// Opens the stored, still compressed, blob for an address and marks it as
    /// recently used. Used to move blobs between lagers without recompressing.
    pub fn open_raw(&self, address: &Address) -> Result<(File, Kind)> {
        let mut path = self.root.join(shard_path(address, SHARDING_LEVELS));

        for kind in [Kind::File, Kind::Dir] {
            path.set_extension(kind.extension());
            // Open directly rather than checking existence first: a concurrent
            // eviction between the two steps must read as a miss, not an error.
            match File::open(&path) {
                Ok(file) => {
                    // Touching can race with eviction too; a vanished file is a miss.
                    if let Err(e) = file.set_modified(SystemTime::now())
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        return Err(e.into());
                    }
                    return Ok((file, kind));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::NotFound { address: *address })
    }

    /// Stores an already compressed blob, as produced by `open_raw` on another
    /// lager. The blob is written to a temporary file and renamed into place so
    /// that concurrent readers never observe a partial entry. Returns whether the
    /// entry was new.
    pub fn store_raw<R: Read>(&self, address: &Address, kind: Kind, mut blob: R) -> Result<bool> {
        let mut dest = self.root.join(shard_path(address, SHARDING_LEVELS));
        dest.set_extension(kind.extension());

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let existed = dest.exists();

        // Unique per process *and* per write, so concurrent uploads of the same
        // address within one server process never share a temp file.
        let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut tmp_name = dest.file_name().unwrap().to_os_string();
        tmp_name.push(format!("{}-{}-{}", TEMP_SUFFIX, std::process::id(), seq));
        let tmp = dest.with_file_name(tmp_name);

        let result = (|| -> Result<()> {
            let mut file = File::create_new(&tmp)?;
            std::io::copy(&mut blob, &mut file)?;
            file.flush()?;
            std::fs::rename(&tmp, &dest)?;
            Ok(())
        })();

        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        self.remove_other_kind(address, kind)?;
        result.map(|_| !existed)
    }

    /// An address holds exactly one entry. After storing one kind, drop any
    /// stale entry of the other kind so lookups cannot return the old one.
    fn remove_other_kind(&self, address: &Address, kept: Kind) -> Result<()> {
        let other = match kept {
            Kind::File => Kind::Dir,
            Kind::Dir => Kind::File,
        };
        let mut path = self.root.join(shard_path(address, SHARDING_LEVELS));
        path.set_extension(other.extension());
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Removes the entry, whatever its kind. Removing an absent entry is not an error.
    pub fn remove(&self, address: &Address) -> Result<()> {
        let mut path = self.root.join(shard_path(address, SHARDING_LEVELS));
        for kind in [Kind::File, Kind::Dir] {
            path.set_extension(kind.extension());
            match std::fs::remove_file(&path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    pub(crate) fn dir(&self) -> PathBuf {
        self.root.clone()
    }
}

#[cfg(test)]
mod tests {
    use crate::ADDRESS_SIZE;

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

        let address = Address::from([0u8; ADDRESS_SIZE]);

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

        let address = Address::from([1u8; ADDRESS_SIZE]);

        lager.store_at(&address, &temp_dir_path).unwrap();

        let retrieve_path = dir.path().join("retrieved_dir");
        std::fs::create_dir(&retrieve_path).unwrap();
        lager.retrieve(&address, &retrieve_path).unwrap();

        let content1 = std::fs::read(retrieve_path.join("file1.txt")).unwrap();
        let content2 = std::fs::read(retrieve_path.join("file2.txt")).unwrap();
        assert_eq!(content1, b"File 1");
        assert_eq!(content2, b"File 2");
    }

    #[test]
    fn test_retrieve_nonexistent_address() {
        let dir = TempDir::new("lager_test").unwrap();
        let lager = Lager::new(dir.path()).unwrap();

        let address = Address::from([2u8; ADDRESS_SIZE]);
        let retrieve_path = dir.path().join("nonexistent_retrieved.txt");

        let result = lager.retrieve(&address, &retrieve_path);
        assert!(matches!(result, Err(Error::NotFound { .. })));
    }
}

#[cfg(test)]
mod raw_tests {
    use super::*;
    use crate::ADDRESS_SIZE;
    use tempdir::TempDir;

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
                    let lager = Lager::new(&root).unwrap();
                    let payload = vec![i; 4096];
                    lager
                        .store_raw(&address, Kind::File, payload.as_slice())
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let (mut file, _) = Lager::new(&root).unwrap().open_raw(&address).unwrap();
        let mut got = Vec::new();
        file.read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), 4096);
        assert!(got.iter().all(|b| *b == got[0]), "blob mixes two writes");
        let leftovers: Vec<_> = walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(TEMP_SUFFIX))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind");
    }

    #[test]
    fn raw_roundtrip_between_lagers() {
        let dir = TempDir::new("lager_raw").unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let la = Lager::new(&a).unwrap();
        let lb = Lager::new(&b).unwrap();

        let src = dir.path().join("src.txt");
        std::fs::write(&src, b"payload").unwrap();
        let address = Address::from([7u8; ADDRESS_SIZE]);
        la.store_at(&address, &src).unwrap();

        let (blob, kind) = la.open_raw(&address).unwrap();
        assert_eq!(kind, Kind::File);
        assert!(lb.store_raw(&address, kind, blob).unwrap());

        let (blob, kind) = la.open_raw(&address).unwrap();
        assert!(!lb.store_raw(&address, kind, blob).unwrap());

        let out = dir.path().join("out.txt");
        lb.retrieve(&address, &out).unwrap();
        assert_eq!(std::fs::read(out).unwrap(), b"payload");
    }

    #[test]
    fn storing_a_different_kind_replaces_the_entry() {
        let dir = TempDir::new("lager_kind").unwrap();
        let lager = Lager::new(dir.path()).unwrap();
        let address = Address::from([3u8; ADDRESS_SIZE]);

        let src = dir.path().join("f.txt");
        std::fs::write(&src, b"file").unwrap();
        lager.store_at(&address, &src).unwrap();
        assert_eq!(lager.open_raw(&address).unwrap().1, Kind::File);

        let d = dir.path().join("d");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("x"), b"x").unwrap();
        lager.store_at(&address, &d).unwrap();
        assert_eq!(lager.open_raw(&address).unwrap().1, Kind::Dir);

        let (blob, kind) = lager.open_raw(&address).unwrap();
        let other = Lager::new(dir.path().join("o").tap_create()).unwrap();
        other.store_raw(&address, kind, blob).unwrap();
        let (_, kind) = other.open_raw(&address).unwrap();
        assert_eq!(kind, Kind::Dir);

        let mut files: Vec<_> = walkdir::WalkDir::new(dir.path().join("o"))
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .collect();
        assert_eq!(files.len(), 1, "exactly one physical entry per address");
        files.clear();
    }

    trait TapCreate {
        fn tap_create(self) -> Self;
    }
    impl TapCreate for std::path::PathBuf {
        fn tap_create(self) -> Self {
            std::fs::create_dir_all(&self).unwrap();
            self
        }
    }
}
