# galdeck-daemon

A configurable daemon that turns the Stream Deck module in a **Corsair
Galleon 100 SD** keyboard into a launcher: label your keys, give them
icons, bind them to commands, keystrokes or built-in actions, page between
sets, show clocks, graphs, media, weather and timers, and turn the knobs for
volume, an app's sound, the output, tracks, pages or anything else -- with a
browser-based editor to set it all up.

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

mkdir -p ~/.config/galdeck && cp -r config/v2/. ~/.config/galdeck/
mkdir -p ~/.config/systemd/user && cp systemd/galdeck.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now galdeck

galdeck status
```

## Configuring

Everything lives in `~/.config/galdeck/`: a small `galdeck.toml`, a file per
profile and one per theme (see [Profiles and themes](#profiles-and-themes)),
starting from the commented example in [config/v2](config/v2). The
configuration UI edits the same files. `galdeck reload` applies edits made by
hand without restarting.

Keys are numbered row-major from the top-left of the 3×4 grid:

```
 0  1  2
 3  4  5
 6  7  8
 9 10 11
```

A key can carry a label, an [icon](#icons), a style, and one of: a shell
command, a page to switch to, a profile to switch to, `back = true` to return
to wherever the current page was reached from, or
[states](#keys-that-cycle-through-states) to step through. An encoder can bind
commands to press, clockwise and anticlockwise, and rests at a ring colour.

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

A theme can also say how widgets look, under `[widgets]`: a view, a colour
and a card for every widget, graphs as an area, a line or bars, bars
rounded, flat or segmented, and how far round a dial goes and how thick it
is. A `[widgets.<kind>]` table goes over that for one kind, so
`[widgets.clock]` with `view = "nixie"` makes every clock a nixie clock. A
widget's own settings still win, and a profile or a page can have a
`[widgets]` of its own over its theme's.

And how things move, under `[motion]`: `press` answers a key the moment it
is pressed with a flash towards a colour or a dip towards black, `alarm`
makes a widget past its threshold pulse, breathe, blink or beat between its
usual colours and `@warning` or `@critical`, and `rings` gives every knob
without an animation of its own something to do at rest, unless its ring
shows a level, a position or which mode it is in. Nothing moves unless a
theme or its profile says so.

The configuration UI's **Theme** tab edits themes: the palette, the style,
the widgets, the background and the keyboard lighting. For every value it
shows whether the theme sets it or inherits it, and from where, and what
emptying it would leave. The preview beside the form is drawn by the daemon
with the unsaved edits applied, so it is what the deck will show. The tab
can also start a theme, based on another or as a copy of one, and switch
the profile showing to it.

See [config/v2](config/v2) for a commented worked example. A single-file
`config.toml` from before still works — it is translated on load, and nothing
on disk is rewritten.

Turning a knob steps a lit segment around its ring and a click flashes it,
so a knob answers even when its command has no visible effect. Fast spins
run their detents in order through a bounded per-knob queue rather than
racing; `GALDECK_DELTA` carries the signed step count to your command.

### Actions, knob presets and keystrokes

Every gesture — a key's `exec`, `hold` and `double`, a knob's `press`, `cw`,
`ccw` and `hold` — takes a shell command, something built in, or keystrokes:

```toml
exec = "firefox"                              # a shell command, as before
exec = { action = "play_pause" }              # built in: no scripts, no playerctl
cw = { action = "volume_up", step = 5 }       # with a step
exec = { action = "play_pause", target = "spotify" }
exec = { keys = "ctrl+shift+t" }              # a keystroke
```

The built-ins are sound (`volume_up`, `volume_down`, `volume_mute`, `mic_up`,
`mic_down`, `mic_mute`, and `push_to_talk`, which keeps the microphone live
only while its key is held), outputs (`next_output`, `previous_output`,
`set_output`), one app's sound (`app_volume_up`, `app_volume_down`,
`app_mute`, and `next_app` on a knob), media over MPRIS (`play_pause`,
`next_track`, `previous_track`, `seek_forward`, `seek_backward`), navigation
(`next_page`, `previous_page`, `home_page`, `next_profile`, `previous_profile`,
`start_profile`), the deck's own brightness (`deck_brighter`, `deck_dimmer`),
a knob's modes (`next_mode`, on a knob), timers (`timer_toggle`,
`timer_reset`, on a key showing a timer or stopwatch), and the pointer
(`scroll_up`/`down`/`left`/`right`, `zoom_in`, `zoom_out`, `zoom_reset`). Sound goes through `wpctl` (or `pactl`) with no shell, never
above 100%; turning up unmutes. A spin is one change the size of the spin, not
eight processes.

A knob can take a **preset** — `volume`, `mic`, `outputs`, `app_volume`,
`tracks`, `seek`, `pages`, `profiles`, `deck_brightness`, `scroll`, `zoom`,
`tabs`, `workspaces`, `up_down`, `left_right` — and knobs can be set in
`galdeck.toml` for every profile, in a profile for all its pages, or on a
page. The layers combine one gesture at a time, so a volume knob set once is
on every page, and a page that sets only `press` keeps the turn. A knob with a
`hold` waits for its release before pressing, and a press that turned into a
turn counts as neither. While a knob turns, its ring shows the level or the
page you are on, and a strip along the bottom of the screen says what changed.

A knob can also switch between two to four **modes**, each a preset:

```toml
[[encoders]]
encoder = 0
modes = ["volume", { preset = "app_volume", ring = "#88c0d0" }, "tracks"]
```

Holding the knob for about two thirds of a second moves to the next mode
(`next_mode`, which a knob with modes holds unless told otherwise); the
screen names it and the ring rests in the mode's colour, so you can see which
mode is on without turning. A knob keeps its mode on every page and profile
until you switch it or change the list. Setting a `hold` on the knob turns the
switching off.

`outputs` turns through the sound outputs that are plugged in, one per turn
however fast the spin, and a press mutes. `outputs = ["Headphones", "Speaker"]`
in `galdeck.toml` limits and orders them by any part of their name; a key can
go straight to one with `{ action = "set_output", target = "Headphones" }`.
`app_volume` turns the volume of the app playing sound and keeps to that app;
a press moves on to the next app, or mutes the app when the preset has a
`target`. Keys use `app_volume_up`, `app_volume_down` and `app_mute`, which act
on whatever is playing unless given a target. Both need PipeWire's `wpctl` and
`pw-dump`; without them the screen says what is missing. WirePlumber remembers
each app's volume and mute, so an app comes back at the level the knob left
it.

Keystroke names are key *positions* on a US keyboard (`ctrl`, `shift`, `alt`,
`super`, letters, digits, `f1`–`f24`, `kp_0`–`kp_9`, `kp_enter`, `page_up`,
`left`, `enter`, `escape`, ...), so they are right on every layout; the UI
records them from a real key press. They go through a virtual keyboard the
daemon creates with `/dev/uinput`, which needs the logged-in user to be allowed
to use it:

```sh
sudo cp udev/71-galdeck-uinput.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
```

With that rule, any program the logged-in user runs can create a virtual
keyboard. Set `virtual_input = false` in `galdeck.toml` to keep the daemon
from ever opening it. Magic SysRq, power, sleep and similar keys have no names
and cannot be sent.

A key showing what is playing plays and pauses it when tapped, and one showing
the output volume mutes it, unless something else is bound. The UI's **New
page…** makes a numpad, a digits row, media controls or F13–F19 macro keys in
one go.

A key whose tap mutes something -- `volume_mute`, `mic_mute`, or a volume
widget's own tap -- turns red with a struck-through speaker or microphone in
its corner while the sound server says it is muted, and a `push_to_talk` key
turns amber while the microphone is live. These show only what the sound
server reported, never a guess, and are read again every two seconds while
such a key or a level ring is showing, so muting from the desktop or a headset
shows on the deck too.

### Keys that cycle through states

A key can step through two to eight **states** -- a toggle is two -- and show
the one it is in. Each tap moves it on to the next, from the last back to the
first, and runs the command of the state it moves to:

```toml
[[pages.keys]]
key = 5
label = "Wi-Fi"
status = "nmcli radio wifi"          # prints `enabled` or `disabled`
hold = "gnome-control-center wifi"

