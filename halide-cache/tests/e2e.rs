use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const BUILDER: &str = r#"
import sys
_, source, obj, header = sys.argv
data = open(source, "rb").read()
open(obj, "wb").write(b"object:" + data)
open(header, "wb").write(b"header:" + data)
"#;

struct Checkout {
    dir: TempDir,
}

impl Checkout {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("builder.py"), BUILDER).unwrap();
        fs::write(root.join("input.txt"), "kernel source\n").unwrap();
        fs::write(root.join("dep.txt"), "dependency 1\n").unwrap();
        fs::create_dir_all(root.join("pylib/halide")).unwrap();
        fs::write(root.join("pylib/halide/__init__.py"), "").unwrap();
        fs::write(root.join("pylib/halide/halide_.so"), "halide 19").unwrap();
        fs::create_dir_all(root.join("out")).unwrap();
        Checkout { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn write(&self, relative: &str, content: &str) {
        fs::write(self.path(relative), content).unwrap();
    }

    fn output(&self, relative: &str) -> String {
        fs::read_to_string(self.path(relative)).unwrap()
    }

    fn run(&self, cache: &Path, extra: &[&str]) -> Output {
        let root = self.dir.path();
        let (object, header) = (self.path("out/kernel.o"), self.path("out/kernel.h"));
        Command::new(env!("CARGO_BIN_EXE_halide-cache"))
            .env("PYTHONPATH", root.join("pylib"))
            .arg("--dependencies")
            .arg(self.path("dep.txt"))
            .arg(self.path("input.txt"))
            .arg("--cache-dir")
            .arg(cache)
            .arg("--base-dir")
            .arg(root)
            .arg("--generated-object")
            .arg(&object)
            .arg("--generated-header")
            .arg(&header)
            .args(extra)
            .arg("--")
            .arg("python")
            .arg(self.path("builder.py"))
            .arg(self.path("input.txt"))
            .arg(&object)
            .arg(&header)
            .output()
            .unwrap()
    }

    fn ok(&self, cache: &Path, extra: &[&str]) -> (String, String) {
        let out = self.run(cache, extra);
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        );
        assert!(
            out.status.success(),
            "halide-cache failed:\n{stdout}\n{stderr}"
        );
        (stdout, stderr)
    }

    fn assert_built_from(&self, source: &str) {
        assert_eq!(self.output("out/kernel.o"), format!("object:{source}"));
        assert_eq!(self.output("out/kernel.h"), format!("header:{source}"));
    }
}

fn files_with(dir: &Path, pred: impl Fn(&str) -> bool) -> usize {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && pred(&e.file_name().to_string_lossy()))
        .count()
}

fn entries(dir: &Path) -> usize {
    files_with(dir, |name| !name.contains(".tmp"))
}

const HIT: &str = "Cache hits for Halide target kernel";

#[test]
fn miss_then_hit_restores_identical_outputs() {
    let (checkout, cache) = (Checkout::new(), TempDir::new().unwrap());

    let (stdout, _) = checkout.ok(cache.path(), &[]);
    assert!(!stdout.contains(HIT));
    let stored = entries(cache.path());
    assert!(stored > 0);

    fs::remove_file(checkout.path("out/kernel.o")).unwrap();
    fs::remove_file(checkout.path("out/kernel.h")).unwrap();
    let (stdout, _) = checkout.ok(cache.path(), &[]);
    assert!(stdout.contains(HIT), "{stdout}");
    assert_eq!(entries(cache.path()), stored, "a hit stores nothing");
    checkout.assert_built_from("kernel source\n");
}

#[test]
fn changed_inputs_miss_and_reverted_inputs_hit_again() {
    let (checkout, cache) = (Checkout::new(), TempDir::new().unwrap());
    checkout.ok(cache.path(), &[]);
    let one = entries(cache.path());

    checkout.write("dep.txt", "dependency 2\n");
    let (stdout, _) = checkout.ok(cache.path(), &[]);
    assert!(!stdout.contains(HIT), "a changed dependency must miss");
    let two = entries(cache.path());
    assert!(two > one);

    checkout.write("input.txt", "other source\n");
    let (stdout, _) = checkout.ok(cache.path(), &[]);
    assert!(!stdout.contains(HIT), "a changed input must miss");
    checkout.assert_built_from("other source\n");
    assert!(entries(cache.path()) > two);

    checkout.write("dep.txt", "dependency 1\n");
    checkout.write("input.txt", "kernel source\n");
    let (stdout, _) = checkout.ok(cache.path(), &[]);
    assert!(
        stdout.contains(HIT),
        "reverting the inputs must hit: {stdout}"
    );
    checkout.assert_built_from("kernel source\n");
}

#[test]
fn checkouts_at_different_paths_share_entries() {
    let (a, b, cache) = (Checkout::new(), Checkout::new(), TempDir::new().unwrap());
    a.ok(cache.path(), &[]);
    let (stdout, _) = b.ok(cache.path(), &[]);
    assert!(
        stdout.contains(HIT),
        "the checkout path must not be part of the key: {stdout}"
    );
    b.assert_built_from("kernel source\n");
}
