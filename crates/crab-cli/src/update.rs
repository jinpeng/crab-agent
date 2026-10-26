//! Managed-install self-update: `crab update`.
//!
//! The installer ([install.sh](https://github.com/jinpeng/crab-agent)) lays
//! out a pi-style managed install:
//!
//! ```text
//! ~/.crab/bin/crab                  launcher script (resolves at every exec)
//! ~/.crab/install/current-version   the version the launcher execs
//! ~/.crab/install/managed-install.json
//! ~/.crab/install/releases/<ver>/crab
//! ```
//!
//! Updating means: download the new release's tarball, verify it against the
//! release's SHA256SUMS, stage it under `releases/<new>`, then rewrite
//! `current-version` — the last write is the commit point, so a crash before
//! it leaves the previous version active. The running process is unaffected;
//! the launcher picks up the new version on the next start.

use std::path::{Path, PathBuf};

use sha2::Digest;

/// Upstream repository the releases come from.
pub const REPO: &str = "jinpeng/crab-agent";

/// Map the running binary's platform to a release target triple.
/// Mirrors the matrix in .github/workflows/release.yml.
pub fn target_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

/// Managed-install root for a binary at `exe`, or None when `exe` is not a
/// release binary of a managed install (e.g. a cargo-install or dev build).
///
/// Expected layout: `<root>/install/releases/<ver>/<bin>` plus the
/// `managed-install.json` marker written by the installer.
pub fn managed_root_for_exe(exe: &Path) -> Option<PathBuf> {
    let exe = exe.canonicalize().ok()?;
    let releases = exe.parent()?.parent()?;
    let install = releases.parent()?;
    if releases.file_name()? != "releases" || install.file_name()? != "install" {
        return None;
    }
    let root = install.parent()?;
    if root.join("install/managed-install.json").is_file() {
        Some(root.to_path_buf())
    } else {
        None
    }
}

/// Managed-install root of the running crab binary, if any.
pub fn detect_managed_root() -> Option<PathBuf> {
    managed_root_for_exe(&std::env::current_exe().ok()?)
}

/// Version the launcher currently resolves, from `install/current-version`.
pub fn current_version(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("install/current-version")).ok()?;
    let v = text.lines().next()?.trim().to_string();
    if v.is_empty() || v.contains(['/', '\\']) {
        return None;
    }
    Some(v)
}

/// Extract the release tag from the GitHub "latest release" API JSON
/// (`"tag_name": "v0.4.0"`). The API answers on a single line, so scan for
/// the key rather than assume one field per line.
pub fn parse_latest_tag(json: &str) -> Option<String> {
    let key = "\"tag_name\"";
    let at = json.find(key)? + key.len();
    let rest = json[at..].trim_start().strip_prefix(':')?.trim();
    let rest = rest.trim_start_matches('"');
    let rest = rest.trim_start_matches('v');
    let end = rest.find(|c: char| c == '"' || c == ',' || c.is_whitespace())?;
    let tag = &rest[..end];
    parse_version(tag).map(|_| tag.to_string())
}

/// Parse `MAJOR.MINOR.PATCH` (pre-release suffix tolerated, ignored).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Strict semver-ish ordering: `latest` is installable over `current` only
/// when it compares greater.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => true,
    }
}

/// Verify `bytes` against the SUMS-file line naming `archive_name`
/// (`<hex>  <name>`). False when no matching line or the digest differs.
pub fn verify_sums(sums: &str, archive_name: &str, bytes: &[u8]) -> bool {
    let mut expected = sums.lines().filter_map(|l| {
        let (hex, name) = l.split_once("  ")?;
        name.trim_end().eq(archive_name).then_some(hex.trim())
    });
    let actual = sha256_hex(bytes);
    expected.any(|hex| hex.eq_ignore_ascii_case(&actual))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Error text for a failed request, naming the proxy escape hatch — on
/// networks where GitHub needs a proxy the fix is environmental, and the
/// message should say so instead of leaving a bare timeout.
fn net_err(e: impl std::fmt::Display) -> String {
    format!(
        "could not reach GitHub ({e}).\n\
         \x20   If your network requires a proxy: export HTTPS_PROXY=http://<host>:<port> first"
    )
}

/// Latest release version (without the `v` prefix) from the GitHub API.
pub async fn fetch_latest(client: &reqwest::Client) -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let json = client
        .get(&url)
        .send()
        .await
        .map_err(net_err)?
        .text()
        .await
        .map_err(net_err)?;
    parse_latest_tag(&json).ok_or_else(|| {
        "could not parse the latest release tag (unexpected GitHub API response)".to_string()
    })
}

