#!/usr/bin/env python3
"""Draw the hero card at the top of the README.

    scripts/hero.py

Writes docs/hero.svg, which the README shows, and docs/social-preview.png,
the same card with square corners for GitHub to show wherever the repository
is linked. GitHub does not read that one from the repository: upload it
under Settings, General, Social preview.

GitHub shows an SVG as an image, and an image loads no fonts, so every line
of text is turned into outlines here and the card looks the same everywhere.
Drawing it needs the Ubuntu Sans fonts (fonts-ubuntu), fontTools
(python3-fonttools) and, for the PNG, Google Chrome or Chromium.
"""
import math
import os
import shutil
import subprocess
import sys

from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from fontTools.varLib import instancer

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FONTS = "/usr/share/fonts/truetype/ubuntu"
W, H = 1280, 640

# Nord, which the example configuration's theme is built on, and the nixie
# theme's neon for the clock.
POLAR, STORM = "#2e3440", "#3b4252"
SNOW, SNOW2, SNOW1 = "#eceff4", "#e5e9f0", "#d8dee9"
FROST, FROST2, FROST3 = "#88c0d0", "#81a1c1", "#5e81ac"
RED, ORANGE, YELLOW, GREEN, PURPLE = "#bf616a", "#d08770", "#ebcb8b", "#a3be8c", "#b48ead"
NEON, BRASS = "#ff7a22", "#8a6a4a"


def num(v):
    v = round(v, 2)
    return str(int(v)) if v == int(v) else f"{v:g}"


class Face:
    """One instance of a variable font: its outlines, advances and kerning."""

    def __init__(self, file, **axes):
        font = TTFont(os.path.join(FONTS, file))
        if "fvar" in font:
            font = instancer.instantiateVariableFont(font, axes)
        self.upm = font["head"].unitsPerEm
        self.cmap = font.getBestCmap()
        self.glyphs = font.getGlyphSet()
        self.advance = {name: adv for name, (adv, _) in font["hmtx"].metrics.items()}
        # Pair kerning from GPOS, which is all a line of Latin text needs.
        self.kern_lookups = []
        gpos = font["GPOS"].table if "GPOS" in font else None
        if gpos:
            wanted = {
                i
                for rec in gpos.FeatureList.FeatureRecord
                if rec.FeatureTag == "kern"
                for i in rec.Feature.LookupListIndex
            }
            for i in sorted(wanted):
                lookup = gpos.LookupList.Lookup[i]
                subtables = []
                for sub in lookup.SubTable:
                    if lookup.LookupType == 9:
                        if sub.ExtensionLookupType != 2:
                            continue
                        sub = sub.ExtSubTable
                    elif lookup.LookupType != 2:
                        continue
                    subtables.append((set(sub.Coverage.glyphs), sub))
                self.kern_lookups.append(subtables)

    def kerning(self, left, right):
        total = 0
        for subtables in self.kern_lookups:
            for covered, sub in subtables:
                if left not in covered:
                    continue
                if sub.Format == 1:
                    pairs = sub.PairSet[sub.Coverage.glyphs.index(left)].PairValueRecord
                    rec = next((r for r in pairs if r.SecondGlyph == right), None)
                    if rec is None:
                        continue
                    value = rec.Value1
                else:
                    c1 = sub.ClassDef1.classDefs.get(left, 0)
                    c2 = sub.ClassDef2.classDefs.get(right, 0)
                    value = sub.Class1Record[c1].Class2Record[c2].Value1
                total += getattr(value, "XAdvance", 0) or 0
                break
        return total

    def outline(self, text, tracking=0.0):
        """A line of text as path data in font units, and its advance."""
        pen = SVGPathPen(self.glyphs, ntos=num)
        step = round(tracking * self.upm)
        x, prev = 0, None
        for ch in text:
            name = self.cmap.get(ord(ch))
            if name is None:
                sys.exit(f"{ch!r} is not in the font")
            if prev:
                x += self.kerning(prev, name)
            self.glyphs[name].draw(TransformPen(pen, (1, 0, 0, 1, x, 0)))
            x += self.advance[name] + step
            prev = name
        return pen.getCommands(), x - step

    def width(self, text, size, tracking=0.0):
        return self.outline(text, tracking)[1] * size / self.upm

    def text(self, text, x, y, size, fill, tracking=0.0, anchor="start", attrs=""):
        d, advance = self.outline(text, tracking)
        width = advance * size / self.upm
        x -= {"start": 0, "middle": width / 2, "end": width}[anchor]
        s = size / self.upm
        return (
            f'<path transform="translate({num(x)} {num(y)}) scale({s:.5f} {-s:.5f})" '
            f'fill="{fill}" {attrs} d="{d}"/>'
        )


