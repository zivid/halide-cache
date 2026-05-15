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

struct Dependencies<'a> {
    path: &'a Path,
    dependencies: &'a [PathBuf],
    env: &'a [String],
    cmdline: &'a [&'a str],
}

impl<'a> Dependencies<'a> {
    fn make_address(&self) -> anyhow::Result<lager::Address> {
        let mut hasher = blake3::Hasher::new();

        hasher.update(self.path.as_os_str().as_encoded_bytes());
        hasher.update(&[0u8]);
        hasher.update(&[0u8]);

        for d in self.dependencies {
            let file = std::fs::File::open(d)?;
            hasher.update_reader(file)?;
            hasher.update(&[0u8]);
        }
        hasher.update(&[0u8]);

        for e in self.env {
            hasher.update(e.as_bytes());
            hasher.update(&[0u8]);
        }
        hasher.update(&[0u8]);

        for e in self.cmdline {
            hasher.update(e.as_bytes());
            hasher.update(&[0u8]);
        }
        hasher.update(&[0u8]);

        let mut buf = [0u8; _];
        hasher.finalize_xof().fill(&mut buf);
        Ok(buf.into())
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let cache_dir = args.cache_dir;

    if !cache_dir.exists() {
        fs::create_dir_all(&cache_dir)?;
    }

    let lager = Lager::new(Path::new(&cache_dir))?;

    let zivid_env = collect_zivid_env();

    let base_dir = match args.base_dir {
        Some(d) => d,
        None => find_repo_root()?,
    };

    let generated_object = args
        .generated_object
        .strip_prefix(&base_dir)
        .unwrap_or(&args.generated_object);

    let generated_header = args
        .generated_header
        .strip_prefix(&base_dir)
        .unwrap_or(&args.generated_header);

    let cmdline = args
        .builder
        .iter()
        .map(|c| {
            Path::new(c)
                .strip_prefix(&base_dir)
                .ok()
                .and_then(|p| p.to_str())
                .unwrap_or(c)
        })
        .collect::<Vec<&str>>();

    let object_dependencies = Dependencies {
        path: generated_object,
        dependencies: &args.dependencies,
        env: &zivid_env,
        cmdline: &cmdline,
    };

    let header_dependencies = Dependencies {
        path: generated_header,
        dependencies: &args.dependencies,
        env: &zivid_env,
        cmdline: &cmdline,
    };

    let header_address = header_dependencies.make_address()?;
    let object_address = object_dependencies.make_address()?;
    if cache_hit(
        &args.generated_object,
        &args.generated_header,
        &lager,
        &object_address,
        &header_address,
    )? {
        return Ok(());
    }

    let status = Command::new(&args.builder[0])
        .args(&args.builder[1..])
        .status()?;

    if status.success() {
        lager.store_at(&object_address, &args.generated_object)?;
        lager.store_at(&header_address, &args.generated_header)?;
    }

    try_cleaning_up(lager)?;

    Ok(())
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

fn collect_zivid_env() -> Vec<String> {
    let mut v = std::env::vars()
        .filter_map(|(k, v)| k.starts_with("ZIVID_").then(|| format!("{}={}", k, v)))
        .collect::<Vec<_>>();
    v.sort();
    v
}

fn cache_hit(
    generated_object: &Path,
    generated_header: &Path,
    lager: &Lager,
    object_address: &Address,
    header_address: &Address,
) -> anyhow::Result<bool> {
    match (
        lager.retrieve(object_address, generated_object),
        lager.retrieve(header_address, generated_header),
    ) {
        (Ok(_), Ok(_)) => {
            println!(
                "Cache hits for Halide objects: {:?} and {:?}",
                generated_object, generated_header
            );
            Ok(true)
        }
        (
            Err(lager::Error::NotFound { address: _ }),
            Err(lager::Error::NotFound { address: _ }),
        ) => Ok(false),
        (Err(oe), Err(he)) => Err(anyhow::anyhow!(oe).context(he)),
        (Ok(_), Err(e)) => Err(anyhow::anyhow!(e).context("Retrieving the object was successful")),
        (Err(e), Ok(_)) => Err(anyhow::anyhow!(e).context("Retrieving the header was successful")),
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