[[pages.keys.states]]
name = "off"
match = ["disabled"]
label = "Wi-Fi off"
icon = "network-wireless-disabled-symbolic"
style = { key_bg = "#3b4252" }
exec = "nmcli radio wifi off"

[[pages.keys.states]]
name = "on"
match = ["enabled"]
label = "Wi-Fi"
icon = "network-wireless-symbolic"
style = { key_bg = "#5e81ac" }
exec = "nmcli radio wifi on"
```

A state's `label`, `icon`, `style` and `animation` replace the key's own while
the key is in it, and what it leaves out the key's own supplies. Its `exec` --
a command, a built-in or keystrokes -- runs as the key *enters* the state: the
key always shows the state it is in, never what the next tap will do.

`status` is a shell command that prints which state the key is in, so the key
follows a change made anywhere else, such as the desktop's own quick settings.
It runs every five seconds while the key's page is showing
(`status_interval_ms`, at least 2000), and:

- only the first line with anything on it counts. It is trimmed and compared
  with each state's `match`, or with the state's `name` when it has none,
  ignoring case and one pair of surrounding quotes, so gsettings'
  `'prefer-dark'` matches `prefer-dark`;
- it runs with `LC_ALL=C.UTF-8`, so tools answer in English whatever language
  the desktop speaks, and it is stopped after five seconds, along with
  everything it started;
- a read that fails -- a non-zero exit, nothing printed, too slow -- or prints
  something no state matches leaves the key as it was, with a `?` in its
  corner, and the key is read less often, down to once a minute, until a read
  works or the key is pressed;
- a key with `status` shows its own look until the first read; one without
  starts in its first state, and shows what it last did.

Showing a state again never runs its command: not at start, not on a page or
profile switch or a reload, not when a read finds the key in another state,
and not from the UI's **Show this**. Only a tap, `next_state` or
`previous_state`, or the UI's **Switch to this** runs it. When the command
fails -- exits non-zero -- the key goes back to the state it was in, flashes a
warning, and the screen shows the line it printed to stderr. A command still
running after ten seconds counts as done and is left to finish, never killed:
`nmcli connection up` can take a while. Tapping again while one runs only moves
the key on, and once it is done only the state the key ended on runs its
command, if that is not the one just entered. A thin bar along the key's bottom
edge shows while a command takes longer than a quarter of a second. Once it is
done, the key is read again at 0.3, 1 and 3 seconds; for those three seconds a
read that disagrees with the tap counts only once two reads a second apart
agree.

The screen names each change you make -- `Wi-Fi · on`, or `Power · Perform 3/3`
with three states or more -- but not a change a read found. A key keeps its
state across page and profile switches and reloads, though not a restart,
after which it reads it again.

The tap is taken, so a key with states has no `exec`, `page`, `profile`, `back`,
`plugin` or `widget`; `hold` and `double` are free, and `next_state` and
`previous_state` step it from either:

```toml
hold = { action = "previous_state" }
```

A state's own command cannot be one of those two, a timer's, or
`push_to_talk`.

In the configuration UI, **toggle or cycle** in a key's tap opens an editor for
its states, each with the daemon's picture of the key in it. The gallery's
**Toggles** are ready-made keys, their commands checked on GNOME 50 (Ubuntu
26.04): Do Not Disturb, Power profile, Wi-Fi, Bluetooth, Dark style, Night
light, Keep awake, Airplane mode, Touchpad and VPN, beside Mic mute and Mute,
which are built in and show what the sound server says. Drag one onto a key,
or select a key and click one. Each card says how many states it has and marks
what this machine lacks: `needs nmcli`, `GNOME only`. The VPN asks which
connection, and holding a toggle's key opens its page of GNOME's Settings.
Putting one on a key runs nothing: the key reads its state.

State commands run in the daemon's environment. A development daemon started
from a snap's terminal, VS Code's for one, inherits the snap's
`GSETTINGS_SCHEMA_DIR`, under which `gsettings` reads the wrong values or none;
try the toggles from a daemon started the usual way, with **Switch to this**.

### Icons

A key's `icon`, or a state's, is a picture or a name from the desktop's icon
theme:

```toml
icon = "network-wireless-symbolic"   # a name
icon = "firefox"                     # an app's own icon, by name
icon = "logo-spotify"                # an app's logo, compiled into the daemon
icon = "~/Pictures/deck/mail.png"    # a file: PNG, JPEG, GIF or SVG
```

Anything with a `/` in it, or starting with `~`, is a file; anything else is a
name, made of letters, digits and `_ . + -`. A name ending in `.png`, `.svg` or
`.jpg` is warned about, since it was probably meant as a file. Names are looked
up in the icon theme `icon_theme` names in `galdeck.toml`, else in the
desktop's own, as its settings portal says (Adwaita when it cannot say), then
in the themes that one inherits from, then Adwaita, then hicolor, preferring a
scalable SVG.
A name that is not found is tried again with `-symbolic` added or taken off,
and a file found for it that cannot be drawn is passed over for the next one.
One found nowhere leaves the key with its label alone, and the UI says so
under its icon; the UI also suggests every symbolic name the themes have. The
theme is found again on every reload.

A *symbolic* icon -- one whose file name ends in `-symbolic`, as most of
GNOME's own do -- is drawn in the key's label colour (`key_label_color`, which
a state can set), the way the desktop draws them in its text colour. Any other
keeps its own colours.

The daemon also carries the logos of about 160 popular apps -- browsers, chat,
music and video, game launchers, editors and IDEs, AI assistants, terminals,
office apps and distributions -- so a key that opens one can show it whether
or not the app, or a theme with its icon, is installed. `logo-spotify` draws
the logo in white on a rounded tile of the brand's colour, as a launcher
shows the app; `logo-spotify-symbolic` draws the logo alone in the label
colour, like any symbolic icon. **Apps…** beside an icon field in the UI shows
them all to pick from. They are written to `~/.cache/galdeck/` on start and
looked up after every icon theme, so a theme with an icon of the same name
wins. The list, and where the logos come from, is in
[`crates/galdeck-daemon/logos/`](crates/galdeck-daemon/logos/README.md).

SVG is drawn by [resvg](https://crates.io/crates/resvg), within limits that
keep one icon from holding up the deck: at most 256 KiB, no `<!DOCTYPE>` or
entities, no compressed `.svgz`, no text, no pictures inside it, and caps on
filters, layers, pattern tiles, copies, path data, path segments and dashes,
the length of edges drawn and the area filled, how far out a path reaches, and
gradient stops. A PNG, JPEG or GIF file is at most 4 MiB and 4096×4096, and is
shrunk to fit but never enlarged; an icon from a theme is scaled to fill its
box. An icon that cannot be drawn leaves the key with its label, and the UI
says why under its icon.

### Widgets and animations

A key can carry a **widget** — something that changes:

```toml
[[pages.keys]]
key = 9
label = "--:--"          # shown until the widget produces text

