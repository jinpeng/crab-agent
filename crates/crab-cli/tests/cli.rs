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

#[test]
fn continue_reuses_latest_session() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("crab")
        .unwrap()
        .arg("--provider")
        .arg("mock")
        .arg("--print")
        .arg("first")
        .current_dir(dir.path())
        .env("CRAB_API_KEY", "t")
        .assert()
        .success();
    let count1 = std::fs::read_dir(dir.path().join(".crab/sessions"))
        .unwrap()
        .count();
    Command::cargo_bin("crab")
        .unwrap()
        .arg("--provider")
        .arg("mock")
        .arg("--print")
        .arg("second")
        .arg("--continue")
        .current_dir(dir.path())
        .env("CRAB_API_KEY", "t")
        .assert()
        .success();
    let count2 = std::fs::read_dir(dir.path().join(".crab/sessions"))
        .unwrap()
        .count();
    assert_eq!(count1, count2, "--continue must reuse, not create");
    assert!(count1 >= 1);
}

#[test]
fn continue_without_prior_session_errors() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("crab")
        .unwrap()
        .arg("--provider")
        .arg("mock")
        .arg("--print")
        .arg("x")
        .arg("--continue")
        .current_dir(dir.path())
        .env("CRAB_API_KEY", "t")
        .assert()
        .failure();
}
