#!/usr/bin/env bash
#
# Architectural invariants that are cheaper to check than to remember.
#
# The workspace is split into more crates than a project this size would
# usually justify. Each boundary is worth its ceremony only because it is
# checked, so this script is what earns the split.
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

PURE=(crates/galdeck-model/src crates/galdeck-core/src)

printf '\nlayering\n'

# The pure crates are pure so their tests can name every instant. A sixty
# second soak that takes sixty seconds is a soak nobody runs.
printf '%-58s' "no wall clock in the pure crates"
no_match '(Instant|SystemTime)::now\(\)' "${PURE[@]}" && printf 'ok\n' || fail=1

# Ambient IO in the pure crates would make them untestable in the same way.
printf '%-58s' "no process or socket IO in the pure crates"
no_match 'std::(process|net)::' "${PURE[@]}" && printf 'ok\n' || fail=1

# One file may name the concrete device type. Everything else goes through
# the Deck trait, which is what keeps the workspace testable with no keyboard.
# galdeck-cli is allowed: `galdeck detect` deliberately bypasses the daemon
# and opens the device passively.
printf '%-58s' "only the hardware adapter names Galleon"
offenders=$(code_grep 'Galleon' crates/*/src \
    | grep -v '^crates/galdeck-device/src/hardware.rs:' \
    | grep -v '^crates/galdeck-cli/src/main.rs:')
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

printf '\n'
if [ "$fail" -ne 0 ]; then
    echo 'layering check failed'
    exit 1
fi
echo 'layering ok'