[pages.keys.widget]
kind = "clock"           # see the table below
format = "%H:%M"
```

| kind          | shows                                   | `source`                                      |
|---------------|-----------------------------------------|-----------------------------------------------|
| `clock`/`date`| the time, formatted with `format`; `timezone = "Asia/Tokyo"` for a world clock |          |
| `uptime`      | time since boot, `3d 4h`                |                                               |
| `cpu`         | CPU use, %                              |                                               |
| `memory`      | memory in use, %                        |                                               |
| `temperature` | a hwmon sensor, °C or `units = "fahrenheit"` | chip or label: `k10temp`, `nvme`, `Tctl`; default the CPU |
| `gpu`         | GPU use, % (sysfs, else `nvidia-smi`)   | DRM card, `card1`                             |
| `network`     | throughput down and up                  | interface; default all but `lo`               |
| `disk`        | space used, %                           | mount point; default `/`                      |
| `weather`     | conditions and a short forecast         | `latitude` and `longitude`, or a `place` the UI finds by name |
| `media`       | what an MPRIS player is playing         | player, `spotify`; default whichever plays    |
| `volume`      | the volume, %, or `muted`               | `mic` for the default input; default output  |
| `battery`     | charge, %, with `↑` while charging      | power supply, `BAT0`; default the first      |
| `fan`         | a fan's speed, RPM                      | chip or fan label; default the first fan     |
| `load`        | the one-minute load average             |                                               |
| `command`     | the first line of a shell command       |                                               |
| `timer`       | counts down from `duration`; keys only  |                                               |
| `stopwatch`   | counts up; keys only                    |                                               |

Anything that measures something can be drawn with `view = "graph"` (recent
history), `"bar"` or `"gauge"` (a dial) as well as the default `"text"`, and a
clock with `view = "analog"` (a face with hands) or `view = "nixie"`, with an
optional `title`, `max` and `color`. `weather` and `media` draw as cards: the
cover fills a media key, and a wide tile gets the title, artist and progress
beside it.

A widget that measures something can warn: `warn = 80` and `critical = 90`, in
the units it shows, turn it amber and then red (the theme's `@warning` and
`@critical`, if it has them). Whether higher or lower is worse follows from
the two -- `warn = 20, critical = 10` counts down -- and a battery counts down
when only one is given. A reading has to come back past the line by a little
before the colour goes, so a value hovering at a threshold does not flicker.

A nixie clock gives each digit of its `format` a glowing tube -- six, to the
second, unless the format says otherwise; `%l` leaves the first tube dark
before ten o'clock. The tubes stand in a dark, warm room lit mostly by
themselves, their unlit cathodes and anode mesh showing against the glow, with
haze and dust drifting through the light and now and then a flickering tube.
It glows orange unless it has a `color`, in that room unless it has a
`background`, and refreshes ten times a second to animate unless
`interval_ms` says otherwise.

A `timer`'s `duration` is `25m`, `1h 30m`, `90s`, or `4:30` and `1:30:00`
(minutes, or hours, then two-digit fields). Tap it to start, pause and resume,
and hold to reset. A running timer ignores the hold, so a slow tap cannot wipe
out twenty minutes: pause it first. A stopwatch resets whenever it is held. The
key shows the time large, under its `title` or else its label, dimmed while
paused; a timer can also be a `"bar"` or `"gauge"` of what is left. It keeps
counting while its page is not showing, and through a save that leaves it the
same length. When a timer finishes the screen says so -- until the deck is
next touched, if its page is not showing -- its key flashes for ten seconds
and then stays red until tapped, and its `on_done` action, if it has one,
runs once:

```toml
[pages.keys.widget]
kind = "timer"
duration = "4m"
title = "Tea"
on_done = "pw-play /usr/share/sounds/freedesktop/stereo/alarm-clock-elapsed.oga"
```

Timers count on the monotonic clock, which stops while the machine is
suspended, so a suspend pauses them.

Weather comes from [Open-Meteo](https://open-meteo.com), which needs no key.
It is only asked about the coordinates you give, at most once a minute
(every fifteen by default), and a failed refresh keeps the last forecast up.

`command`, `gpu`, `weather` and `media` run on worker threads, one per kind of
slowness, so a hung script or a slow network never delays the deck or each
other. A command is killed if it takes more than five seconds. A sample that
produces the same text as last time costs nothing at all.

The info screen can show widgets too, laid out on a 12x6 grid:

```toml
[[pages.lcd]]
column = 0               # top-left cell
row = 0
columns = 12             # span
rows = 4

