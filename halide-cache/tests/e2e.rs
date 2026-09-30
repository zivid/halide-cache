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

fn temp_files(dir: &Path) -> usize {
    files_with(dir, |name| name.contains(".tmp"))
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

#[test]
fn halide_binary_is_part_of_the_key_but_its_python_layer_is_not() {
    let (checkout, cache) = (Checkout::new(), TempDir::new().unwrap());
    checkout.ok(cache.path(), &[]);

    checkout.write("pylib/halide/__init__.py", "# edited python layer\n");
    let (stdout, _) = checkout.ok(cache.path(), &[]);
    assert!(stdout.contains(HIT), "{stdout}");

    checkout.write("pylib/halide/halide_.so", "halide 20");
    let (stdout, _) = checkout.ok(cache.path(), &[]);
    assert!(!stdout.contains(HIT), "a new halide binary must miss");
}

#[test]
fn builder_without_halide_is_an_error() {
    let (checkout, cache) = (Checkout::new(), TempDir::new().unwrap());
    fs::remove_dir_all(checkout.path("pylib/halide")).unwrap();
    let has_halide = Command::new("python")
        .args([
            "-c",
            "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec('halide') else 1)",
        ])
        .status()
        .is_ok_and(|s| s.success());
    if has_halide {
        return;
    }
    let out = checkout.run(cache.path(), &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot import halide"), "{stderr}");
    assert_eq!(entries(cache.path()), 0);
}

#[test]
fn halide_without_native_library_is_an_error() {
    let (checkout, cache) = (Checkout::new(), TempDir::new().unwrap());
    fs::remove_file(checkout.path("pylib/halide/halide_.so")).unwrap();
    let out = checkout.run(cache.path(), &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no native library"), "{stderr}");
}

#[cfg(not(windows))]
mod remote {
    use super::*;
    use std::process::Child;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into()
    }

    fn server_binary() -> &'static Path {
        static BIN: OnceLock<PathBuf> = OnceLock::new();
        BIN.get_or_init(|| {
            let status = Command::new(env!("CARGO"))
                .args(["build", "--quiet", "-p", "halide-cache-server"])
                .status()
                .unwrap();
            assert!(status.success(), "cannot build halide-cache-server");
            Path::new(env!("CARGO_BIN_EXE_halide-cache")).with_file_name("halide-cache-server")
        })
    }

    struct Server {
        child: Child,
        url: String,
        data: TempDir,
    }

    impl Server {
        fn start(args: &[&str]) -> Self {
            let data = TempDir::new().unwrap();
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let child = Command::new(server_binary())
                .arg("--listen")
                .arg(format!("127.0.0.1:{port}"))
                .arg("--data-dir")
                .arg(data.path())
                .args(args)
                .env("RUST_LOG", "error")
                .spawn()
                .unwrap();
            let server = Server {
                child,
                url: format!("http://127.0.0.1:{port}"),
                data,
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            while agent()
                .get(format!("{}/healthz", server.url))
                .call()
                .is_err()
            {
                assert!(Instant::now() < deadline, "server did not start");
                std::thread::sleep(Duration::from_millis(50));
            }
            server
        }

        fn stats(&self) -> String {
            agent()
                .get(format!("{}/v1/stats", self.url))
                .call()
                .unwrap()
                .body_mut()
                .read_to_string()
                .unwrap()
        }

        fn put(&self, body: impl ureq::AsSendBody) -> u16 {
            let address = "a".repeat(128);
            agent()
                .put(format!("{}/v1/blobs/{address}", self.url))
                .send(body)
                .unwrap()
                .status()
                .as_u16()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    const REMOTE_HIT: &str = "Remote cache hits for Halide target kernel";

    #[test]
    fn a_build_on_one_machine_is_a_hit_on_another() {
        let server = Server::start(&[]);
        let (a, b) = (Checkout::new(), Checkout::new());
        let (cache_a, cache_b) = (TempDir::new().unwrap(), TempDir::new().unwrap());

        a.ok(cache_a.path(), &["--remote", &server.url]);
        assert!(entries(server.data.path()) > 0, "the build is uploaded");

        let (stdout, _) = b.ok(cache_b.path(), &["--remote", &server.url]);
        assert!(stdout.contains(REMOTE_HIT), "{stdout}");
        assert_eq!(a.output("out/kernel.o"), b.output("out/kernel.o"));
        assert_eq!(a.output("out/kernel.h"), b.output("out/kernel.h"));
        assert!(entries(cache_b.path()) > 0, "a remote hit is kept locally");

        let (stdout, _) = b.ok(cache_b.path(), &["--remote", &server.url]);
        assert!(
            stdout.contains(HIT) && !stdout.contains(REMOTE_HIT),
            "{stdout}"
        );
    }

    #[test]
    fn a_server_that_is_down_does_not_fail_the_build() {
        let url = {
            let server = Server::start(&[]);
            server.url.clone()
        };
        let (checkout, cache) = (Checkout::new(), TempDir::new().unwrap());
        let (_, stderr) = checkout.ok(cache.path(), &["--remote", &url]);
        assert!(stderr.contains("remote lookup failed"), "{stderr}");
        checkout.assert_built_from("kernel source\n");
    }

    #[test]
    fn a_corrupt_blob_on_the_server_is_discarded_and_rebuilt() {
        let server = Server::start(&[]);
        let (a, b) = (Checkout::new(), Checkout::new());
        a.ok(TempDir::new().unwrap().path(), &["--remote", &server.url]);
        for entry in walkdir::WalkDir::new(server.data.path()) {
            let entry = entry.unwrap();
            if entry.file_type().is_file() {
                fs::write(entry.path(), b"zst").unwrap();
            }
        }
        let (_, stderr) = b.ok(TempDir::new().unwrap().path(), &["--remote", &server.url]);
        assert!(stderr.contains("corrupt remote entry"), "{stderr}");
        b.assert_built_from("kernel source\n");
    }

    #[test]
    fn oversized_uploads_are_rejected() {
        let server = Server::start(&["--max-blob-size", "1KiB"]);
        let big = vec![7u8; 4096];
        assert_eq!(
            server.put(ureq::SendBody::from_reader(&mut &big[..])),
            413,
            "chunked"
        );
        assert_eq!(server.put(&big[..]), 413, "with a length");
        assert_eq!(entries(server.data.path()), 0);
        assert_eq!(temp_files(server.data.path()), 0);
        assert_eq!(server.put(&big[..512]), 201, "the server keeps working");
    }

    #[test]
    fn concurrent_clients_racing_under_eviction() {
        let server = Server::start(&["--size", "20", "--evict-interval", "1"]);
        let checkouts: Vec<Checkout> = (0..8).map(|_| Checkout::new()).collect();
        for _round in 0..3 {
            std::thread::scope(|s| {
                for checkout in &checkouts {
                    let url = server.url.clone();
                    s.spawn(move || {
                        let cache = TempDir::new().unwrap();
                        checkout.ok(cache.path(), &["--remote", &url]);
                        checkout.assert_built_from("kernel source\n");
                    });
                }
            });
            assert_eq!(temp_files(server.data.path()), 0);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while server.stats().contains("\"evictions\":0") {
            assert!(Instant::now() < deadline, "the server never evicted");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