SANS_BOLD = Face("UbuntuSans[wdth,wght].ttf", wght=700, wdth=100)
SANS = Face("UbuntuSans[wdth,wght].ttf", wght=400, wdth=100)
SANS_MEDIUM = Face("UbuntuSans[wdth,wght].ttf", wght=500, wdth=100)
NIXIE = Face("UbuntuSans[wdth,wght].ttf", wght=200, wdth=75)
MONO = Face("UbuntuSansMono[wght].ttf", wght=500)


def arc(cx, cy, r, start, end):
    """An arc path, angles in degrees clockwise from twelve o'clock."""

    def at(a):
        t = math.radians(a - 90)
        return cx + r * math.cos(t), cy + r * math.sin(t)

    x0, y0 = at(start)
    x1, y1 = at(end)
    large = 1 if end - start > 180 else 0
    return f"M{num(x0)} {num(y0)} A{num(r)} {num(r)} 0 {large} 1 {num(x1)} {num(y1)}"


# ---- the keys: icons in a key's own coordinates, 0 to K --------------------

K = 74  # key edge
G = 10  # gap between keys


def key_terminal():
    return (
        f'<rect x="17" y="22" width="40" height="31" rx="5" fill="{POLAR}" stroke="{FROST}" stroke-width="2.4"/>'
        f'<path d="M25 31.5 l6 5 l-6 5" fill="none" stroke="{GREEN}" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"/>'
        f'<path d="M34.5 42 h9" stroke="{GREEN}" stroke-width="2.6" stroke-linecap="round"/>'
    )


def key_globe():
    return (
        f'<g fill="none" stroke="{FROST}" stroke-width="2.3">'
        f'<circle cx="37" cy="37" r="16"/><ellipse cx="37" cy="37" rx="7" ry="16"/>'
        f'<path d="M21 37 H53 M23.2 29 H50.8 M23.2 45 H50.8"/></g>'
    )


def key_folder():
    return (
        f'<path d="M18 26 q0 -3 3 -3 h10 l4 4 h18 q3 0 3 3 v3 H18 z" fill="{ORANGE}"/>'
        f'<path d="M18 31 h38 v18 q0 3 -3 3 h-32 q-3 0 -3 -3 z" fill="{YELLOW}"/>'
    )


def key_play_pause():
    return (
        f'<path d="M20 24 L20 50 L40 37 Z" fill="{GREEN}" stroke="{GREEN}" stroke-width="2" stroke-linejoin="round"/>'
        f'<rect x="44" y="25" width="5" height="24" rx="1.5" fill="{SNOW}"/>'
        f'<rect x="52" y="25" width="5" height="24" rx="1.5" fill="{SNOW}"/>'
    )


def key_next():
    return (
        f'<g fill="{SNOW}" stroke="{SNOW}" stroke-width="2" stroke-linejoin="round">'
        f'<path d="M19 26 L19 48 L35 37 Z"/><path d="M35 26 L35 48 L51 37 Z"/></g>'
        f'<rect x="52" y="25" width="4.5" height="24" rx="1.5" fill="{SNOW}"/>'
    )


def key_volume():
    level = 0.7
    return (
        f'<path d="M19 30 h7 l9 -8 v26 l-9 -8 h-7 z" fill="{FROST}" stroke="{FROST}" stroke-width="1.5" stroke-linejoin="round"/>'
        f'<g fill="none" stroke="{FROST}" stroke-width="2.4" stroke-linecap="round">'
        f'<path d="M40.5 29.5 a8 8 0 0 1 0 11"/><path d="M45.5 24.5 a15 15 0 0 1 0 21"/></g>'
        f'<rect x="16" y="57" width="42" height="4" rx="2" fill="{POLAR}"/>'
        f'<rect x="16" y="57" width="{num(42 * level)}" height="4" rx="2" fill="{FROST}"/>'
    )


