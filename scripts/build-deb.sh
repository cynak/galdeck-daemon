#!/usr/bin/env bash
#
# Build a Debian package of the daemon and the `galdeck` command, from the
# checkouts beside each other, with the systemd user unit and the udev rule
# that gives the deck to whoever is at the seat.
#
#   scripts/build-deb.sh     build both binaries in release mode and package them
#
# The package lands in the daemon's target directory, under debian/. The
# release workflow runs exactly this; on a machine of your own it needs
# dpkg-dev (for dpkg-shlibdeps) and python3 besides cargo.
#
# What it does not install: the rule opening /dev/uinput to keystroke and
# scroll actions. That one lets any program at the seat inject input, so it
# ships under /usr/share/galdeck-daemon/udev/ for you to copy in on purpose.
set -euo pipefail
umask 022

REPO="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
CLI="$REPO/../galdeck-cli"
PKG=galdeck-daemon

bold() { printf '\n\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$*"; }

case "${1:-}" in
    "") ;;
    -h|--help) sed -n '3,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) bad "unknown option: $1"; exit 2 ;;
esac

# ---- preconditions ---------------------------------------------------------

if [ ! -f "$CLI/Cargo.toml" ]; then
    bad "no checkout at $CLI"
    warn "git clone https://github.com/cynak/galdeck-cli $CLI"
    exit 1
fi
for tool in cargo dpkg-deb dpkg-shlibdeps python3; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        bad "$tool is not on PATH"
        [ "$tool" = dpkg-shlibdeps ] && warn "sudo apt install dpkg-dev"
        exit 1
    fi
done

# Run a command in a directory: cargo finds .cargo/config.toml (the local
# framework patch, for one) from where it runs, not from --manifest-path.
in_dir() { (cd "$1" && shift && "$@"); }

# One field of one package from `cargo metadata`, asked of cargo rather than
# parsed out of Cargo.toml.
metadata() {
    local workspace="$1" package="$2" field="$3"
    in_dir "$workspace" cargo metadata --format-version 1 --locked \
        | python3 -c '
import json, sys
meta = json.load(sys.stdin)
package, field = sys.argv[1], sys.argv[2]
if field == "target_directory":
    print(meta[field])
else:
    print(next(p[field] for p in meta["packages"] if p["name"] == package))
' "$package" "$field"
}

# A package built against a patched framework is not the one CI would
# release. Fine for trying the packaging out; say so.
if [ -f "$REPO/.cargo/config.toml" ] && grep -q '^\[patch\.crates-io\]' "$REPO/.cargo/config.toml"; then
    warn "building against a patched galdeck (.cargo/config.toml) -- not what a release ships"
fi

# ---- build -----------------------------------------------------------------

for spec in "$CLI:galdeck-cli" "$REPO:galdeck-daemon"; do
    workspace="${spec%%:*}" package="${spec##*:}"
    bold "building $package ($(git -C "$workspace" rev-parse --short HEAD 2>/dev/null || echo '?'), from $workspace)"
    in_dir "$workspace" cargo build --release --locked -p "$package"
done

daemon_bin="$(metadata "$REPO" "" target_directory)/release/galdeck-daemon"
cli_bin="$(metadata "$CLI" "" target_directory)/release/galdeck"
out="$(metadata "$REPO" "" target_directory)/debian"
version="$(metadata "$REPO" galdeck-daemon version)"
# The framework's own udev rule, from the very source cargo built against.
framework="$(dirname -- "$(metadata "$REPO" galdeck manifest_path)")"

# A pre-release such as 0.2.0-rc.1 must sort before 0.2.0, and in Debian
# versions only a tilde does that.
deb_version="${version//-/\~}"
arch="$(dpkg --print-architecture)"
deb="$out/${PKG}_${deb_version}_${arch}.deb"
# The release commit's author maintains the package, unless told otherwise.
maintainer="${DEB_MAINTAINER:-$(git -C "$REPO" log -1 --format='%an <%ae>')}"
# Timestamps in the archive come from the commit, not from the clock, so a
# rebuild of the same commit gives the same package.
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git -C "$REPO" log -1 --format=%ct)}"

# ---- stage the tree --------------------------------------------------------

bold "packaging $PKG $deb_version ($arch)"
root="$out/$PKG"
rm -rf "$root"
mkdir -p "$root"

install -Dm755 "$daemon_bin" "$root/usr/bin/galdeck-daemon"
install -Dm755 "$cli_bin" "$root/usr/bin/galdeck"

# The shipped unit starts ~/.cargo/bin/galdeck-daemon and opens with how to
# copy it there by hand; the packaged one starts /usr/bin's and says how to
# turn it on.
unit="$root/usr/lib/systemd/user/galdeck.service"
mkdir -p "$(dirname -- "$unit")"
{
    printf '# galdeck user service, installed by the %s package.\n' "$PKG"
    printf '# Turn it on for yourself:  systemctl --user enable --now galdeck\n\n'
    sed -e '1,/^$/d' \
        -e 's|^ExecStart=%h/\.cargo/bin/galdeck-daemon|ExecStart=/usr/bin/galdeck-daemon|' \
        "$REPO/systemd/galdeck.service"
} > "$unit"
if ! grep -q '^ExecStart=/usr/bin/galdeck-daemon' "$unit"; then
    bad "systemd/galdeck.service no longer starts %h/.cargo/bin/galdeck-daemon; update this script"
    exit 1
