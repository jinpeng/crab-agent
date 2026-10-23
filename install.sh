#!/bin/sh
# Managed installer for `crab` — pi-style versioned installs with self-update.
#
#   curl -fsSL https://raw.githubusercontent.com/jinpeng/crab-agent/main/install.sh | sh
#
# What it does:
#   - downloads a precompiled release binary (no Rust toolchain needed)
#   - verifies it against the release's SHA256SUMS
#   - installs into a versioned layout under ~/.crab/install/releases/<ver>/
#   - writes a tiny launcher at ~/.crab/bin/crab and symlinks it into
#     ~/.local/bin (add that to PATH if it is not already there)
#
# Re-running installs the latest release (or CRAB_VERSION). Manage the install
# with `crab update`; remove with `CRAB_UNINSTALL=1 sh install.sh`.
#
# Configuration, sessions, and metrics in ~/.crab are never touched by
# install, update, or uninstall.
set -eu

CRAB_REPO="${CRAB_REPO:-jinpeng/crab-agent}"
CRAB_BIN_NAME="crab"
CRAB_ROOT="${CRAB_HOME:-$HOME/.crab}"
CRAB_VERSION="${CRAB_VERSION:-}"
CRAB_UNINSTALL="${CRAB_UNINSTALL:-}"
CRAB_INSTALLER_URL="https://raw.githubusercontent.com/$CRAB_REPO/main/install.sh"

crab_err() { printf 'error: %s\n' "$*" >&2; exit 1; }

crab_usage() {
    cat <<'EOF'
Managed installer for crab (https://github.com/jinpeng/crab-agent).

  curl -fsSL https://raw.githubusercontent.com/jinpeng/crab-agent/main/install.sh | sh

Usage: install.sh [--version X.Y.Z] [--uninstall]

  --version X   install a specific release instead of the latest
  --uninstall   remove the managed install (keeps config/sessions/metrics)

Environment: CRAB_VERSION (same as --version), CRAB_HOME (default ~/.crab),
CRAB_REPO (default jinpeng/crab-agent).
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --uninstall) CRAB_UNINSTALL=1 ;;
        --version)
            shift
            [ $# -gt 0 ] || crab_err "--version needs a value (e.g. --version 0.4.0)"
            CRAB_VERSION="$1"
            ;;
        --version=*) CRAB_VERSION="${1#--version=}" ;;
        -h|--help) crab_usage; exit 0 ;;
        *) crab_err "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

# --- Uninstall ---------------------------------------------------------------
#
# Removes the launcher, versioned releases, and the entrypoint symlink — but
# keeps config.toml, pricing.toml, sessions/, and metrics.db.

if [ -n "$CRAB_UNINSTALL" ]; then
    if [ ! -f "$CRAB_ROOT/install/managed-install.json" ]; then
        crab_err "no managed installation found under $CRAB_ROOT (nothing to uninstall)"
    fi
    entrypoint=$(cat "$CRAB_ROOT/install/managed-install.json" 2>/dev/null \
        | sed -n 's/.*"path": *"\([^"]*\)".*/\1/p' | head -n1)
    if [ -n "$entrypoint" ] && [ -L "$entrypoint" ]; then
        target=$(readlink "$entrypoint" || true)
        case "$target" in
            "$CRAB_ROOT"/*) rm -f "$entrypoint"; printf 'removed %s\n' "$entrypoint" ;;
            *) printf 'kept %s (points elsewhere: %s)\n' "$entrypoint" "$target" ;;
        esac
    fi
    rm -rf "$CRAB_ROOT/bin" "$CRAB_ROOT/install"
    printf 'uninstalled crab (config, sessions, and metrics in %s were kept)\n' "$CRAB_ROOT"
    exit 0
fi

# --- Preflight: platform -> Rust target triple --------------------------------

crab_os=$(uname -s)
crab_arch=$(uname -m)
case "$crab_arch" in
    arm64|aarch64) crab_arch=aarch64 ;;
    x86_64|amd64)  crab_arch=x86_64 ;;
    *) crab_err "unsupported architecture: $crab_arch (build from source: cargo install --git https://github.com/$CRAB_REPO)" ;;
esac
case "$crab_os" in
    Darwin)
        [ "$crab_arch" = aarch64 ] || crab_err "no prebuilt binary for x86_64 macOS; build from source: cargo install --git https://github.com/$CRAB_REPO"
        crab_target=aarch64-apple-darwin
        ;;
    Linux)
        if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
            crab_err "no prebuilt binary for musl Linux; build from source: cargo install --git https://github.com/$CRAB_REPO"
        fi
        crab_target="$crab_arch-unknown-linux-gnu"
        ;;
    *) crab_err "unsupported OS: $crab_os (build from source: cargo install --git https://github.com/$CRAB_REPO)" ;;
esac

command -v tar >/dev/null 2>&1 || crab_err "tar is required"
if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
    crab_err "curl or wget is required"
fi
if command -v sha256sum >/dev/null 2>&1; then
    crab_sha256check() { sha256sum -c -; }
elif command -v shasum >/dev/null 2>&1; then
    crab_sha256check() { shasum -a 256 -c -; }
else
    crab_err "sha256sum or shasum is required"
fi
crab_fetch() {  # crab_fetch <url> <out>
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$2" "$1"
    else
        wget -qO "$2" "$1"
    fi
}

# --- Resolve the version to install -------------------------------------------

if [ -z "$CRAB_VERSION" ]; then
    printf 'Checking the latest crab release ...\n'
    crab_api=$(mktemp "${TMPDIR:-/tmp}/crab-release.XXXXXX")
    crab_fetch "https://api.github.com/repos/$CRAB_REPO/releases/latest" "$crab_api"
    CRAB_VERSION=$(sed -n 's/.*"tag_name": *"v\([0-9][0-9A-Za-z.+-]*\)".*/\1/p' "$crab_api" | head -n1)
    rm -f "$crab_api"
    [ -n "$CRAB_VERSION" ] || crab_err "could not determine the latest release from the GitHub API"
