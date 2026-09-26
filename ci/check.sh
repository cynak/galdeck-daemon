#!/usr/bin/env bash
#
# Everything CI runs, in the order CI runs it, before you push.
#
#   ci/check.sh          fmt, clippy, tests, MSRV, layering
#   ci/check.sh fast     fmt, clippy, tests only -- the inner-loop subset
#   ci/check.sh msrv     just the 1.88 build
#
# CI is the authority; this is a local mirror of .github/workflows/ci.yml. If
# the two ever disagree, the workflow is right and this file is stale.
set -uo pipefail

REPO="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

MSRV="$(grep -m1 '^rust-version' Cargo.toml | cut -d'"' -f2)"
CLI="$REPO/../galdeck-cli"

bold() { printf '\n\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$*"; }

failed=()
step() {
    local name="$1"; shift
    bold "$name"
    if "$@"; then
        ok "$name"
    else
        bad "$name"
        failed+=("$name")
    fi
}

# ---- preconditions ---------------------------------------------------------
#
# This repository does not build alone: the control protocol and the
# configuration model come from the CLI by relative path. Saying so plainly
# beats a wall of "failed to read ../galdeck-cli/..." from cargo.
if [ ! -f "$CLI/Cargo.toml" ]; then
    bad "no checkout at $CLI"
    warn "git clone https://github.com/cynak/galdeck-cli $CLI"
    exit 1
fi
printf '\033[1mprotocol\033[0m   %s @ %s\n' \
    "$CLI" "$(git -C "$CLI" rev-parse --short HEAD 2>/dev/null || echo '?')"

# ---- which framework are we building against? ------------------------------
#
# .cargo/config.toml can patch `galdeck` to the checkout beside this one. That
# changes what a green run means, so it is stated up front. It also costs the
# lockfile its source and checksum for that package, which is why --locked is
# dropped while the patch is live.
PATCHED=0
if [ -f .cargo/config.toml ] && grep -q '^\[patch\.crates-io\]' .cargo/config.toml; then
    PATCHED=1
fi

LOCKED=(--locked)
if [ "$PATCHED" = "1" ]; then
    LOCKED=()
    printf '\033[1mframework\033[0m  local: %s\n' "$(sed -n 's/.*path *= *"\([^"]*\)".*/\1/p' .cargo/config.toml | head -1)"
    warn "building against a patched galdeck -- CI builds the published one"
    warn "Cargo.lock is modified as a side effect; do not commit it"
else
    printf '\033[1mframework\033[0m  crates.io, as CI builds it\n'
fi

# ---- the steps -------------------------------------------------------------

run_fmt()      { cargo fmt --all --check; }
run_clippy()   { cargo clippy --workspace --all-targets "${LOCKED[@]}" -- -D warnings; }
run_test()     { cargo test --workspace "${LOCKED[@]}"; }
run_layering() { ./ci/layering.sh; }

run_msrv() {
    if ! rustup toolchain list 2>/dev/null | grep -q "^$MSRV"; then
        warn "toolchain $MSRV not installed -- rustup toolchain install $MSRV"
        return 0
    fi
    cargo "+$MSRV" check --workspace --all-targets "${LOCKED[@]}"
}

case "${1:-all}" in
    fast)
        step "fmt"      run_fmt
        step "clippy"   run_clippy
        step "test"     run_test
        ;;
    msrv)
        step "msrv ($MSRV)" run_msrv
        ;;
    all)
        step "fmt"          run_fmt
        step "clippy"       run_clippy
        step "test"         run_test
        step "msrv ($MSRV)" run_msrv
        step "layering"     run_layering
        ;;
    -h|--help|help)
        sed -n '3,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        bad "unknown target: $1"; exit 1 ;;
esac

# ---- the lockfile guard ----------------------------------------------------
if [ "$PATCHED" = "1" ] && ! git diff --quiet -- Cargo.lock 2>/dev/null; then
    bold "lockfile"
    warn "Cargo.lock carries the patch. Before committing:"
    warn "    git checkout Cargo.lock"
fi

bold "summary"
if [ ${#failed[@]} -ne 0 ]; then
    for f in "${failed[@]}"; do bad "$f"; done
    exit 1
fi
ok "all checks passed"
