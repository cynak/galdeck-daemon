# galdeck-daemon

A configurable daemon that turns the Stream Deck module in a **Corsair
Galleon 100 SD** keyboard into a launcher: label your keys, give them
icons, bind them to commands, page between sets, and drive volume with the
knobs.

Built on [galdeck](https://github.com/cynak/galdeck), the hardware
framework — this repository is the user-experience layer, and doubles as
the reference example of consuming that framework.

It also needs [galdeck-cli](https://github.com/cynak/galdeck-cli), which owns
the `galdeck` command, the control protocol and the configuration model. The
dependency runs one way: the CLI is usable on its own, and this daemon and its
configuration UI are what require it. Check the two out side by side —
`../galdeck-cli` is where the build looks.

## Quick start

```sh
# both repositories, side by side: this one builds against ../galdeck-cli
git clone https://github.com/cynak/galdeck-cli
git clone https://github.com/cynak/galdeck-daemon
cd galdeck-daemon

# device access (build needs libudev headers: apt install libudev-dev)
sudo cp systemd/../../galdeck/udev/70-galdeck.rules /etc/udev/rules.d/ 2>/dev/null \
  || echo "grab udev/70-galdeck.rules from the galdeck repository"
sudo udevadm control --reload && sudo udevadm trigger
# replug the keyboard

cargo install --path crates/galdeck-daemon
cargo install --path ../galdeck-cli/crates/galdeck-cli

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

A key can carry a label, an icon, a style, and one of: a shell command, a
page to switch to, a profile to switch to, or `back = true` to return to
wherever the current page was reached from. An encoder can bind commands to
press, clockwise and anticlockwise, and rests at a ring colour.

A key can also answer differently to a tap, a hold and a double tap:

```toml
[[pages.keys]]
key = 1
label = "Browser"
exec = "xdg-open https://example.com"
hold = "xdg-open https://example.com/settings"
double = "xdg-open about:blank"
```

Binding `hold` or `double` changes *when* `exec` fires — until the key is
released nobody knows which gesture it was. A key with neither still fires the
instant it goes down, so nothing you have already gets slower.

### Profiles and themes

A configuration directory looks like this:

```
~/.config/galdeck/
  galdeck.toml          version, brightness, which profile to start in
  profiles/<id>.toml    one per profile; the id is the filename
  themes/<id>.toml      one per theme
```

A **profile** is a set of pages with a theme; switch between them with
`galdeck profile <name>` or from a key. A **theme** is a palette plus style
defaults. Anywhere a colour is expected you can write `#rrggbb` or `@name`
naming a palette entry, and a theme can `extends` another and override only
what differs — so redefining one palette entry moves everything that
referenced it.

Style resolves from the most general layer to the most specific: built-in
defaults, then the theme, the profile, the page, and finally the individual
key or encoder. A config with no theme at all looks exactly as it did before
themes existed.

See [config/v2](config/v2) for a commented worked example. A single-file
`config.toml` from before still works — it is translated on load, and nothing
on disk is rewritten.

Turning a knob steps a lit segment around its ring and a click flashes it,
so a knob answers even when its command has no visible effect. Fast spins
run their detents in order through a bounded per-knob queue rather than
racing; `GALDECK_DELTA` carries the signed step count to your command.

### Widgets and animations

A key can carry a **widget** — something that changes:

```toml
[[pages.keys]]
key = 9
label = "--:--"          # shown until the widget produces text

[pages.keys.widget]
kind = "clock"           # clock, date, cpu, memory, command
format = "%H:%M"
```

`command` runs a shell command on a worker thread and shows its first line of
output; it is killed if it takes more than five seconds, so a wedged script
cannot hold up the deck. A sample that produces the same text as last time
costs nothing at all.

A key or a ring can also carry an **animation**:

```toml
[pages.keys.animation]
kind = "breathe"         # pulse, breathe, blink; rings also spin and comet
period_ms = 3000
to = "@aurora-green"     # the colour it moves towards
```

Frames are rendered and encoded once when the page is applied and then cycled,
so playing one costs nothing. Turning or clicking a knob still takes
precedence over its ring animation.

### Plugins

A plugin is a separate process the daemon starts, speaking line-delimited JSON
over stdin and stdout. It lives in a directory under `plugins/`, and the
directory name is its id:

```
~/.config/galdeck/plugins/counter/
  plugin.toml       name, description, and the command to run
  counter.py        the plugin itself
```

A key hands itself over with:

```toml
[pages.keys.plugin]
id = "counter"

[pages.keys.plugin.options]     # passed through verbatim
label = "presses"
```

The plugin is told when its keys appear, disappear and are pressed, and can set
their text and colour — colours may be `@tokens`, so it stays inside your
palette. It cannot touch keys it was not given, push arbitrary images, or ask
the daemon to run commands.

Everything it does happens on threads of its own, and both message queues are
bounded, so a plugin that wedges or floods cannot slow the deck down — which
matters because the thread that owns the device also owns the module's
half-second keepalive.

See [config/v2/plugins/counter](config/v2/plugins/counter) for a working one in
forty lines of Python; [`galdeck-plugin`](crates/galdeck-plugin) has the message
types and a small Rust SDK.

## The configuration UI

```sh
galdeck-daemon --http 8787
# configuration UI: http://127.0.0.1:8787/?token=…
```

Open the printed address. The page shows the twelve keys as the images the
panel is actually being sent, the LCD, and both encoder rings; click one to
edit its label, icon, action and colour. Saving validates the whole
configuration first and refuses anything that would break it, and your
comments and formatting are preserved.

It is **off unless you ask for it**, binds loopback only, and requires the
token printed at startup — this surface can set the shell commands the daemon
runs, so it also refuses any request whose `Host` or `Origin` is not its own,
which is what stops a page you happen to visit from driving it through your
browser.

## CLI

The `galdeck` command lives in
[galdeck-cli](https://github.com/cynak/galdeck-cli), which has the full table.
The ones this page refers to:

| Command | What it does |
|---|---|
| `galdeck status` | Daemon and device state |
| `galdeck profile <name>` | Switch profile |
| `galdeck reload` | Re-read the config; a broken edit leaves the running one alone |
| `galdeck ui` | Print the configuration UI's address, token included |

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

`--device virtual` runs the whole daemon against a deck that exists only in
memory, so the configuration, themes and profiles can be developed and tested
on a machine with no keyboard attached. Give it a socket of its own and it runs
happily alongside the installed service:

```sh
cargo run --bin galdeck-daemon -- --device virtual --config config/v2 \
    --socket /tmp/galdeck-dev.sock --http 8787

galdeck --socket /tmp/galdeck-dev.sock status
```

`$GALDECK_SOCKET` does the same thing for both binaries if you would rather not
pass the flag every time.


```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./ci/layering.sh
```

None of that runs without `../galdeck-cli` beside this directory: the control
protocol and the configuration model are found by relative path, so a clone of
this repository on its own does not build. That is the split doing its job
rather than a rough edge — the CLI stands alone, and this does not.

`ci/layering.sh` checks the boundaries the workspace is split along, including
that the two shared crates are still declared in a shape `cargo publish` will
accept, and that the CLI has not acquired a dependency pointing back this way.

The [`galdeck`](https://crates.io/crates/galdeck) framework comes from
crates.io. To work against an unpublished change to it, add a patch in
`.cargo/config.toml` (which is not committed):

```toml
[patch.crates-io]
galdeck = { path = "../galdeck" }
```

The same trick works for the CLI's crates once they are published, if you
would rather not rely on the sibling checkout:

```toml
[patch.crates-io]
galdeck-ipc = { path = "../galdeck-cli/crates/galdeck-ipc" }
galdeck-model = { path = "../galdeck-cli/crates/galdeck-model" }
```

## License

MIT.