fi
case "$CRAB_VERSION" in
    ''|*[!0-9A-Za-z.+-]*|*..*) crab_err "invalid version: $CRAB_VERSION" ;;
esac
crab_tag="v$CRAB_VERSION"

if [ -f "$CRAB_ROOT/install/current-version" ]; then
    crab_current=$(head -n1 "$CRAB_ROOT/install/current-version")
    if [ "$crab_current" = "$CRAB_VERSION" ]; then
        printf 'crab %s is already installed; re-installing\n' "$CRAB_VERSION"
    else
        printf 'Updating crab %s -> %s\n' "$crab_current" "$CRAB_VERSION"
    fi
elif command -v "$CRAB_BIN_NAME" >/dev/null 2>&1; then
    crab_existing=$("$CRAB_BIN_NAME" --version 2>/dev/null || echo "crab (unknown)")
    printf 'note: found an existing non-managed %s; a managed install under %s will take precedence when earlier on PATH\n' "$crab_existing" "$CRAB_ROOT" >&2
fi

# --- Download, verify, stage ---------------------------------------------------

crab_release_base="https://github.com/$CRAB_REPO/releases/download/$crab_tag"
crab_archive="crab-$CRAB_VERSION-$crab_target.tar.gz"
crab_staging="$CRAB_ROOT/install/staging"
rm -rf "$crab_staging"
mkdir -p "$crab_staging"

printf 'Downloading %s ...\n' "$crab_archive"
crab_fetch "$crab_release_base/$crab_archive" "$crab_staging/$crab_archive"
crab_fetch "$crab_release_base/SHA256SUMS" "$crab_staging/SHA256SUMS"

printf 'Verifying checksum ...\n'
(
    cd "$crab_staging"
    grep "  $crab_archive\$" SHA256SUMS | crab_sha256check >/dev/null \
        || crab_err "checksum mismatch for $crab_archive"
)

printf 'Installing crab %s (%s) ...\n' "$CRAB_VERSION" "$crab_target"
(
    cd "$crab_staging"
    tar -xzf "$crab_archive"
)
[ -f "$crab_staging/$CRAB_BIN_NAME" ] || crab_err "archive did not contain $CRAB_BIN_NAME"
chmod 755 "$crab_staging/$CRAB_BIN_NAME"

# --- Switch: release dir first, version pointer last ---------------------------
#
# The launcher reads current-version at every start, so writing it last is the
# commit point — a crash before this line leaves the previous version active.

crab_release_dir="$CRAB_ROOT/install/releases/$CRAB_VERSION"
rm -rf "$crab_release_dir"
mkdir -p "$crab_release_dir"
mv "$crab_staging/$CRAB_BIN_NAME" "$crab_release_dir/$CRAB_BIN_NAME"
rm -rf "$crab_staging"

