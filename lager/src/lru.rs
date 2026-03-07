use crate::{Address, Lager, Result, lager::SHARDING_LEVELS};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::SystemTime;
use walkdir::WalkDir;

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
                self.size += metadata.len();

                self.heap.push(Item {
                    address: Address::from_hex(
                        entry.path().file_stem().unwrap().to_str().unwrap(),
                    )?,
                    modified: metadata.modified()?,
                    size: metadata.len(),
                });
            }
        }

        Ok(())
    }

    pub fn evict_until(&mut self, target_size: u64) -> Result<()> {
        while self.size > target_size {
            if let Some(item) = self.heap.pop() {
                self.lager.remove(&item.address)?;
                self.size -= item.size;
            } else {
                break;
            }
        }

        Ok(())
    }

    pub fn lager_size(&self) -> u64 {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;
    use tempdir::TempDir;

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
        let addr1 = Address::from([1u8; 32]);
        let addr2 = Address::from([2u8; 32]);
        let addr3 = Address::from([3u8; 32]);

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
