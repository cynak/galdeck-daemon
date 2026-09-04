# galdeck-daemon

A configurable daemon that turns the Stream Deck module in a **Corsair
Galleon 100 SD** keyboard into a launcher: label your keys, give them
icons, bind them to commands, page between sets, and drive volume with the
knobs.

Built on [galdeck](https://github.com/cynak/galdeck), the hardware
framework — this repository is the user-experience layer, and doubles as
the reference example of consuming that framework.

## Quick start

```sh
# device access (build needs libudev headers: apt install libudev-dev)
sudo cp systemd/../../galdeck/udev/70-galdeck.rules /etc/udev/rules.d/ 2>/dev/null \
  || echo "grab udev/70-galdeck.rules from the galdeck repository"
sudo udevadm control --reload && sudo udevadm trigger
# replug the keyboard

cargo install --path crates/galdeck-daemon
cargo install --path crates/galdeck-cli

mkdir -p ~/.config/galdeck && cp config/galdeck.example.toml ~/.config/galdeck/config.toml
mkdir -p ~/.config/systemd/user && cp systemd/galdeck.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now galdeck

galdeck status
```

## Configuring

Everything lives in `~/.config/galdeck/config.toml`; see the commented
[example](config/galdeck.example.toml). `galdeck reload` applies edits
without restarting.

Keys are numbered row-major from the top-left of the 3×4 grid:

```
 0  1  2
 3  4  5
 6  7  8
 9 10 11
```

A key can carry a label, a color, an icon, and either a shell command or a
page to switch to. An encoder can bind commands to press, clockwise, and
anticlockwise, and rests at a ring color of your choosing.

Turning a knob steps a lit segment around its ring and a click flashes it,
so a knob answers even when its command has no visible effect. Fast spins
run their detents in order through a bounded per-knob queue rather than
racing; `GALDECK_DELTA` carries the signed step count to your command.

## CLI

| Command | What it does |
|---|---|
| `galdeck detect` | Find the module and read its firmware (works without the daemon, changes nothing) |
| `galdeck status` | Daemon and device state |
| `galdeck page <name>` | Switch page |
| `galdeck brightness <0-100>` | Set panel brightness |
| `galdeck reload` | Re-read the config |
| `galdeck ping` | Check the daemon is alive |

## Running as a service

- Apps you launch from a key are children of the daemon, so the unit sets
  `KillMode=process`; without it, `systemctl --user restart galdeck` would
  close the windows you opened from the deck. Their memory still counts
  toward the service in `systemctl status` — cosmetic, not a leak.
- GUI actions need the systemd user manager to know your graphical session
  (`systemctl --user show-environment` should list `WAYLAND_DISPLAY` or
  `DISPLAY`). GNOME and KDE do this for you.
- Logs: `journalctl --user -u galdeck -f`. Set `RUST_LOG=debug` in the unit
  for per-event tracing.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The `galdeck` dependency is a path dependency to a sibling checkout until
the framework is published; adjust it in the workspace `Cargo.toml` if your
layout differs.

## License

MIT.