[pages.lcd.widget]
kind = "media"
```

A page with tiles ignores its `lcd_text`. The grid is 12 by 6 unless a page
or profile sets `lcd_columns` (up to 24) and `lcd_rows` (up to 12); a tile
that leaves out `columns` or `rows` runs to the edge of whichever grid it is
on. The UI's grid editor carries a page's widgets across when it changes.

Any widget can have a background of its own -- `background` (a colour or
`@token`), `image` (scaled to cover it) and `opacity` from 0 to 1. On the
screen that replaces the card behind the tile; at `opacity = 0` whatever is
behind the page shows straight through.

### Backgrounds

A page, a profile or a theme can put a picture or an animation behind the
screen, the keys, or both:

```toml
[pages.background]
span = "both"            # lcd, keys, or both
image = "/home/you/Pictures/dusk.gif"  # PNG, JPEG or GIF; a GIF plays
# animation = "aurora"   # gradient, waves, plasma, starfield, rain, fire, bubbles
# colors = ["@accent", "#b48ead", "#1a1d24"]
fps = 10                 # 1-20, for an animation or a GIF
dim = 0.25               # darken so labels stay readable
```

`both` is one picture across the whole panel, not the same picture twice:
the screen and the keys are regions of one display, and the calibration says
where each sits, so a wallpaper runs continuously from the screen down
through the keycaps. Unbound keys show their slice too. A key with a
`key_bg` of its own keeps it.

The nearest background wins: a page's replaces its profile's, which
replaces its theme's -- so a theme with a background is an animated theme.
Each animation frame is a JPEG for the screen and for every key it covers,
so keep `fps` modest; the paint queue drops frames rather than falling
behind if the USB link cannot keep up.

The configuration UI has all of this: a gallery of ready-made widgets to
drag onto the screen or a key, a **Background…** editor, and uploads, which
are kept under `assets/` in the config directory.

A key or a ring can also carry an **animation**:

```toml
[pages.keys.animation]
kind = "breathe"         # pulse, breathe, blink, heartbeat, rainbow; rings also spin and comet
period_ms = 3000
to = "@aurora-green"     # the colour it moves towards
```

Frames are rendered and encoded once when the page is applied and then cycled,
so playing one costs nothing. Turning or clicking a knob still takes
precedence over its ring animation.

### Keyboard lighting

A theme or a profile can light the keyboard too -- every key, and the light
bar along the top -- in the same palette as the deck:

```toml
[lighting]
effect = "wave"                  # static, gradient, breathe, wave, spectrum, off
colors = ["@frost", "@aurora-green"]
speed = 0.25                     # cycles a second
brightness = 60                  # 0-100
bar = "@accent"                  # the light bar, if it should not follow the effect
keys = { "w a s d" = "@snow" }   # keys of their own, over the effect

