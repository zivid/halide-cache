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

const KEY_SCHEME_VERSION: &str = "halide-cache-key-v1";

struct KeyInputs<'a> {
    dependencies: &'a [PathBuf],
    env: &'a [String],
    cmdline: &'a [String],
    builder_exe: &'a Path,
    halide: &'a Path,
}

impl KeyInputs<'_> {
    fn address(&self, object: &str, header: &str) -> anyhow::Result<Address> {
        fn field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
            hasher.update(bytes);
            hasher.update(&[0u8]);
        }
        const GROUP_END: &[u8] = b"";

        let mut h = blake3::Hasher::new();

        field(&mut h, KEY_SCHEME_VERSION.as_bytes());
        field(&mut h, std::env::consts::OS.as_bytes());
        field(&mut h, std::env::consts::ARCH.as_bytes());
        h.update_reader(fs::File::open(self.builder_exe)?)?;
        h.update(&[0u8]);
        hash_package(&mut h, self.halide)?;
        field(&mut h, GROUP_END);

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

fn hash_package(h: &mut blake3::Hasher, package: &Path) -> anyhow::Result<()> {
    const NATIVE: [&str; 4] = ["so", "pyd", "dll", "dylib"];
    let is_native = |p: &Path| {
        p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| NATIVE.contains(&e))
            || p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".so."))
    };
    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(package)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| is_native(p))
        .collect();
    if files.is_empty() {
        anyhow::bail!(
            "the halide package at {} contains no native library; it is not a usable Halide install",
            package.display()
        );
    }
    files.sort();
    for file in &files {
        let relative = file.strip_prefix(package).unwrap_or(file);
        h.update(normalize_separators(&relative.to_string_lossy()).as_bytes());
        h.update(&[0u8]);
        h.update_reader(fs::File::open(file)?)?;
        h.update(&[0u8]);
    }
    Ok(())
}

fn locate_halide(python: &Path) -> anyhow::Result<PathBuf> {
    const PROBE: &str = "import importlib.util, sys\n\
        spec = importlib.util.find_spec('halide')\n\
        print(spec.origin if spec is not None and spec.origin else '')";
    let output = Command::new(python)
        .args(["-c", PROBE])
        .output()
        .map_err(|e| anyhow::anyhow!("cannot run {}: {e}", python.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "{} failed while locating the halide package ({}): {}",
            python.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let origin = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if origin.is_empty() {
        anyhow::bail!(
            "{} cannot import halide: the Halide Python package is not installed for this interpreter",
            python.display()
        );
    }
    let origin = PathBuf::from(origin);
    let package = if origin
        .file_name()
        .is_some_and(|n| n.to_str().is_some_and(|n| n.starts_with("__init__.")))
    {
        origin.parent().map(Path::to_path_buf).unwrap_or(origin)
    } else {
        origin
    };
    if !package.exists() {
        anyhow::bail!(
            "halide package reported at {} does not exist",
            package.display()
        );
    }
    Ok(package)
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
    let stripper = PathStripper::new(&base_dir);
    let cmdline: Vec<String> = args.builder.iter().map(|c| stripper.strip(c)).collect();
    let zivid_env = collect_zivid_env();
    let builder_exe = which::which(&args.builder[0])
        .map_err(|e| anyhow::anyhow!("cannot locate builder {:?}: {e}", args.builder[0]))?;
    let halide = locate_halide(&builder_exe)?;

    let inputs = KeyInputs {
        dependencies: &args.dependencies,
        env: &zivid_env,
        cmdline: &cmdline,
        builder_exe: &builder_exe,
        halide: &halide,
    };
    let stripped = |path: &Path| -> anyhow::Result<String> {
        let utf8 = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("output path is not valid UTF-8: {path:?}"))?;
        Ok(stripper.strip(utf8))
    };
    let address = inputs.address(
        &stripped(&args.generated_object)?,
        &stripped(&args.generated_header)?,
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
        lager.store_at(&entry.address, &entry.paths())?;
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

struct PathStripper {
    prefix: String,
}

impl PathStripper {
    fn new(base_dir: &Path) -> Self {
        let mut prefix = normalize_separators(&base_dir.to_string_lossy());
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        PathStripper { prefix }
    }

    fn strip(&self, input: &str) -> String {
        let s = normalize_separators(input);
        if self.prefix == "/" {
            return s;
        }
        s.replace(&self.prefix, "")
    }
}

fn normalize_separators(s: &str) -> String {
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.to_owned()
    }
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
    match lager.retrieve(&entry.address, &entry.paths()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_prefix_anywhere_in_argument() {
        let stripper = PathStripper::new(Path::new("/home/user/repo"));

        assert_eq!(stripper.strip("/home/user/repo/build/x.o"), "build/x.o");
        assert_eq!(
            stripper.strip("--output=/home/user/repo/build/x.o"),
            "--output=build/x.o"
        );
        assert_eq!(stripper.strip("/opt/conan/bin/gen"), "/opt/conan/bin/gen");
        assert_eq!(stripper.strip("target=x86-64-linux"), "target=x86-64-linux");
    }

    #[test]
    fn the_interpreter_is_part_of_the_key() {
        let dir = std::env::temp_dir().join(format!("halide-cache-test-{}", std::process::id()));
        let halide = dir.join("halide");
        fs::create_dir_all(&halide).unwrap();
        fs::write(halide.join("halide_.so"), b"halide").unwrap();
        let (a, b) = (dir.join("python-a"), dir.join("python-b"));
        fs::write(&a, b"python 3.12").unwrap();
        fs::write(&b, b"python 3.13").unwrap();

        let cmdline = ["gen".to_owned()];
        let address = |python: &Path| {
            KeyInputs {
                dependencies: &[],
                env: &[],
                cmdline: &cmdline,
                builder_exe: python,
                halide: &halide,
            }
            .address("out.o", "out.h")
            .unwrap()
        };
        assert_ne!(address(&a), address(&b));
        assert_eq!(address(&a), address(&a));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn root_prefix_is_ignored() {
        let stripper = PathStripper::new(Path::new("/"));
        assert_eq!(stripper.strip("/usr/bin/gen"), "/usr/bin/gen");
    }
}
