use crate::AppState;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default, Clone, Copy, Serialize)]
pub struct Host {
    pub cpus: usize,
    pub cpu_percent: Option<f64>,
    pub load: [f64; 3],
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    pub disk_total_bytes: u64,
    pub disk_available_bytes: u64,
}

pub async fn sample_loop(state: Arc<AppState>, data_dir: PathBuf, interval: Duration) {
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    let mut previous = cpu_times();
    loop {
        tokio::time::sleep(interval).await;
        let current = cpu_times();
        let (memory_total_bytes, memory_available_bytes) = memory().unwrap_or_default();
        let (disk_total_bytes, disk_available_bytes) = disk(&data_dir).unwrap_or_default();
        state.metrics.record_host(Host {
            cpus,
            cpu_percent: previous.zip(current).and_then(cpu_percent),
            load: load().unwrap_or_default(),
            memory_total_bytes,
            memory_available_bytes,
            disk_total_bytes,
            disk_available_bytes,
        });
        previous = current;
    }
}

fn cpu_times() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let fields: Vec<u64> = stat
        .lines()
        .next()?
        .split_whitespace()
        .skip(1)
        .take(8)
        .filter_map(|f| f.parse().ok())
        .collect();
    if fields.len() < 5 {
        return None;
    }
    let idle = fields[3] + fields[4];
    Some((idle, fields.iter().sum()))
}

fn cpu_percent(((idle0, total0), (idle1, total1)): ((u64, u64), (u64, u64))) -> Option<f64> {
    let total = total1.checked_sub(total0).filter(|&t| t > 0)?;
    let idle = idle1.saturating_sub(idle0).min(total);
    Some(100.0 * (total - idle) as f64 / total as f64)
}

fn load() -> Option<[f64; 3]> {
    let loadavg = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut values = loadavg.split_whitespace().map(|v| v.parse().ok());
    Some([values.next()??, values.next()??, values.next()??])
}

fn memory() -> Option<(u64, u64)> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib = |name: &str| {
        meminfo.lines().find_map(|line| {
            line.strip_prefix(name)?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse::<u64>()
                .ok()
        })
    };
    Some((kib("MemTotal:")? * 1024, kib("MemAvailable:")? * 1024))
}

fn disk(path: &Path) -> Option<(u64, u64)> {
    let fs = rustix::fs::statvfs(path).ok()?;
    Some((fs.f_blocks * fs.f_frsize, fs.f_bavail * fs.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_is_the_busy_share_of_elapsed_time() {
        assert_eq!(cpu_percent(((100, 1000), (150, 1100))), Some(50.0));
        assert_eq!(cpu_percent(((100, 1000), (200, 1100))), Some(0.0));
        assert_eq!(cpu_percent(((100, 1000), (100, 1000))), None);
    }

    #[test]
    fn reads_the_host() {
        let (total, available) = memory().unwrap();
        assert!(total > 0 && available <= total);
        assert!(load().is_some());
        assert!(cpu_times().is_some());
        let (disk_total, disk_available) = disk(Path::new("/")).unwrap();
        assert!(disk_total > 0 && disk_available <= disk_total);
    }
}
