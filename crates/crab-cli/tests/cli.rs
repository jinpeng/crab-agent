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

/// Hermetic managed-install round trip: install.sh installs the pinned
/// release into a temp HOME, the launcher runs it, uninstall keeps config.
/// Network-dependent (downloads the real release asset).
#[test]
fn managed_install_installs_runs_and_uninstalls() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let installer = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("install.sh");
    std::fs::create_dir_all(home.join(".crab")).unwrap();
    std::fs::write(
        home.join(".crab/config.toml"),
        "[providers.p]\napi_key = \"k\"\n",
    )
    .unwrap();

    // PATH 收窄，避免开发者本机已装的 crab 触发"existing install"分支差异。
    let minimal_path = "/usr/bin:/bin";
    let mut install = Command::new("sh");
    install
        .arg(&installer)
        .env("HOME", home)
        .env("PATH", minimal_path)
        .env("CRAB_VERSION", "0.4.0");
    install.assert().success();

    // Launcher resolves through the entrypoint symlink to the real binary.
    let entrypoint = home.join(".local/bin/crab");
    assert!(entrypoint.exists());
    Command::new(&entrypoint)
        .env("PATH", minimal_path)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains("0.4.0"));
    // Managed markers exist.
    assert_eq!(
        std::fs::read_to_string(home.join(".crab/install/current-version")).unwrap(),
        "0.4.0\n"
    );

    // Uninstall removes binaries but keeps config.
    let mut uninstall = Command::new("sh");
    uninstall
        .arg(&installer)
        .env("HOME", home)
        .env("PATH", minimal_path)
        .env("CRAB_UNINSTALL", "1");
    uninstall.assert().success();
    assert!(!home.join(".crab/bin/crab").exists());
    assert!(!home.join(".crab/install").exists());
    assert!(!entrypoint.exists());
    assert!(home.join(".crab/config.toml").exists(), "config kept");

    // Second uninstall refuses cleanly.
    let mut again = Command::new("sh");
    again
        .arg(&installer)
        .env("HOME", home)
        .env("PATH", minimal_path)
        .env("CRAB_UNINSTALL", "1");
    again.assert().failure();
}

/// download_release fetches the real v0.4.0 asset, verifies SHA256SUMS,
/// and extracts a runnable binary. Network-dependent.
#[test]
fn managed_update_download_verifies_and_extracts() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent("crab-test")
            .build()
            .unwrap();
        let staging = tempfile::tempdir().unwrap();
        let triple = crab_bin_target_triple();
        let binary = update::download_release(&client, "0.4.0", triple, staging.path())
            .await
            .unwrap();
        let out = Command::new(&binary).arg("--version").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("0.4.0"));
    });
}

/// crab-cli is a binary crate, so the integration tests pull update.rs in
/// directly to exercise the download/verify/extract chain for real.
#[path = "../src/update.rs"]
#[allow(dead_code)]
mod update;

fn crab_bin_target_triple() -> &'static str {
    update::target_triple().expect("this platform has a prebuilt release")
}