/// Download one release tarball into `staging`, verify it against the
/// release's SHA256SUMS, and extract it (via the system `tar`, which the
/// installer already requires). Returns the extracted binary path.
pub async fn download_release(
    client: &reqwest::Client,
    version: &str,
    triple: &str,
    staging: &Path,
) -> Result<PathBuf, String> {
    let base = format!("https://github.com/{REPO}/releases/download/v{version}");
    let archive_name = format!("crab-{version}-{triple}.tar.gz");

    let fetch = |url: String| async move {
        client
            .get(&url)
            .send()
            .await
            .map_err(net_err)?
            .bytes()
            .await
            .map_err(net_err)
    };

    let sums = fetch(format!("{base}/SHA256SUMS")).await?;
    let sums =
        String::from_utf8(sums.to_vec()).map_err(|_| "SHA256SUMS is not text".to_string())?;
    let tarball = fetch(format!("{base}/{archive_name}")).await?;
    if !verify_sums(&sums, &archive_name, &tarball) {
        return Err(format!("checksum mismatch for {archive_name}; aborting"));
    }

    std::fs::create_dir_all(staging).map_err(|e| format!("staging dir: {e}"))?;
    let tar_path = staging.join(&archive_name);
    std::fs::write(&tar_path, &tarball).map_err(|e| format!("staging write: {e}"))?;
    let status = std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(&archive_name)
        .current_dir(staging)
        .status()
        .map_err(|e| format!("tar: {e}"))?;
    if !status.success() {
        return Err("tar could not extract the archive".to_string());
    }
    let binary = staging.join("crab");
    binary.is_file().then_some(binary).ok_or_else(|| {
        std::fs::remove_dir_all(staging).ok();
        "archive did not contain the crab binary".to_string()
    })
}

/// `crab update`: bring a managed install to the latest release.
/// Prints progress; returns a non-zero exit on failure.
pub async fn run_update() -> Result<(), String> {
    for line in update_once().await? {
        println!("{line}");
    }
    Ok(())
}

/// The update flow as display lines (no printing) — the TUI shows them as a
/// notice bubble, the CLI prints them.
pub async fn update_once() -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    let Some(root) = detect_managed_root() else {
        return Err(
            "this crab is not a managed install (no managed-install.json next to it).\n\
             \x20   Managed:   curl -fsSL https://raw.githubusercontent.com/jinpeng/crab-agent/main/install.sh | sh\n\
             \x20   Or rebuild: cargo install --force --git https://github.com/jinpeng/crab-agent"
                .to_string(),
        );
    };
    let Some(triple) = target_triple() else {
        return Err(format!(
            "no prebuilt binary for {}/{}; build from source: cargo install --force --git https://github.com/{REPO}",
            std::env::consts::OS,
            std::env::consts::ARCH,
        ));
    };
    let current = current_version(&root).ok_or_else(|| {
        "managed-install.json exists but current-version is unreadable; re-run the installer"
            .to_string()
    })?;

    let client = reqwest::Client::builder()
        .user_agent(format!("crab-update/{current}"))
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let latest = fetch_latest(&client).await?;
    if !is_newer(&latest, &current) {
        lines.push(format!("crab v{current} is up to date"));
        return Ok(lines);
    }
    lines.push(format!(
        "Updating crab v{current} -> v{latest} ({triple}) ..."
    ));

    let staging = root.join("install/staging");
    std::fs::remove_dir_all(&staging).ok();
    let binary = download_release(&client, &latest, triple, &staging).await?;

    let release_dir = root.join("install/releases").join(&latest);
    std::fs::remove_dir_all(&release_dir).ok();
    std::fs::create_dir_all(&release_dir).map_err(|e| format!("release dir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).ok();
    }
    std::fs::rename(&binary, release_dir.join("crab")).map_err(|e| format!("install: {e}"))?;
    std::fs::remove_dir_all(&staging).ok();

    // Commit point: the launcher re-reads this file at every start.
    std::fs::write(root.join("install/current-version"), format!("{latest}\n"))
        .map_err(|e| format!("current-version: {e}"))?;

    prune_releases(&root)?;
    lines.push(format!(
        "updated to v{latest} — new `crab` processes pick it up automatically"
    ));
    Ok(lines)
}

