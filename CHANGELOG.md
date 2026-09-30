# Changelog

Notable changes to galdeck-daemon. Versions follow [semver](https://semver.org);
until 1.0 the configuration format may change between minor versions.

## 0.1.1

- **A Debian package.** Each tagged release now has a `.deb` with the daemon,
  the `galdeck` command, the systemd user unit and the udev rule for the deck,
  built and published by a release workflow. `scripts/build-deb.sh` builds
  the same package locally.

## 0.1.0

The first release. It turns the Stream Deck module in a Corsair Galleon 100
SD into a launcher, and lights the keyboard itself. You can set it up in TOML
files or in a configuration UI in the browser. It needs
[galdeck](https://crates.io/crates/galdeck) 0.3.1, and
[galdeck-cli](https://github.com/cynak/galdeck-cli) checked out beside it for
the `galdeck` command, the control protocol and the configuration model.

- **Keys and pages.** Keys carry labels and icons: a file, a name from the
  desktop's icon theme, or one of the app logos built into the daemon. They
  run shell commands, keystrokes or built-in actions on a tap, a hold or a
  double tap. Pages and profiles switch between sets of keys. A key can
  cycle through states, such as a Wi-Fi toggle, and read back the state the
  system is in.
- **Knobs.** Volume, one app's sound, the audio output, tracks, pages or any
  command, with presets layered from `galdeck.toml` down to a page. A knob
  can have modes, and its ring shows a level, a position or the mode it is
  in.
- **Widgets and animations.** Clocks, world clocks and nixie tubes; CPU,
  memory, GPU, temperatures, fans, network, disk and battery, as text,
  graphs, bars or dials; weather, what's playing, volume, timers,
  stopwatches and the output of a command. Keys can be animated, and a
  picture or an animation can sit behind the screen, the keys, or both.
- **Themes.** Palettes, styles, widget looks and motion (a flash on a
  press, an alarm that pulses, knobs that move at rest), inherited through
  `extends`, with a profile able to change any of it.
- **Keyboard lighting.** Every key and the light bar, in the theme's
  palette: still, gradient, wave, breathing and spectrum effects, keys and
  groups of keys in colours of their own, and, if you opt in, keys that
  ripple or glow when pressed. Colours are converted for the LEDs so that
  they look as they do on screen. See
  [docs/keyboard-lighting.md](docs/keyboard-lighting.md).
- **Configuration UI.** Served on loopback with `--http`, and opened signed
  in by `galdeck ui`. It edits keys, knobs, pages, themes and the keyboard's
  lighting, with previews drawn by the daemon itself, so they show what the
  hardware will. Every edit is checked before it is saved, and saving keeps
  the files' comments and formatting.
- **Plugins.** Separate processes that drive only the keys they are given.
- **Running it.** As a systemd user service; with `--device virtual`, on a
  machine with no keyboard, for development.
