#!/usr/bin/env bash
#
# Rebuild the CLI and the daemon from the checkouts beside each other,
# install both, and restart the daemon's service: the loop after pulling or
# editing either repository.
#
#   scripts/rebuild.sh               build both, install, restart the service
#   scripts/rebuild.sh --no-restart  build and install, leave the service be
#   scripts/rebuild.sh --cli         only the CLI (nothing to restart)
#   scripts/rebuild.sh --daemon      only the daemon
#   scripts/rebuild.sh --dry-run     say what would happen, change nothing
#
# The binaries go to $GALDECK_BIN_DIR, else $CARGO_HOME/bin (~/.cargo/bin),
# which is where the shipped systemd unit starts the daemon from. Each is
# installed by rename, so a daemon still running keeps its old binary until
# it is restarted -- which is also why a daemon started by hand, outside the
# service, goes on running the old build until someone stops it.
set -uo pipefail

REPO="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
CLI="$REPO/../galdeck-cli"
BIN_DIR="${GALDECK_BIN_DIR:-${CARGO_HOME:-$HOME/.cargo}/bin}"
UNIT=galdeck
SOCKET="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/galdeck.sock"

bold() { printf '\n\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$*"; }

usage() { sed -n '3,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

build_cli=1
build_daemon=1
restart=1
dry_run=0
for arg in "$@"; do
    case "$arg" in
        --cli)        build_daemon=0; restart=0 ;;
        --daemon)     build_cli=0 ;;
        --no-restart) restart=0 ;;
        --dry-run)    dry_run=1 ;;
        -h|--help)    usage; exit 0 ;;
        *)            bad "unknown option: $arg"; usage; exit 2 ;;
    esac
done

# Run a command in a directory: cargo finds .cargo/config.toml (the local
# framework patch, for one) from where it runs, not from --manifest-path.
in_dir() { (cd "$1" && shift && "$@"); }

# Run a command, or only say it on a dry run.
run() {
    if [ "$dry_run" = "1" ]; then
        printf '  would run: %s\n' "$*"
    else
        "$@"
    fi
}

# ---- preconditions ---------------------------------------------------------
#
# The daemon does not build alone: the protocol and the model come from the
# CLI's checkout by relative path.
if [ ! -f "$CLI/Cargo.toml" ]; then
    bad "no checkout at $CLI"
    warn "git clone https://github.com/cynak/galdeck-cli $CLI"
    exit 1
fi
if ! command -v cargo >/dev/null 2>&1; then
    bad "cargo is not on PATH"
    exit 1
fi

# Where a workspace's build lands, wherever CARGO_TARGET_DIR or its config
# puts it -- asked of cargo rather than guessed.
target_dir() {
    (cd "$1" && cargo metadata --no-deps --format-version 1 2>/dev/null) \
        | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p'
}

# Build one binary of one workspace in release mode and install it by
# rename, so the file is never half-written under a running process.
build_and_install() {
    local workspace="$1" package="$2" binary="$3"
    bold "building $binary ($(git -C "$workspace" rev-parse --short HEAD 2>/dev/null || echo '?'), from $workspace)"
    if ! run in_dir "$workspace" cargo build --release -p "$package"; then
        bad "the $binary build failed; nothing was installed"
        exit 1
    fi
    local built
    built="$(target_dir "$workspace")/release/$binary"
    if [ "$dry_run" = "0" ] && [ ! -x "$built" ]; then
        bad "no binary at $built after the build"
        exit 1
    fi
    run mkdir -p "$BIN_DIR"
    run install -m 755 "$built" "$BIN_DIR/.$binary.new"
    run mv -f "$BIN_DIR/.$binary.new" "$BIN_DIR/$binary"
    ok "installed $BIN_DIR/$binary"
}

[ "$build_cli" = "1" ] && build_and_install "$CLI" galdeck-cli galdeck
[ "$build_daemon" = "1" ] && build_and_install "$REPO" galdeck-daemon galdeck-daemon

if [ "$restart" = "0" ]; then
    exit 0
fi