/// Keep the current and previous release, remove the rest.
fn prune_releases(root: &Path) -> Result<(), String> {
    type Versioned = (std::cmp::Reverse<(u64, u64, u64)>, PathBuf);
    let releases_dir = root.join("install/releases");
    let mut versions: Vec<Versioned> = std::fs::read_dir(&releases_dir)
        .map_err(|e| format!("releases dir: {e}"))?
        .filter_map(|e| {
            let path = e.ok()?.path();
            let v = parse_version(path.file_name()?.to_str()?)?;
            Some((std::cmp::Reverse(v), path))
        })
        .collect();
    versions.sort();
    for (_, path) in versions.drain(2.min(versions.len())..) {
        std::fs::remove_dir_all(&path).map_err(|e| format!("prune {}: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_root(tmp: &tempfile::TempDir) -> PathBuf {
        let root = tmp.path().join(".crab");
        std::fs::create_dir_all(root.join("install/releases/0.4.0")).unwrap();
        std::fs::write(
            root.join("install/managed-install.json"),
            "{\"kind\":\"crab-managed-install\"}",
        )
        .unwrap();
        std::fs::write(root.join("install/current-version"), "0.4.0\n").unwrap();
        std::fs::write(root.join("install/releases/0.4.0/crab"), b"bin").unwrap();
        root
    }

    #[test]
    fn detects_managed_root_from_release_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fake_root(&tmp);
        let exe = root.join("install/releases/0.4.0/crab");
        // canonicalize 后比较（macOS 上 /var → /private/var）。
        let want = root.canonicalize().unwrap();
        assert_eq!(managed_root_for_exe(&exe), Some(want.clone()));
        // 非 managed 位置（如 cargo install 的 ~/.cargo/bin/crab）→ None。
        assert_eq!(
            managed_root_for_exe(&tmp.path().join("elsewhere/crab")),
            None
        );
        // 缺 marker → None。
        std::fs::remove_file(root.join("install/managed-install.json")).unwrap();
        assert_eq!(managed_root_for_exe(&exe), None);
    }

    #[test]
    fn reads_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fake_root(&tmp);
        assert_eq!(current_version(&root).as_deref(), Some("0.4.0"));
    }

    #[test]
    fn parses_latest_tag_from_api_json() {
        // 真实 API 是单行 JSON（字段间无换行）。
        let single = "{\"url\":\"x\",\"tag_name\":\"v0.4.0\",\"name\":\"v0.4.0\"}";
        assert_eq!(parse_latest_tag(single).as_deref(), Some("0.4.0"));
        // 排版过的多行形态同样可解析。
        let pretty = "{\n  \"url\": \"x\",\n  \"tag_name\": \"v0.4.0\",\n  \"name\": \"v0.4.0\"\n}";
        assert_eq!(parse_latest_tag(pretty).as_deref(), Some("0.4.0"));
        assert_eq!(parse_latest_tag("{\"name\": \"no tag here\"}"), None);
    }

    #[test]
    fn compares_versions_semverishly() {
        assert!(is_newer("0.4.1", "0.4.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.4.0", "0.4.0"));
        assert!(!is_newer("0.3.9", "0.4.0"));
        assert!(is_newer("0.5.0-rc1", "0.4.0"));
        assert_eq!(parse_version("v0.4.0"), None); // 不带 v 前缀的入口
    }

    #[test]
    fn verifies_sha256_sums() {
        // echo -n hello | sha256sum
        let sums = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  crab-0.4.0-aarch64-apple-darwin.tar.gz\n";
        assert!(verify_sums(
            sums,
            "crab-0.4.0-aarch64-apple-darwin.tar.gz",
            b"hello"
        ));
        assert!(!verify_sums(sums, "other.tar.gz", b"hello"));
        assert!(!verify_sums(
            sums,
            "crab-0.4.0-aarch64-apple-darwin.tar.gz",
            b"tampered"
        ));
        assert!(!verify_sums("", "anything", b"hello"));
    }

    #[test]
    fn target_triple_matches_ci_matrix_or_none() {
        // 本机必在矩阵内（macOS arm64）或显式 None。
        if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            assert_eq!(target_triple(), Some("aarch64-apple-darwin"));
        }
        if cfg!(target_os = "linux") {
            assert!(target_triple().is_some());
        }
    }

    #[test]
    fn prunes_old_releases_keeps_current_and_previous() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fake_root(&tmp);
        for v in ["0.3.0", "0.2.0", "0.1.0"] {
            std::fs::create_dir_all(root.join("install/releases").join(v)).unwrap();
        }
        prune_releases(&root).unwrap();
        let remaining: Vec<String> = std::fs::read_dir(root.join("install/releases"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        assert!(remaining.contains(&"0.4.0".into()), "{remaining:?}");
        assert!(remaining.contains(&"0.3.0".into()), "{remaining:?}");
        assert!(!remaining.contains(&"0.2.0".into()), "{remaining:?}");
        assert!(!remaining.contains(&"0.1.0".into()), "{remaining:?}");
    }
}
