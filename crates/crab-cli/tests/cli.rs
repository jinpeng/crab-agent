use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn print_mode_runs_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("crab").unwrap();
    cmd.arg("--provider")
        .arg("mock")
        .arg("--print")
        .arg("hello")
        .current_dir(dir.path())
        .env("CRAB_API_KEY", "test");
    cmd.assert().success();
    // sessions are runtime artifacts under cwd, isolated to the tempdir
    assert!(dir.path().join(".crab/sessions").exists());
}

#[test]
fn print_mode_streams_text_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("crab").unwrap();
    cmd.arg("--provider")
        .arg("mock")
        .arg("--print")
        .arg("hi")
        .current_dir(dir.path())
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

#[test]
fn unknown_provider_lists_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".crab")).unwrap();
    std::fs::write(
        dir.path().join(".crab/config.toml"),
        "default_provider=\"a\"\n[providers.a]\nbase_url=\"http://x\"\nmodel=\"m\"\napi_key=\"k\"\n",
    ).unwrap();
    let mut cmd = assert_cmd::Command::cargo_bin("crab").unwrap();
    cmd.arg("--provider").arg("zzz").arg("--print").arg("hi")
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .assert().failure()
        .stderr(predicates::str::contains("zzz").and(predicates::str::contains("a")));
}

#[test]
fn config_profile_resolves_and_runs_mock_style() {
    // real provider would need network; just assert a valid profile name passes arg parsing
    // (we can't reach network here, so point at mock via a profile that maps to nothing —
    // simpler: verify config loads without error when profile exists)
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".crab")).unwrap();
    std::fs::write(
        dir.path().join(".crab/config.toml"),
        "default_provider=\"a\"\n[providers.a]\nbase_url=\"http://localhost:1\"\nmodel=\"m\"\napi_key=\"k\"\n",
    ).unwrap();
    // --provider a against an unreachable localhost should fail with a network error, not a config error
    let mut cmd = assert_cmd::Command::cargo_bin("crab").unwrap();
    cmd.arg("--provider").arg("a").arg("--print").arg("hi")
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .assert().failure()
        .stderr(predicates::str::contains("unknown provider").not());
}