def key_cpu():
    points = [30, 34, 27, 38, 44, 36, 41, 52, 47, 40, 45, 55, 50, 58, 49, 43, 47, 39]
    xs = [8 + i * (58 / (len(points) - 1)) for i in range(len(points))]
    line = " ".join(f"{num(x)} {num(K - 12 - (p - 25) * 0.62)}" for x, p in zip(xs, points))
    return (
        f'<path d="M8 {K - 8} L{line} L66 {K - 8} Z" fill="url(#cpu-fill)"/>'
        f'<path d="M{line}" fill="none" stroke="{FROST}" stroke-width="1.8" stroke-linejoin="round" stroke-linecap="round"/>'
        + SANS_MEDIUM.text("cpu", 11, 22, 12, SNOW1)
        + SANS_BOLD.text("23%", 64, 23, 15, SNOW, anchor="end")
    )


def key_weather():
    rays = ""
    for i in range(8):
        t = math.radians(i * 45)
        x0, y0 = 30 + 12.5 * math.cos(t), 30 + 12.5 * math.sin(t)
        x1, y1 = 30 + 16.5 * math.cos(t), 30 + 16.5 * math.sin(t)
        rays += f'<path d="M{num(x0)} {num(y0)} L{num(x1)} {num(y1)}"/>'
    return (
        f'<g stroke="{YELLOW}" stroke-width="2.4" stroke-linecap="round">{rays}</g>'
        f'<circle cx="30" cy="30" r="8.5" fill="{YELLOW}"/>'
        f'<g fill="{SNOW2}"><circle cx="35" cy="45" r="7.5"/><circle cx="45" cy="40" r="10"/>'
        f'<circle cx="54.5" cy="46" r="6.5"/><rect x="28" y="45" width="32" height="7.5" rx="3.75"/></g>'
    )


def key_timer():
    return (
        f'<circle cx="37" cy="37" r="18" fill="none" stroke="{POLAR}" stroke-width="4.5"/>'
        f'<path d="{arc(37, 37, 18, 0, 250)}" fill="none" stroke="{ORANGE}" stroke-width="4.5" stroke-linecap="round"/>'
        + MONO.text("4:20", 37, 41.5, 12.5, SNOW, anchor="middle")
    )


def key_mic_muted():
    return (
        f'<rect width="{K}" height="{K}" rx="10" fill="{RED}" opacity="0.22"/>'
        f'<g fill="none" stroke="{SNOW}" stroke-width="2.4" stroke-linecap="round">'
        f'<rect x="31" y="18" width="12" height="22" rx="6"/>'
        f'<path d="M25 33 a12 12 0 0 0 24 0 M37 45 v7 M31 52 h12"/></g>'
        f'<path d="M22 19 L52 55" stroke="{RED}" stroke-width="3.2" stroke-linecap="round"/>'
    )


def key_lock():
    return (
        f'<path d="M29 34 v-6 a8 8 0 0 1 16 0 v6" fill="none" stroke="{PURPLE}" stroke-width="3.2"/>'
        f'<rect x="24.5" y="33" width="25" height="20" rx="3.5" fill="{PURPLE}"/>'
        f'<circle cx="37" cy="41.5" r="2.6" fill="{POLAR}"/><rect x="36" y="42" width="2" height="6" rx="1" fill="{POLAR}"/>'
    )


def key_gamepad():
    return (
        f'<path d="M26 28 H48 Q55 28 56.5 36 L59 47 Q60 54 54.5 54 Q51.5 54 49 50 L46 46 H28 '
        f'L25 50 Q22.5 54 19.5 54 Q14 54 15 47 L17.5 36 Q19 28 26 28 Z" fill="{FROST2}"/>'
        f'<g fill="{POLAR}"><rect x="21.5" y="36.3" width="11" height="3.4" rx="1"/>'
        f'<rect x="25.3" y="32.5" width="3.4" height="11" rx="1"/>'
        f'<circle cx="47" cy="34.5" r="2.3"/><circle cx="51.5" cy="39" r="2.3"/></g>'
    )