[lighting.reactive]              # keys answering presses
effect = "ripple"                # ripple, glow, none
color = "@accent"
fade_ms = 800
```

`[lighting]` folds through a theme's `extends` like its palette, and a
profile's changes only what it names. `effect = "off"` hands the keyboard
back to its own effects; with no `[lighting]` anywhere, the daemon never
touches it. A change fades in rather than snapping, and a still effect is
sent once and then only kept alive.

The configuration UI's **Keyboard** tab shows what the keyboard is showing,
live, and designs lighting without writing any of this: drag effects,
reactions and colours onto a drawing of the keyboard, watch it play, and save
it to the profile or its theme. [docs/keyboard-lighting.md](docs/keyboard-lighting.md)
walks through setting it up, designing and saving, and every field and key
name.

The keyboard needs the lighting line of `udev/70-galdeck.rules` from galdeck.
`[lighting.reactive]` also needs that file's commented-out line for the
keyboard's key reports: they are every key press, readable by any program in
your session while the lighting is taken over, so they are off unless you
choose them. The daemon uses only where a pressed key is, and never logs a
press; without the line, the keys simply do not answer, and the log says why
once.

The keyboard never takes its lighting back by itself. The daemon hands it
back when it stops, or when the lighting is switched off; if it is ever
killed outright and the keyboard is left showing its last frame, replug the
keyboard, or run `cargo run --example keyboard_wave -- off` in the galdeck
repository.

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
# configuration UI on http://127.0.0.1:8787/ — run `galdeck ui` to open it

galdeck ui
# Opening http://127.0.0.1:8787/ in your browser. …
```