# Launcher: resolves itself through symlinks, reads current-version, execs the
# release binary. Single source of truth for "which crab is current".
mkdir -p "$CRAB_ROOT/bin"
cat > "$CRAB_ROOT/bin/$CRAB_BIN_NAME" <<'EOF'
#!/bin/sh
# crab launcher: exec the managed release selected by install/current-version.
case "$0" in
  */*) crab_launcher="$0" ;;
  *) crab_launcher=$(command -v "$0") || exit 127 ;;
esac
while [ -L "$crab_launcher" ]; do
  crab_link=$(readlink "$crab_launcher") || exit 1
  case "$crab_link" in
    /*) crab_launcher="$crab_link" ;;
    *) crab_launcher=${crab_launcher%/*}/$crab_link ;;
  esac
done
crab_bin_dir=${crab_launcher%/*}
crab_root=${crab_bin_dir%/*}
crab_current_file=$crab_root/install/current-version
if ! IFS= read -r crab_version < "$crab_current_file"; then
  printf 'Could not read managed crab version from %s.\n' "$crab_current_file" >&2
  exit 1
fi
case "$crab_version" in
  ""|.|..|*[!0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz._+-]*)
    printf 'Managed crab version file is invalid: %s\n' "$crab_current_file" >&2
    exit 1
    ;;
esac
crab_bin=$crab_root/install/releases/$crab_version/crab
if [ ! -x "$crab_bin" ]; then
  printf 'Managed crab executable is missing: %s\n' "$crab_bin" >&2
  printf 'Re-run the installer: %s\n' "https://github.com/jinpeng/crab-agent#install" >&2
  exit 1
fi
exec "$crab_bin" "$@"
EOF
chmod 755 "$CRAB_ROOT/bin/$CRAB_BIN_NAME"

cat > "$CRAB_ROOT/install/managed-install.json" <<EOF
{
  "kind": "crab-managed-install",
  "schemaVersion": 1,
  "layout": "releases-v1",
  "entrypoint": {
    "type": "symlink",
    "path": "$HOME/.local/bin/$CRAB_BIN_NAME"
  }
}
EOF

printf '%s\n' "$CRAB_VERSION" > "$CRAB_ROOT/install/current-version"

# Prune old releases, keeping the current and the previous one.
for old in $(ls "$CRAB_ROOT/install/releases" \
        | sort -t. -k1,1nr -k2,2nr -k3,3nr \
        | sed -n '3,$p'); do
    rm -rf "$CRAB_ROOT/install/releases/$old"
done

# --- Entrypoint on PATH --------------------------------------------------------

crab_entry_dir="$HOME/.local/bin"
crab_entrypoint="$crab_entry_dir/$CRAB_BIN_NAME"
mkdir -p "$crab_entry_dir"
ln -sfn "$CRAB_ROOT/bin/$CRAB_BIN_NAME" "$crab_entrypoint"

crab_resolved=$("$CRAB_ROOT/bin/$CRAB_BIN_NAME" --version 2>/dev/null || echo 'crab (failed to run)')

case ":$PATH:" in
    *":$crab_entry_dir:"*)
        printf '\nInstalled %s -> %s\n' "$crab_resolved" "$crab_entrypoint"
        ;;
    *)
        printf '\nInstalled %s -> %s\n' "$crab_resolved" "$crab_entrypoint"
        path_line="export PATH=\"$crab_entry_dir:\$PATH\""
        if [ -t 0 ] && [ -t 1 ]; then
            printf '%s is not on your PATH. Add it now? [Y/n] ' "$crab_entry_dir"
            read -r crab_reply
            case "$crab_reply" in
                ''|[Yy]*)
                    case "${SHELL:-}" in
                        */zsh)  crab_rc="$HOME/.zprofile" ;;
                        */bash) crab_rc="$HOME/.bashrc" ;;
                        *)      crab_rc='' ;;
                    esac
                    if [ -n "$crab_rc" ]; then
                        grep -qF "$crab_entry_dir" "$crab_rc" 2>/dev/null \
                            || printf '\n%s\n' "$path_line" >> "$crab_rc"
                        printf 'appended to %s — open a new shell for it to take effect\n' "$crab_rc"
                    else
                        printf 'add this to your shell profile:\n\n    %s\n' "$path_line"
                    fi
                    ;;
                *)
                    printf 'add this to your shell profile:\n\n    %s\n' "$path_line"
                    ;;
            esac
        else
            printf '\nNote: %s is not on your PATH. Add this to your shell profile:\n\n    %s\n' "$crab_entry_dir" "$path_line"
        fi
        ;;
esac

cat <<EOF

Managed install under $CRAB_ROOT
  update:    crab update
  uninstall: CRAB_UNINSTALL=1 sh install.sh  (from $CRAB_INSTALLER_URL)

Next steps:

  1. Point crab at a model with an API key:

       export CRAB_API_KEY=sk-...

     (OPENAI_API_KEY also works if CRAB_API_KEY is unset.)

  2. Try it out:

       crab --print "hi"

Docs: https://github.com/$CRAB_REPO
EOF