KEYS = [
    key_terminal, key_globe, key_folder,
    key_play_pause, key_next, key_volume,
    key_cpu, key_weather, key_timer,
    key_mic_muted, key_lock, key_gamepad,
]


# ---- the module -------------------------------------------------------------


def knob(cx, cy, lit, color):
    """A knob and its ring of four LEDs, `lit` of them on."""
    out = [f'<circle cx="{num(cx)}" cy="{num(cy)}" r="27" fill="none" stroke="#0b0d11" stroke-width="7"/>']
    for i in range(4):
        start = i * 90 + 12
        path = arc(cx, cy, 27, start, start + 66)
        if i < lit:
            out.append(
                f'<path d="{path}" fill="none" stroke="{color}" stroke-width="4.5" '
                f'stroke-linecap="round" filter="url(#glow)"/>'
            )
        else:
            out.append(
                f'<path d="{path}" fill="none" stroke="#262b35" stroke-width="4.5" stroke-linecap="round"/>'
            )
    out.append(f'<circle cx="{num(cx)}" cy="{num(cy)}" r="20.5" fill="url(#knob)" stroke="#07080a" stroke-width="1.5"/>')

    ticks = "".join(
        f'M{num(cx + 17.5 * math.cos(math.radians(a)))} {num(cy + 17.5 * math.sin(math.radians(a)))} '
        f'L{num(cx + 20 * math.cos(math.radians(a)))} {num(cy + 20 * math.sin(math.radians(a)))} '
        for a in range(0, 360, 12)
    )
    out.append(f'<path d="{ticks}" stroke="#ffffff" stroke-opacity="0.08" stroke-width="1.4"/>')
    out.append(f'<circle cx="{num(cx)}" cy="{num(cy)}" r="13" fill="url(#knob-top)"/>')
    out.append(f'<path d="M{num(cx)} {num(cy - 17)} v6" stroke="{SNOW}" stroke-width="2.4" stroke-linecap="round"/>')
    return "".join(out)


def tube(x, y, w, h, digit):
    """A nixie tube lit with one digit."""
    r = w / 2
    shape = (
        f"M{num(x)} {num(y + r)} A{num(r)} {num(r)} 0 0 1 {num(x + w)} {num(y + r)} "
        f"V{num(y + h - 5)} Q{num(x + w)} {num(y + h)} {num(x + w - 5)} {num(y + h)} "
        f"H{num(x + 5)} Q{num(x)} {num(y + h)} {num(x)} {num(y + h - 5)} Z"
    )
    size = 70
    base = y + h - 16
    return (
        f'<path d="{shape}" fill="url(#glass)" stroke="#6b4a32" stroke-opacity="0.7" stroke-width="1.2"/>'
        f'<path d="{shape}" fill="url(#mesh)"/>'
        + NIXIE.text("8", x + w / 2, base, size, NEON, anchor="middle", attrs='opacity="0.1"')
        + f'<g filter="url(#neon)">'
        + NIXIE.text(digit, x + w / 2, base, size, "#ff9a4a", anchor="middle")
        + "</g>"
        + f'<path d="M{num(x + 6)} {num(y + r - 4)} Q{num(x + 7)} {num(y + 8)} {num(x + r)} {num(y + 5)}" '
        f'fill="none" stroke="#ffffff" stroke-opacity="0.18" stroke-width="1.6" stroke-linecap="round"/>'
    )


