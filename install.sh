#!/bin/sh
# Install `crab` from the git repo without cloning manually.
#
#   curl -fsSL https://raw.githubusercontent.com/jinpeng/crab-agent/main/install.sh | sh
#
# Safe to re-run: `cargo install --force` replaces an existing binary in place.
set -euo pipefail

REPO_URL="https://github.com/jinpeng/crab-agent"
BIN_NAME="crab"
CARGO_BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"
INSTALLED_BIN="$CARGO_BIN_DIR/$BIN_NAME"

err() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

# --- 1. Make sure a Rust toolchain is available -------------------------------

if ! command -v cargo >/dev/null 2>&1; then
    cat >&2 <<'EOF'
error: `cargo` was not found on your PATH.

`crab` is written in Rust and is built from source. Install a Rust toolchain
first:

    https://rustup.rs

rustup takes care of cargo along with the compiler. Once it is installed,
re-run this script.
EOF
    exit 1
fi

# --- 2. Build and install the binary ------------------------------------------
#
# The repository is a Cargo workspace; the `crab` binary lives in the
# `crab-cli` crate. `--git` fetches and builds straight from the repo (no
# manual clone), and `--bin crab` selects exactly that binary. `--force`
# makes re-runs overwrite an existing install instead of erroring out.

printf 'Installing %s from %s ...\n' "$BIN_NAME" "$REPO_URL"
cargo install --git "$REPO_URL" --bin "$BIN_NAME" --force

# --- 3. Report where it landed and whether its directory is on PATH -----------

if [ -x "$INSTALLED_BIN" ]; then
    printf '\nInstalled %s -> %s\n' "$BIN_NAME" "$INSTALLED_BIN"
else
    # Fall back to asking cargo where it put things (non-standard layouts).
    printf '\nInstalled %s (could not confirm %s).\n' "$BIN_NAME" "$INSTALLED_BIN"
fi

case ":$PATH:" in
    *":$CARGO_BIN_DIR:"*)
        : # already on PATH, nothing to do
        ;;
    *)
        cat <<EOF

Warning: $CARGO_BIN_DIR is not on your PATH, so \`$BIN_NAME\` may not run by name.
Add it by appending this to your shell profile (~/.bashrc, ~/.zshrc, ...):

    export PATH="$CARGO_BIN_DIR:\$PATH"

Then open a new shell (or re-source the profile).
EOF
        ;;
esac

# --- 4. Short next steps ------------------------------------------------------

cat <<'EOF'

Next steps:

  1. Point crab at a model with an API key:

       export CRAB_API_KEY=sk-...

     (OPENAI_API_KEY also works if CRAB_API_KEY is unset.)

  2. Try it out:

       crab --print "hi"

     Or start the interactive session by just running `crab`.

Docs and configuration: https://github.com/jinpeng/crab-agent
EOF