fi

install -Dm644 "$framework/udev/70-galdeck.rules" "$root/usr/lib/udev/rules.d/70-galdeck.rules"
install -Dm644 "$REPO/udev/71-galdeck-uinput.rules" "$root/usr/share/$PKG/udev/71-galdeck-uinput.rules"

# The example configuration, for copying to ~/.config/galdeck.
mkdir -p "$root/usr/share/$PKG/config"
cp -R "$REPO/config/v2/." "$root/usr/share/$PKG/config/"
# Examples, not programs: the plugin in there runs as `python3 counter.py`.
find "$root/usr/share/$PKG/config" -type f -exec chmod 644 {} +

doc="$root/usr/share/doc/$PKG"
mkdir -p "$doc"
gzip -9n < "$REPO/README.md" > "$doc/README.md.gz"
# Not changelog.gz: in a package versioned like this one, that name is
# read as a changelog in Debian's own format.
gzip -9n < "$REPO/CHANGELOG.md" > "$doc/CHANGELOG.md.gz"
printf '%s (%s) unstable; urgency=medium\n\n  * Release %s. What changed is in CHANGELOG.md.gz.\n\n -- %s  %s\n' \
    "$PKG" "$deb_version" "$version" "$maintainer" "$(date -R -u -d "@$SOURCE_DATE_EPOCH")" \
    | gzip -9n > "$doc/changelog.gz"
"$REPO/scripts/third-party-licenses.py" "$REPO:galdeck-daemon" "$CLI:galdeck-cli" \
    | gzip -9n > "$doc/third-party-licenses.gz"
{
    echo "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"
    echo "Upstream-Name: $PKG"
    echo "Source: https://github.com/cynak/galdeck-daemon"
    echo "Comment: /usr/bin/galdeck is built from https://github.com/cynak/galdeck-cli,"
    echo " under the same licence. Both binaries also carry Rust crates under their own"
    echo " licences, listed with their full texts in third-party-licenses.gz."
    echo
    echo "Files: *"
    echo "Copyright: $(sed -n 's/^Copyright (c) //p' "$REPO/LICENSE" | head -1)"
    echo "License: MIT"
    echo
    echo "License: MIT"
    # The body of LICENSE, less its title and copyright line, in the
    # indented form the format asks for.
    sed -e '1,/^Copyright/d' "$REPO/LICENSE" | sed -e '1{/^$/d}' -e 's/^$/./' -e 's/^/ /'
} > "$doc/copyright"

# Maintainer scripts: apply the udev rule to a keyboard already plugged in,
# and forget it on removal. Neither may fail the install over it, and neither
# runs where there is no udev (a container, a chroot).
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ] && [ -d /run/udev ] && command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules || true
    udevadm trigger --subsystem-match=hidraw --action=change || true
fi
EOF
cat > "$root/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = remove ] && [ -d /run/udev ] && command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules || true
fi
EOF
chmod 755 "$root/DEBIAN/postinst" "$root/DEBIAN/postrm"
# Whatever the checkout's umask left group-writable, installed files are not.
chmod -R go-w "$root"

# ---- control ---------------------------------------------------------------

# dpkg-shlibdeps works out the shared libraries the binaries link and the
# lowest versions of them that provide every symbol used. It insists on a
# debian/control to read, though it needs nothing from it.
shlibs="$out/shlibdeps"
rm -rf "$shlibs"
mkdir -p "$shlibs/debian"
: > "$shlibs/debian/control"
depends="$(in_dir "$shlibs" dpkg-shlibdeps -O "$root/usr/bin/galdeck-daemon" "$root/usr/bin/galdeck" \
    | sed -n 's/^shlibs:Depends=//p')"
rm -rf "$shlibs"
if [ -z "$depends" ]; then
    bad "dpkg-shlibdeps found no dependencies"
    exit 1
fi

cat > "$root/DEBIAN/control" <<EOF
Package: $PKG
Version: $deb_version
Architecture: $arch
Maintainer: $maintainer
Installed-Size: $(du -sk --exclude=DEBIAN "$root" | cut -f1)
Depends: $depends
Section: utils
Priority: optional
Homepage: https://github.com/cynak/galdeck-daemon
Description: Stream Deck daemon for the Corsair Galleon 100 SD keyboard
 Turns the Stream Deck module in a Corsair Galleon 100 SD keyboard into a
 launcher: keys with labels, icons and actions, pages and profiles, clocks,
 graphs, media and timers, knobs for volume and more, keyboard lighting, and
 a configuration UI in the browser.
 .
 Includes the galdeck command, which controls the daemon and opens its
 configuration UI.
EOF

mkdir -p "$out"
rm -f "$deb"
dpkg-deb --root-owner-group -Zxz --build "$root" "$deb" >/dev/null
ok "$deb"
dpkg-deb --field "$deb" Version Depends | sed 's/^/    /'
