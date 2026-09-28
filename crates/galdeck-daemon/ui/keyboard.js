// The Keyboard tab: the keyboard's own lighting, drawn from its LED layout.
//
// Imported by app.js, which signs in first. This reads the token that leaves
// in sessionStorage at the moment of each call, and asks for nothing until
// the tab is opened, so it never races the sign-in.
//
// Everything shown is drawn by the daemon. The live view is what the keyboard
// was last sent, and a design is the keyboard's own renderer run on lighting
// that is not saved -- so this is only the glass it is shown through, and
// cannot disagree with the keyboard.
//
// A design is edited in one place: the TOML in the text box. Dropping an
// effect, a reaction or a colour changes the lighting the daemon last read
// from that text and writes it back, so dragging and typing never undo each
// other; saving sends that same lighting to a profile or theme file.

/// How often the live view asks what the keyboard shows.
const LIVE_EVERY_MS = 50;
/// Pixels per key width in the drawing, and a key's face as a share of the
/// space it takes, so neighbours keep a gap between them.
const UNIT = 40;
const FACE = 0.86;
/// How long a design's preview runs before it loops, and how smoothly.
const PREVIEW_SECONDS = 4;
const PREVIEW_FPS = 20;
/// When a key clicked in a design is pressed, into the preview.
const PRESS_AT = 0.3;
/// Keys wider than one key, in key widths. The daemon says where each key's
/// middle is; how wide it is, it does not need to know.
const WIDE = {
  Space: 6.25, LShift: 2.25, RShift: 2.75, Enter: 2.25, Backspace: 2,
  Tab: 1.5, CapsLock: 1.75, Backslash: 1.5, LCtrl: 1.25, LWin: 1.25,
  LAlt: 1.25, RAlt: 1.25, Fn: 1.25, Menu: 1.25, RCtrl: 1.25,
};
/// What a key shows while there is nothing to show.
const DARK = "#101217";
/// The lighting a design starts from.
const EXAMPLE = `effect = "wave"
colors = ["#88c0d0", "#b48ead"]
speed = 0.25
brightness = 80

[reactive]
effect = "ripple"
color = "#ffffff"
`;
/// How a dragged item names itself to what it is dropped on.
const DRAG_TYPE = "application/x-galdeck-lighting";
/// The speed slider runs on a log scale over what the daemon takes, 0.01 to
/// 4 cycles a second, so the slow end, where most looks live, is not a
/// sliver of it. Its middle is the daemon's default, 0.2.
const SPEED_MIN = 0.01;
const SPEED_MAX = 4;
const toSpeed = (v) => Number((SPEED_MIN * (SPEED_MAX / SPEED_MIN) ** (v / 100)).toPrecision(2));
const fromSpeed = (s) => (100 * Math.log(s / SPEED_MIN)) / Math.log(SPEED_MAX / SPEED_MIN);

/// Ready-made looks for the whole keyboard. Dropping one sets the effect,
/// its colours and pace, and keeps painted keys, the bar and the reaction.
const EFFECTS = [
  { id: "white", name: "Plain white", effect: "static", colors: ["#ffffff"] },
  { id: "frost", name: "Frost", effect: "gradient", colors: ["#88c0d0", "#5e81ac"] },
  { id: "aurora", name: "Aurora wave", effect: "wave", colors: ["#2ec27e", "#00c6ff", "#b48ead"], speed: 0.2 },
  { id: "sunset", name: "Sunset", effect: "gradient", colors: ["#ff7e5f", "#feb47b", "#6a3093"] },
  { id: "ocean", name: "Ocean breath", effect: "breathe", colors: ["#0077be", "#00c6ff"], speed: 0.15 },
  { id: "rainbow", name: "Rainbow", effect: "spectrum", speed: 0.1 },
  { id: "fire", name: "Fire", effect: "wave", colors: ["#ff2200", "#ff8c00", "#ffd000"], speed: 0.6 },
  { id: "nixie", name: "Nixie glow", effect: "breathe", colors: ["#ff7a22"], speed: 0.08, brightness: 35 },
  { id: "matrix", name: "Matrix", effect: "wave", colors: ["#003b00", "#00ff41"], speed: 0.35 },
  { id: "candy", name: "Candy", effect: "wave", colors: ["#ff6ec7", "#7df9ff"], speed: 0.25 },
  { id: "off", name: "Keyboard's own", effect: "off" },
];
/// How the keys can answer a press.
const REACTIONS = [
  { id: "ripple", name: "Ripple", reactive: { effect: "ripple", color: "#ffffff" } },
  { id: "glow", name: "Glow", reactive: { effect: "glow", color: "#ffffff" } },
  { id: "accent", name: "Accent ripple", reactive: { effect: "ripple", color: "@accent" } },
  { id: "slow", name: "Slow glow", reactive: { effect: "glow", color: "#ffffff", fade_ms: 2000 } },
  { id: "none", name: "No reaction", reactive: null },
];
/// Colours to paint with: onto a key, a group, or the light bar.
const COLOURS = [
  "#ffffff", "#ff2222", "#ff8c00", "#ffd000", "#2ec27e",
  "#00c6ff", "#3b5bff", "#b48ead", "#ff6ec7", "#000000",
];

