#!/usr/bin/env bash
#
# Architectural invariants that are cheaper to check than to remember.
#
# The workspace is split into more crates than a project this size would
# usually justify. Each boundary is worth its ceremony only because it is
# checked, so this script is what earns the split.
#
# The largest boundary is not in this repository at all: the control protocol
# and the configuration model live with the CLI, in ../galdeck-cli, and the
# dependency runs one way. The CLI repository enforces its own half; what is
# checked here is that this side keeps declaring it the way publishing needs.
set -uo pipefail

fail=0
note() { printf '  %s\n' "$*"; }
check() {
    printf '%-58s' "$1"
    shift
    if "$@"; then printf 'ok\n'; else printf 'FAIL\n'; fail=1; fi
}

# Lines of real code, with comments and doc comments stripped out.
code_grep() {
    local pattern="$1"; shift
    grep -rnE "$pattern" "$@" --include='*.rs' 2>/dev/null | grep -vE ':[[:space:]]*//'
}

no_match() {
    local out
    out=$(code_grep "$@")
    if [ -n "$out" ]; then
        printf 'FAIL\n'
        printf '%s\n' "$out" | sed 's/^/    /'
        return 1
    fi
    return 0
}

# galdeck-model was the other one; it lives in ../galdeck-cli now, and that
# repository checks it with the same two greps.
PURE=(crates/galdeck-core/src)

printf '\nlayering\n'

# The pure crate is pure so its tests can name every instant. A sixty
# second soak that takes sixty seconds is a soak nobody runs.
printf '%-58s' "no wall clock in the pure crate"
no_match '(Instant|SystemTime)::now\(\)' "${PURE[@]}" && printf 'ok\n' || fail=1

# Ambient authority in the pure crate would make it untestable in the same
# way. This targets spawning and sockets specifically rather than the whole
# `std::process` module: reading our own pid is neither, and a staging filename
# legitimately wants it.
printf '%-58s' "no subprocesses or sockets in the pure crate"
no_match 'std::process::(Command|exit|abort)|std::net::' "${PURE[@]}" && printf 'ok\n' || fail=1

# One file may name the concrete device type. Everything else goes through
# the Deck trait, which is what keeps the workspace testable with no keyboard.
printf '%-58s' "only the hardware adapter names Galleon"
offenders=$(code_grep 'Galleon' crates/*/src \
    | grep -v '^crates/galdeck-device/src/hardware.rs:')
if [ -n "$offenders" ]; then
    printf 'FAIL\n'; printf '%s\n' "$offenders" | sed 's/^/    /'; fail=1
else
    printf 'ok\n'
fi

# The dependency runs device -> core, never the reverse: the fake deck needs
# the simulated clock, so core has to be the floor of the graph.
printf '%-58s' "galdeck-core does not depend on galdeck-device"
if grep -q 'galdeck-device' crates/galdeck-core/Cargo.toml 2>/dev/null; then
    printf 'FAIL\n'; note 'galdeck-core/Cargo.toml names galdeck-device'; fail=1
else
    printf 'ok\n'
fi

# The two crates that come from the CLI repository must carry a version as
# well as a path. A bare path builds perfectly well here and then fails at
# `cargo publish`, which strips `path` and refuses a dependency with no
# version left -- a long way from the edit that caused it.
for shared in galdeck-ipc galdeck-model; do
    printf '%-58s' "$shared is declared with a version and a path"
    line=$(grep -E "^$shared = " Cargo.toml)
    if [ -z "$line" ]; then
        printf 'FAIL\n'; note "Cargo.toml does not declare $shared"; fail=1
    elif ! printf '%s' "$line" | grep -q 'version *='; then
        printf 'FAIL\n'; note "no version, so it cannot be published: $line"; fail=1
    elif ! printf '%s' "$line" | grep -q 'path *= *"\.\./galdeck-cli/'; then
        printf 'FAIL\n'; note "does not point at the sibling checkout: $line"; fail=1
    else
        printf 'ok\n'
    fi
done

# Nothing here may be reachable from the CLI. It is the half of the boundary
# this repository can break by accident -- adding a path dependency the other
# way round would compile and quietly make the CLI need the UI. Skipped when
# the sibling is absent, because then there is nothing to contradict.
printf '%-58s' "the CLI repository does not depend on this one"
if [ ! -d ../galdeck-cli ]; then
    printf 'skipped (no ../galdeck-cli)\n'
else
    back=$(grep -rnE 'galdeck-(daemon|core|device|http|plugin)' ../galdeck-cli/Cargo.toml ../galdeck-cli/crates/*/Cargo.toml 2>/dev/null)
    if [ -n "$back" ]; then
        printf 'FAIL\n'; printf '%s\n' "$back" | sed 's/^/    /'; fail=1
    else
        printf 'ok\n'
    fi
fi

printf '\n'
if [ "$fail" -ne 0 ]; then
    echo 'layering check failed'
    exit 1
fi
echo 'layering ok'
