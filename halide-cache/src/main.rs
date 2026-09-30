mod archive;

use clap::Parser;
use dirs::home_dir;
use lager::{Address, LRU, Lager};
use named_lock::NamedLock;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, num_args = 1..)]
    dependencies: Vec<PathBuf>,
    #[arg(long)]
    generated_object: PathBuf,
    #[arg(long)]
    generated_header: PathBuf,
    #[arg(long)]
    base_dir: Option<PathBuf>,
    #[arg(long, default_value_os_t = home_dir().unwrap().join(".cache/halide-cache"))]
    cache_dir: PathBuf,
    #[arg(last = true)]
    builder: Vec<String>,
}

const MAX_CACHE_SIZE_BYTES: u64 = 10737418240; // 10 GiB

struct KeyInputs<'a> {
    dependencies: &'a [PathBuf],
    env: &'a [String],
    cmdline: &'a [String],
}

impl KeyInputs<'_> {
    fn address(&self, object: &str, header: &str) -> anyhow::Result<Address> {
        fn field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
            hasher.update(bytes);
            hasher.update(&[0u8]);
        }
        const GROUP_END: &[u8] = b"";

        let mut h = blake3::Hasher::new();

        field(&mut h, object.as_bytes());
        field(&mut h, header.as_bytes());
        field(&mut h, GROUP_END);

        for d in self.dependencies {
            h.update_reader(fs::File::open(d)?)?;
            h.update(&[0u8]);
        }
        field(&mut h, GROUP_END);

        for e in self.env {
            field(&mut h, e.as_bytes());
        }
        field(&mut h, GROUP_END);

        for e in self.cmdline {
            field(&mut h, e.as_bytes());
        }
        field(&mut h, GROUP_END);

        let mut buf = [0u8; _];
        h.finalize_xof().fill(&mut buf);
        Ok(buf.into())
    }
}

struct Entry {
    object: PathBuf,
    header: PathBuf,
    address: Address,
}

impl Entry {
    fn paths(&self) -> [&Path; 2] {
        [&self.object, &self.header]
    }

    fn target(&self) -> impl std::fmt::Display + '_ {
        self.object
            .file_prefix()
            .expect("Generated object has a file name")
            .display()
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    fs::create_dir_all(&args.cache_dir)?;
    let lager = Lager::new(&args.cache_dir)?;

    let base_dir = match args.base_dir {
        Some(d) => d,
        None => find_repo_root()?,
    };
    let cmdline: Vec<String> = args
        .builder
        .iter()
        .map(|c| strip_base(&base_dir, c))
        .collect();
    let zivid_env = collect_zivid_env();

    let inputs = KeyInputs {
        dependencies: &args.dependencies,
        env: &zivid_env,
        cmdline: &cmdline,
    };
    let address = inputs.address(
        &strip_base(&base_dir, &args.generated_object.to_string_lossy()),
        &strip_base(&base_dir, &args.generated_header.to_string_lossy()),
    )?;
    let entry = Entry {
        object: args.generated_object,
        header: args.generated_header,
        address,
    };

    if let Found::Hit = restore_outputs(&lager, &entry)? {
        return Ok(());
    }

    let status = Command::new(&args.builder[0])
        .args(&args.builder[1..])
        .status()?;

    if status.success() {
        archive::store(&lager, &entry.address, &entry.paths())?;
    }

    try_cleaning_up(lager)
}

fn try_cleaning_up(lager: Lager) -> anyhow::Result<()> {
    let lock = NamedLock::create("lager_lock")?;
    if let Ok(_guard) = lock.lock() {
        let mut lru = LRU::new(lager);
        lru.scan()?;
        if lru.lager_size() > MAX_CACHE_SIZE_BYTES {
            lru.evict_until(MAX_CACHE_SIZE_BYTES)?;
        }
    }
    Ok(())
}

fn strip_base(base_dir: &Path, arg: &str) -> String {
    Path::new(arg)
        .strip_prefix(base_dir)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or(arg)
        .to_owned()
}

fn collect_zivid_env() -> Vec<String> {
    let mut v = std::env::vars()
        .filter_map(|(k, v)| k.starts_with("ZIVID_").then(|| format!("{}={}", k, v)))
        .collect::<Vec<_>>();
    v.sort();
    v
}

enum Found {
    Hit,
    Miss,
}

fn restore_outputs(lager: &Lager, entry: &Entry) -> anyhow::Result<Found> {
    match archive::restore(lager, &entry.address, &entry.paths()) {
        Ok(()) => {
            println!("Cache hits for Halide target {}", entry.target());
            Ok(Found::Hit)
        }
        Err(lager::Error::NotFound { .. }) => Ok(Found::Miss),
        Err(e) => Err(e.into()),
    }
}

fn find_repo_root() -> anyhow::Result<PathBuf> {
    let mut cwd = std::env::current_dir()?;

    loop {
        if cwd.join(".git").exists() {
            return Ok(cwd);
        }
        if !cwd.pop() {
            anyhow::bail!("Could not determine root");
        }
    }
}