# ---- restart the service ---------------------------------------------------

bold "restarting $UNIT.service"
if ! systemctl --user cat "$UNIT" >/dev/null 2>&1; then
    warn "no $UNIT.service is installed for this user, so nothing was restarted"
    warn "mkdir -p ~/.config/systemd/user && cp $REPO/systemd/galdeck.service ~/.config/systemd/user/"
    warn "systemctl --user daemon-reload && systemctl --user enable --now $UNIT"
    exit 0
fi

# The unit may start a binary other than the one just installed, in which case
# a restart changes nothing, and saying so beats a puzzling "still old".
exec_path="$(systemctl --user show -p ExecStart --value "$UNIT" | sed -n 's/.*path=\([^ ;]*\).*/\1/p' | head -1)"
if [ -n "$exec_path" ] && [ "$exec_path" != "$BIN_DIR/galdeck-daemon" ]; then
    warn "the service starts $exec_path, not $BIN_DIR/galdeck-daemon"
fi

# A daemon started by hand on the default socket keeps the service from
# starting at all: it exits at once with "another galdeck-daemon is already
# listening", and systemd retries every two seconds for ever. Stop it -- it is
# running an old build anyway, which is why this script is being run.
main_pid="$(systemctl --user show -p MainPID --value "$UNIT")"
# The daemon binds `galdeck.sock.<pid>` and renames it into place, and ss
# reports the name it bound, so match either.
holder="$(ss -xlpn 2>/dev/null \
    | awk -v sock="$SOCKET" '$5 == sock || index($5, sock ".") == 1' \
    | sed -n 's/.*pid=\([0-9]*\).*/\1/p' | head -1)"
if [ -n "$holder" ] && [ "$holder" != "$main_pid" ]; then
    warn "pid $holder, started outside the service, holds $SOCKET: stopping it"
    warn "  $(ps -o args= -p "$holder" 2>/dev/null)"
    run kill -TERM "$holder"
    if [ "$dry_run" = "0" ]; then
        for _ in $(seq 50); do
            kill -0 "$holder" 2>/dev/null || break
            sleep 0.1
        done
        if kill -0 "$holder" 2>/dev/null; then
            bad "pid $holder did not stop within 5 s; stop it yourself and run this again"
            exit 1
        fi
    fi
fi

# After many failed starts systemd may have given up on the unit; start it
# from a clean slate.
run systemctl --user reset-failed "$UNIT" 2>/dev/null
if ! run systemctl --user restart "$UNIT"; then
    bad "systemctl --user restart $UNIT failed"
    journalctl --user -u "$UNIT" -n 20 --no-pager -o cat
    exit 1
fi

if [ "$dry_run" = "1" ]; then
    exit 0
fi

# Up means answering on its socket, not merely started: a daemon can be
# running and still refusing, as the loop above shows.
cli="$BIN_DIR/galdeck"
command -v "$cli" >/dev/null 2>&1 || cli=galdeck
for _ in $(seq 100); do
    if "$cli" --socket "$SOCKET" ping >/dev/null 2>&1; then
        ok "$UNIT.service is up (pid $(systemctl --user show -p MainPID --value "$UNIT")) and answering on $SOCKET"
        break
    fi
    sleep 0.1
done
if ! "$cli" --socket "$SOCKET" ping >/dev/null 2>&1; then
    bad "$UNIT.service did not answer within 10 s"
    journalctl --user -u "$UNIT" -n 20 --no-pager -o cat
    exit 1
fi

# Other daemons -- a fake deck on a socket of its own, say -- were not
# restarted and still run whatever they were started with.
main_pid="$(systemctl --user show -p MainPID --value "$UNIT")"
others="$(ps -C galdeck-daemon -o pid=,args= 2>/dev/null | awk -v main="$main_pid" '$1 != main')"
if [ -n "$others" ]; then
    warn "other galdeck-daemon processes still run the build they were started with:"
    while IFS= read -r line; do warn "  $line"; done <<< "$others"
fi
ok "run \`galdeck ui\` to open the configuration UI"