`galdeck ui` asks the daemon for a one-time link and opens your browser on it,
signed in. The page shows the twelve keys as the images the panel is actually
being sent, the LCD, and both encoder rings; click one to edit its label, icon,
action and colours. Saving validates the whole configuration first and refuses
anything that would break it, and your comments and formatting are preserved.

A gallery beside the deck holds widgets, timers, toggles, knob presets and
actions to drag onto a key, a knob or the screen, where tiles can be moved and
resized on the grid. The forms cover every gesture -- recording keystrokes from
a real key press, trying a built-in on the spot -- a key's
[states](#keys-that-cycle-through-states), with the icon theme's names and
[app logos](#icons) to pick from, a knob's layers and modes, timers, thresholds, backgrounds, pages made
from templates, the outputs a knob turns through, and the panel's calibration.

- `galdeck ui --print` prints the link instead of opening a browser. It is
  valid for 5 minutes and works once. Over SSH, or with no display, that is
  what `galdeck ui` does by itself.
- Over SSH it also prints the tunnel to run on the machine your browser is on,
  `ssh -N -L 8787:127.0.0.1:8787 <host>`. Keep the same port number on both
  sides: the daemon answers only to its own address, so a tunnel to another
  local port loads the page and then has every request refused.
- `galdeck ui --new-token` gives the UI a new token first, which signs out
  every tab that had it open. The page suggests it when a link it was opened
  with had already been used: if that wasn't you, someone else used it.

It is **off unless you ask for it**, and this surface can set the shell
commands the daemon runs, so:

- It binds loopback only, and serves only connections opened by the user the
  daemon runs as. Every account on the machine can reach 127.0.0.1, so the
  daemon looks each connection up in the kernel's socket table
  (`/proc/net/tcp`) and closes any that belong to another account, a service
  or a container.
- The API needs a token, which is never printed, logged or put on a command
  line. A link carries a one-time code instead, good for a minute when
  `galdeck ui` opens the browser itself and five when it is printed; the page
  trades it for the token and takes it out of the address bar first. The
  token is kept beside the control socket in `galdeck-ui.token`, readable
  only by you, so restarting the daemon does not sign anyone out.
- It refuses any request whose `Host` or `Origin` is not its own, which is
  what stops a page you happen to visit from driving it through your browser.
- Its pages carry a strict Content-Security-Policy: no inline script or style,
  and nothing fetched from anywhere but the daemon itself.

Daemons before this printed the address with the token in it at every start,
and under systemd that went to the journal, which the `adm` group can read.
The first start after upgrading deletes the old `galdeck-ui-token` file and
mints a new token, so the one in the journal no longer works.

## CLI

The `galdeck` command lives in
[galdeck-cli](https://github.com/cynak/galdeck-cli), which has the full table.
The ones this page refers to:

| Command | What it does |
|---|---|
| `galdeck status` | Daemon and device state |
| `galdeck profile <name>` | Switch profile |
| `galdeck reload` | Re-read the config; a broken edit leaves the running one alone |
| `galdeck ui [--print \| --open] [--new-token]` | Open the configuration UI signed in, or print a one-time link to it |

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
- After pulling or editing either checkout, `scripts/rebuild.sh` builds the
  CLI and the daemon, installs both in `~/.cargo/bin` and restarts the
  service, waiting until it answers. It also stops a daemon started by hand
  on the service's socket (one of those keeps the service from starting at
  all) and lists any others, such as a fake deck, still running an old build.
  `--no-restart`, `--cli`, `--daemon` and `--dry-run` do less.

## Development

`--device virtual` runs the whole daemon against a deck that exists only in
memory, so the configuration, themes and profiles can be developed and tested
on a machine with no keyboard attached. Give it a socket of its own and it runs
happily alongside the installed service; `--http 0` picks a free port, so it
does not collide with the service's UI either. Point `--config` at a copy:
the UI writes to whatever directory it is given, and the commands a key or
knob runs still run on this machine.

```sh
cp -r ~/.config/galdeck ~/galdeck-sandbox
cargo run --bin galdeck-daemon -- --device virtual --config ~/galdeck-sandbox \
    --socket /tmp/galdeck-dev.sock --http 0

galdeck --socket /tmp/galdeck-dev.sock status
galdeck --socket /tmp/galdeck-dev.sock ui
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

The daemon's binary also carries code under BSD licences, which ask for their
notices to be kept. [resvg](https://crates.io/crates/resvg), which draws SVG
icons, brings in tiny-skia and tiny-skia-path (BSD-3-Clause, Copyright (c)
2011 Google Inc. and (c) 2020 Yevhenii Reizner) and arrayref (BSD-2-Clause,
Copyright (c) 2015 David Roundy). Their full licence texts ship with each
crate's source.

The app logos are from [Simple Icons](https://simpleicons.org), under CC0,
and remain their owners' trademarks; see
[`crates/galdeck-daemon/logos/`](crates/galdeck-daemon/logos/README.md).
