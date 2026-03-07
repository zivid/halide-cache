use clap::Parser;
use lager::{Address, LRU, Lager};
use named_lock::NamedLock;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, num_args = 1..)]
    dependencies: Vec<PathBuf>,
    #[arg(long)]
    generated_object: PathBuf,
    #[arg(long)]
    generated_header: PathBuf,
    #[arg(last = true)]
    builder: Vec<String>,
}

const MAX_CACHE_SIZE_BYTES: u64 = 10737418240; // 10 GiB

fn main() {
    let args = Args::parse();

    let cache_dir = match env::var("HALIDE_CACHE_DIR") {
        Ok(env) => env,
        Err(e) => {
            eprintln!("HALIDE_CACHE_DIR environment variable not set: {}", e);
            std::process::exit(1);
        }
    };

    if !Path::new(&cache_dir).exists() {
        fs::create_dir_all(&cache_dir).unwrap();
    }
    let lager = match Lager::new(Path::new(&cache_dir)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to initialize Lager cache: {}", e);
            std::process::exit(1);
        }
    };

    let zivid_env = collect_zivid_env();

    let mut object_dependencies = zivid_env.clone();
    object_dependencies.push(
        args.generated_object
            .clone()
            .into_os_string()
            .into_string()
            .unwrap(),
    );

    let mut header_dependencies = zivid_env;
    header_dependencies.push(
        args.generated_header
            .clone()
            .into_os_string()
            .into_string()
            .unwrap(),
    );

    hash_all_dependencies_contents(
        args.dependencies,
        &mut object_dependencies,
        &mut header_dependencies,
    );

    let object_address = hash_vector(&object_dependencies);
    let header_address = hash_vector(&header_dependencies);

    if cache_hit(
        &args.generated_object,
        &args.generated_header,
        &lager,
        &object_address,
        &header_address,
    ) {
        return;
    }

    let status = Command::new(&args.builder[0])
        .args(&args.builder[1..])
        .status()
        .expect("Failed to execute command");

    if status.success() {
        lager
            .store_at(&object_address, &args.generated_object)
            .expect("Store at failed for the Halide object");
        lager
            .store_at(&header_address, &args.generated_header)
            .expect("Store at failed for the Halide header");
    }

    try_cleaning_up(lager);
}

fn try_cleaning_up(lager: Lager) {
    let lock = NamedLock::create("lager_lock").unwrap();
    if let Ok(_guard) = lock.lock() {
        let mut lru = LRU::new(lager);
        lru.scan().unwrap();
        if lru.lager_size() > MAX_CACHE_SIZE_BYTES {
            lru.evict_until(MAX_CACHE_SIZE_BYTES).unwrap();
        }
    }
}

fn collect_zivid_env() -> Vec<String> {
    let mut v = std::env::vars()
        .filter_map(|(k, v)| k.starts_with("ZIVID_").then(|| format!("{}={}", k, v)))
        .collect::<Vec<_>>();
    v.sort();
    return v;
}

fn hash_all_dependencies_contents(
    dependencies: Vec<PathBuf>,
    object_dependencies: &mut Vec<String>,
    header_dependencies: &mut Vec<String>,
) {
    for dep in &dependencies {
        let file_content_hash = match compute_hash_of_file(dep) {
            Ok(hash) => hash,
            Err(e) => {
                eprintln!("Failed to compute hash for {:?}: {}", dependencies, e);
                std::process::exit(1);
            }
        };
        object_dependencies.push(file_content_hash.clone());
        header_dependencies.push(file_content_hash);
    }
}

fn cache_hit(
    generated_object: &PathBuf,
    generated_header: &PathBuf,
    lager: &Lager,
    object_address: &Address,
    header_address: &Address,
) -> bool {
    match lager.retrieve(object_address, generated_object.as_path()) {
        Ok(_) => {
            println!("Cache hit for Halide object. {:?}", generated_object);
            match lager.retrieve(header_address, generated_header.as_path()) {
                Ok(_) => {
                    println!("Cache hit for Halide header {:?}", generated_header);
                    return true;
                }
                Err(e) => {
                    eprintln!("Error for Halide header {}", e);
                }
            }
        }
        Err(_) => {
            // Cache miss, proceed to build
        }
    }
    false
}

fn serialize_vector(vec: &[String]) -> String {
    format!(
        "[{}]",
        vec.iter()
            .map(|s| format!("\"{}\"", s))
            .collect::<Vec<String>>()
            .join(",")
    )
}
fn compute_hash_of_file<P: AsRef<std::path::Path>>(
    path: P,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    let hash = hasher.finalize();
    Ok(hash.to_hex().to_string())
}

fn hash_vector(vector: &[String]) -> Address {
    let input = serialize_vector(vector);
    let mut hasher = blake3::Hasher::new();
    hasher.update(input.as_bytes());
    let hash = hasher.finalize().to_hex().to_string();
    match hash.into_bytes().as_slice().try_into() {
        Ok(addr) => addr,
        Err(_) => panic!("Hash length mismatch"),
    }
}
