use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn scaffolded_binary_runs() {
    let mut cmd = Command::cargo_bin("media-importer").expect("binary exists");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("media-importer Rust scaffold"));
}
