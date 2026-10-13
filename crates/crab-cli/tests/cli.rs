use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn print_mode_runs_and_exits_zero() {
    let mut cmd = Command::cargo_bin("crab").unwrap();
    cmd.arg("--provider")
        .arg("mock")
        .arg("--print")
        .arg("hello")
        .env("CRAB_API_KEY", "test");
    cmd.assert().success();
}

#[test]
fn print_mode_streams_text_to_stdout() {
    let mut cmd = Command::cargo_bin("crab").unwrap();
    cmd.arg("--provider")
        .arg("mock")
        .arg("--print")
        .arg("hi")
        .env("CRAB_API_KEY", "test");
    // mock provider emits "mock response"; real --print must stream it out
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("mock response"));
}