def module(mx, my):
    pad, knob_row, gap1, gap2 = 22, 58, 14, 14
    grid = 3 * K + 2 * G
    screen_h = grid * 384 / 720
    width = grid + 2 * pad
    height = pad - 4 + knob_row + gap1 + screen_h + gap2 + 4 * K + 3 * G + pad
    x0 = mx - width / 2
    gx = x0 + pad
    out = [
        f'<rect x="{num(x0)}" y="{num(my)}" width="{num(width)}" height="{num(height)}" rx="26" '
        f'fill="url(#bezel)" stroke="#2c313c" stroke-width="1.5" filter="url(#shadow)"/>',
        f'<rect x="{num(x0 + 1.5)}" y="{num(my + 1.5)}" width="{num(width - 3)}" height="{num(height - 3)}" rx="24.5" '
        f'fill="none" stroke="#ffffff" stroke-opacity="0.05"/>',
    ]
    ky = my + pad - 4 + knob_row / 2
    out.append(knob(gx + K / 2, ky, 3, FROST))
    out.append(knob(gx + 2 * (K + G) + K / 2, ky, 1, NEON))

    sy = my + pad - 4 + knob_row + gap1
    out.append(
        f'<rect x="{num(gx - 3)}" y="{num(sy - 3)}" width="{num(grid + 6)}" height="{num(screen_h + 6)}" rx="8" fill="#050608"/>'
        f'<rect x="{num(gx)}" y="{num(sy)}" width="{num(grid)}" height="{num(screen_h)}" rx="5" fill="url(#room)"/>'
    )
    tw, th, tg, colon = 40, 90, 6, 18
    total = 4 * tw + 2 * tg + colon
    tx = gx + (grid - total) / 2
    ty = sy + 10
    for i, digit in enumerate("1247"):
        x = tx + i * (tw + tg) + (colon - tg if i >= 2 else 0)
        out.append(tube(x, ty, tw, th, digit))
    cx = tx + 2 * tw + tg + colon / 2
    out.append(
        f'<g fill="#ff9a4a" filter="url(#neon)"><circle cx="{num(cx)}" cy="{num(ty + 40)}" r="2.6"/>'
        f'<circle cx="{num(cx)}" cy="{num(ty + 62)}" r="2.6"/></g>'
    )
    out.append(MONO.text("WED 30 SEP", gx + grid / 2, sy + screen_h - 9, 10, BRASS, tracking=0.18, anchor="middle"))

    top = sy + screen_h + gap2
    for i, draw in enumerate(KEYS):
        x = gx + (i % 3) * (K + G)
        y = top + (i // 3) * (K + G)
        out.append(
            f'<g transform="translate({num(x)} {num(y)})">'
            f'<rect y="2" width="{K}" height="{K}" rx="10" fill="#000" opacity="0.5"/>'
            f'<rect width="{K}" height="{K}" rx="10" fill="url(#key)"/>'
            f"{draw()}"
            f'<rect x="0.75" y="0.75" width="{K - 1.5}" height="{K - 1.5}" rx="9.25" fill="none" stroke="#ffffff" stroke-opacity="0.07" stroke-width="1.5"/>'
            f"</g>"
        )
    return "".join(out), (x0, my, width, height)


# ---- the card ---------------------------------------------------------------


def chips(x, y, labels, max_x):
    out, cx, cy = [], x, y
    size, h = 17, 38
    for label, dot in labels:
        w = 16 + 8 + 9 + SANS_MEDIUM.width(label, size) + 18
        if cx + w > max_x:
            cx, cy = x, cy + h + 12
        out.append(
            f'<rect x="{num(cx)}" y="{num(cy)}" width="{num(w)}" height="{h}" rx="{h / 2}" fill="#141821" stroke="#2b313d"/>'
            f'<circle cx="{num(cx + 20)}" cy="{num(cy + h / 2)}" r="4" fill="{dot}"/>'
            + SANS_MEDIUM.text(label, cx + 33, cy + h / 2 + 6, size, SNOW2)
        )
        cx += w + 10
    return "".join(out), cy + h


def card(rounded):
    radius = 28 if rounded else 0
    _, (_, _, _, mh) = module(1000, 0)
    mod, _ = module(1000, (H - mh) / 2)

    # The text block, centred on the card: 342 tall from the top of the
    # eyebrow's capitals to the bottom of the command.
    left = 84
    top = (H - 342) / 2
    body = []
    body.append(MONO.text("CORSAIR GALLEON 100 SD  ·  LINUX", left, top + 11, 15, FROST, tracking=0.16))
    title_size = 80
    body.append(SANS_BOLD.text("galdeck", left, top + 95, title_size, SNOW, tracking=-0.01))
    gw = SANS_BOLD.width("galdeck", title_size, tracking=-0.01)
    body.append(SANS_BOLD.text("-daemon", left + gw + 2, top + 95, title_size, FROST, tracking=-0.01))
    for i, line in enumerate(
        [
            "Turn the Stream Deck built into your keyboard",
            "into a launcher, set up from your browser.",
        ]
    ):
        body.append(SANS.text(line, left, top + 151 + i * 36, 25, SNOW1, attrs='opacity="0.85"'))
    chip_svg, bottom = chips(
        left,
        top + 228,
        [
            ("keys & knobs", FROST),
            ("live widgets", GREEN),
            ("RGB lighting", PURPLE),
            ("browser UI", YELLOW),
        ],
        800,
    )
    body.append(chip_svg)

    cmd_y = bottom + 28
    prompt, command = "$ ", "galdeck ui"
    pw = MONO.width(prompt, 20)
    cw = MONO.width(command, 20)
    body.append(
        f'<rect x="{left}" y="{num(cmd_y)}" width="{num(pw + cw + 44)}" height="48" rx="12" fill="#0a0c10" stroke="#2b313d"/>'
        + MONO.text(prompt, left + 22, cmd_y + 31, 20, GREEN)
        + MONO.text(command, left + 22 + pw, cmd_y + 31, 20, SNOW)
    )

    defs = f"""
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#10131a"/><stop offset="1" stop-color="#090b0f"/>
    </linearGradient>
    <radialGradient id="glow-frost" cx="1000" cy="300" r="420" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{FROST}" stop-opacity="0.26"/><stop offset="1" stop-color="{FROST}" stop-opacity="0"/>
    </radialGradient>
    <radialGradient id="glow-neon" cx="1000" cy="170" r="220" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{NEON}" stop-opacity="0.16"/><stop offset="1" stop-color="{NEON}" stop-opacity="0"/>
    </radialGradient>
    <radialGradient id="glow-purple" cx="120" cy="640" r="520" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{PURPLE}" stop-opacity="0.16"/><stop offset="1" stop-color="{PURPLE}" stop-opacity="0"/>
    </radialGradient>
    <pattern id="dots" width="22" height="22" patternUnits="userSpaceOnUse">
      <circle cx="1" cy="1" r="1" fill="#ffffff" fill-opacity="0.045"/>
    </pattern>
    <linearGradient id="bar" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0" stop-color="{FROST3}"/><stop offset="0.3" stop-color="{FROST}"/>
      <stop offset="0.55" stop-color="{PURPLE}"/><stop offset="0.8" stop-color="{NEON}"/>
      <stop offset="1" stop-color="{YELLOW}"/>
    </linearGradient>
    <linearGradient id="bezel" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#1d2129"/><stop offset="1" stop-color="#101217"/>
    </linearGradient>
    <linearGradient id="key" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{STORM}"/><stop offset="1" stop-color="{POLAR}"/>
    </linearGradient>
    <linearGradient id="room" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#120d09"/><stop offset="1" stop-color="#0a0806"/>
    </linearGradient>
    <linearGradient id="glass" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0" stop-color="#3a2415" stop-opacity="0.55"/>
      <stop offset="0.5" stop-color="#1a110b" stop-opacity="0.25"/>
      <stop offset="1" stop-color="#3a2415" stop-opacity="0.55"/>
    </linearGradient>
    <pattern id="mesh" width="5" height="5" patternUnits="userSpaceOnUse" patternTransform="rotate(30)">
      <path d="M0 0 H5 M0 0 V5" stroke="{NEON}" stroke-opacity="0.09" stroke-width="0.6"/>
    </pattern>
    <radialGradient id="knob" cx="0.4" cy="0.3" r="0.8">
      <stop offset="0" stop-color="#454b57"/><stop offset="1" stop-color="#15181e"/>
    </radialGradient>
    <radialGradient id="knob-top" cx="0.4" cy="0.3" r="0.9">
      <stop offset="0" stop-color="#3a3f4a"/><stop offset="1" stop-color="#1b1e25"/>
    </radialGradient>
    <linearGradient id="cpu-fill" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{FROST}" stop-opacity="0.5"/><stop offset="1" stop-color="{FROST}" stop-opacity="0.04"/>
    </linearGradient>
    <filter id="glow" x="-50%" y="-50%" width="200%" height="200%">
      <feGaussianBlur stdDeviation="3" result="b"/>
      <feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge>
    </filter>
    <filter id="neon" x="-60%" y="-40%" width="220%" height="180%">
      <feGaussianBlur in="SourceGraphic" stdDeviation="1.6" result="b1"/>
      <feGaussianBlur in="SourceGraphic" stdDeviation="6" result="b2"/>
      <feMerge><feMergeNode in="b2"/><feMergeNode in="b2"/><feMergeNode in="b1"/><feMergeNode in="SourceGraphic"/></feMerge>
    </filter>
    <filter id="shadow" x="-30%" y="-20%" width="160%" height="150%">
      <feDropShadow dx="0" dy="18" stdDeviation="20" flood-color="#000" flood-opacity="0.65"/>
    </filter>
    <filter id="bar-glow" x="-10%" y="-400%" width="120%" height="900%">
      <feGaussianBlur stdDeviation="12"/>
    </filter>
    <clipPath id="card"><rect width="{W}" height="{H}" rx="{radius}"/></clipPath>
  </defs>"""

    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-labelledby="title">
  <title id="title">galdeck-daemon: turn the Stream Deck built into a Corsair Galleon 100 SD keyboard into a launcher</title>
  <!-- Drawn by scripts/hero.py; edit that and run it again rather than this file. -->{defs}
  <g clip-path="url(#card)">
    <rect width="{W}" height="{H}" fill="url(#bg)"/>
    <rect width="{W}" height="{H}" fill="url(#dots)"/>
    <rect width="{W}" height="{H}" fill="url(#glow-purple)"/>
    <rect width="{W}" height="{H}" fill="url(#glow-frost)"/>
    <rect width="{W}" height="{H}" fill="url(#glow-neon)"/>
    <rect x="0" y="{H - 16}" width="{W}" height="16" fill="url(#bar)" filter="url(#bar-glow)"/>
    <rect x="0" y="{H - 5}" width="{W}" height="5" fill="url(#bar)"/>
    {"".join(body)}
    {mod}
  </g>
  <rect x="0.5" y="0.5" width="{W - 1}" height="{H - 1}" rx="{max(radius - 0.5, 0)}" fill="none" stroke="#ffffff" stroke-opacity="0.08"/>
</svg>
"""


def render_png(svg_path, png_path, work):
    chrome = next((c for c in ("google-chrome", "chromium", "chromium-browser") if shutil.which(c)), None)
    if chrome is None:
        sys.exit("no Chrome or Chromium to render the PNG with")
    # A profile of its own, so this never lands in (or waits on) a browser
    # that is already open.
    subprocess.run(
        [
            chrome, "--headless=new", "--disable-gpu", "--hide-scrollbars",
            "--no-first-run", "--no-default-browser-check",
            f"--user-data-dir={os.path.join(work, 'chrome')}",
            f"--window-size={W},{H}", f"--screenshot={png_path}",
            "file://" + svg_path,
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def main():
    docs = os.path.join(REPO, "docs")
    work = os.path.join(REPO, "target", "hero")
    os.makedirs(work, exist_ok=True)
    with open(os.path.join(docs, "hero.svg"), "w") as f:
        f.write(card(rounded=True))
    square = os.path.join(work, "social-preview.svg")
    with open(square, "w") as f:
        f.write(card(rounded=False))
    render_png(square, os.path.join(docs, "social-preview.png"), work)
    for name in ("hero.svg", "social-preview.png"):
        path = os.path.join(docs, name)
        print(f"{os.path.relpath(path, REPO)}  {os.path.getsize(path) // 1024} KiB")


if __name__ == "__main__":
    main()
