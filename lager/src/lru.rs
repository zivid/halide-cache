use crate::{Address, Lager, Result, lager::SHARDING_LEVELS, lager::TEMP_SUFFIX};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::SystemTime;
use walkdir::WalkDir;

/// Temporary files older than this are considered abandoned (a writer that
/// was killed mid-upload) and are removed during a scan.
const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

#[derive(Eq, PartialEq)]
struct Item {
    address: Address,
    modified: SystemTime,
    size: u64,
}

impl PartialOrd<Self> for Item {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Item {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap, so we reverse the comparison
        // to make it pop the oldest (least recently used) items first
        other.modified.cmp(&self.modified)
    }
}

/// An entry removed by `LRU::evict_until`.
#[derive(Debug, Clone, Copy)]
pub struct Evicted {
    pub address: Address,
    /// When the entry was last stored or retrieved.
    pub last_used: SystemTime,
    pub size: u64,
}

pub struct LRU {
    heap: BinaryHeap<Item>,
    size: u64,
    lager: Lager,
}

impl LRU {
    pub fn new(lager: Lager) -> Self {
        LRU {
            heap: BinaryHeap::new(),
            size: 0,
            lager,
        }
    }

    pub fn scan(&mut self) -> Result<()> {
        let dir = self.lager.dir();

        for entry in WalkDir::new(dir)
            .follow_links(false)
            .max_depth(SHARDING_LEVELS + 1)
        {
            let entry = entry?;

            let metadata = entry.metadata()?;
            if metadata.is_file() {
                let name = entry.file_name().to_string_lossy();
                if name.contains(TEMP_SUFFIX) {
                    // An in-progress write from another process; not part of the
                    // cache. Unless it is old enough to be an abandoned upload, in
                    // which case it would otherwise consume disk outside the limit forever.
                    let stale = metadata
                        .modified()
                        .ok()
                        .and_then(|m| SystemTime::now().duration_since(m).ok())
                        .is_some_and(|age| age > STALE_TEMP_AGE);
                    if stale {
                        let _ = std::fs::remove_file(entry.path());
                    }
                    continue;
                }
                self.size += metadata.len();

                self.heap.push(Item {
                    address: Address::from_hex(&name[..name.find('.').unwrap_or(name.len())])?,
                    modified: metadata.modified()?,
                    size: metadata.len(),
                });
            }
        }

        Ok(())
    }

    /// Removes least recently used entries until the store is at most
    /// `target_size` bytes. Returns what was removed.
    ///
    /// The scan is a snapshot: an entry may have been retrieved (touched) or
    /// re-stored since. Such entries are skipped rather than removed, so
    /// eviction never deletes something that is in active use. They remain
    /// counted in the size, since they are still on disk.
    pub fn evict_until(&mut self, target_size: u64) -> Result<Vec<Evicted>> {
        let mut evicted = Vec::new();
        while self.size > target_size {
            let Some(item) = self.heap.pop() else {
                break;
            };
            if !self
                .lager
                .remove_if_unused_since(&item.address, item.modified)?
            {
                continue;
            }
            self.size -= item.size;
            evicted.push(Evicted {
                address: item.address,
                last_used: item.modified,
                size: item.size,
            });
        }

        Ok(evicted)
    }

    pub fn lager_size(&self) -> u64 {
        self.size
    }

    pub fn entries(&self) -> usize {
        self.heap.len()
    }
}

#[cfg(test)]
mod tests {
    use crate::ADDRESS_SIZE;

    use super::*;
    use std::thread;
    use std::time::Duration;
    use tempdir::TempDir;

    #[test]
    fn eviction_skips_entries_used_after_the_scan() {
        let dir = TempDir::new("lru_touch").unwrap();
        let root = dir.path().join("lager");
        std::fs::create_dir_all(&root).unwrap();
        let lager = Lager::new(&root).unwrap();
        let src = dir.path().join("f");
        std::fs::write(&src, b"0123456789").unwrap();
        let old = Address::from([1u8; ADDRESS_SIZE]);
        let new = Address::from([2u8; ADDRESS_SIZE]);
        lager.store_at(&old, &src).unwrap();
        thread::sleep(Duration::from_millis(50));
        lager.store_at(&new, &src).unwrap();

        let mut lru = LRU::new(Lager::new(&root).unwrap());
        lru.scan().unwrap();
        // Between the scan and the eviction, `old` gets used.
        thread::sleep(Duration::from_millis(50));
        lager.retrieve(&old, &dir.path().join("out")).unwrap();

        let evicted = lru.evict_until(0).unwrap();
        let evicted: Vec<_> = evicted.iter().map(|e| e.address).collect();
        assert_eq!(evicted, vec![new], "only the untouched entry is removed");
        assert!(lager.open_raw(&old).is_ok(), "the touched entry survives");
    }

