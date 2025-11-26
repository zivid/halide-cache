use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use clap::Parser;
use lager::Lager;
use serde::Serialize;

#[derive(Parser, Serialize, Debug)]
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

#[derive(Serialize)]
struct DependencyList {
    files: Vec<String>,
}

fn main() {
    let args = Args::parse();
    let cache_dir = env::var("HALIDE_CACHE_DIR").unwrap();

    if args.builder.is_empty() {
        eprintln!("Usage: halide-cache --dependencies <files...> -- <command> [args...]");
        std::process::exit(1);
    }

    let lager = match Lager::new(Path::new(&cache_dir)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to initialize Lager cache: {}", e);
            std::process::exit(1);
        }
    };
    let mut object_dependencies = DependencyList {
        files: vec![],
    };

    let mut header_dependencies = DependencyList {
        files: vec![],
    };

    for dep in &args.dependencies {
        let hash = match compute_hash_of_file_with_blake3(dep) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("Failed to compute hash for {:?}: {}", args.dependencies, e);
                std::process::exit(1);
            }
        };
        object_dependencies.files.push(hash.clone());
        header_dependencies.files.push(hash);
    }
    object_dependencies.files.push("object".to_string());
    header_dependencies.files.push("header".to_string());
    let object_json = serde_json::to_string(&object_dependencies).unwrap();
    let header_json = serde_json::to_string(&header_dependencies).unwrap();
    let object_hash = hash_string(&object_json);
    let header_hash = hash_string(&header_json);
    let object_hashish = &object_hash.into_bytes().as_slice().try_into().unwrap();
    let header_hashish = &header_hash.into_bytes().as_slice().try_into().unwrap();

    match lager.retrieve(&object_hashish, args.generated_object.as_path()) {
        Ok(_) => {
            println!("Cache hit for Halide object. {:?}", args.generated_object);
            match lager.retrieve(&header_hashish, args.generated_header.as_path()) {
                Ok(_) => {
                    // Cache hit, no need to build
                    println!("Cache hit for Halide header {:?}", args.generated_header);
                    return;
                }
                Err(e) => {
                    println!("Error for Halide header {}", e);
                    // Cache miss, proceed to build
                }
            }
        }
        Err(e) => {
            println!("Error for Halide object {}", e);
        }
    }
    let status = Command::new(&args.builder[0])
        .args(&args.builder[1..])
        .status()
        .expect("Failed to execute command");
    if status.success(){
        lager.store_at(&object_hashish, &args.generated_object).expect("Store at failed for the Halide object");
        lager.store_at(&header_hashish, &args.generated_header).expect("Store at failed for the Halide header");
        println!("Objects cached successfully. \n With hashes {} and {}", object_hashish, header_hashish);
    }
}

fn compute_hash_of_file_with_blake3<P: AsRef<std::path::Path>>(path: P) -> Result<String, Box<dyn std::error::Error>> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    let hash = hasher.finalize();
    Ok(hash.to_hex().to_string())
}

fn hash_string(input: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(input.as_bytes());
    hasher.finalize().to_hex().to_string()
}