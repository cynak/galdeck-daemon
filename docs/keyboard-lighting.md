# Keyboard lighting

galdeck-daemon can light the Galleon 100 SD's own keys and the light bar
along its top, not just the deck. The lighting follows your theme's palette.
It can move (waves, breathing, a rainbow), paint single keys or whole groups
of keys, and light up keys as you press them.

You can set it up in two ways:

- **Drag and drop** in the configuration UI's **Keyboard** tab, then save.
- **Write it by hand** as a `[lighting]` table in a theme or profile file.

Both end up as the same `[lighting]` table in the same files, so you can
switch between them freely.

- [1. Let the daemon reach the keyboard](#1-let-the-daemon-reach-the-keyboard)
- [2. Open the Keyboard tab](#2-open-the-keyboard-tab)
- [3. Design by dragging](#3-design-by-dragging)
- [4. Save it](#4-save-it)
- [Writing it by hand](#writing-it-by-hand)
- [Key and group names](#key-and-group-names)
- [Where lighting comes from](#where-lighting-comes-from)
- [Troubleshooting](#troubleshooting)
- [About key reports and privacy](#about-key-reports-and-privacy)

## 1. Let the daemon reach the keyboard

The deck and the keyboard are separate USB devices, and each needs its own
line in galdeck's udev rule. If you installed `udev/70-galdeck.rules` from
[galdeck](https://github.com/cynak/galdeck) before keyboard lighting existed,
install it again: the keyboard's line is new.

```sh
sudo cp udev/70-galdeck.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
```

Then unplug the keyboard and plug it back in.

This line gives the daemon the keyboard's lighting, and nothing else. The
interface you type on stays root-only:

```
SUBSYSTEM=="hidraw", KERNELS=="0003:1B1C:2B0C.*", DEVPATH=="*:1.1/*", TAG+="uaccess"
```

**Optional: keys that light up when pressed.** For the lighting to answer key
presses, the daemon also has to read which keys are down. That is the rule
file's commented-out line for interface 2. Read
[About key reports and privacy](#about-key-reports-and-privacy) before you
turn it on. If you want it, remove the `#` from this line in
`/etc/udev/rules.d/70-galdeck.rules`, reload as above, and replug:

```
#SUBSYSTEM=="hidraw", KERNELS=="0003:1B1C:2B0C.*", DEVPATH=="*:1.2/*", TAG+="uaccess"
```

Everything else here works without it. Only the reactions stay dark.

## 2. Open the Keyboard tab

The configuration UI has to be switched on with `--http`:

```sh
galdeck-daemon --http 8787      # or add --http to the systemd unit
galdeck ui                      # opens your browser, signed in
```

Click **Keyboard**. The tab opens in **Live** view: a drawing of the keyboard
showing exactly what the real one shows right now, updated about 20 times a
second. It shows the colours the daemon last sent, so it cannot disagree with
the keyboard.

If the drawing is dark and the tab says the daemon is not lighting the
keyboard, nothing has a `[lighting]` table yet, or it is set to `off`. That is
the normal starting point. Designing one is the next step.

Hover over any key to see its name, its group and its LED number.

## 3. Design by dragging

Click **Design**. The drawing now plays your design instead of the keyboard.
Nothing you do in Design changes the keyboard until you press **Save**.

Below the drawing is a gallery. Drag anything from it onto the drawing:

| Drag | Drop it on | What happens |
|---|---|---|
| An **effect** | the keyboard | Every key takes that effect. Painted keys, the light bar and the reaction stay as they were. |
| A **reaction** | the keyboard | Sets how a key answers when it is pressed: a ripple spreading out from it, a glow on just that key, or nothing. |
| A **colour** | a key | Paints that key. |
| A **colour**, holding **Shift** as you drop | a key | Paints the key's whole group: all the letters, all the digits, and so on. |
| A **colour** | a group's name under **Groups** | Paints that group. `all` paints every key. |
| A **colour** | the light bar (the thin strip along the top) | Sets the light bar's colour, so it stops following the effect. |
| A **colour** | the keyboard's background, between keys | The whole keyboard, still, in that colour. |

Painted keys and the bar sit on top of the effect, so you can put a steady
colour on WASD over a moving wave, for example.

The other controls:

- **Speed** sets how fast a moving effect goes, from 0.01 to 4 cycles a
  second. The middle of the slider is the default, 0.2.
- **Brightness** goes from 0 to 100, as a share of the keyboard's full
  light. It dims everything, painted keys included, without changing any
  colour.
- **Clear painted keys** removes every painted key and the bar colour, and
  leaves the effect.
- **Click a key** on the drawing to watch how it answers a press with the
  current reaction.

Everything you drop is written into the text box under the gallery, as the
`[lighting]` table it will be saved as. You can also type there directly.
Typing and dragging work on the same text, so neither undoes the other. If
the text stops making sense (a missing quote, say), the problem is shown
under the text box, and drops wait until you fix it rather than overwrite
what you typed.

Problems that don't stop it working, such as a key name that doesn't exist,
are listed under the text box too.

### The gallery

| Effect | Writes |
|---|---|
| Plain white | `static`, white |
| Frost | `gradient`, pale blue to deep blue, left to right |
| Aurora wave | `wave` through green, cyan and violet |
| Sunset | `gradient`, coral to peach to purple |
| Ocean breath | `breathe` between two blues |
| Rainbow | `spectrum`: the whole keyboard turning through the hues |
| Fire | a quick `wave` through red, orange and yellow |
| Nixie glow | a slow, dim orange `breathe`, to match the nixie widgets |
| Matrix | `wave` of dark to bright green |
| Candy | `wave` of pink and cyan |
| Keyboard's own | `off`: hands the keyboard back to its built-in effects |

| Reaction | Writes |
|---|---|
| Ripple | a white ring spreading out from the pressed key |
| Glow | the pressed key alone, glowing white and fading |
| Accent ripple | a ripple in the theme's `@accent` colour |
| Slow glow | a glow that takes two seconds to fade |
| No reaction | removes `[reactive]` |

Every gallery item only fills in the text, so you can adjust whatever it
wrote afterwards.

## 4. Save it

Under the text box, **Save to** offers two places:

- **This profile**, `profiles/<name>.toml`. The design shows only while this
  profile is showing.
- **Its theme**, `themes/<name>.toml`. The design shows on every profile that
  uses the theme.

Press **Save**. The design is checked first. If anything would stop the
configuration from loading, nothing is written and the problem is shown.
Otherwise it replaces the `[lighting]` table in that file (all of it, so
nothing from an older design lingers), and the daemon reloads. Switch back to
**Live** to watch the keyboard take it on. It fades over rather than
snapping.

Saving to a profile lays the design over the theme's lighting. Anything the
design leaves out comes from the theme. For example, if the theme sets a bar
colour and your design doesn't, the bar keeps the theme's colour. See
[Where lighting comes from](#where-lighting-comes-from).

The file keeps the rest of its contents: pages, keys, comments elsewhere.
The Theme tab's **Keyboard lighting** section edits the same table, so you
can also fine-tune a theme's lighting there.

## Writing it by hand

The same design, written into a theme or a profile:

```toml
[lighting]
effect = "wave"                 # static, gradient, breathe, wave, spectrum, off
colors = ["@frost", "#b48ead"]  # palette names or #rrggbb
speed = 0.25                    # cycles a second, 0.01 to 4 (default 0.2)
brightness = 80                 # share of full light, 0 to 100 (default 60)
bar = "@accent"                 # the light bar; leave it out to follow the effect

[lighting.keys]                 # keys in a colour of their own, over the effect
"w a s d" = "#ffffff"           # several keys, separated by spaces
digits = "@accent"              # a whole group
Esc = "#ff2222"

[lighting.reactive]             # keys answering presses
effect = "ripple"               # ripple, glow, none (default ripple)
color = "#ffffff"               # default white
fade_ms = 800                   # 100 to 5000 (default 800)
```

In the Keyboard tab's text box, leave out the `lighting.` prefix. Write
`[keys]` and `[reactive]`, as the drops do, because the text box already is
the `[lighting]` table.

The effects:

| Effect | What it does | Uses |
|---|---|---|
| `static` | Every key the first colour. The default. | the first colour |
| `gradient` | The colours spread left to right across the keyboard, still. | all colours |
| `wave` | The colours roll across the keyboard, one full pass per cycle. | all colours, speed |
| `breathe` | The whole keyboard fades in and out, bringing in the next colour on each breath. | all colours, speed |
| `spectrum` | The whole keyboard turns through every hue. | speed |
| `off` | Hands the keyboard back to its own built-in effects. | nothing |

With no `colors`, an effect uses white. Colours are `#rrggbb`, or `@name` for
a colour from the theme's palette, so a theme's lighting changes along with
its palette.

The keys show a colour as your screen does. An LED's light goes straight up
with the level it is sent, where a screen's rises far more slowly at first,
so the daemon converts every colour on its way to the keyboard. Without that,
the weaker channels of a colour would shine several times too bright, and
every colour but a pure one would wash out toward white.

## Key and group names

Names are not case-sensitive. Anywhere a key name goes, a group name or `all`
works too.

| Group | Keys |
|---|---|
| `letters` | Q W E R T Y U I O P A S D F G H J K L Z X C V B N M |
| `digits` | 1 2 3 4 5 6 7 8 9 0 |
| `function` | Esc F1 to F12 PrintScreen ScrollLock Pause |
| `editing` | Backspace Tab CapsLock Enter Space |
| `symbols` | Grave Minus Equals LeftBracket RightBracket Backslash Semicolon Apostrophe Comma Period Slash |
| `navigation` | Insert Home PageUp Delete End PageDown Up Left Down Right |
| `modifiers` | LShift RShift LCtrl LWin LAlt RAlt Fn Menu RCtrl |
| `extra` | Cortana |
| `bar` | Bar1 to Bar7, left to right |

The Galleon has no numpad, so there are no numpad keys. In the Keyboard tab,
hover over a key to see its name.

## Where lighting comes from

The keyboard shows the lighting of the profile that is showing on the deck,
built up in layers:

1. The theme's `[lighting]`, with the themes it `extends` underneath it, the
   same way its palette is built.
2. The profile's own `[lighting]` on top. It changes only what it names:
   a profile that says only `brightness = 30` dims its theme's lighting and
   keeps the rest.

So, to light every profile the same way, put the lighting in the theme. To
make one profile different, give that profile just the differences.

- `effect = "off"` in a layer hands the keyboard back to its own effects.
- With no `[lighting]` anywhere, the daemon never touches the keyboard.
- Switching profiles fades from one lighting to the next.
- A still design is sent once and then only kept alive, so it costs next to
  nothing. A moving one is drawn 60 times a second.

## Troubleshooting

**The Live view is dark and says the daemon is not lighting the keyboard.**
No layer has a `[lighting]` table, or it says `effect = "off"`. Design one and
save it, or check that you saved to the profile that is showing (see
`galdeck status`).

**The Live view moves, but the keyboard doesn't.** The daemon can't reach the
keyboard. Look in its log (`journalctl --user -u galdeck` for the service):

- `no keyboard lighting found; will keep looking`: the keyboard isn't
  plugged in, or udev hasn't given you access. Install the rule
  ([step 1](#1-let-the-daemon-reach-the-keyboard)) and replug. The daemon
  checks again every two seconds, so it doesn't need restarting.
- `could not open the keyboard's lighting: ...`: usually permissions again.
- `keyboard lighting taken over` means it worked.

**Keys don't light up when pressed.** The log says so once:
`lighting is set to answer key presses, but this keyboard's cannot be read`.
Turn on the key-report line in the udev rule
([step 1](#1-let-the-daemon-reach-the-keyboard)) and replug. Also check that
the design has a `[reactive]` table whose effect isn't `none`.

**A warning says `no key or group is called ...`.** A name under `[keys]` is
misspelled; see [Key and group names](#key-and-group-names). The other keys
still light.

**The keyboard is stuck showing one frame.** The keyboard never takes its
lighting back by itself. The daemon hands it back when it stops, but if it
was killed outright (or crashed), the last frame stays. Replug the keyboard,
or run this in the galdeck repository:

```sh
cargo run --example keyboard_wave -- off
```

**The keys flicker between two looks.** Something else is sending the
keyboard frames too: a second copy of the daemon, OpenRGB, or one of
galdeck's examples. Only one program should drive the lighting at a time. A
development copy started with `--device virtual` leaves the real keyboard
alone.

**Saving takes a moment.** A save reloads the whole configuration and redraws
the deck. The button waits until that is done.

## About key reports and privacy

For reactions, the daemon reads the keyboard's key reports: a list of which
keys are held down. While a program drives the lighting, the keyboard sends
one on every key press.

Turning on the udev line lets *any* program in your login session read those
reports while the lighting is taken over. In effect, any program you run
could read what you type on this keyboard. That's why the line is off by
default and the rest of the lighting works without it.

The daemon uses a press only to know *where* on the keyboard to start a
ripple or a glow. It never logs, stores or sends which keys you press. If you
don't want reactions, leave the line commented out: the keys simply won't
answer presses, and nothing else changes.