    #[test]
    fn scan_removes_stale_temp_files_and_keeps_fresh_ones() {
        let dir = TempDir::new("lru_tmp").unwrap();
        let root = dir.path().join("lager");
        let shard = root.join("aa").join("bb");
        std::fs::create_dir_all(&shard).unwrap();
        let stale = shard.join(format!("aabb{}-1-1", TEMP_SUFFIX));
        let fresh = shard.join(format!("aabb{}-1-2", TEMP_SUFFIX));
        std::fs::write(&stale, b"x").unwrap();
        std::fs::write(&fresh, b"x").unwrap();
        std::fs::File::open(&stale)
            .unwrap()
            .set_modified(SystemTime::now() - STALE_TEMP_AGE - Duration::from_secs(1))
            .unwrap();

        let mut lru = LRU::new(Lager::new(&root).unwrap());
        lru.scan().unwrap();
        assert_eq!(lru.entries(), 0);
        assert_eq!(lru.lager_size(), 0);
        assert!(!stale.exists(), "stale temp file should be removed");
        assert!(fresh.exists(), "fresh temp file should be kept");
    }

    #[test]
    fn test_lru() {
        let dir = TempDir::new("lru_test").unwrap();
        let lager_dir = dir.path().join("lager");
        std::fs::create_dir_all(&lager_dir).unwrap();
        let lager = Lager::new(&lager_dir).unwrap();

        // Create test files with different timestamps
        let temp_file1 = dir.path().join("file1.txt");
        let temp_file2 = dir.path().join("file2.txt");
        let temp_file3 = dir.path().join("file3.txt");

        std::fs::write(&temp_file1, b"Content 1").unwrap();
        std::fs::write(&temp_file2, b"Content 2 longer").unwrap();
        std::fs::write(&temp_file3, b"Content 3 even longer text").unwrap();

        // Store files in lager with different addresses
        let addr1 = Address::from([1u8; ADDRESS_SIZE]);
        let addr2 = Address::from([2u8; ADDRESS_SIZE]);
        let addr3 = Address::from([3u8; ADDRESS_SIZE]);

        lager.store_at(&addr1, &temp_file1).unwrap();
        lager.store_at(&addr2, &temp_file2).unwrap();
        lager.store_at(&addr3, &temp_file3).unwrap();

        // Now access addr3 and addr2 to make them recently used
        // This should update their modification times
        let retrieve1 = dir.path().join("retrieve1.txt");
        let retrieve2 = dir.path().join("retrieve2.txt");

        thread::sleep(Duration::from_millis(50));
        lager.retrieve(&addr3, &retrieve1).unwrap();

        thread::sleep(Duration::from_millis(50));
        lager.retrieve(&addr2, &retrieve2).unwrap();

        // Create LRU and scan
        let mut lru = LRU::new(Lager::new(&lager_dir).unwrap());
        lru.scan().unwrap();

        // Verify initial size is greater than 0
        assert!(lru.lager_size() > 0);
        let initial_size = lru.lager_size();

        let target_size = 2 * initial_size / 3;
        lru.evict_until(target_size).unwrap();

        // Verify size was reduced
        assert!(lru.lager_size() <= target_size);

        // Verify that the oldest file (addr1) was deleted
        let test_retrieve = dir.path().join("test_retrieve.txt");

        // addr1 (oldest) should be gone
        let result1 = lager.retrieve(&addr1, &test_retrieve);
        assert!(
            result1.is_err(),
            "Oldest file (addr1) should have been evicted"
        );

        // addr2 (most recently used) should still exist
        let result2 = lager.retrieve(&addr2, &test_retrieve);
        if lru.lager_size() > 0 {
            assert!(
                result2.is_ok(),
                "Most recently used file (addr2) should be preserved"
            );
        }
    }
}
