use clap::Parser;
use dirs::home_dir;
use lager::{Address, LRU, Lager};
use named_lock::NamedLock;
use std::fs;
use std::path::PathBuf;
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
    /// Additional path prefixes to strip from output paths and builder arguments
    /// before hashing, so that addresses are identical across machines. The base
    /// dir is always stripped.
    #[arg(long)]
    strip: Vec<PathBuf>,
    /// Opaque identifier of the builder (e.g. the conan reference and revision of
    /// the Halide generator package). Hashed into the address.
    #[arg(long)]
    builder_id: Option<String>,
    #[arg(long, default_value_os_t = home_dir().unwrap().join(".cache/halide-cache"))]
    cache_dir: PathBuf,
    #[arg(last = true)]
    builder: Vec<String>,
}

const MAX_CACHE_SIZE_BYTES: u64 = 10737418240; // 10 GiB

/// Bump whenever the set or encoding of hashed inputs changes, so that entries
/// produced by older versions can never be confused with new ones on a shared
/// cache server.
const KEY_SCHEME_VERSION: &str = "halide-cache-key-v2";

/// Everything that goes into a cache address except the output path. Both
/// outputs of one generator invocation share these.
struct KeyInputs<'a> {
    dependencies: &'a [PathBuf],
    env: &'a [String],
    cmdline: &'a [String],
    builder_id: Option<&'a str>,
}

impl KeyInputs<'_> {
    /// The address for the output at `path` (already stripped of machine
    /// specific prefixes).
    fn address_for(&self, path: &str) -> anyhow::Result<Address> {
        // Every field is NUL terminated and every group of fields ends with an
        // empty field, so that no two different inputs share a byte stream.
        fn field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
            hasher.update(bytes);
            hasher.update(&[0u8]);
        }
        const GROUP_END: &[u8] = b"";

        let mut h = blake3::Hasher::new();

        field(&mut h, KEY_SCHEME_VERSION.as_bytes());
        // Generated objects are only valid for the platform they were built on.
        field(&mut h, std::env::consts::OS.as_bytes());
        field(&mut h, std::env::consts::ARCH.as_bytes());
        field(&mut h, self.builder_id.unwrap_or("").as_bytes());
        field(&mut h, GROUP_END);

        field(&mut h, path.as_bytes());
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

/// One generator output: where it lives on disk and its cache address.
struct Output {
    path: PathBuf,
    address: Address,
}

/// The two outputs of one generator invocation. They are always cached and
/// looked up together.
struct Outputs {
    object: Output,
    header: Output,
}

impl Outputs {
    fn iter(&self) -> impl Iterator<Item = &Output> {
        [&self.object, &self.header].into_iter()
    }

    /// The Halide target name: the generated files always share its base name.
    fn target(&self) -> impl std::fmt::Display + '_ {
        self.object
            .path
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
    let stripper = PathStripper::new(std::iter::once(&base_dir).chain(&args.strip));
    let cmdline: Vec<String> = args.builder.iter().map(|c| stripper.strip(c)).collect();
    let zivid_env = collect_zivid_env();

    let inputs = KeyInputs {
        dependencies: &args.dependencies,
        env: &zivid_env,
        cmdline: &cmdline,
        builder_id: args.builder_id.as_deref(),
    };
    let output = |path: PathBuf| -> anyhow::Result<Output> {
        // Stripping is string based; a lossy conversion could make two
        // different paths hash alike, so refuse rather than guess.
        let utf8 = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("output path is not valid UTF-8: {path:?}"))?;
        let address = inputs.address_for(&stripper.strip(utf8))?;
        Ok(Output { path, address })
    };
    let outputs = Outputs {
        object: output(args.generated_object)?,
        header: output(args.generated_header)?,
    };

    if retrieve_both(&lager, &outputs)? {
        return Ok(());
    }

    let status = Command::new(&args.builder[0])
        .args(&args.builder[1..])
        .status()?;

    if status.success() {
        for o in outputs.iter() {
            lager.store_at(&o.address, &o.path)?;
        }
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

/// Removes machine specific path prefixes from strings before they are hashed,
/// similar to CTCACHE_STRIP. Prefixes are removed wherever they occur inside an
/// argument (so `--out=/repo/build/x` becomes `--out=build/x`), and path
/// separators are normalised to `/` so Windows and Unix agree.
struct PathStripper {
    prefixes: Vec<String>,
}

impl PathStripper {
    fn new<'a, I: IntoIterator<Item = &'a PathBuf>>(prefixes: I) -> Self {
        let mut prefixes: Vec<String> = prefixes
            .into_iter()
            .map(|p| {
                let mut s = normalize_separators(&p.to_string_lossy());
                if !s.ends_with('/') {
                    s.push('/');
                }
                s
            })
            .filter(|s| s != "/")
            .collect();
        // Longest first so that nested prefixes are handled deterministically.
        prefixes.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        prefixes.dedup();
        PathStripper { prefixes }
    }

    fn strip(&self, input: &str) -> String {
        let mut s = normalize_separators(input);
        for prefix in &self.prefixes {
            s = s.replace(prefix, "");
        }
        s
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

/// Extracts both outputs from the local cache. `Ok(false)` when neither is
/// there; a lone entry or a broken one is an error, since the two are always
/// stored together.
fn retrieve_both(lager: &Lager, outputs: &Outputs) -> anyhow::Result<bool> {
    let object = lager.retrieve(&outputs.object.address, &outputs.object.path);
    let header = lager.retrieve(&outputs.header.address, &outputs.header.path);
    match (object, header) {
        (Ok(()), Ok(())) => {
            println!("Cache hits for Halide target {}", outputs.target());
            Ok(true)
        }
        (Err(lager::Error::NotFound { .. }), Err(lager::Error::NotFound { .. })) => Ok(false),
        (Err(oe), Err(he)) => Err(anyhow::anyhow!(oe).context(he)),
        (Ok(()), Err(e)) => Err(anyhow::anyhow!(e).context("Retrieving the object was successful")),
        (Err(e), Ok(())) => Err(anyhow::anyhow!(e).context("Retrieving the header was successful")),
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
        let base = PathBuf::from("/home/user/repo");
        let extra = PathBuf::from("/opt/conan/");
        let stripper = PathStripper::new([&base, &extra]);

        assert_eq!(stripper.strip("/home/user/repo/build/x.o"), "build/x.o");
        assert_eq!(
            stripper.strip("--output=/home/user/repo/build/x.o"),
            "--output=build/x.o"
        );
        assert_eq!(
            stripper.strip("/opt/conan/halide/bin/gen"),
            "halide/bin/gen"
        );
        assert_eq!(stripper.strip("target=x86-64-linux"), "target=x86-64-linux");
    }

    #[test]
    fn root_prefix_is_ignored() {
        let root = PathBuf::from("/");
        let stripper = PathStripper::new([&root]);
        assert_eq!(stripper.strip("/usr/bin/gen"), "/usr/bin/gen");
    }
}
