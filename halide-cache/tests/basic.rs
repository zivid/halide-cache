#[test]
fn run_basic_tests() {
    let success = std::process::Command::new("tests/run")
        .arg("tests/builder")
        .status();
    assert!(success.is_ok());
    assert!(success.unwrap().success());
}
