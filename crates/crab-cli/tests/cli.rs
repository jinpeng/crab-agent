use assert_cmd::Command;
use predicates::prelude::*;

/// Hermetic crab invocation: HOME is the tempdir, so config load finds no
/// ~/.crab/config.toml — no MCP servers spawn (the suite must not depend on
/// the developer's machine, network, or npx), and no API keys leak in.
fn crab_in(dir: &tempfile::TempDir) -> Command {
    let mut cmd = Command::cargo_bin("crab").unwrap();
    cmd.current_dir(dir.path())
        .env("HOME", dir.path())
        .env("CRAB_API_KEY", "test");
    cmd
}

#[test]
fn print_mode_runs_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = crab_in(&dir);
    cmd.arg("--provider")
        .arg("mock")
        .arg("--print")
        .arg("hello");
    cmd.assert().success();
    // sessions are runtime artifacts under cwd, isolated to the tempdir
    assert!(dir.path().join(".crab/sessions").exists());
}

#[test]
fn print_mode_streams_text_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = crab_in(&dir);
    cmd.arg("--provider").arg("mock").arg("--print").arg("hi");
    // mock provider emits "mock response"; real --print must stream it out
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("mock response"));
}

#[test]
fn continue_reuses_latest_session() {
    let dir = tempfile::tempdir().unwrap();
    crab_in(&dir)
        .arg("--provider")
        .arg("mock")
        .arg("--print")
        .arg("first")
        .assert()
        .success();
    let count1 = std::fs::read_dir(dir.path().join(".crab/sessions"))
        .unwrap()
        .count();
    crab_in(&dir)
        .arg("--provider")
        .arg("mock")
        .arg("--print")
        .arg("second")
        .arg("--continue")
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
    crab_in(&dir)
        .arg("--provider")
        .arg("mock")
        .arg("--print")
        .arg("x")
        .arg("--continue")
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
    let mut cmd = crab_in(&dir);
    cmd.arg("--provider")
        .arg("zzz")
        .arg("--print")
        .arg("hi")
        .assert()
        .failure()
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
    let mut cmd = crab_in(&dir);
    cmd.arg("--provider")
        .arg("a")
        .arg("--print")
        .arg("hi")
        .assert()
        .failure()
        .stderr(predicates::str::contains("unknown provider").not());
}