const SVG = "http://www.w3.org/2000/svg";
const el = (id) => document.getElementById(id);

async function call(request) {
  const token = sessionStorage.getItem("galdeck-token") ?? "";
  const response = await fetch("/api/call", {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${token}`,
    },
    body: JSON.stringify(request),
  });
  if (!response.ok) {
    throw new Error(`${response.status}: ${await response.text()}`);
  }
  const reply = await response.json();
  if (reply.result === "error") throw new Error(reply.message);
  return reply;
}

const state = {
  /// Each LED's shape, and what it is, by its index in a frame.
  shapes: new Map(),
  leds: [],
  drawn: false,
  tilesBuilt: false,
  mode: "live",
  live: 0,
  /// A design's frames, and when they started playing.
  clip: null,
  started: 0,
  animation: 0,
  typing: 0,
  /// The design as the daemon last read it, and the text it read it from.
  /// `form` is null when that text does not read: a drop would overwrite
  /// what is being typed, so drops wait for it to be fixed.
  form: null,
  formText: null,
  /// How many previews have been asked for, so only the latest is shown.
  asked: 0,
  /// Whether the tab was showing when last looked at.
  open: false,
  /// The text last saved, and where to.
  saved: null,
};

function status(text, isError = false) {
  const node = el("kb-status");
  node.textContent = text;
  node.classList.toggle("error", isError);
}

/// Colour every key from a frame: six hex digits per LED, in frame order.
function paint(frame) {
  for (const [index, shape] of state.shapes) {
    const at = index * 6;
    shape.style.setProperty("fill", `#${frame.slice(at, at + 6)}`);
  }
}

function paintDark() {
  for (const shape of state.shapes.values()) shape.style.setProperty("fill", DARK);
}

/// Let `node` take a dragged gallery item; `target` says what it is.
function takesDrops(node, target) {
  node.addEventListener("dragover", (event) => {
    if (!event.dataTransfer.types.includes(DRAG_TYPE)) return;
    event.preventDefault();
    // A key is inside the board: only the key it would land on lights up.
    event.stopPropagation();
    event.dataTransfer.dropEffect = "copy";
    node.classList.add("kb-drop");
  });
  node.addEventListener("dragleave", () => node.classList.remove("kb-drop"));
  node.addEventListener("drop", (event) => {
    node.classList.remove("kb-drop");
    const raw = event.dataTransfer.getData(DRAG_TYPE);
    if (!raw) return;
    event.preventDefault();
    event.stopPropagation();
    dropped(JSON.parse(raw), target, event.shiftKey);
  });
}

async function drawBoard() {
  const { leds } = await call({ cmd: "keyboard_layout" });
  state.leds = leds;
  const board = el("kb-board");
  board.replaceChildren();
  state.shapes.clear();
  const xs = leds.map((led) => led.x);
  const ys = leds.map((led) => led.y);
  const left = (Math.min(...xs) - 1.5) * UNIT;
  const top = (Math.min(...ys) - 0.8) * UNIT;
  const width = (Math.max(...xs) - Math.min(...xs) + 3) * UNIT;
  const height = (Math.max(...ys) - Math.min(...ys) + 1.6) * UNIT;
  board.setAttribute("viewBox", `${left} ${top} ${width} ${height}`);
  for (const led of leds) {
    const bar = led.group === "bar";
    const w = (bar ? 2.6 : (WIDE[led.name] ?? 1) - (1 - FACE)) * UNIT;
    const h = (bar ? 0.3 : FACE) * UNIT;
    const shape = document.createElementNS(SVG, "rect");
    shape.setAttribute("x", `${led.x * UNIT - w / 2}`);
    shape.setAttribute("y", `${led.y * UNIT - h / 2}`);
    shape.setAttribute("width", `${w}`);
    shape.setAttribute("height", `${h}`);
    shape.setAttribute("rx", bar ? "3" : "5");
    shape.classList.add("kb-key");
    const title = document.createElementNS(SVG, "title");
    title.textContent = `${led.name} (${led.group}, LED ${led.index})`;
    shape.append(title);
    shape.addEventListener("pointerenter", () => {
      el("kb-hover").textContent = `${led.name} -- ${led.group}, LED ${led.index}`;
    });
    shape.addEventListener("click", () => pressed(led.name));
    takesDrops(shape, { kind: "key", led });
    board.append(shape);
    state.shapes.set(led.index, shape);
  }
  takesDrops(board, { kind: "board" });
  paintDark();
  buildGroups();
  state.drawn = true;
}

/// A swatch showing `colours`, with nothing written into the markup: the
/// page's policy drops style attributes, so the look is set here.
function swatch(colours) {
  const node = document.createElement("span");
  node.className = "kb-swatch";
  const shown = colours.map((c) => (c.startsWith("#") ? c : "#88c0d0"));
  node.style.setProperty(
    "background",
    shown.length > 1 ? `linear-gradient(90deg, ${shown.join(", ")})` : shown[0],
  );
  return node;
}

function tile(item, colours, describe) {
  const node = document.createElement("div");
  node.className = "kb-tile";
  node.draggable = true;
  node.title = describe;
  node.append(swatch(colours));
  const name = document.createElement("span");
  name.textContent = item.name;
  node.append(name);
  node.addEventListener("dragstart", (event) => {
    event.dataTransfer.setData(DRAG_TYPE, JSON.stringify(item));
    event.dataTransfer.effectAllowed = "copy";
  });
  return node;
}

/// The effects, reactions and colours to drag: the same every time.
function buildTiles() {
  el("kb-effects").replaceChildren(
    ...EFFECTS.map((effect) => {
      const colours = effect.effect === "spectrum"
        ? ["#ff2222", "#ffd000", "#2ec27e", "#00c6ff", "#3b5bff", "#ff6ec7"]
        : effect.effect === "off" ? ["#2c313c"] : effect.colors;
      return tile({ kind: "effect", ...effect }, colours, `${effect.effect}: drag onto the keyboard`);
    }),
  );
  el("kb-reactions").replaceChildren(
    ...REACTIONS.map((reaction) => tile(
      { kind: "reaction", ...reaction },
      [reaction.reactive?.color ?? "#2c313c"],
      "drag onto the keyboard: how a pressed key answers",
    )),
  );
  el("kb-colours").replaceChildren(
    ...COLOURS.map((colour) => tile(
      { kind: "colour", name: colour, colour },
      [colour],
      "drag onto a key (hold Shift for its whole group), a group, or the light bar",
    )),
  );
  state.tilesBuilt = true;
}

/// The groups a colour can be dropped on, as the layout names them.
function buildGroups() {
  const groups = ["all", ...new Set(state.leds.map((led) => led.group))];
  el("kb-groups").replaceChildren(
    ...groups.map((group) => {
      const chip = document.createElement("span");
      chip.className = "kb-chip";
      chip.textContent = group;
      chip.title = `drop a colour here to paint ${group === "all" ? "every key" : `the ${group}`}`;
      takesDrops(chip, { kind: "group", group });
      return chip;
    }),
  );
}

/// Whether the tab is on screen; nothing is asked of the daemon when not.
function showing() {
  return el("tab-keyboard").classList.contains("active");
}

async function liveTick() {
  state.live = 0;
  if (!showing() || state.mode !== "live") return;
  try {
    const { frame } = await call({ cmd: "keyboard_frame" });
    // Asked before a switch to designing, answered after: not wanted.
    if (!showing() || state.mode !== "live") return;
    if (frame) {
      paint(frame);
      status("What the keyboard shows now.");
    } else {
      paintDark();
      status("The daemon is not lighting the keyboard: give a theme or a profile a [lighting] table, or design one and save it.");
    }
  } catch (error) {
    status(error.message, true);
  }
  if (showing() && state.mode === "live") {
    state.live = setTimeout(liveTick, LIVE_EVERY_MS);
  }
}

function showProblems(diagnostics) {
  const list = el("kb-problems");
  list.replaceChildren(
    ...diagnostics.map((d) => {
      const item = document.createElement("li");
      item.textContent = `${d.code} ${d.path}: ${d.message}`;
      return item;
    }),
  );
}

/// Draw the design in the text box, with `presses` answered along the way.
async function preview(presses = []) {
  const lighting = el("kb-lighting").value;
  const asked = ++state.asked;
  try {
    const reply = await call({
      cmd: "preview_lighting",
      lighting,
      seconds: PREVIEW_SECONDS,
      fps: PREVIEW_FPS,
      presses,
    });
    // Overtaken while it was drawn -- by a later preview, an edit, or a
    // switch to live -- what it shows is no longer what is written.
    if (asked !== state.asked || el("kb-lighting").value !== lighting || state.mode !== "design") {
      return;
    }
    state.clip = reply;
    state.started = performance.now();
    state.form = reply.form ?? null;
    state.formText = lighting;
    syncControls();
    showProblems(reply.diagnostics ?? []);
    if (!reply.form) {
      status("The lighting text does not read: see the problem below.", true);
    } else if (presses.length) {
      status(`Pressing ${presses[0].key}.`);
    } else if (state.saved?.text === lighting) {
      status(`Your design, as saved to ${state.saved.file}.`);
    } else {
      status("Your design, not saved yet.");
    }
    play();
  } catch (error) {
    if (asked === state.asked) status(error.message, true);
  }
}

/// Play the preview on a timer at its own rate, each frame chosen by the
/// time since it started, so a late tick skips ahead rather than slowing it.
function play() {
  clearTimeout(state.animation);
  const step = () => {
    if (state.mode !== "design" || !state.clip || !state.clip.frames.length) return;
    const frames = state.clip.frames;
    const since = (performance.now() - state.started) / 1000;
    paint(frames[Math.floor(since * state.clip.fps) % frames.length]);
    state.animation = setTimeout(step, 1000 / state.clip.fps);
  };
  step();
}

/// A key clicked on the drawing: in a design, see how a press is answered.
function pressed(name) {
  if (state.mode === "design") preview([{ key: name, at: PRESS_AT }]);
}

// Writing a design back as TOML.

const quoted = (text) => JSON.stringify(text);
const decimal = (x) => (Number.isInteger(x) ? `${x}.0` : `${+x.toFixed(3)}`);
const tomlKey = (name) => (/^[A-Za-z0-9_-]+$/.test(name) ? name : quoted(name));

function toml(form) {
  const lines = [];
  if (form.effect) lines.push(`effect = ${quoted(form.effect)}`);
  if (form.colors) lines.push(`colors = [${form.colors.map(quoted).join(", ")}]`);
  if (form.speed != null) lines.push(`speed = ${decimal(form.speed)}`);
  if (form.brightness != null) lines.push(`brightness = ${Math.round(form.brightness)}`);
  if (form.bar) lines.push(`bar = ${quoted(form.bar)}`);
  const keys = Object.entries(form.keys ?? {});
  if (keys.length) {
    lines.push("", "[keys]");
    for (const [name, colour] of keys) lines.push(`${tomlKey(name)} = ${quoted(colour)}`);
  }
  if (form.reactive) {
    lines.push("", "[reactive]");
    const r = form.reactive;
    if (r.effect) lines.push(`effect = ${quoted(r.effect)}`);
    if (r.color) lines.push(`color = ${quoted(r.color)}`);
    if (r.fade_ms != null) lines.push(`fade_ms = ${Math.round(r.fade_ms)}`);
  }
  return `${lines.join("\n")}\n`;
}

/// Change the design: `change` edits a copy of what the daemon last read,
/// which is written back to the text box and drawn again.
async function edit(change) {
  const text = el("kb-lighting").value;
  if (text !== state.formText) {
    // Typed since it was last read: read it now, so the change lands on
    // what is written rather than on what was.
    let read;
    try {
      read = await call({ cmd: "preview_lighting", lighting: text, seconds: 0, fps: 1 });
    } catch (error) {
      status(error.message, true);
      return;
    }
    // Typed again, or changed by another drop, while it was read.
    if (el("kb-lighting").value !== text) {
      edit(change);
      return;
    }
    state.form = read.form ?? null;
    state.formText = text;
    if (!read.form) showProblems(read.diagnostics ?? []);
  }
  if (!state.form) {
    status("The lighting text does not read, so it was left as it is: fix it, then try again.", true);
    return;
  }
  const next = structuredClone(state.form);
  next.keys ??= {};
  change(next);
  const written = toml(next);
  el("kb-lighting").value = written;
  state.form = next;
  state.formText = written;
  syncControls();
  clearTimeout(state.typing);
  state.typing = setTimeout(() => preview(), 150);
}

function dropped(item, target, shift) {
  if (item.kind === "effect") {
    edit((form) => {
      form.effect = item.effect;
      if (item.colors) form.colors = item.colors;
      else delete form.colors;
      if (item.speed != null) form.speed = item.speed;
      if (item.brightness != null) form.brightness = item.brightness;
    });
  } else if (item.kind === "reaction") {
    edit((form) => {
      if (item.reactive) form.reactive = { ...item.reactive };
      else delete form.reactive;
    });
  } else if (item.kind === "colour") {
    edit((form) => {
      if (target.kind === "key" && target.led.group === "bar") {
        form.bar = item.colour;
      } else if (target.kind === "key") {
        form.keys[shift ? target.led.group : target.led.name] = item.colour;
      } else if (target.kind === "group" && target.group === "bar") {
        form.bar = item.colour;
      } else if (target.kind === "group") {
        form.keys[target.group] = item.colour;
      } else {
        // Dropped on the keyboard itself: the whole keyboard, that colour.
        form.effect = "static";
        form.colors = [item.colour];
      }
    });
  }
}

/// Show the design's pace and brightness on the sliders, with the daemon's
/// defaults for what it leaves unsaid.
function syncControls() {
  if (!state.form) return;
  const speed = state.form.speed ?? 0.2;
  const brightness = state.form.brightness ?? 60;
  el("kb-speed").value = `${fromSpeed(Math.min(Math.max(speed, SPEED_MIN), SPEED_MAX))}`;
  el("kb-speed-shown").textContent = `${speed} a second`;
  el("kb-brightness").value = `${brightness}`;
  el("kb-brightness-shown").textContent = `${brightness}%`;
}

// Saving.

const value = {
  str: (v) => ({ type: "string", value: v }),
  int: (v) => ({ type: "integer", value: Math.round(v) }),
  float: (v) => ({ type: "float", value: v }),
};

/// The edits that make a file's `[lighting]` the design: all of it
/// replaced, so nothing of an older design lingers.
function patches(form) {
  const set = (path, v) => ({ op: "set", path: `lighting.${path}`, value: v });
  const out = [{ op: "remove", path: "lighting" }];
  out.push(set("effect", value.str(form.effect ?? "static")));
  if (form.colors) out.push(set("colors", { type: "array", value: form.colors.map(value.str) }));
  if (form.speed != null) out.push(set("speed", value.float(form.speed)));
  if (form.brightness != null) out.push(set("brightness", value.int(form.brightness)));
  if (form.bar) out.push(set("bar", value.str(form.bar)));
  for (const [name, colour] of Object.entries(form.keys ?? {})) {
    // A file is patched by dotted path, which a dot or a bracket in a
    // name would break apart. No key or group has either.
    if (/[.[\]]/.test(name)) throw new Error(`key names have no dots or brackets in them: ${name}`);
    out.push(set(`keys.${name}`, value.str(colour)));
  }
  const r = form.reactive;
  if (r) {
    out.push(set("reactive.effect", value.str(r.effect ?? "ripple")));
    if (r.color) out.push(set("reactive.color", value.str(r.color)));
    if (r.fade_ms != null) out.push(set("reactive.fade_ms", value.int(r.fade_ms)));
  }
  return out;
}

/// Offer this profile, and the theme it uses, as places to save to.
async function fillTargets() {
  const select = el("kb-target");
  try {
    const [{ profile }, { themes }] = await Promise.all([
      call({ cmd: "status" }),
      call({ cmd: "get_themes" }),
    ]);
    const options = [];
    if (profile) options.push([`profiles/${profile}.toml`, `this profile (${profile})`]);
    const theme = themes.find((t) => (t.profiles ?? []).includes(profile));
    if (theme) options.push([theme.file, `its theme (${theme.name || theme.id}), for every profile using it`]);
    select.replaceChildren(
      ...options.map(([file, label]) => {
        const option = document.createElement("option");
        option.value = file;
        option.textContent = label;
        return option;
      }),
    );
  } catch (error) {
    status(error.message, true);
  }
}

/// What a file's check says that bears on a lighting save: its errors,
/// which stop any save, and whatever it says of the lighting. The rest --
/// hints about a page's keys, say -- belongs to the other tabs.
const bearing = (diagnostics) =>
  (diagnostics ?? []).filter((d) => d.severity === "error" || (d.path ?? "").includes("lighting"));

async function save() {
  const file = el("kb-target").value;
  if (!file) {
    status("There is nowhere to save to: the daemon named no profile.", true);
    return;
  }
  const button = el("kb-save");
  button.disabled = true;
  status("Saving...");
  try {
    // Read from the text as it is now rather than as it was last drawn,
    // which may be a keystroke behind.
    const text = el("kb-lighting").value;
    const read = await call({ cmd: "preview_lighting", lighting: text, seconds: 0, fps: 1 });
    if (!read.form) {
      showProblems(read.diagnostics ?? []);
      status("Not saved: the lighting text does not read. Fix it and save again.", true);
      return;
    }
    const edits = patches(read.form);
    // Checked first, as every other save here is: errors are shown and
    // nothing is written; warnings are shown and it is saved anyway.
    const check = await call({ cmd: "validate_config", file, patches: edits });
    showProblems(bearing(check.diagnostics));
    if ((check.diagnostics ?? []).some((d) => d.severity === "error")) {
      status("Not saved: the problems below would stop it loading.", true);
      return;
    }
    const reply = await call({ cmd: "apply_config", file, patches: edits });
    if (reply.result === "diagnostics") {
      showProblems(bearing(reply.diagnostics));
      status("Refused, so nothing was saved: see the problems below.", true);
      return;
    }
    state.saved = { text, file };
    // A save reloads everything, which can take a moment: by the time it
    // is done the live view may be showing, and says what it shows itself.
    if (state.mode === "design") {
      status(`Saved to ${file}. Switch to Live to see the keyboard showing it.`);
    }
  } catch (error) {
    status(error.message, true);
  } finally {
    button.disabled = false;
  }
}

function setMode(mode) {
  state.mode = mode;
  el("kb-mode-live").classList.toggle("active", mode === "live");
  el("kb-mode-design").classList.toggle("active", mode === "design");
  el("kb-design").hidden = mode !== "design";
  clearTimeout(state.live);
  clearTimeout(state.animation);
  if (mode === "live") {
    liveTick();
  } else {
    if (!state.tilesBuilt) buildTiles();
    if (!el("kb-lighting").value.trim()) el("kb-lighting").value = EXAMPLE;
    fillTargets();
    preview();
  }
}

/// Start when the tab is opened, and stop asking when it is left. Its class
/// can be written without it changing, which is neither.
async function opened() {
  if (showing() === state.open) return;
  state.open = showing();
  if (!state.open) {
    clearTimeout(state.live);
    clearTimeout(state.animation);
    return;
  }
  try {
    if (!state.drawn) await drawBoard();
    setMode(state.mode);
  } catch (error) {
    status(error.message, true);
  }
}

el("kb-mode-live").addEventListener("click", () => setMode("live"));
el("kb-mode-design").addEventListener("click", () => setMode("design"));
el("kb-lighting").addEventListener("input", () => {
  clearTimeout(state.typing);
  state.typing = setTimeout(() => preview(), 400);
});
el("kb-speed").addEventListener("input", (event) => {
  edit((form) => {
    form.speed = toSpeed(Number(event.target.value));
  });
});
el("kb-brightness").addEventListener("input", (event) => {
  edit((form) => {
    form.brightness = Number(event.target.value);
  });
});
el("kb-clear-keys").addEventListener("click", () => {
  edit((form) => {
    form.keys = {};
    delete form.bar;
  });
});
el("kb-save").addEventListener("click", save);
new MutationObserver(opened).observe(el("tab-keyboard"), {
  attributes: true,
  attributeFilter: ["class"],
});
