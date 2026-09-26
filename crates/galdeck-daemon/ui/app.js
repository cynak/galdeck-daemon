// The galdeck configuration UI.
//
// Plain ES modules with no build step, because the daemon serves this from
// bytes compiled into the binary and CI has no node in it. The whole thing is
// a thin client: every decision about what is valid lives in the daemon, and
// this asks.

const KEY_COUNT = 12;
const ENCODER_COUNT = 2;

// The token arrives in the URL once, then lives in sessionStorage so it is not
// left sitting in the address bar or in browser history.
const token = (() => {
  const url = new URL(location.href);
  const fromUrl = url.searchParams.get("token");
  if (fromUrl) {
    sessionStorage.setItem("galdeck-token", fromUrl);
    url.searchParams.delete("token");
    history.replaceState(null, "", url);
    return fromUrl;
  }
  return sessionStorage.getItem("galdeck-token") ?? "";
})();

const el = (id) => document.getElementById(id);

/// Thrown when the daemon rejects our token, which is worth telling apart from
/// every other failure because the user can do something about it.
class StaleToken extends Error {}
const state = {
  status: null, layout: null, config: null, selected: null, calibration: null,
  /// What widget `source` fields can name on this machine, fetched when
  /// first needed.
  sources: null,
  /// The selection whose form has unsaved edits, if any. A refresh leaves
  /// that form alone: events arrive whenever anything changes, and one
  /// landing mid-edit would otherwise put back what was there before.
  draft: null,
};

async function call(request) {
  const response = await fetch("/api/call", {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${token}`,
    },
    body: JSON.stringify(request),
  });
  if (response.status === 401 || response.status === 403) {
    // Whatever we had is no good. Drop it, so a reload with a fresh link is
    // not fighting a stored value that will never work again.
    sessionStorage.removeItem("galdeck-token");
    throw new StaleToken(await response.text());
  }
  if (!response.ok) {
    throw new Error(`${response.status}: ${await response.text()}`);
  }
  const reply = await response.json();
  if (reply.result === "error") throw new Error(reply.message);
  return reply;
}

function toast(message, isError = false) {
  const node = el("toast");
  node.textContent = message;
  node.hidden = false;
  node.style.borderColor = isError ? "var(--danger)" : "var(--ok)";
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (node.hidden = true), 2600);
}

function showDiagnostics(diagnostics) {
  const node = el("diagnostics");
  const interesting = (diagnostics ?? []).filter((d) => d.severity !== "hint");
  if (interesting.length === 0) {
    node.hidden = true;
    return;
  }
  const items = interesting
    .map((d) => {
      const where = d.start ? `${d.start.line}:${d.start.col}` : d.path;
      const help = d.help ? ` — ${escapeHtml(d.help)}` : "";
      return `<li class="${d.severity}"><code>${escapeHtml(where)}</code> ${escapeHtml(d.message)}${help}</li>`;
    })
    .join("");
  node.innerHTML = `<strong>${interesting.length} problem(s)</strong><ul>${items}</ul>`;
  node.hidden = false;
}

function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

/// Give every `data-colour` element under `root` that colour as `--colour`,
/// for its stylesheet rule to draw with. Markup never carries a `style`
/// attribute: the page's Content-Security-Policy (`style-src 'self'`) drops
/// them, and a colour set through the CSSOM is never parsed as markup.
function paintColours(root) {
  for (const node of root.querySelectorAll("[data-colour]")) {
    node.style.setProperty("--colour", node.dataset.colour);
  }
}

// ---------------------------------------------------------------- rendering

function renderStatus() {
  const s = state.status;
  const chip = el("device");
  chip.className = `chip ${s.connected ? "connected" : "disconnected"}`;
  chip.textContent = s.connected
    ? `connected · ${s.firmware ?? "?"}`
    : "no device";

  fillSelect(el("profile"), s.profiles, s.profile);
  el("brightness").value = s.brightness;
  el("brightness-value").textContent = s.brightness;
}

function fillSelect(select, values, current) {
  const same =
    select.options.length === values.length &&
    [...select.options].every((option, i) => option.value === values[i]);
  if (!same) {
    select.innerHTML = values
      .map((v) => `<option value="${escapeHtml(v)}">${escapeHtml(v)}</option>`)
      .join("");
  }
  select.value = current;
}

/// Cache-bust the preview images, since the daemon sends no-store but the
/// browser still reuses an in-flight identical URL.
let previewVersion = 0;

function renderDeck() {
  const layout = state.layout;
  if (!layout) return;
  fillSelect(el("page"), layout.pages, layout.page);

  const grid = el("grid");
  grid.innerHTML = "";
  for (let index = 0; index < KEY_COUNT; index++) {
    const info = layout.keys.find((k) => k.key === index);
    const button = document.createElement("button");
    button.className = "key";
    if (!info) button.classList.add("empty");
    if (state.selected?.kind === "key" && state.selected.key === index) {
      button.classList.add("selected");
    }
    button.dataset.key = index;
    // Every key, bound or not: an unbound key still shows its slice of a
    // background. One with nothing at all has no preview, and hides.
    const behind = layout.background && layout.background.span !== "lcd";
    if (info || behind) {
      const img = document.createElement("img");
      img.alt = `key ${index}`;
      img.addEventListener("error", () => (img.hidden = true));
      img.addEventListener("load", () => (img.hidden = false));
      img.src = `/api/preview/key/${index}.jpg?v=${previewVersion}&token=${encodeURIComponent(token)}`;
      button.append(img);
    }
    button.insertAdjacentHTML("beforeend", `<span class="index">${index}</span>`);
    button.addEventListener("click", () => select({ kind: "key", key: index }));
    grid.append(button);
  }

  el("lcd").src = `/api/preview/lcd.jpg?v=${previewVersion}&token=${encodeURIComponent(token)}`;
  renderLcdTiles();
  renderEncoders();
  renderInspector();
}

async function renderEncoders() {
  const wrap = el("encoders");
  let rings = [];
  try {
    const response = await fetch(`/api/preview?token=${encodeURIComponent(token)}`);
    if (response.ok) rings = (await response.json()).rings ?? [];
  } catch { /* the preview is decoration; the editor still works without it */ }

  wrap.innerHTML = "";
  for (let index = 0; index < ENCODER_COUNT; index++) {
    const info = state.layout?.encoders.find((e) => e.encoder === index);
    const segments = rings[index] ?? ["#000", "#000", "#000", "#000"];
    const node = document.createElement("div");
    node.className = "encoder";
    node.dataset.encoder = index;
    if (state.selected?.kind === "encoder" && state.selected.encoder === index) {
      node.classList.add("selected");
    }
    // Which mode it is in, when it has modes: the ring's colour says it on
    // the deck, and this says it here.
    const modes = info?.modes?.length ?? 0;
    const mode = modes >= 2 && info.mode != null ? ` ${info.mode + 1}/${modes}` : "";
    node.innerHTML = `<div class="ring"></div>
      <small>${index === 0 ? "left" : "right"}${info?.turn_preset
        ? ` · ${escapeHtml(presetTitle(info.turn_preset))}${mode}`
        : info ? "" : " · unset"}</small>`;
    // Through the CSSOM rather than a style attribute, which the page's
    // Content-Security-Policy would drop, leaving the ring blank.
    const ring = node.querySelector(".ring");
    segments.forEach((colour, i) => ring.style.setProperty(`--s${i}`, colour));
    node.addEventListener("click", () => select({ kind: "encoder", encoder: index }));
    wrap.append(node);
  }
}

function select(what) {
  state.selected = what;
  state.draft = null;
  renderDeck();
}

// --------------------------------------------------------------- inspector

function renderInspector() {
  const node = el("inspector");
  const layout = state.layout;
  const selected = state.selected;
  if (!layout || !selected) {
    node.innerHTML = `<p class="empty">Select a key or an encoder.</p>`;
    return;
  }
  if (state.draft === JSON.stringify(selected)) return;

  if (selected.kind === "key") {
    const info = layout.keys.find((k) => k.key === selected.key);
    node.innerHTML = info ? keyForm(info) : emptyKeyForm(selected.key);
  } else if (selected.kind === "lcd") {
    node.innerHTML = screenForm(layout);
  } else if (selected.kind === "background") {
    node.innerHTML = backgroundForm(layout);
  } else if (selected.kind === "page") {
    node.innerHTML = pageForm();
  } else if (selected.kind === "outputs") {
    node.innerHTML = outputsForm(layout);
  } else if (selected.kind === "tile") {
    const info = layout.lcd.find((t) => t.index === selected.index);
    node.innerHTML = info ? tileForm(info) : screenForm(layout);
  } else {
    const info = layout.encoders.find((e) => e.encoder === selected.encoder);
    node.innerHTML = encoderForm(info, selected.encoder);
  }
  wireInspector();
}

/// What a key's tap is set to, for its picker.
function tapKind(info) {
  if (info.profile) return "profile";
  if (info.page) return "page";
  if (info.back) return "back";
  return info.tap?.kind ?? "none";
}

/// What the key's widget does on a tap when nothing else is bound, if it
/// does anything: the daemon's own rule, read from the widget.
function implicitTap(info) {
  if (info.tap?.kind === "implicit") return info.tap.label;
  if (info.widget?.kind === "media") return "play or pause";
  if (info.widget?.kind === "volume" && !info.widget.source) return "mute or unmute the output";
  if (TIMERS.has(info.widget?.kind)) return "start, pause or resume the timer";
  return null;
}

/// What holding a key does when nothing else is bound, if anything: a
/// timer's reset, only as the daemon reports it -- a timer whose tap is bound
/// to something else has no reset on its hold.
function implicitHold(info) {
  return info.hold?.kind === "implicit" ? info.hold.label : null;
}

function keyForm(info) {
  const timer = TIMERS.has(info.widget?.kind);
  const tap = implicitTap(info);
  const hold = implicitHold(info);
  const counting = info.timer ? ` · ${escapeHtml(info.timer.state)}` : "";
  return `
    <h2>Key ${info.key}</h2>
    ${info.widget
      ? `<div class="field"><label>Showing</label>
           <span class="hint"><code>${escapeHtml(info.widget.kind)}</code> widget →
           ${escapeHtml(info.text ?? "—")}${counting}</span></div>`
      : ""}
    <div class="field">
      <label for="f-label">Label</label>
      <input id="f-label" value="${escapeHtml(info.label ?? "")}">
      ${info.widget ? `<span class="hint">${timer
        ? "Written under the time, unless the widget has a caption of its own."
        : "Shown until the widget produces something."}</span>` : ""}
    </div>
    <div class="field">
      <label for="f-icon">Icon</label>
      <input id="f-icon" value="${escapeHtml(info.icon ?? "")}" placeholder="/path/to/icon.png">
    </div>
    ${actionPicker("tap", info.tap, "Does what", {
      key: true,
      kind: tapKind(info),
      page: info.page,
      profile: info.profile,
      implicit: tap && `the widget's own: ${tap}`,
      where: "key-tap",
      timer,
    })}
    <details class="more" ${(info.hold && info.hold.kind !== "implicit") || info.double ? "open" : ""}>
      <summary>Hold and double tap</summary>
      ${actionPicker("hold", info.hold, "When held", {
        implicit: hold && `the widget's own: ${hold}`,
        where: "key",
        timer,
      })}
      ${actionPicker("double", info.double, "On a double tap", { where: "key", timer })}
      <span class="hint">Binding either makes a tap wait for the key to come back up.</span>
    </details>
    ${keyNamesList()}
    ${targetLists()}
    <div class="field">
      <label for="f-bg">Background</label>
      <div class="row">
        <input id="f-bg" type="color" value="${escapeHtml(info.background)}">
        <button id="f-bg-clear" ${info.background_is_own ? "" : "disabled"}>use the theme's</button>
      </div>
      <span class="hint">${info.background_is_own
        ? "Set on this key."
        : "Inherited from the theme."}</span>
    </div>
    ${widgetFields(info.widget, "key")}
    ${animationFields(info.animation, false)}
    <div class="actions">
      <button id="f-save" class="primary">Save</button>
      <button id="f-remove" class="danger">Remove key</button>
    </div>`;
}

// ------------------------------------------------------------------ widgets

/// Every widget kind, grouped the way someone looking for one thinks of them.
const WIDGET_KINDS = [
  ["Time", [["clock", "the time"], ["date", "the date"], ["uptime", "time since boot"]]],
  ["System", [
    ["cpu", "CPU use"], ["memory", "memory use"], ["temperature", "a temperature"],
    ["gpu", "GPU use"], ["fan", "a fan's speed"], ["load", "load average"],
    ["network", "network throughput"], ["disk", "disk space"], ["battery", "battery charge"],
  ]],
  ["Elsewhere", [
    ["media", "what's playing"], ["volume", "the volume"], ["weather", "the weather"],
    ["command", "a command's output"],
  ]],
  ["Timers", [["timer", "a countdown"], ["stopwatch", "a stopwatch"]]],
];

/// Kinds that count, started and stopped by tapping their key. On the
/// screen there is no key to tap, so they are for keys only.
const TIMERS = new Set(["timer", "stopwatch"]);

/// Exactly which settings each kind has, beyond the ones every widget has.
///
/// The form shows these and nothing else, and saves these and nothing else:
/// a CPU widget has no location to ask about, and a stale one left in the
/// file would be a warning about something nobody can see.
const KIND_FIELDS = {
  clock: ["format", "timezone", "view"],
  date: ["format", "timezone"],
  uptime: [],
  battery: ["view", "source"],
  fan: ["view", "source"],
  load: ["view"],
  volume: ["view", "source"],
  cpu: ["view"],
  memory: ["view"],
  gpu: ["view", "source"],
  temperature: ["view", "source", "units"],
  network: ["view", "source"],
  disk: ["view", "source"],
  weather: ["location", "units"],
  media: ["source", "color"],
  command: ["command", "view"],
  timer: ["duration", "view", "on_done"],
  stopwatch: [],
};

/// What the daemon uses when `interval_ms` is left out. Mirrored here only
/// to show as a placeholder; leaving the field empty is what keeps it.
const DEFAULT_INTERVAL = {
  clock: 1000, date: 60000, cpu: 2000, memory: 2000, temperature: 2000, gpu: 2000,
  network: 2000, disk: 60000, weather: 900000, media: 1000, command: 5000,
  battery: 30000, fan: 2000, load: 5000, uptime: 60000, volume: 500,
};

/// The refresh a widget gets when `interval_ms` is left out. An animated
/// view refreshes at its frame rate rather than at its data's.
function defaultInterval(kind, view) {
  return view === "nixie" ? 100 : DEFAULT_INTERVAL[kind] ?? 1000;
}

/// The format a clock or a date gets when `format` is left out. Nixie tubes
/// count the seconds.
function defaultFormat(kind, view) {
  if (kind === "date") return "%a %d %b";
  return view === "nixie" ? "%H:%M:%S" : "%H:%M";
}

/// Kinds that measure something, and so can be a graph or a bar.
const NUMERIC = new Set([
  "cpu", "memory", "temperature", "gpu", "network", "disk", "command",
  "battery", "fan", "load", "volume",
]);

/// The ways each kind can be drawn. Anything that measures something has
/// the lot; a clock has a face, or tubes.
function viewsFor(kind) {
  if (NUMERIC.has(kind)) {
    return [["text", "text"], ["graph", "a graph of recent readings"], ["bar", "a bar"], ["gauge", "a dial"]];
  }
  if (kind === "clock") {
    return [["text", "text"], ["analog", "a clock face"], ["nixie", "nixie tubes"]];
  }
  // A countdown can show what is left of it; a stopwatch has no end to be
  // part of the way to.
  if (kind === "timer") return [["text", "text"], ["bar", "a bar"], ["gauge", "a dial"]];
  return [["text", "text"]];
}
/// Kinds that always draw as a card of their own.
const CARDS = new Set(["weather", "media"]);

const SOURCE_HINT = {
  temperature: "A chip or a sensor label. Empty means the CPU.",
  gpu: "A DRM card. Empty tries each card, then nvidia-smi.",
  network: "An interface. Empty adds up everything but loopback.",
  disk: "A mount point. Empty means /.",
  media: "A player. Empty follows whichever one is playing.",
  battery: "A power supply such as BAT0. Empty means the first battery.",
  fan: "A chip or a fan label. Empty means the first fan there is.",
  volume: "Empty means the default output; mic, the default input.",
};

/// Whether a widget setting applies to what is chosen.
///
/// One function decides both whether a field is shown and whether it is
/// saved, so a hidden field can never leave a stale setting behind.
function widgetFieldApplies(field, kind, view, ctx) {
  if (kind === "none") return false;
  const own = KIND_FIELDS[kind] ?? [];
  const graphic = own.includes("view") && view !== "text";
  switch (field) {
    // Every widget has these.
    case "has":
    case "background":
      return true;
    // A key's label already says what it is; a tile has no label, and a
    // graph always has room for a caption. A timer's time takes the whole
    // key, and its caption says which timer it is.
    case "title": return graphic || (ctx === "tile" && !CARDS.has(kind)) || TIMERS.has(kind);
    case "scale": return graphic && NUMERIC.has(kind);
    case "alarm": return NUMERIC.has(kind);
    case "color": return graphic || own.includes("color");
    // A timer moves on at each second of its own, not on a refresh.
    case "interval": return !TIMERS.has(kind);
    default: return own.includes(field);
  }
}

/// Whitespace as Rust's `trim` sees it. JavaScript's own also takes a
/// byte-order mark, which the daemon does not, and leaves U+0085, which it
/// takes: between a duration's units the two would disagree about a pasted
/// one.
const RUST_SPACE = "[\\t-\\r \\u0085\\u00a0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000]+";
const RUST_SPACE_START = new RegExp(`^${RUST_SPACE}`);
const RUST_SPACE_END = new RegExp(`${RUST_SPACE}$`);

/// A timer's `duration` in seconds, by the daemon's own rules (the model's
/// `parse_duration`): `90s`, `4m`, `1h30m`, `1h 30m`, `m:ss` or `h:mm:ss`,
/// and nothing else. `null` when it is not one. Mirrored here only to say
/// what a duration means while it is typed; saving still asks the daemon.
function parseDuration(text) {
  // As a u64 would hold it: anything bigger is no duration, not a wrap.
  const MAX = 2n ** 64n - 1n;
  const digits = (part) => {
    if (!/^[0-9]+$/.test(part)) return null;
    const n = BigInt(part);
    return n <= MAX ? n : null;
  };
  // Saving trims it the way this script does, and the daemon then the way
  // Rust does.
  text = text.trim().replace(RUST_SPACE_START, "").replace(RUST_SPACE_END, "");
  let seconds;
  if (text.includes(":")) {
    // After the first part, two digits below sixty, as a clock writes them.
    const sixtieth = (part) => {
      const n = digits(part);
      return n !== null && part.length === 2 && n < 60n ? n : null;
    };
    const parts = text.split(":");
    const numbers = parts.length === 2 ? [digits(parts[0]), 0n, sixtieth(parts[1])]
      : parts.length === 3 ? [digits(parts[0]), sixtieth(parts[1]), sixtieth(parts[2])]
      : null;
    if (!numbers || numbers.includes(null)) return null;
    const [first, minutes, second] = numbers;
    seconds = first * (parts.length === 2 ? 60n : 3600n) + minutes * 60n + second;
  } else {
    // Hours, minutes and seconds, each at most once and in that order,
    // with or without spaces between them.
    if (text === "") return null;
    const units = [["h", 3600n], ["m", 60n], ["s", 1n]];
    let next = 0;
    let rest = text;
    seconds = 0n;
    while (rest !== "") {
      const end = rest.search(/[^0-9]/);
      if (end < 0) return null;
      const number = digits(rest.slice(0, end));
      const unit = String.fromCodePoint(rest.codePointAt(end));
      const at = units.findIndex(([name], i) => i >= next && name === unit);
      if (number === null || at < 0) return null;
      next = at + 1;
      seconds += number * units[at][1];
      if (seconds > MAX) return null;
      rest = rest.slice(end + unit.length).replace(RUST_SPACE_START, "");
    }
  }
  return seconds <= MAX ? Number(seconds) : null;
}

/// What a typed duration means, in words, with the daemon's warnings about
/// one that is too long or too short to be what was meant.
function describeDuration(text) {
  if (text.trim() === "") return { words: "", warn: "a timer needs a duration" };
  const seconds = parseDuration(text);
  if (seconds === null) return { words: "", warn: "not a duration: try 25m, 1h 30m, 90s or 4:30" };
  const parts = [[Math.floor(seconds / 3600), "h"], [Math.floor(seconds / 60) % 60, "min"], [seconds % 60, "s"]]
    .filter(([n]) => n > 0).map(([n, unit]) => `${n} ${unit}`);
  const words = `= ${parts.join(" ") || "0 s"}`;
  if (seconds > 24 * 3600) return { words, warn: "longer than a day" };
  if (seconds < 1) return { words, warn: "done as soon as it starts" };
  return { words, warn: "" };
}

/// The id in an `id="..."` attribute. The helpers below take the attribute
/// whole, so every id the script looks up is written out literally where it
/// is built -- which is what lets a test catch a lookup with a typo in it.
const idOf = (attribute) => attribute.match(/id="([^"]+)"/)[1];

/// A colour setting: a picker, and a text field that can also hold a theme
/// token the picker cannot show. The text is what is saved.
function colourField(idAttribute, label, value, hex, placeholder, hint) {
  const id = idOf(idAttribute);
  return `
    <div class="field">
      <label for="${id}">${label}</label>
      <div class="row">
        <input type="color" data-picks="${id}" value="${escapeHtml(hex ?? "#88c0d0")}">
        <input ${idAttribute} value="${escapeHtml(value ?? "")}" placeholder="${placeholder}">
      </div>
      ${hint ? `<span class="hint">${hint}</span>` : ""}
    </div>`;
}

/// A picture setting: a path, and a button that uploads one into the config
/// directory and fills the path in.
function pictureField(idAttribute, label, value, hint) {
  const id = idOf(idAttribute);
  return `
    <div class="field">
      <label for="${id}">${label}</label>
      <div class="row">
        <input ${idAttribute} value="${escapeHtml(value ?? "")}" placeholder="none">
        <label class="upload" title="Upload a picture">Upload…
          <input type="file" accept="image/png,image/jpeg,image/gif" data-uploads="${id}" hidden>
        </label>
      </div>
      ${hint ? `<span class="hint">${hint}</span>` : ""}
    </div>`;
}

/// The widget section. On a key, `kind = none` is how a widget is removed,
/// so there is one control rather than a checkbox and a dropdown that can
/// disagree. A tile is nothing but its widget, so it has no `none`.
function widgetFields(widget, ctx) {
  const kind = widget?.kind ?? (ctx === "tile" ? "clock" : "none");
  const view = widget?.view || "text";
  const shown = (field) => (widgetFieldApplies(field, kind, view, ctx) ? "" : "hidden");
  // A timer on the screen could never be started, so the screen is not
  // offered one; a tile that already is one keeps it, to be moved to a key.
  const offered = (value) => ctx !== "tile" || !TIMERS.has(value) || value === kind;
  const groups = WIDGET_KINDS.map(([group, kinds]) => [group, kinds.filter(([value]) => offered(value))])
    .filter(([, kinds]) => kinds.length > 0)
    .map(([group, kinds]) =>
    `<optgroup label="${group}">${kinds
      .map(([value, label]) =>
        `<option value="${value}" ${kind === value ? "selected" : ""}>${label}</option>`)
      .join("")}</optgroup>`).join("");
  const duration = describeDuration(widget?.duration ?? "");
  const interval =
    widget && widget.interval_ms !== defaultInterval(kind, view) ? widget.interval_ms : "";
  const viewOptions = viewsFor(kind)
    .map(([value, label]) => `<option value="${value}" ${view === value ? "selected" : ""}>${label}</option>`)
    .join("");
  const units = widget?.units ?? "";
  const opacity = Math.round((widget?.opacity ?? 1) * 100);
  return `
    <fieldset data-ctx="${ctx}">
      <legend>Widget</legend>
      <div class="field">
        <label for="f-widget">Shows</label>
        <select id="f-widget">
          ${ctx === "tile" ? "" : `<option value="none" ${kind === "none" ? "selected" : ""}>nothing — just the label</option>`}
          ${groups}
        </select>
      </div>
      <div class="field" data-wfield="view" ${shown("view")}>
        <label for="f-widget-view">Drawn as</label>
        <select id="f-widget-view">${viewOptions}</select>
      </div>
      <div class="field" data-wfield="duration" ${shown("duration")}>
        <label for="f-widget-duration">Counts down from
          <output id="f-widget-duration-value">${escapeHtml(duration.words)}</output></label>
        <input id="f-widget-duration" value="${escapeHtml(widget?.duration ?? "")}" placeholder="25m">
        <span class="hint warn" id="f-widget-duration-warn">${escapeHtml(duration.warn)}</span>
        <span class="hint"><code>25m</code>, <code>1h 30m</code>, <code>90s</code>, or <code>4:30</code>
          for minutes and seconds and <code>1:30:00</code> with hours. Tap to start or pause, hold to
          reset, while nothing else is bound to the key's tap.</span>
      </div>
      <div class="field" data-wfield="title" ${shown("title")}>
        <label for="f-widget-title">Caption</label>
        <input id="f-widget-title" value="${escapeHtml(widget?.title ?? "")}"
               placeholder="${TIMERS.has(kind) ? "the key's label" : "e.g. CPU"}">
      </div>
      <div class="field" data-wfield="format" ${shown("format")}>
        <label for="f-widget-format">Format</label>
        <input id="f-widget-format" value="${escapeHtml(widget?.format ?? "")}"
               placeholder="${defaultFormat(kind, view)}">
        <span class="hint">strftime, so <code>%H:%M</code>, <code>%I:%M %p</code> or <code>%a %d %b</code>.</span>
      </div>
      <div class="field" data-wfield="timezone" ${shown("timezone")}>
        <label for="f-widget-timezone">Time zone</label>
        <input id="f-widget-timezone" list="f-widget-timezones" value="${escapeHtml(widget?.timezone ?? "")}"
               placeholder="this machine's">
        <datalist id="f-widget-timezones"></datalist>
        <span class="hint">For a world clock: <code>Asia/Tokyo</code>, <code>America/New_York</code>.</span>
      </div>
      <div class="field" data-wfield="command" ${shown("command")}>
        <label for="f-widget-command">Command</label>
        <input id="f-widget-command" value="${escapeHtml(widget?.command ?? "")}">
        <span class="hint">First line of output, killed after five seconds. A leading number can be graphed.</span>
      </div>
      <div class="field" data-wfield="source" ${shown("source")}>
        <label for="f-widget-source">Source</label>
        <div class="row">
          <input id="f-widget-source" list="f-widget-sources" value="${escapeHtml(widget?.source ?? "")}"
                 placeholder="automatic">
          <button id="f-widget-sources-refresh" type="button" title="Look again">↻</button>
        </div>
        <datalist id="f-widget-sources"></datalist>
        <span class="hint" id="f-widget-source-hint">${SOURCE_HINT[kind] ?? ""}</span>
      </div>
      <div class="field" data-wfield="location" ${shown("location")}>
        <label for="f-widget-latitude">Location</label>
        <div class="row">
          <input id="f-widget-latitude" type="number" step="0.01" min="-90" max="90"
                 placeholder="latitude" value="${widget?.latitude ?? ""}">
          <input id="f-widget-longitude" type="number" step="0.01" min="-180" max="180"
                 placeholder="longitude" value="${widget?.longitude ?? ""}">
        </div>
        <div class="row">
          <input id="f-widget-place" value="${escapeHtml(widget?.place ?? "")}" placeholder="search for a place">
          <button id="f-widget-search" type="button">Search</button>
        </div>
        <ul id="f-widget-places" class="place-list"></ul>
        <button id="f-widget-locate" type="button">Use this browser's location</button>
        <span class="hint">Saved in your config and sent only to Open-Meteo, which needs no account.</span>
      </div>
      <div class="field" data-wfield="units" ${shown("units")}>
        <label for="f-widget-units">Units</label>
        <select id="f-widget-units">
          <option value="" ${units === "" ? "selected" : ""}>°C</option>
          <option value="fahrenheit" ${units === "fahrenheit" ? "selected" : ""}>°F</option>
        </select>
      </div>
      <div ${shown("color")} data-wfield="color">
        ${colourField('id="f-widget-color"', "Colour", widget?.color, widget?.color_hex,
          "the theme's text colour", "The line, bar or progress. A hex colour or a theme token such as <code>@accent</code>.")}
      </div>
      <div class="subgroup" data-wfield="background" ${shown("background")}>
        <h3>Background</h3>
        ${colourField('id="f-widget-bg"', "Colour", widget?.background, widget?.background_hex,
          ctx === "tile" ? "the screen's card" : "the key's colour", "")}
        ${pictureField('id="f-widget-image"', "Picture", widget?.image,
          "Scaled to cover the widget. Drawn under its text.")}
        <div class="field">
          <label for="f-widget-opacity">Opacity <output id="f-widget-opacity-value">${opacity}%</output></label>
          <input id="f-widget-opacity" type="range" min="0" max="100" step="5" value="${opacity}"
                 data-touched="${widget?.opacity == null ? "" : "yes"}">
          <span class="hint">At 0 the page's background shows straight through.</span>
        </div>
      </div>
      <div class="field" data-wfield="alarm" ${shown("alarm")}>
        <label for="f-widget-warn">Warn at / critical at</label>
        <div class="row">
          <input id="f-widget-warn" type="number" step="any" value="${widget?.warn ?? ""}" placeholder="amber">
          <input id="f-widget-critical" type="number" step="any" value="${widget?.critical ?? ""}" placeholder="red">
        </div>
        <span class="hint">In the widget's own units. Warn above critical means lower is worse, as for a battery.</span>
      </div>
      <details class="more" data-wfield="on_done" ${shown("on_done")} ${widget?.on_done ? "open" : ""}>
        <summary>When it finishes</summary>
        ${actionPicker("on_done", widget?.on_done, "Also", { where: "other" })}
        <div class="field">
          <span class="hint">Once, as it runs out, whichever page is showing: play a sound with
            <code>pw-play</code>, say. The key flashes and the screen says so either way.</span>
        </div>
      </details>
      <details class="more" data-wfield="has" ${shown("has")}>
        <summary>More</summary>
        <div class="field" data-wfield="interval" ${shown("interval")}>
          <label for="f-widget-interval">Refresh every (ms)</label>
          <input id="f-widget-interval" type="number" min="100" step="100" value="${interval}"
                 placeholder="${defaultInterval(kind, view)}">
        </div>
        <div class="field" data-wfield="scale" ${shown("scale")}>
          <label for="f-widget-max">Full at</label>
          <div class="row">
            <input id="f-widget-max" type="number" min="0" step="any" value="${widget?.max ?? ""}"
                   placeholder="automatic">
            <input id="f-widget-history" type="number" min="2" max="600" step="1"
                   value="${widget?.history ?? ""}" placeholder="60 readings">
          </div>
          <span class="hint">The top of the scale, and how many readings a graph keeps.
            Percentages top out at 100 unless told otherwise.</span>
        </div>
        <div class="field">
          <label for="f-widget-placeholder">Until it has something</label>
          <input id="f-widget-placeholder" value="${escapeHtml(widget?.placeholder ?? "")}"
                 placeholder="${ctx === "tile" ? "blank" : "the label"}">
        </div>
      </details>
    </fieldset>`;
}

/// Show the widget fields that apply to what is chosen now.
function updateWidgetFields() {
  const kindSelect = el("f-widget");
  if (!kindSelect) return;
  const ctx = kindSelect.closest("fieldset").dataset.ctx;
  const kind = kindSelect.value;
  // The views on offer follow the kind; one that no longer applies falls
  // back to text rather than being saved for something that cannot draw it.
  const viewSelect = el("f-widget-view");
  const views = viewsFor(kind);
  const keep = views.some(([value]) => value === viewSelect.value) ? viewSelect.value : "text";
  viewSelect.innerHTML = views
    .map(([value, label]) => `<option value="${value}" ${value === keep ? "selected" : ""}>${label}</option>`)
    .join("");
  const view = viewSelect.value;
  for (const field of document.querySelectorAll("[data-wfield]")) {
    field.hidden = !widgetFieldApplies(field.dataset.wfield, kind, view, ctx);
  }
  el("f-widget-interval").placeholder = defaultInterval(kind, view);
  el("f-widget-format").placeholder = defaultFormat(kind, view);
  el("f-widget-title").placeholder = TIMERS.has(kind) ? "the key's label" : "e.g. CPU";
  el("f-widget-source-hint").textContent = SOURCE_HINT[kind] ?? "";
  if (kind in SOURCE_HINT) fillSources(kind);
  showDuration();
}

/// Say what the typed duration means beside it.
function showDuration() {
  const input = el("f-widget-duration");
  if (!input) return;
  const { words, warn } = describeDuration(input.value);
  el("f-widget-duration-value").textContent = words;
  el("f-widget-duration-warn").textContent = warn;
}

/// Offer what `source` could name, fetched from the daemon the first time.
async function fillSources(kind, again = false) {
  if (!state.sources || again) {
    state.sources = await call({ cmd: "widget_sources" }).catch(() => null);
  }
  const list = el("f-widget-sources");
  if (!list || el("f-widget")?.value !== kind) return;
  list.innerHTML = (state.sources?.[kind] ?? [])
    .map((o) => `<option value="${escapeHtml(o.value)}" label="${escapeHtml(o.detail ?? "")}"></option>`)
    .join("");
  const found = state.sources?.[kind]?.length ?? 0;
  el("f-widget-source-hint").textContent =
    `${SOURCE_HINT[kind]} ${found === 0 ? "Nothing found to suggest." : `${found} found.`}`;
}

/// Store a picture the browser picked in the daemon's config directory, and
/// say where it went.
async function uploadPicture(file) {
  // The daemon reads at most 4 MB of request, and base64 is a third bigger.
  if (file.size > 3 * 1024 * 1024) throw new Error("pictures sent from here are limited to 3 MB");
  const data = await new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
  return (await call({ cmd: "save_asset", name: file.name, data })).path;
}

/// Colour pickers that write into their text field, uploads that write
/// into their path field. Anywhere in the inspector.
function wirePickers() {
  for (const pick of document.querySelectorAll("[data-picks]")) {
    const text = document.getElementById(pick.dataset.picks);
    pick.addEventListener("input", () => (text.value = pick.value));
    text.addEventListener("input", () => {
      if (/^#[0-9a-f]{6}$/i.test(text.value)) pick.value = text.value;
    });
  }
  for (const input of document.querySelectorAll("[data-uploads]")) {
    input.addEventListener("change", async () => {
      const file = input.files?.[0];
      if (!file) return;
      try {
        const path = await uploadPicture(file);
        const target = document.getElementById(input.dataset.uploads);
        target.value = path;
        // Counts as an edit, so a refresh does not undo it.
        target.dispatchEvent(new Event("input", { bubbles: true }));
        toast(`saved ${file.name}`);
      } catch (e) {
        toast(e.message, true);
      }
    });
  }
}

function wireWidget() {
  const kind = el("f-widget");
  if (!kind) return;
  kind.addEventListener("change", updateWidgetFields);
  el("f-widget-view").addEventListener("change", updateWidgetFields);
  el("f-widget-duration").addEventListener("input", showDuration);
  if (kind.value in SOURCE_HINT) fillSources(kind.value);
  // Every zone the browser knows, which is the same tz database the daemon
  // reads.
  const zones = typeof Intl.supportedValuesOf === "function" ? Intl.supportedValuesOf("timeZone") : [];
  el("f-widget-timezones").innerHTML = zones.map((z) => `<option value="${escapeHtml(z)}">`).join("");
  el("f-widget-sources-refresh").addEventListener("click", () => fillSources(kind.value, true));

  const opacity = el("f-widget-opacity");
  opacity.addEventListener("input", () => {
    opacity.dataset.touched = "yes";
    el("f-widget-opacity-value").textContent = `${opacity.value}%`;
  });

  // Look a place up by name. The results are other people's text, so they
  // are built as elements, never as markup.
  el("f-widget-search").addEventListener("click", async () => {
    const list = el("f-widget-places");
    list.replaceChildren();
    try {
      const reply = await call({ cmd: "geocode", name: el("f-widget-place").value });
      if (reply.places.length === 0) toast("no place by that name", true);
      for (const place of reply.places) {
        const item = document.createElement("li");
        const button = document.createElement("button");
        button.type = "button";
        button.textContent = place.label;
        button.addEventListener("click", () => {
          el("f-widget-place").value = place.name;
          el("f-widget-latitude").value = place.latitude.toFixed(2);
          el("f-widget-longitude").value = place.longitude.toFixed(2);
          el("f-widget-latitude").dispatchEvent(new Event("input", { bubbles: true }));
          list.replaceChildren();
        });
        item.append(button);
        list.append(item);
      }
    } catch (e) {
      toast(e.message, true);
    }
  });

  el("f-widget-locate").addEventListener("click", async () => {
    const coords = await locate();
    if (!coords) return;
    el("f-widget-latitude").value = coords.latitude;
    el("f-widget-longitude").value = coords.longitude;
    el("f-widget-latitude").dispatchEvent(new Event("input", { bubbles: true }));
  });
}

/// Where the browser says it is, to two places: about a kilometre, plenty
/// for a forecast and no more precise than it needs to be in a file people
/// share. `null`, having said why, when it will not say.
async function locate() {
  if (!navigator.geolocation) {
    toast("this browser cannot share a location", true);
    return null;
  }
  return new Promise((resolve) =>
    navigator.geolocation.getCurrentPosition(
      ({ coords }) => resolve({
        latitude: Number(coords.latitude.toFixed(2)),
        longitude: Number(coords.longitude.toFixed(2)),
      }),
      (e) => {
        toast(`no location: ${e.message}`, true);
        resolve(null);
      },
      { maximumAge: 3600000, timeout: 15000 }));
}

/// The animation section. Rings get two kinds keys do not.
function animationFields(animation, isRing, hint = "") {
  const kind = animation?.kind ?? "none";
  const option = (value, label) =>
    `<option value="${value}" ${kind === value ? "selected" : ""}>${label}</option>`;
  return `
    <fieldset>
      <legend>Animation</legend>
      <div class="field">
        <label for="f-anim">Moves</label>
        <select id="f-anim">
          ${option("none", "not at all")}
          ${option("pulse", "pulse")}
          ${option("breathe", "breathe")}
          ${option("blink", "blink")}
          ${option("heartbeat", "heartbeat")}
          ${option("rainbow", "rainbow")}
          ${isRing ? option("spin", "spin") : ""}
          ${isRing ? option("comet", "comet") : ""}
        </select>
      </div>
      <div class="field" data-anim="has" ${kind === "none" ? "hidden" : ""}>
        <label for="f-anim-period">Cycle (ms)</label>
        <input id="f-anim-period" type="number" min="120" max="60000" step="100"
               value="${animation?.period_ms ?? 2000}">
      </div>
      <div class="field" data-anim="has" ${kind === "none" ? "hidden" : ""}>
        <label for="f-anim-to">Towards</label>
        <input id="f-anim-to" type="color" value="${escapeHtml(animation?.to ?? "#ffffff")}">
        <span class="hint">Frames are rendered once when the page loads.</span>
      </div>
      ${hint ? `<p class="hint">${hint}</p>` : ""}
    </fieldset>`;
}

function emptyKeyForm(key) {
  return `
    <h2>Key ${key}</h2>
    <p class="empty">Nothing is bound here.</p>
    <div class="actions"><button id="f-add">Add this key</button></div>`;
}

function wireInspector() {
  wireActionPickers();
  wirePageForm();
  el("f-ring")?.addEventListener("input", () => {
    const own = el("f-ring-own");
    if (own) own.checked = true;
  });
  const layer = el("f-knob-layer");
  layer?.addEventListener("change", () => {
    // Show what is set at the chosen layer: a different entry entirely.
    state.knobLayer = { ...(state.knobLayer ?? {}), [state.selected.encoder]: layer.value };
    state.draft = null;
    renderInspector();
  });
  for (const card of document.querySelectorAll(".preset-card input")) {
    card.addEventListener("change", () => {
      for (const other of document.querySelectorAll(".preset-card")) {
        other.classList.toggle("chosen", other.contains(card) && card.checked);
      }
    });
  }
  wireWidget();

  const anim = el("f-anim");
  anim?.addEventListener("change", () => {
    for (const field of document.querySelectorAll("[data-anim]")) {
      field.hidden = anim.value === "none";
    }
  });

  el("f-save")?.addEventListener("click", save);
  el("f-add")?.addEventListener("click", add);
  el("f-remove")?.addEventListener("click", remove);
  el("f-bg-clear")?.addEventListener("click", clearBackground);
  el("f-lcd-save")?.addEventListener("click", saveScreenText);
  for (const button of document.querySelectorAll("[data-tile]")) {
    button.addEventListener("click", () =>
      select({ kind: "tile", index: Number(button.dataset.tile) }));
  }
  wirePickers();
  wireBackground();
  wireGrid();
  wireKnobForm();
  wireOutputsForm();
  fillTargets().catch(() => {});
}

// ------------------------------------------------------------------ actions

/// Words for what a browser key code means, in the daemon's key names. The
/// names are positions, as `code` is, so capturing a key press here is right
/// on every keyboard layout.
const CODE_NAMES = {
  ArrowUp: "up", ArrowDown: "down", ArrowLeft: "left", ArrowRight: "right",
  PageUp: "page_up", PageDown: "page_down", Backspace: "backspace", Enter: "enter",
  Escape: "escape", Tab: "tab", Space: "space", Delete: "delete", Insert: "insert",
  Home: "home", End: "end", Minus: "minus", Equal: "equal", Comma: "comma",
  Period: "period", Slash: "slash", Semicolon: "semicolon", Quote: "apostrophe",
  Backquote: "grave", BracketLeft: "bracket_left", BracketRight: "bracket_right",
  Backslash: "backslash", IntlBackslash: "less", PrintScreen: "print", Pause: "pause",
  ScrollLock: "scroll_lock", CapsLock: "caps_lock", NumLock: "num_lock",
  ContextMenu: "menu", NumpadAdd: "kp_add", NumpadSubtract: "kp_subtract",
  NumpadMultiply: "kp_multiply", NumpadDivide: "kp_divide", NumpadDecimal: "kp_decimal",
  NumpadEnter: "kp_enter", NumpadEqual: "kp_equal",
};
const MODIFIER_CODES = new Set([
  "ControlLeft", "ControlRight", "ShiftLeft", "ShiftRight", "AltLeft", "AltRight",
  "MetaLeft", "MetaRight",
]);

/// A browser key press as a chord: `ctrl+shift+t`.
function chordOf(event) {
  const parts = [];
  if (event.ctrlKey) parts.push("ctrl");
  if (event.shiftKey) parts.push("shift");
  if (event.altKey) parts.push(event.code === "AltRight" ? "altgr" : "alt");
  if (event.metaKey) parts.push("super");
  if (!MODIFIER_CODES.has(event.code)) {
    const code = event.code;
    const name = CODE_NAMES[code]
      ?? (code.startsWith("Key") ? code.slice(3).toLowerCase() : null)
      ?? (code.startsWith("Digit") ? code.slice(5) : null)
      ?? (code.startsWith("Numpad") ? `kp_${code.slice(6)}` : null)
      ?? (/^F\d+$/.test(code) ? code.toLowerCase() : null);
    if (name) parts.push(name);
  }
  return parts.join("+");
}

/// Whether a built-in acts on its key's own timer. The model names them all
/// `timer_…`, and they mean nothing on a key without one.
const isTimerAction = (b) => b.name.startsWith("timer_");

/// Whether a built-in belongs in a picker for `where`: a key's tap
/// (`key-tap`), its hold or double tap (`key`), a knob (`knob`), or a
/// timer's `on_done` (`other`). `timer` says the key has a timer or a
/// stopwatch. The rules are the model's slot rules; the daemon warns about
/// anything set by hand in the wrong place, and what is set is always
/// offered, so opening a form never quietly changes it.
function builtInOffered(b, where, timer, selected) {
  if (b.name === selected) return true;
  if (b.knobs_only) return where === "knob";
  if (isTimerAction(b)) return timer && (where === "key-tap" || where === "key");
  // Push-to-talk: it answers the key coming back up, which only a tap sees.
  if (b.keys_only) return where === "key-tap";
  return true;
}

/// The built-ins as grouped options, from the daemon's catalog so the list
/// here cannot drift from what it understands.
function builtInOptions(selected, where, timer) {
  const groups = new Map();
  for (const b of state.catalog?.built_ins ?? []) {
    if (!builtInOffered(b, where, timer, selected)) continue;
    if (!groups.has(b.group)) groups.set(b.group, []);
    groups.get(b.group).push(b);
  }
  const needs = mixerProblem() ? " — needs PipeWire" : "";
  return [...groups].map(([group, items]) =>
    `<optgroup label="${escapeHtml(group)}">${items.map((b) =>
      `<option value="${escapeHtml(b.name)}" ${b.name === selected ? "selected" : ""}>${escapeHtml(b.label)}${b.needs_mixer ? needs : ""}</option>`)
      .join("")}</optgroup>`).join("");
}

function builtInInfo(name) {
  return (state.catalog?.built_ins ?? []).find((b) => b.name === name);
}

function presetInfo(name) {
  return (state.catalog?.presets ?? []).find((p) => p.name === name);
}

/// A preset's name for a person: "Output switcher", from the catalog.
function presetTitle(name) {
  return presetInfo(name)?.title || name.replaceAll("_", " ");
}

/// Whether the virtual keyboard and pointer can be used, and if not, why.
function virtualInputProblem() {
  const s = state.status?.capabilities?.virtual_input ?? "";
  if (s === "off") return "Keystrokes and scrolling are off: virtual_input = false in galdeck.toml.";
  if (s.startsWith("unavailable")) return `Keystrokes and scrolling cannot work here: ${s.slice(13)}.`;
  return null;
}

/// Why outputs cannot be switched nor an app's volume set here, if they
/// cannot. An older daemon says nothing, which is not a reason to warn.
function mixerProblem() {
  const s = state.status?.capabilities?.mixer ?? "";
  if (!s.startsWith("unavailable")) return null;
  return `Needs PipeWire: ${s.slice("unavailable: ".length)}.`;
}

/// What a `target` is called, by the kind of thing it names.
const TARGET_WORDS = { audio_node: "Device", player: "Player", app: "App", sink: "Output" };

/// What an empty `target` means, or what to type, by kind: short, to fit
/// a field's width.
const TARGET_PLACEHOLDERS = {
  audio_node: "the default output",
  player: "whichever is playing",
  app: "whichever is playing",
  sink: "part of its name",
};

/// The same at length, for a field's tooltip.
const TARGET_HINTS = {
  audio_node: "empty is the default output; mic is the default input, or a node id from wpctl status",
  player: "empty follows whichever player is playing, or name one, such as spotify",
  app: "empty is the app playing sound, or name its program, such as firefox",
  sink: "part of an output's name, such as Headphones",
};

/// Suggestions for every kind of target, filled in by fillTargets once the
/// daemon says what this machine has. A target field names one by `list`.
function targetLists() {
  return `
    <datalist id="f-targets-audio_node"><option value="mic" label="the default input"></option></datalist>
    <datalist id="f-targets-player"></datalist>
    <datalist id="f-targets-app"></datalist>
    <datalist id="f-targets-sink"></datalist>`;
}

/// The outputs and apps on this machine, asked for once a form wants them
/// and kept for a few seconds, which is as long as a form takes to open.
async function audioTargets() {
  if (state.targets && Date.now() - state.targets.at < 5000) return state.targets;
  try {
    const reply = await call({ cmd: "audio_targets" });
    state.targets = { at: Date.now(), outputs: reply.outputs ?? [], apps: reply.apps ?? [] };
  } catch (e) {
    state.targets = { at: Date.now(), outputs: [], apps: [], error: e.message };
  }
  return state.targets;
}

/// Words for an output beside its name: whether sound goes there now, and
/// whether it can play at all.
function outputState(output) {
  return [output.default ? "in use" : "", output.usable ? "" : "unplugged"].filter(Boolean).join(", ");
}

/// What to write to reach an output: its nickname when it has one, which
/// is what a person recognises and what matching tries first.
const outputName = (output) => output.nick ?? output.description ?? output.name;

/// Put `[value, label]` suggestions in datalist `id`, if the open form has
/// it. Names come from other programs, so they go in as properties, never
/// as markup.
function fillList(id, items) {
  const list = document.getElementById(id);
  if (!list) return;
  list.replaceChildren(...items.map(([value, label]) => {
    const option = document.createElement("option");
    option.value = value;
    option.label = label;
    return option;
  }));
}

/// Fill the output and app suggestions in the open form.
async function fillTargets() {
  if (!el("f-targets-sink")) return;
  // A label says what the value does not: the name shown, when it is not
  // the one written, and the state.
  const label = (value, display, words) =>
    [display !== value ? display : "", words].filter(Boolean).join(" · ");
  const found = await audioTargets();
  fillList("f-targets-sink", found.outputs.map((o) =>
    [outputName(o), label(outputName(o), o.display, outputState(o))]));
  fillList("f-targets-app", found.apps.map((a) =>
    [a.app, label(a.app, a.display, a.running ? "playing" : "")]));
  renderFoundOutputs();
}

/// Fill the player suggestions, from the same list a media widget's source
/// offers, once the open form first has a field that names a player: which
/// may be only after a preset or an action is changed to one.
async function fillPlayers() {
  const list = document.getElementById("f-targets-player");
  if (!list || list.dataset.asked) return;
  list.dataset.asked = "yes";
  state.sources ??= await call({ cmd: "widget_sources" }).catch(() => null);
  fillList("f-targets-player", (state.sources?.media ?? []).map((o) => [o.value, o.detail ?? ""]));
}

/// Point a target field at the suggestions for `kind`, and say what an
/// empty one means, naming the kind unless a label beside it already does.
/// Hidden when there is no kind: nothing would read it.
function fitTarget(input, kind, labelled = false) {
  input.hidden = !kind;
  if (!kind) return;
  if (kind === "player") fillPlayers().catch(() => {});
  const word = TARGET_WORDS[kind] ?? "Target";
  input.setAttribute("list", `f-targets-${kind}`);
  input.placeholder = labelled ? TARGET_PLACEHOLDERS[kind] ?? "" : `${word}: ${TARGET_PLACEHOLDERS[kind] ?? ""}`;
  input.title = `${word}: ${TARGET_HINTS[kind] ?? ""}`;
}

/// One gesture's editor: what a tap, hold, press or turn does.
///
/// `info` is the gesture as the daemon described it. `options.key` offers
/// what only a key's tap can do (pages, profiles, back), and
/// `options.implicit` what the key's widget does when nothing is set, in
/// words. `options.where` and `options.timer` choose the built-ins on
/// offer; see builtInOffered.
function actionPicker(slot, info, label, options = {}) {
  const kind = options.kind ?? info?.kind ?? "none";
  const choice = (value, text) => `<option value="${value}" ${kind === value ? "selected" : ""}>${text}</option>`;
  const pages = (state.layout?.pages ?? [])
    .map((p) => `<option ${p === options.page ? "selected" : ""}>${escapeHtml(p)}</option>`).join("");
  const profiles = (state.status?.profiles ?? [])
    .map((p) => `<option ${p === options.profile ? "selected" : ""}>${escapeHtml(p)}</option>`).join("");
  const shown = (part) => (part === kind ? "" : "hidden");
  const meta = builtInInfo(info?.action);
  return `
    <div class="field action-picker" data-slot="${slot}">
      <label>${escapeHtml(label)}</label>
      <select data-part="kind">
        ${options.implicit
          // Unset is the widget's own, so "nothing" is not something the
          // config can say here.
          ? choice("implicit", escapeHtml(options.implicit))
          : choice("none", "nothing")}
        ${choice("builtin", "something built in")}
        ${choice("keys", "press keys")}
        ${choice("shell", "run a command")}
        ${options.key ? choice("page", "switch page") : ""}
        ${options.key ? choice("profile", "switch profile") : ""}
        ${options.key ? choice("back", "go back") : ""}
      </select>
      <div data-show="builtin" ${shown("builtin")}>
        <select data-part="action">${builtInOptions(info?.action, options.where ?? "other", !!options.timer)}</select>
        <div class="row" data-part="extras">
          <input data-part="step" type="number" min="0" step="any" value="${info?.step ?? ""}"
                 placeholder="${meta?.step_default ?? ""}" title="How far each press or detent goes">
          <input data-part="target" value="${escapeHtml(info?.target ?? "")}" placeholder="default">
        </div>
        <span class="hint" data-part="about"></span>
      </div>
      <div data-show="keys" ${shown("keys")}>
        <div class="row">
          <input data-part="keys" list="f-key-names" value="${escapeHtml(info?.keys ?? "")}"
                 placeholder="ctrl+shift+t">
          <button type="button" data-part="capture" title="Press the keys to record them">Record…</button>
        </div>
        <span class="hint">Keys by where they are on a US keyboard, so they are right on any layout.</span>
      </div>
      <div data-show="shell" ${shown("shell")}>
        <input data-part="command" value="${escapeHtml(info?.command ?? "")}" placeholder="firefox">
        <span class="hint">Run with <code>sh -c</code>.</span>
      </div>
      <div data-show="page" ${shown("page")}><select data-part="page">${pages}</select></div>
      <div data-show="profile" ${shown("profile")}><select data-part="profile">${profiles}</select></div>
      <div class="row picker-foot">
        <span class="hint warn" data-part="warn"></span>
        <button type="button" data-part="try" title="Do it now, as the deck would">Try it</button>
      </div>
    </div>`;
}

/// What a picker is set to, as the fields of an action table (or a
/// navigation, which lives in the key's own fields).
function pickerValue(picker) {
  const part = (name) => picker.querySelector(`[data-part="${name}"]`);
  const kind = part("kind").value;
  switch (kind) {
    case "builtin": {
      const fields = { action: str(part("action").value) };
      const step = part("step").value.trim();
      const target = part("target").value.trim();
      const meta = builtInInfo(part("action").value);
      if (step !== "" && meta?.step_unit) fields.step = float(Number(step));
      if (target !== "" && meta?.takes_target) fields.target = str(target);
      return { kind, fields };
    }
    case "keys":
      return { kind, fields: { keys: str(part("keys").value.trim()) } };
    case "shell":
      return { kind, fields: { exec: str(part("command").value.trim()) } };
    case "page":
      return { kind, page: part("page").value };
    case "profile":
      return { kind, profile: part("profile").value };
    default:
      return { kind };
  }
}

/// Patches that write a picker into `path`: removed first, so a string
/// becoming a table (or the reverse) never trips over the old value.
function actionPatches(patches, path, value) {
  patches.push({ op: "remove", path });
  if (value.kind === "shell") {
    if (value.fields.exec.value !== "") patches.push({ op: "set", path, value: value.fields.exec });
  } else if (value.kind === "builtin" || value.kind === "keys") {
    for (const [name, v] of Object.entries(value.fields)) {
      patches.push({ op: "set", path: `${path}.${name}`, value: v });
    }
  }
}

function wireActionPickers(root = document) {
  for (const picker of root.querySelectorAll(".action-picker")) {
    const part = (name) => picker.querySelector(`[data-part="${name}"]`);
    const update = () => {
      const kind = part("kind").value;
      for (const section of picker.querySelectorAll("[data-show]")) {
        section.hidden = section.dataset.show !== kind;
      }
      const meta = kind === "builtin" ? builtInInfo(part("action").value) : null;
      if (meta) {
        part("step").hidden = !meta.step_unit;
        part("step").placeholder = meta.step_unit ? `${meta.step_default} ${meta.step_unit}` : "";
        fitTarget(part("target"), meta.takes_target ? meta.target_kind ?? "audio_node" : null);
        part("about").textContent = meta.step_unit
          ? `Each press or detent: ${meta.step_min}–${meta.step_max} ${meta.step_unit}.`
          : "";
      }
      const needsInput = kind === "keys" || meta?.needs_virtual_input;
      part("warn").textContent = (needsInput ? virtualInputProblem() : null)
        ?? (meta?.needs_mixer ? mixerProblem() : null) ?? "";
      // The editor refuses what acts on the key or knob it is bound to:
      // from here there is none.
      part("try").hidden = !["builtin", "keys", "shell"].includes(kind)
        || (meta && (meta.keys_only || meta.knobs_only));
    };
    part("kind").addEventListener("change", update);
    part("action").addEventListener("change", update);
    update();

    // Record the next key press in the browser, modifiers and all.
    part("capture").addEventListener("click", () => {
      const button = part("capture");
      button.textContent = "Press keys…";
      const listener = (event) => {
        event.preventDefault();
        event.stopPropagation();
        if (MODIFIER_CODES.has(event.code)) return;
        window.removeEventListener("keydown", listener, true);
        part("keys").value = chordOf(event);
        part("keys").dispatchEvent(new Event("input", { bubbles: true }));
        button.textContent = "Record…";
      };
      window.addEventListener("keydown", listener, true);
    });

    part("try").addEventListener("click", async () => {
      const value = pickerValue(picker);
      if (!value.fields) return;
      try {
        await call({ cmd: "run_action", fields: value.fields });
        toast("done");
      } catch (e) {
        toast(e.message, true);
      }
    });
  }
}

/// The key-name suggestions every chord field shares.
function keyNamesList() {
  return `<datalist id="f-key-names">${(state.catalog?.keys ?? [])
    .map((k) => `<option value="${escapeHtml(k.name)}">`).join("")}</datalist>`;
}

// --------------------------------------------------------------------- knobs

const LAYER_WORDS = { global: "every profile", profile: "this profile", page: "this page" };

/// The file and array path a knob layer lives in.
function layerHome(layer) {
  if (layer === "global") return { file: "galdeck.toml", array: "encoders" };
  if (layer === "profile") return { file: state.layout.file, array: "encoders" };
  return { file: state.layout.file, array: `pages[${state.layout.page_index}].encoders` };
}

/// This knob's own entry at a layer, if it has one.
function layerEntry(info, layer) {
  return (info?.layers ?? []).find((l) => l.layer === layer);
}

/// How many entries a layer's array already has, across both knobs: where
/// an appended entry lands.
function layerCount(layer) {
  return (state.layout?.encoders ?? []).filter((e) => layerEntry(e, layer)).length;
}

const LAYER_ORDER = ["global", "profile", "page"];

/// The nearest layer under `layer` that sets `field` (ring, animation).
function inheritedFrom(info, layer, field) {
  const below = LAYER_ORDER.slice(0, LAYER_ORDER.indexOf(layer)).reverse();
  const from = below.find((l) => layerEntry(info, l)?.[field]);
  return from ? { layer: from, value: layerEntry(info, from)[field] } : null;
}

/// The nearest layer over `layer` that sets `field`, and so wins over it here.
function overriddenBy(info, layer, field) {
  return LAYER_ORDER.slice(LAYER_ORDER.indexOf(layer) + 1)
    .find((l) => layerEntry(info, l)?.[field]) ?? null;
}

function describeGesture(g) {
  if (!g) return "nothing";
  const from = g.origin && g.origin !== "page" ? ` <small>(${LAYER_WORDS[g.origin]})</small>` : "";
  return `${escapeHtml(g.label)}${from}`;
}

/// Where the ring rests in a mode that names no colour: the daemon's
/// choices (engine.rs MODE_RINGS and mode_rings), mirrored so the pickers
/// show what the ring will do.
const MODE_RINGS = ["#88c0d0", "#ebcb8b", "#b48ead", "#d08770", "#a3be8c", "#5e81ac"];

/// Where each mode rests, given the colour each names (or null) and the
/// knob's own: what a mode names, the knob's own for the first, else the
/// first of MODE_RINGS far from every colour already showing.
function modeRings(named, base) {
  const rgb = (hex) => [1, 3, 5].map((i) => parseInt(String(hex).slice(i, i + 2), 16) || 0);
  const distance = (a, b) => rgb(a).reduce((sum, v, i) => sum + Math.abs(v - rgb(b)[i]), 0);
  const shown = [...named.filter(Boolean), named[0] ?? base];
  return named.map((own, mode) => {
    if (own) return own;
    if (mode === 0) return base;
    const nearest = (c) => Math.min(...shown.map((s) => distance(c, s)));
    const pick = MODE_RINGS.find((c) => nearest(c) > 60)
      // Ties go to the later, as Rust's max_by_key does.
      ?? MODE_RINGS.reduce((best, c) => (nearest(c) >= nearest(best) ? c : best));
    shown.push(pick);
    return pick;
  });
}

/// Most modes a knob switches between: its ring has four segments to say
/// which one is on.
const MAX_MODES = 4;

/// Where each row of the modes editor rests: its own colour when it has
/// one, else the default among the others.
function modeDefaults(info, rows) {
  const named = rows.map((row) => (row.dataset.own === "yes"
    ? row.querySelector('[data-part="ring"]').value.toLowerCase() : null));
  return modeRings(named, (info.ring ?? "#000000").toLowerCase());
}

/// Whether another layer gives the knob modes that holding it switches
/// here: one over `layer`, whose turn wins whatever this one does, or one
/// under it while this layer leaves the turn alone. A hold set at any of
/// them stops the switching all the same.
function modesElsewhere(info, layer, turnsHere) {
  const at = LAYER_ORDER.indexOf(layer);
  return (info.modes?.length ?? 0) >= 2
    && LAYER_ORDER.some((l, i) => i !== at && (i > at || !turnsHere)
      && (layerEntry(info, l)?.modes?.length ?? 0) >= 2);
}

/// One mode in the modes editor: what turning does in it, its own step and
/// target, and the colour its ring rests at. `mode.ring` is set only when
/// the mode names a colour; otherwise the picker shows `rest`, the default,
/// and is not saved unless it is changed.
function modeRow(mode, index, rest) {
  const needs = mixerProblem() ? " — needs PipeWire" : "";
  const options = (state.catalog?.presets ?? []).map((p) =>
    `<option value="${escapeHtml(p.name)}" ${p.name === mode.preset ? "selected" : ""}>${
      escapeHtml(p.title || p.name)}${p.needs_mixer ? needs : ""}</option>`).join("");
  return `
    <div class="mode-row" data-own="${mode.ring ? "yes" : ""}">
      <div class="row">
        <span class="mode-number">${index + 1}</span>
        <select data-part="preset" title="What turning does in this mode">${options}</select>
        <input data-part="ring" type="color" value="${escapeHtml(mode.ring ?? rest)}"
               title="Where the ring rests while this mode is on">
        <button type="button" data-part="remove" title="Remove this mode">×</button>
      </div>
      <div class="row">
        <input data-part="step" type="number" min="0" step="any" value="${mode.step ?? ""}"
               placeholder="step: the preset's own" title="How far each detent goes in this mode">
        <input data-part="target" value="${escapeHtml(mode.target ?? "")}">
      </div>
    </div>`;
}

/// The modes as the editor has them, as they would be written. A colour is
/// kept whenever the layer wrote it or it was picked, even if it happens to
/// be a default: only the defaults move when modes are added or removed.
function collectModes() {
  return [...document.querySelectorAll("#f-modes .mode-row")].map((row) => {
    const part = (name) => row.querySelector(`[data-part="${name}"]`);
    const preset = part("preset").value;
    const step = part("step").value.trim();
    const target = presetInfo(preset)?.target_kind ? part("target").value.trim() : "";
    const ring = part("ring").value.toLowerCase();
    return {
      preset,
      step: step === "" || Number.isNaN(Number(step)) ? null : Number(step),
      target,
      ring: row.dataset.own === "yes" ? ring : null,
    };
  });
}

/// Patches that write `modes`: plain preset names while that is all they
/// are, which reads best, else an array of tables, one per mode. A patch
/// cannot write an inline table, and a list cannot mix the two.
function modePatches(patches, path, modes) {
  patches.push({ op: "remove", path });
  if (modes.every((m) => m.step === null && !m.target && !m.ring)) {
    patches.push({ op: "set", path, value: { type: "array", value: modes.map((m) => str(m.preset)) } });
    return;
  }
  for (const mode of modes) {
    const fields = { preset: str(mode.preset) };
    if (mode.step !== null) fields.step = float(mode.step);
    if (mode.target) fields.target = str(mode.target);
    if (mode.ring) fields.ring = str(mode.ring);
    patches.push({ op: "append", path, fields });
  }
}

/// The knob form's first line: which mode it is in, when it has modes, and
/// what each gesture does once every layer is folded in.
function knobSummary(info) {
  const r = info.resolved ?? {};
  const count = info.modes?.length ?? 0;
  const hold = r.hold?.kind === "implicit" ? "next mode" : r.hold ? describeGesture(r.hold) : null;
  const mode = count >= 2 && info.mode != null
    ? `Mode ${info.mode + 1}/${count} · ${escapeHtml(info.modes[info.mode]?.title ?? "")} · Hold: ${hold ?? "nothing"}<br>`
    : "";
  return `<p class="hint knob-summary">${mode}Turn right: ${describeGesture(r.cw)}
    · Turn left: ${describeGesture(r.ccw)} · Press: ${describeGesture(r.press)}${
    !mode && hold ? ` · Hold: ${hold}` : ""}</p>`;
}

function encoderForm(info, encoder) {
  info = info ?? { encoder, layers: [], resolved: {}, ring: "#000000" };
  // Edit where the knob is already set, nearest first; a knob set nowhere is
  // best set for the whole profile, so it is the same on every page.
  const layer = state.knobLayer?.[encoder]
    ?? ["page", "profile", "global"].find((l) => layerEntry(info, l))
    ?? "profile";
  const entry = layerEntry(info, layer);
  const needsMixer = mixerProblem() ? `<small class="warn">needs PipeWire</small>` : "";
  const presetCards = (state.catalog?.presets ?? []).map((p) => `
    <label class="preset-card ${entry?.preset === p.name ? "chosen" : ""}">
      <input type="radio" name="f-knob-preset" value="${escapeHtml(p.name)}" ${entry?.preset === p.name ? "checked" : ""}>
      <strong>${escapeHtml(p.title || p.name)}</strong>
      <small>${escapeHtml(p.label)}</small>
      ${p.needs_virtual_input && virtualInputProblem() ? `<small class="warn">needs keystrokes</small>` : ""}
      ${p.needs_mixer ? needsMixer : ""}
    </label>`).join("");
  // This layer's own modes, or a start on some: its preset, and another.
  const ownModes = entry?.modes ?? [];
  const first = entry?.preset ?? info.turn_preset ?? "volume";
  const modes = ownModes.length > 0 ? ownModes : [
    { preset: first, step: entry?.step, target: entry?.target },
    { preset: first === "volume" ? "scroll" : "volume" },
  ];
  const usesModes = ownModes.length > 0;
  const rests = modeRings(modes.map((m) => m.ring?.toLowerCase() ?? null),
    (info.ring ?? "#000000").toLowerCase());
  const g = entry?.gestures ?? {};
  const levelOption = (value) =>
    `<option value="${value}" ${layer === value ? "selected" : ""}>${LAYER_WORDS[value]}</option>`;
  // Only this layer's own colour and animation are shown as its values; what
  // it inherits is a hint, so saving here never copies it in.
  const ringBelow = inheritedFrom(info, layer, "ring");
  const ringShown = entry?.ring ?? ringBelow?.value ?? info.base_ring ?? info.ring ?? "#000000";
  const ringOver = overriddenBy(info, layer, "ring");
  const ringHint = [
    entry?.ring ? `Set for ${LAYER_WORDS[layer]}.`
      : `Inherited from ${ringBelow ? LAYER_WORDS[ringBelow.layer] : "the theme"}.`,
    ringOver ? `Here, the colour for ${LAYER_WORDS[ringOver]} wins.` : "",
  ].join(" ");
  const animBelow = inheritedFrom(info, layer, "animation");
  const animOver = overriddenBy(info, layer, "animation");
  const animationHint = [
    !entry?.animation && animBelow
      ? `Inherited: ${escapeHtml(animBelow.value.kind)} from ${LAYER_WORDS[animBelow.layer]}; clear it there to stop it.`
      : "",
    animOver ? `Here, the animation for ${LAYER_WORDS[animOver]} wins.` : "",
  ].join(" ").trim();
  return `
    <h2>Knob ${encoder} (${encoder === 0 ? "left" : "right"})</h2>
    ${knobSummary(info)}
    <div class="field">
      <label for="f-knob-layer">Set for</label>
      <select id="f-knob-layer">${levelOption("page")}${levelOption("profile")}${levelOption("global")}</select>
      <span class="hint">A page's own setting replaces the profile's one gesture at a time, and the
        profile's replaces the one for every profile.</span>
    </div>
    <fieldset class="turning">
      <legend>Turning</legend>
      <div class="knob-kind" role="radiogroup">
        <label><input type="radio" name="f-knob-kind" value="one" ${usesModes ? "" : "checked"}> One preset</label>
        <label><input type="radio" name="f-knob-kind" value="modes" ${usesModes ? "checked" : ""}> Modes (hold to switch)</label>
      </div>
      <div data-knob-kind="one" ${usesModes ? "hidden" : ""}>
        <div class="preset-cards">
          <label class="preset-card ${!entry?.preset ? "chosen" : ""}">
            <input type="radio" name="f-knob-preset" value="" ${!entry?.preset ? "checked" : ""}>
            <strong>None</strong><small>Only what is set below.</small>
          </label>
          ${presetCards}
        </div>
        <div class="field">
          <label for="f-knob-step">Step</label>
          <input id="f-knob-step" type="number" min="0" step="any" value="${entry?.step ?? ""}" placeholder="the preset's own">
          <span class="hint">Percent for sound and brightness, seconds for skipping, notches for scrolling.</span>
        </div>
        <div class="field" id="f-knob-target-field" hidden>
          <label for="f-knob-target" id="f-knob-target-label">Target</label>
          <input id="f-knob-target" value="${escapeHtml(entry?.target ?? "")}">
          <span class="hint">Empty leaves it to the preset. Only the preset's own kind of action reads it.</span>
        </div>
      </div>
      <div data-knob-kind="modes" ${usesModes ? "" : "hidden"}>
        <div id="f-modes" class="modes">${modes.map((mode, index) => modeRow(mode, index, rests[index])).join("")}</div>
        <button id="f-mode-add" type="button">Add a mode</button>
        <p class="hint">Two to four. Holding the knob moves on to the next, and the ring rests in the
          mode's colour. Each mode has its own step and target.</p>
      </div>
      <button id="f-knob-outputs" type="button" hidden>Which outputs it switches between…</button>
    </fieldset>
    <details class="more" ${g.press || g.cw || g.ccw || g.hold ? "open" : ""}>
      <summary>Customise</summary>
      <p class="hint">Anything set here replaces the preset's for that gesture.</p>
      ${actionPicker("cw", g.cw, "Turn right", { where: "knob" })}
      ${actionPicker("ccw", g.ccw, "Turn left", { where: "knob" })}
      ${actionPicker("press", g.press, "Press", { where: "knob" })}
      ${actionPicker("hold", g.hold, "Hold", { where: "knob" })}
      <p class="hint" id="f-knob-hold-hint"></p>
    </details>
    <div class="field">
      <label for="f-ring">Ring colour</label>
      <div class="row">
        <input id="f-ring" type="color" value="${escapeHtml(ringShown)}" data-initial="${escapeHtml(ringShown)}">
        <label class="inline"><input id="f-ring-own" type="checkbox" ${entry?.ring ? "checked" : ""}>
          own colour</label>
      </div>
      <span class="hint">${ringHint}</span>
    </div>
    ${animationFields(entry?.animation, true, animationHint)}
    ${keyNamesList()}
    ${targetLists()}
    <div class="actions">
      <button id="f-save" class="primary">Save</button>
      ${entry ? `<button id="f-remove" class="danger">Clear for ${LAYER_WORDS[layer]}</button>` : ""}
    </div>`;
}

/// Keep the knob form saying what applies to what is chosen: the preset or
/// the modes, the target field for the preset's kind of target, each mode's
/// own, and what holding does.
function wireKnobForm() {
  const list = el("f-modes");
  if (!list) return;
  const encoder = state.selected.encoder;
  const info = state.layout.encoders.find((e) => e.encoder === encoder)
    ?? { encoder, layers: [], resolved: {}, ring: "#000000" };
  const layer = el("f-knob-layer").value;
  const touched = () => (state.draft = JSON.stringify(state.selected));

  const update = () => {
    const kind = document.querySelector('input[name="f-knob-kind"]:checked').value;
    for (const part of document.querySelectorAll("[data-knob-kind]")) {
      part.hidden = part.dataset.knobKind !== kind;
    }
    const preset = document.querySelector('input[name="f-knob-preset"]:checked')?.value ?? "";
    const targetKind = presetInfo(preset)?.target_kind;
    el("f-knob-target-field").hidden = !targetKind;
    if (targetKind) {
      el("f-knob-target-label").textContent = TARGET_WORDS[targetKind] ?? "Target";
      fitTarget(el("f-knob-target"), targetKind, true);
    }
    const rows = [...list.querySelectorAll(".mode-row")];
    const rests = modeDefaults(info, rows);
    rows.forEach((row, index) => {
      const part = (name) => row.querySelector(`[data-part="${name}"]`);
      part("remove").disabled = rows.length <= 2;
      row.querySelector(".mode-number").textContent = index + 1;
      fitTarget(part("target"), presetInfo(part("preset").value)?.target_kind);
      if (row.dataset.own !== "yes") part("ring").value = rests[index];
    });
    el("f-mode-add").disabled = rows.length >= MAX_MODES;
    const presets = kind === "modes" ? rows.map((row) => row.querySelector('[data-part="preset"]').value) : [preset];
    el("f-knob-outputs").hidden = !presets.includes("outputs");

    // With modes, holding the knob switches them unless a hold is set. One
    // set to the next mode still does. A turn of this layer's own is as
    // much its turn as a preset.
    const bound = (slot) =>
      document.querySelector(`.action-picker[data-slot="${slot}"] [data-part="kind"]`).value !== "none";
    const modesOn = kind === "modes" || modesElsewhere(info, layer, preset !== "" || bound("cw") || bound("ccw"));
    const hold = document.querySelector('.action-picker[data-slot="hold"]');
    const holdKind = hold.querySelector('[data-part="kind"]');
    const switches = holdKind.value === "none"
      || (holdKind.value === "builtin" && hold.querySelector('[data-part="action"]').value === "next_mode");
    holdKind.querySelector('option[value="none"]').textContent = modesOn ? "switch mode" : "nothing";
    el("f-knob-hold-hint").textContent = !modesOn ? ""
      : switches ? "Holding switches mode. Setting a hold here turns that off."
      : "A hold is set, so the modes can never be switched. Set it back to “switch mode”.";
  };

  for (const input of document.querySelectorAll('input[name="f-knob-kind"], input[name="f-knob-preset"]')) {
    input.addEventListener("change", update);
  }
  for (const part of document.querySelectorAll('.action-picker [data-part="kind"], .action-picker[data-slot="hold"] [data-part="action"]')) {
    part.addEventListener("change", update);
  }
  list.addEventListener("change", update);
  list.addEventListener("input", (event) => {
    if (event.target.dataset.part === "ring") event.target.closest(".mode-row").dataset.own = "yes";
  });
  list.addEventListener("click", (event) => {
    if (event.target.dataset.part !== "remove") return;
    event.target.closest(".mode-row").remove();
    touched();
    update();
  });
  el("f-mode-add").addEventListener("click", () => {
    const rows = collectModes();
    if (rows.length >= MAX_MODES) return;
    const unused = (state.catalog?.presets ?? []).find((p) => !rows.some((m) => m.preset === p.name));
    // Its colour is filled in with the others' by update().
    list.insertAdjacentHTML("beforeend", modeRow({ preset: unused?.name ?? "volume" }, rows.length, "#000000"));
    touched();
    update();
  });
  el("f-knob-outputs").addEventListener("click", () => select({ kind: "outputs" }));
  update();
  // What was there, so saving rewrites the modes only when they changed and
  // a colour written as a theme token stays one.
  list.dataset.initial = JSON.stringify(collectModes());
}

/// The knob form's patches, and the file they go in.
function collectEncoderPatches(info, encoder) {
  const layer = el("f-knob-layer").value;
  const { file, array } = layerHome(layer);
  const entry = layerEntry(info, layer);
  const patches = [];
  let base;
  if (entry) {
    base = entry.path;
  } else {
    patches.push({ op: "append", path: array, fields: { encoder: int(encoder) } });
    base = `${array}[${layerCount(layer)}]`;
  }
  const kind = document.querySelector('input[name="f-knob-kind"]:checked')?.value ?? "one";
  if (kind === "modes") {
    const modes = collectModes();
    if (modes.length < 2 || modes.length > MAX_MODES) {
      throw new Error(`a knob switches between two and ${MAX_MODES} modes`);
    }
    // Each mode has its own step and target, and a preset beside them
    // would only be ignored.
    for (const field of ["preset", "step", "target"]) {
      patches.push({ op: "remove", path: `${base}.${field}` });
    }
    if (!entry?.modes?.length || JSON.stringify(modes) !== el("f-modes").dataset.initial) {
      modePatches(patches, `${base}.modes`, modes);
    }
  } else {
    const preset = document.querySelector('input[name="f-knob-preset"]:checked')?.value ?? "";
    setOrRemove(patches, `${base}.preset`, preset);
    const step = el("f-knob-step").value.trim();
    patches.push(step === "" || !preset
      ? { op: "remove", path: `${base}.step` }
      : { op: "set", path: `${base}.step`, value: float(Number(step)) });
    // Only a preset that takes a target reads one.
    const target = presetInfo(preset)?.target_kind ? el("f-knob-target").value.trim() : "";
    setOrRemove(patches, `${base}.target`, target);
    if (entry?.modes?.length) patches.push({ op: "remove", path: `${base}.modes` });
  }
  for (const picker of document.querySelectorAll(".action-picker")) {
    actionPatches(patches, `${base}.${picker.dataset.slot}`, pickerValue(picker));
  }
  // Written only when this layer's own value changes: an untouched `@accent`
  // stays a token, and nothing inherited is copied down.
  const ring = el("f-ring");
  if (el("f-ring-own").checked) {
    if (!entry?.ring || ring.value.toLowerCase() !== ring.dataset.initial.toLowerCase()) {
      patches.push({ op: "set", path: `${base}.style.ring`, value: str(ring.value) });
    }
  } else if (entry?.ring) {
    patches.push({ op: "remove", path: `${base}.style.ring` });
  }
  if (animationChanged(entry?.animation)) collectAnimation(patches, base, true);
  return { patches, file };
}

async function removeEncoderLayer(info, encoder) {
  const layer = el("f-knob-layer").value;
  const entry = layerEntry(info, layer);
  if (!entry) return;
  const { file, array } = layerHome(layer);
  const index = Number(entry.path.match(/\[(\d+)\]$/)[1]);
  await apply([{ op: "remove_at", path: array, index }], file);
  void encoder;
}

// ------------------------------------------------------------------- outputs

/// The `outputs` list in galdeck.toml: which outputs switching goes through,
/// and in what order. It is the one setting there that is about the machine
/// rather than the deck, so it has a form of its own.
function outputsForm(layout) {
  const problem = mixerProblem();
  return `
    <h2>Sound outputs</h2>
    <p class="hint">Which outputs switching goes through, in this order: an output switcher knob,
      and the next and previous output actions. Each entry is part of an output's name, and picks
      the connected outputs it names. With none, switching goes through every output that is
      plugged in.</p>
    ${problem ? `<p class="hint warn">${escapeHtml(problem)}</p>` : ""}
    <div id="f-outputs" class="outputs">${(layout.outputs ?? []).map(outputRow).join("")}</div>
    <button id="f-output-add" type="button">Add an output</button>
    <div class="field found">
      <label>On this machine now</label>
      <ul id="f-outputs-found" class="found-list"><li class="hint">Looking…</li></ul>
    </div>
    ${targetLists()}
    <div class="actions">
      <button id="f-outputs-save" class="primary">Save</button>
      <span class="hint">In galdeck.toml, so for every profile.</span>
    </div>`;
}

function outputRow(name) {
  return `
    <div class="row output-row">
      <input data-part="name" list="f-targets-sink" value="${escapeHtml(name)}" placeholder="part of an output's name">
      <button type="button" data-part="up" title="Earlier in the order">↑</button>
      <button type="button" data-part="remove" title="Leave it out">×</button>
    </div>`;
}

/// What audio_targets found, under the outputs editor, each with a button
/// that adds it. The names come from other programs, so they are set as
/// text and never parsed as markup.
function renderFoundOutputs() {
  const list = el("f-outputs-found");
  if (!list || !state.targets) return;
  const line = (text) => {
    const item = document.createElement("li");
    item.className = "hint";
    item.textContent = text;
    return item;
  };
  if (state.targets.error) {
    list.replaceChildren(line(`Nothing to show: ${state.targets.error}`));
    return;
  }
  if (state.targets.outputs.length === 0) {
    list.replaceChildren(line("No outputs found."));
    return;
  }
  list.replaceChildren(...state.targets.outputs.map((output) => {
    const item = document.createElement("li");
    const add = document.createElement("button");
    add.type = "button";
    add.title = "Add it to the list";
    add.textContent = "+";
    add.addEventListener("click", () => {
      el("f-outputs").insertAdjacentHTML("beforeend", outputRow(""));
      el("f-outputs").lastElementChild.querySelector("input").value = outputName(output);
      state.draft = JSON.stringify(state.selected);
    });
    const name = document.createElement("span");
    name.textContent = output.display;
    const words = document.createElement("small");
    words.textContent = outputState(output);
    item.append(add, name, words);
    return item;
  }));
}

function wireOutputsForm() {
  const list = el("f-outputs");
  if (!list) return;
  const touched = () => (state.draft = JSON.stringify(state.selected));
  list.addEventListener("click", (event) => {
    const row = event.target.closest(".output-row");
    const part = event.target.dataset.part;
    if (part === "remove") {
      row.remove();
    } else if (part === "up" && row.previousElementSibling) {
      row.previousElementSibling.before(row);
    } else {
      return;
    }
    touched();
  });
  el("f-output-add").addEventListener("click", () => {
    list.insertAdjacentHTML("beforeend", outputRow(""));
    list.lastElementChild.querySelector("input").focus();
    touched();
  });
  el("f-outputs-save").addEventListener("click", saveOutputs);
}

async function saveOutputs() {
  // Each once: a second mention of the same output adds nothing to the
  // order the first gave it.
  const names = [...document.querySelectorAll('#f-outputs [data-part="name"]')]
    .map((input) => input.value.trim())
    .filter((name, i, all) => name
      && all.findIndex((other) => other.toLowerCase() === name.toLowerCase()) === i);
  // Set in place when there is a list, so it stays where it was written.
  const patches = names.length === 0
    ? [{ op: "remove", path: "outputs" }]
    : [{ op: "set", path: "outputs", value: { type: "array", value: names.map(str) } }];
  await apply(patches, "galdeck.toml").catch((e) => toast(e.message, true));
}

// --------------------------------------------------------------------- pages

/// Ready-made pages. Each key is [label, action fields].
const PAGE_TEMPLATES = {
  blank: { label: "Blank", keys: [] },
  numpad: {
    label: "Numpad",
    note: "Keypad keys: they type digits while Num Lock is on, which the deck turns on when it can tell it is off.",
    keys: [
      ["7", { keys: "kp_7" }], ["8", { keys: "kp_8" }], ["9", { keys: "kp_9" }],
      ["4", { keys: "kp_4" }], ["5", { keys: "kp_5" }], ["6", { keys: "kp_6" }],
      ["1", { keys: "kp_1" }], ["2", { keys: "kp_2" }], ["3", { keys: "kp_3" }],
      ["0", { keys: "kp_0" }], [".", { keys: "kp_decimal" }], ["⏎", { keys: "kp_enter" }],
    ],
    labelSize: 64,
  },
  digits: {
    label: "Digits (top row)",
    note: "The number row's keys, for apps and games that ignore the keypad. On AZERTY these need shift.",
    keys: [
      ["7", { keys: "7" }], ["8", { keys: "8" }], ["9", { keys: "9" }],
      ["4", { keys: "4" }], ["5", { keys: "5" }], ["6", { keys: "6" }],
      ["1", { keys: "1" }], ["2", { keys: "2" }], ["3", { keys: "3" }],
      ["0", { keys: "0" }], ["⌫", { keys: "backspace" }], ["⏎", { keys: "enter" }],
    ],
    labelSize: 64,
  },
  media: {
    label: "Media",
    note: "Built-in sound and media controls, no scripts needed.",
    keys: [
      ["Prev", { action: "previous_track" }], ["Play", { action: "play_pause" }], ["Next", { action: "next_track" }],
      ["−10s", { action: "seek_backward", step: 10 }], ["Mute", { action: "volume_mute" }], ["+10s", { action: "seek_forward", step: 10 }],
      ["Vol −", { action: "volume_down", step: 5 }], ["Mic", { action: "mic_mute" }], ["Vol +", { action: "volume_up", step: 5 }],
    ],
  },
  macro: {
    label: "Macro keys (F13–F19)",
    note: "Keys no keyboard has, for binding in your desktop's or apps' own shortcut settings.",
    keys: ["f13", "f14", "f15", "f16", "f17", "f18", "f19"].map((f) => [f.toUpperCase(), { keys: f }]),
  },
};

/// Whether a new page would give the left knob modes from its profile or
/// from every profile: the nearer of the two that sets its turn does it
/// with modes. Holding it then switches them, and a way home bound to that
/// hold would stop it.
function newPageSwitchesModes() {
  const info = state.layout.encoders.find((e) => e.encoder === 0);
  for (const layer of ["profile", "global"]) {
    const entry = layerEntry(info, layer);
    if ((entry?.modes?.length ?? 0) >= 2) return true;
    if (entry?.preset || entry?.gestures?.cw || entry?.gestures?.ccw) return false;
  }
  return false;
}

function pageForm() {
  const free = [...Array(KEY_COUNT).keys()].filter((k) => !state.layout.keys.some((i) => i.key === k));
  const templates = Object.entries(PAGE_TEMPLATES)
    .map(([id, t]) => `<option value="${id}">${escapeHtml(t.label)}</option>`).join("");
  return `
    <h2>New page</h2>
    <div class="field">
      <label for="f-page-id">Name</label>
      <input id="f-page-id" placeholder="numpad" pattern="[A-Za-z0-9_-]+">
      <span class="hint">Letters, digits, dashes and underscores.</span>
    </div>
    <div class="field">
      <label for="f-page-template">Start from</label>
      <select id="f-page-template">${templates}</select>
      <span class="hint" id="f-page-note"></span>
    </div>
    <div class="field">
      <label for="f-page-link">Open it from this page</label>
      <select id="f-page-link">
        <option value="">no key</option>
        ${free.map((k) => `<option value="${k}">key ${k}</option>`).join("")}
      </select>
    </div>
    <p class="hint">${newPageSwitchesModes()
      ? "Holding the left knob switches its modes there too, so it is not also a way home."
      : "Holding the left knob goes back to the profile's home page from any new page."}</p>
    <div class="actions"><button id="f-page-create" class="primary">Create</button></div>`;
}

function wirePageForm() {
  const template = el("f-page-template");
  if (!template) return;
  const note = () => (el("f-page-note").textContent = PAGE_TEMPLATES[template.value].note ?? "");
  template.addEventListener("change", () => {
    note();
    if (!el("f-page-id").value) el("f-page-id").placeholder = template.value;
  });
  note();
  el("f-page-create").addEventListener("click", createPage);
}

function typedAction(fields) {
  return Object.fromEntries(Object.entries(fields).map(([k, v]) =>
    [k, typeof v === "number" ? float(v) : str(v)]));
}

async function createPage() {
  const template = PAGE_TEMPLATES[el("f-page-template").value];
  const id = (el("f-page-id").value.trim() || el("f-page-template").value).replace(/[^A-Za-z0-9_-]/g, "-");
  if (state.layout.pages.includes(id)) {
    toast(`there is already a page called ${id}`, true);
    return;
  }
  const index = state.layout.pages.length;
  const page = `pages[${index}]`;
  const patches = [{ op: "append", path: "pages", fields: { id: str(id) } }];
  if (template.labelSize) {
    patches.push({ op: "set", path: `${page}.style.key_label_size`, value: float(template.labelSize) });
  }
  template.keys.forEach(([label, action], key) => {
    patches.push({ op: "append", path: `${page}.keys`, fields: { key: int(key), label: str(label) } });
    for (const [name, value] of Object.entries(typedAction(action))) {
      patches.push({ op: "set", path: `${page}.keys[${key}].exec.${name}`, value });
    }
  });
  // A way back from any new page, without spending one of its keys; but
  // not over a hold that switches modes, which would never switch there.
  if (!newPageSwitchesModes()) {
    patches.push({ op: "append", path: `${page}.encoders`, fields: { encoder: int(0) } });
    patches.push({ op: "set", path: `${page}.encoders[0].hold.action`, value: str("home_page") });
  }
  const link = el("f-page-link").value;
  if (link !== "") {
    const here = state.layout.page_index;
    const count = state.layout.keys.length;
    patches.push({
      op: "append", path: `pages[${here}].keys`,
      fields: { key: int(Number(link)), label: str(template.label.split(" ")[0]), page: str(id) },
    });
    void count;
  }
  if (await apply(patches).catch((e) => (toast(e.message, true), false))) {
    toast(`created ${id}`);
  }
}

// ------------------------------------------------------------------ editing

const str = (v) => ({ type: "string", value: v });
const int = (v) => ({ type: "integer", value: v });
const bool = (v) => ({ type: "boolean", value: v });
const float = (v) => ({ type: "float", value: v });

function keyPath(index) {
  return `pages[${state.layout.page_index}].keys[${index}]`;
}

/// Set a field, or remove it when the value is empty — an empty string in the
/// config is not the same as an absent key, and leaving `exec = ""` behind
/// would bind the key to a command that does nothing.
function setOrRemove(patches, path, value) {
  if (value === null || value === undefined || value === "") {
    patches.push({ op: "remove", path });
  } else {
    patches.push({ op: "set", path, value: str(value) });
  }
}

function collectKeyPatches(info) {
  const base = keyPath(info.index);
  const patches = [];
  setOrRemove(patches, `${base}.label`, el("f-label").value.trim());
  setOrRemove(patches, `${base}.icon`, el("f-icon").value.trim());

  // Exactly one tap, so the others are cleared rather than left to argue.
  const pickers = Object.fromEntries([...document.querySelectorAll(".action-picker")]
    .map((picker) => [picker.dataset.slot, pickerValue(picker)]));
  const tap = pickers.tap;
  actionPatches(patches, `${base}.exec`, tap);
  setOrRemove(patches, `${base}.page`, tap.kind === "page" ? tap.page : "");
  setOrRemove(patches, `${base}.profile`, tap.kind === "profile" ? tap.profile : "");
  if (tap.kind === "back") {
    patches.push({ op: "set", path: `${base}.back`, value: bool(true) });
  } else {
    patches.push({ op: "remove", path: `${base}.back` });
  }
  actionPatches(patches, `${base}.hold`, pickers.hold);
  actionPatches(patches, `${base}.double`, pickers.double);

  const bg = el("f-bg").value;
  if (info.background_is_own || bg.toLowerCase() !== info.background.toLowerCase()) {
    patches.push({ op: "set", path: `${base}.style.key_bg`, value: str(bg) });
  }
  collectWidget(patches, `${base}.widget`, "key");
  collectAnimation(patches, base, false);
  return patches;
}

/// Widget settings, or its removal.
///
/// Settings that do not apply to the chosen kind are removed rather than
/// left behind, so changing a widget's kind never leaves the old one's
/// settings in the file.
function collectWidget(patches, path, ctx) {
  const kind = el("f-widget")?.value ?? "none";
  if (kind === "none") {
    patches.push({ op: "remove", path });
    return;
  }
  const view = el("f-widget-view").value;
  const applies = (field) => widgetFieldApplies(field, kind, view, ctx);
  const text = (id, field) => (applies(field) ? el(id).value.trim() : "");
  const number = (id, field) => {
    const raw = applies(field) ? el(id).value.trim() : "";
    return raw === "" || Number.isNaN(Number(raw)) ? null : Number(raw);
  };
  const setNumber = (at, value, make) =>
    patches.push(value === null
      ? { op: "remove", path: `${path}.${at}` }
      : { op: "set", path: `${path}.${at}`, value: make(value) });

  patches.push({ op: "set", path: `${path}.kind`, value: str(kind) });
  setNumber("interval_ms", number("f-widget-interval", "interval"), (v) => int(Math.round(v)));
  setOrRemove(patches, `${path}.duration`, text("f-widget-duration", "duration"));
  const onDone = document.querySelector('.action-picker[data-slot="on_done"]');
  if (onDone && applies("on_done")) {
    actionPatches(patches, `${path}.on_done`, pickerValue(onDone));
  } else {
    patches.push({ op: "remove", path: `${path}.on_done` });
  }
  // `text` is the default, and a default written out is one more line for
  // someone reading the file to wonder about.
  setOrRemove(patches, `${path}.view`, applies("view") && view !== "text" ? view : "");
  setOrRemove(patches, `${path}.title`, text("f-widget-title", "title"));
  setOrRemove(patches, `${path}.format`, text("f-widget-format", "format"));
  setOrRemove(patches, `${path}.timezone`, text("f-widget-timezone", "timezone"));
  setOrRemove(patches, `${path}.command`, text("f-widget-command", "command"));
  setOrRemove(patches, `${path}.source`, text("f-widget-source", "source"));
  setOrRemove(patches, `${path}.units`, text("f-widget-units", "units"));
  setNumber("latitude", number("f-widget-latitude", "location"), float);
  setOrRemove(patches, `${path}.place`, text("f-widget-place", "location"));
  setNumber("warn", number("f-widget-warn", "alarm"), float);
  setNumber("critical", number("f-widget-critical", "alarm"), float);
  setNumber("longitude", number("f-widget-longitude", "location"), float);
  setNumber("max", number("f-widget-max", "scale"), float);
  setNumber("history", number("f-widget-history", "scale"), (v) => int(Math.round(v)));
  setOrRemove(patches, `${path}.color`, text("f-widget-color", "color"));
  setOrRemove(patches, `${path}.background`, text("f-widget-bg", "background"));
  setOrRemove(patches, `${path}.image`, text("f-widget-image", "background"));
  // Only once it has been moved: an untouched slider at 100% is the
  // default, not a setting.
  const opacity = el("f-widget-opacity");
  // On the screen there is always a card to fade; on a key, only what
  // the widget brings.
  const hasBacking = ctx === "tile"
    || text("f-widget-bg", "background") || text("f-widget-image", "background");
  setNumber("opacity",
    opacity.dataset.touched && hasBacking ? Number(opacity.value) / 100 : null, float);
  setOrRemove(patches, `${path}.placeholder`, text("f-widget-placeholder", "has"));
}

/// Whether the animation fields say something other than `own`.
function animationChanged(own) {
  const kind = el("f-anim")?.value ?? "none";
  if (kind === "none" || !own) return (kind === "none") !== !own;
  return kind !== own.kind
    || Number(el("f-anim-period").value) !== own.period_ms
    || el("f-anim-to").value.toLowerCase() !== (own.to ?? "#ffffff").toLowerCase();
}

function collectAnimation(patches, base, isRing) {
  const kind = el("f-anim")?.value ?? "none";
  if (kind === "none") {
    patches.push({ op: "remove", path: `${base}.animation` });
    return;
  }
  patches.push({ op: "set", path: `${base}.animation.kind`, value: str(kind) });
  patches.push({
    op: "set",
    path: `${base}.animation.period_ms`,
    value: int(Number(el("f-anim-period").value) || 2000),
  });
  patches.push({
    op: "set",
    path: `${base}.animation.to`,
    value: str(el("f-anim-to").value),
  });
  void isRing;
}

async function apply(patches, file = state.layout.file) {
  const check = await call({ cmd: "validate_config", file, patches });
  const blocking = (check.diagnostics ?? []).filter((d) => d.severity === "error");
  showDiagnostics(check.diagnostics);
  if (blocking.length > 0) {
    toast(`${blocking.length} error(s); nothing saved`, true);
    return false;
  }
  const applied = await call({ cmd: "apply_config", file, patches });
  if (applied.result === "diagnostics") {
    showDiagnostics(applied.diagnostics);
    toast("refused; nothing saved", true);
    return false;
  }
  toast("saved");
  state.draft = null;
  await refresh();
  return true;
}

async function save() {
  try {
    const selected = state.selected;
    if (selected.kind === "encoder") {
      const info = state.layout.encoders.find((e) => e.encoder === selected.encoder);
      const { patches, file } = collectEncoderPatches(info, selected.encoder);
      await apply(patches, file);
      return;
    }
    const patches =
      selected.kind === "key"
        ? collectKeyPatches(state.layout.keys.find((k) => k.key === selected.key))
        : collectTilePatches(state.layout.lcd.find((t) => t.index === selected.index));
    await apply(patches);
  } catch (e) {
    toast(e.message, true);
  }
}

async function add() {
  try {
    const selected = state.selected;
    const page = state.layout.page_index;
    const patches =
      selected.kind === "key"
        ? [{ op: "append", path: `pages[${page}].keys`,
             fields: { key: int(selected.key), label: str(`Key ${selected.key}`) } }]
        : [{ op: "append", path: `pages[${page}].encoders`,
             fields: { encoder: int(selected.encoder) } }];
    await apply(patches);
  } catch (e) {
    toast(e.message, true);
  }
}

async function remove() {
  try {
    const selected = state.selected;
    const page = state.layout.page_index;
    if (selected.kind === "tile") {
      const removed = await apply([{ op: "remove_at", path: `pages[${page}].lcd`, index: selected.index }]);
      if (removed) select({ kind: "lcd" });
      return;
    }
    if (selected.kind === "encoder") {
      await removeEncoderLayer(
        state.layout.encoders.find((e) => e.encoder === selected.encoder), selected.encoder);
      return;
    }
    const patches =
      selected.kind === "key"
        ? [{ op: "remove_at", path: `pages[${page}].keys`,
             index: state.layout.keys.find((k) => k.key === selected.key).index }]
        : [{ op: "remove_at", path: `pages[${page}].encoders`,
             index: state.layout.encoders.find((e) => e.encoder === selected.encoder).index }];
    await apply(patches);
  } catch (e) {
    toast(e.message, true);
  }
}

async function clearBackground() {
  try {
    const info = state.layout.keys.find((k) => k.key === state.selected.key);
    await apply([{ op: "remove", path: `${keyPath(info.index)}.style.key_bg` }]);
  } catch (e) {
    toast(e.message, true);
  }
}

// ------------------------------------------------------------------- screen

/// The grid tiles sit on. From the daemon when it says, so the numbers are
/// not written down twice.
function lcdGrid() {
  return state.layout?.lcd_grid ?? { columns: 12, rows: 6, width: 720, height: 384 };
}

function tilePath(index) {
  return `pages[${state.layout.page_index}].lcd[${index}]`;
}

function describeTile(tile) {
  const view = tile.widget.view && tile.widget.view !== "text" ? ` ${tile.widget.view}` : "";
  return `${tile.widget.kind}${view}`;
}

function screenForm(layout) {
  const tiles = layout.lcd ?? [];
  const list = tiles.length === 0
    ? `<p class="empty">No widgets on this page's screen yet, so it shows the text below.</p>`
    : `<ul class="tile-list">${tiles
        .map((t) => `<li><button data-tile="${t.index}">
            <span>${escapeHtml(describeTile(t))}</span>
            <small>${escapeHtml(t.text ?? "")} · ${t.columns}×${t.rows} at ${t.column},${t.row}</small>
          </button></li>`)
        .join("")}</ul>`;
  return `
    <h2>Screen</h2>
    ${list}
    ${gridFields(layout)}
    <p class="hint">Drag a widget from the gallery onto the screen to add it, or click one there
      to put it in the first space it fits. Drag a widget on the screen to move it, and its
      corner to resize it.</p>
    <div class="field" ${tiles.length === 0 ? "" : "hidden"}>
      <label for="f-lcd-text">Text</label>
      <input id="f-lcd-text" value="${escapeHtml(layout.lcd_text ?? "")}" placeholder="${escapeHtml(layout.page)}">
      <span class="hint">Shown when the screen has no widgets. Empty shows the page's name.</span>
    </div>
    <div class="actions" ${tiles.length === 0 ? "" : "hidden"}>
      <button id="f-lcd-save" class="primary">Save</button>
    </div>`;
}

/// The layout grid's settings.
function gridFields(layout) {
  const grid = lcdGrid();
  const origin = layout.lcd_grid_origin;
  const level = origin === "profile" ? "profile" : "page";
  const option = (value, label) => `<option value="${value}" ${level === value ? "selected" : ""}>${label}</option>`;
  return `
    <fieldset>
      <legend>Grid</legend>
      <div class="grid4">
        <label>columns<input id="f-grid-columns" type="number" min="1" max="24" step="1" value="${grid.columns}"></label>
        <label>rows<input id="f-grid-rows" type="number" min="1" max="12" step="1" value="${grid.rows}"></label>
        <label class="span2">for
          <select id="f-grid-level">
            ${option("page", "this page")}
            ${option("profile", "every page in this profile")}
          </select>
        </label>
      </div>
      <span class="hint">${origin ? `Set on this ${origin}.` : "The default, 12 by 6."}
        Cells are ${Math.round(grid.width / grid.columns)}×${Math.round(grid.height / grid.rows)} pixels.
        Widgets here keep their place and proportion when it changes.</span>
      <div class="actions">
        <button id="f-grid-save">Apply</button>
        <button id="f-grid-reset" ${origin ? "" : "disabled"}>Back to 12×6</button>
      </div>
    </fieldset>`;
}

/// Change the grid, carrying this page's widgets across so each covers
/// about the same part of the screen on the new one.
async function saveGrid(columns, rows, level) {
  const page = state.layout.page_index;
  const old = lcdGrid();
  const patches = [];
  const at = (name) => (level === "page" ? `pages[${page}].${name}` : name);
  const other = (name) => (level === "page" ? name : `pages[${page}].${name}`);
  if (columns === null) {
    for (const name of ["lcd_columns", "lcd_rows"]) {
      patches.push({ op: "remove", path: `pages[${page}].${name}` }, { op: "remove", path: name });
    }
    columns = 12;
    rows = 6;
  } else {
    patches.push({ op: "set", path: at("lcd_columns"), value: int(columns) });
    patches.push({ op: "set", path: at("lcd_rows"), value: int(rows) });
    // Setting it for the profile clears this page's own, or it would go on
    // hiding the change on the page being looked at.
    if (level === "profile") {
      patches.push({ op: "remove", path: other("lcd_columns") }, { op: "remove", path: other("lcd_rows") });
    }
  }
  const scale = (value, from, to) => Math.round((value * to) / from);
  for (const tile of state.layout.lcd ?? []) {
    const column = Math.min(columns - 1, scale(tile.column, old.columns, columns));
    const row = Math.min(rows - 1, scale(tile.row, old.rows, rows));
    const width = Math.max(1, Math.min(columns - column, scale(tile.columns, old.columns, columns)));
    const height = Math.max(1, Math.min(rows - row, scale(tile.rows, old.rows, rows)));
    const base = tilePath(tile.index);
    patches.push(
      { op: "set", path: `${base}.column`, value: int(column) },
      { op: "set", path: `${base}.row`, value: int(row) },
      { op: "set", path: `${base}.columns`, value: int(width) },
      { op: "set", path: `${base}.rows`, value: int(height) },
    );
  }
  await apply(patches).catch((e) => toast(e.message, true));
}

function wireGrid() {
  el("f-grid-save")?.addEventListener("click", () => {
    const columns = Math.round(Number(el("f-grid-columns").value));
    const rows = Math.round(Number(el("f-grid-rows").value));
    if (!(columns >= 1 && columns <= 24 && rows >= 1 && rows <= 12)) {
      toast("a grid is 1 to 24 columns by 1 to 12 rows", true);
      return;
    }
    saveGrid(columns, rows, el("f-grid-level").value);
  });
  el("f-grid-reset")?.addEventListener("click", () => saveGrid(null, null, "page"));
}

function tileForm(info) {
  const grid = lcdGrid();
  const cell = (name, value, min, max) =>
    `<label>${name}<input id="f-tile-${name}" type="number" min="${min}" max="${max}" step="1" value="${value}"></label>`;
  return `
    <h2>Screen widget ${info.index + 1}</h2>
    ${info.text ? `<div class="field"><label>Showing</label>
       <span class="hint">${escapeHtml(info.text)}</span></div>` : ""}
    <div class="field">
      <label>Place</label>
      <div class="grid4">
        ${cell("column", info.column, 0, grid.columns - 1)}
        ${cell("row", info.row, 0, grid.rows - 1)}
        ${cell("columns", info.columns, 1, grid.columns)}
        ${cell("rows", info.rows, 1, grid.rows)}
      </div>
      <span class="hint">The screen is ${grid.columns} columns by ${grid.rows} rows, counted from 0.</span>
    </div>
    ${widgetFields(info.widget, "tile")}
    <div class="actions">
      <button id="f-save" class="primary">Save</button>
      <button id="f-remove" class="danger">Remove widget</button>
    </div>`;
}

function collectTilePatches(info) {
  const base = tilePath(info.index);
  const patches = [];
  for (const name of ["column", "row", "columns", "rows"]) {
    const value = Math.round(Number(el(`f-tile-${name}`).value));
    patches.push({ op: "set", path: `${base}.${name}`, value: int(value) });
  }
  collectWidget(patches, `${base}.widget`, "tile");
  return patches;
}

/// Patches that add a tile. Appended bare, then filled in by path, because
/// an append carries only scalars and the widget is a table of its own.
function newTilePatches(rect, widget) {
  const page = state.layout.page_index;
  const index = state.layout.lcd?.length ?? 0;
  // Only the required fields go in the append, whose fields come out in
  // alphabetical order; the spans are set after, so the file reads column,
  // row, columns, rows as a person would write it.
  const patches = [
    { op: "append", path: `pages[${page}].lcd`, fields: { column: int(rect.column), row: int(rect.row) } },
    { op: "set", path: `${tilePath(index)}.columns`, value: int(rect.columns) },
    { op: "set", path: `${tilePath(index)}.rows`, value: int(rect.rows) },
  ];
  // The first tile replaces the page's text, which would then sit in the
  // file doing nothing and draw a hint on every reload.
  if (index === 0 && state.layout.lcd_text) {
    patches.push({ op: "remove", path: `pages[${page}].lcd_text` });
  }
  for (const [key, value] of Object.entries(widget)) {
    const typed = typeof value === "number"
      ? (Number.isInteger(value) ? int(value) : float(value))
      : str(value);
    patches.push({ op: "set", path: `${tilePath(index)}.widget.${key}`, value: typed });
  }
  return { patches, index };
}

async function addTile(rect, widget) {
  const { patches, index } = newTilePatches(rect, widget);
  // Selected before the refresh the save triggers, so the new tile opens in
  // the inspector ready to be adjusted.
  const previous = state.selected;
  state.selected = { kind: "tile", index };
  const saved = await apply(patches).catch((e) => (toast(e.message, true), false));
  if (!saved) select(previous ?? { kind: "lcd" });
}

function overlaps(a, b) {
  return a.column < b.column + b.columns && b.column < a.column + a.columns
    && a.row < b.row + b.rows && b.row < a.row + a.rows;
}

/// The first place a tile of this size fits without covering another,
/// scanning as a person reads: across, then down.
function findSpace(columns, rows, except = -1) {
  const grid = lcdGrid();
  const taken = (state.layout.lcd ?? []).filter((t) => t.index !== except);
  for (let row = 0; row + rows <= grid.rows; row++) {
    for (let column = 0; column + columns <= grid.columns; column++) {
      const rect = { column, row, columns, rows };
      if (!taken.some((t) => overlaps(rect, t))) return rect;
    }
  }
  return null;
}

async function saveScreenText() {
  const path = `pages[${state.layout.page_index}].lcd_text`;
  const patches = [];
  setOrRemove(patches, path, el("f-lcd-text").value.trim());
  await apply(patches).catch((e) => toast(e.message, true));
}

function placeTile(node, rect) {
  const grid = lcdGrid();
  node.style.left = `${(rect.column / grid.columns) * 100}%`;
  node.style.top = `${(rect.row / grid.rows) * 100}%`;
  node.style.width = `${(rect.columns / grid.columns) * 100}%`;
  node.style.height = `${(rect.rows / grid.rows) * 100}%`;
}

function renderLcdTiles() {
  const layer = el("lcd-tiles");
  const grid = lcdGrid();
  layer.style.setProperty("--columns", grid.columns);
  layer.style.setProperty("--rows", grid.rows);
  const editing = state.selected?.kind === "lcd" || state.selected?.kind === "tile";
  el("lcd-wrap").classList.toggle("selected", editing);
  layer.innerHTML = "";
  for (const tile of state.layout?.lcd ?? []) {
    const node = document.createElement("div");
    node.className = "tile";
    if (state.selected?.kind === "tile" && state.selected.index === tile.index) {
      node.classList.add("selected");
    }
    node.dataset.index = tile.index;
    placeTile(node, tile);
    node.innerHTML = `<span class="tile-name">${escapeHtml(describeTile(tile))}</span><div class="handle"></div>`;
    layer.append(node);
  }
}

/// Dragging on the screen preview: across empty cells to add a tile, on a
/// tile to move it, on its corner to resize it. A press that goes nowhere is
/// a click, which selects.
///
/// Wired once; the tiles are redrawn underneath it on every refresh, so the
/// handlers work from the layout rather than holding on to nodes.
function wireLcdEditor() {
  const layer = el("lcd-tiles");
  let drag = null;

  const cellAt = (event) => {
    const grid = lcdGrid();
    const box = layer.getBoundingClientRect();
    const clamp = (v, max) => Math.max(0, Math.min(max - 1, v));
    return {
      column: clamp(Math.floor(((event.clientX - box.left) / box.width) * grid.columns), grid.columns),
      row: clamp(Math.floor(((event.clientY - box.top) / box.height) * grid.rows), grid.rows),
    };
  };

  layer.addEventListener("pointerdown", (event) => {
    if (!state.layout || event.button !== 0) return;
    const editing = state.selected?.kind === "lcd" || state.selected?.kind === "tile";
    const tileNode = event.target.closest(".tile");
    const tile = tileNode && state.layout.lcd.find((t) => t.index === Number(tileNode.dataset.index));
    // Until the screen is being edited a press only selects, so a stray
    // click while looking at the deck cannot move anything.
    if (!editing) {
      select(tile ? { kind: "tile", index: tile.index } : { kind: "lcd" });
      return;
    }
    const start = cellAt(event);
    const mode = tile ? (event.target.classList.contains("handle") ? "resize" : "move") : "create";
    const origin = tile
      ? { column: tile.column, row: tile.row, columns: tile.columns, rows: tile.rows }
      : { ...start, columns: 1, rows: 1 };
    const ghost = document.createElement("div");
    ghost.className = "tile ghost";
    ghost.hidden = true;
    layer.append(ghost);
    drag = { mode, tile, start, origin, rect: origin, moved: false, ghost };
    layer.setPointerCapture(event.pointerId);
    event.preventDefault();
  });

  layer.addEventListener("pointermove", (event) => {
    if (!drag) return;
    const grid = lcdGrid();
    const at = cellAt(event);
    const dc = at.column - drag.start.column;
    const dr = at.row - drag.start.row;
    if (dc === 0 && dr === 0 && !drag.moved) return;
    drag.moved = true;
    const o = drag.origin;
    let rect;
    if (drag.mode === "create") {
      const column = Math.min(drag.start.column, at.column);
      const row = Math.min(drag.start.row, at.row);
      rect = {
        column, row,
        columns: Math.abs(at.column - drag.start.column) + 1,
        rows: Math.abs(at.row - drag.start.row) + 1,
      };
    } else if (drag.mode === "move") {
      rect = {
        ...o,
        column: Math.max(0, Math.min(grid.columns - o.columns, o.column + dc)),
        row: Math.max(0, Math.min(grid.rows - o.rows, o.row + dr)),
      };
    } else {
      rect = {
        ...o,
        columns: Math.max(1, Math.min(grid.columns - o.column, o.columns + dc)),
        rows: Math.max(1, Math.min(grid.rows - o.row, o.rows + dr)),
      };
    }
    drag.rect = rect;
    const others = (state.layout.lcd ?? []).filter((t) => t.index !== drag.tile?.index);
    drag.invalid = others.some((t) => overlaps(rect, t));
    drag.ghost.classList.toggle("invalid", drag.invalid);
    drag.ghost.hidden = false;
    placeTile(drag.ghost, rect);
  });

  const finish = async (event, cancelled) => {
    if (!drag) return;
    const { mode, tile, rect, moved, invalid, ghost } = drag;
    drag = null;
    ghost.remove();
    if (layer.hasPointerCapture(event.pointerId)) layer.releasePointerCapture(event.pointerId);
    if (cancelled) return;
    if (!moved) {
      select(tile ? { kind: "tile", index: tile.index } : { kind: "lcd" });
      return;
    }
    if (invalid) {
      toast("widgets cannot overlap", true);
      return;
    }
    if (mode === "create") {
      await addTile(rect, { kind: "clock" });
      return;
    }
    const base = tilePath(tile.index);
    state.selected = { kind: "tile", index: tile.index };
    await apply(["column", "row", "columns", "rows"].map((name) =>
      ({ op: "set", path: `${base}.${name}`, value: int(rect[name]) })))
      .catch((e) => toast(e.message, true));
  };
  layer.addEventListener("pointerup", (event) => finish(event, false));
  layer.addEventListener("pointercancel", (event) => finish(event, true));
}

/// Keep the previews moving while widgets change them.
///
/// The daemon sends no event per repaint -- a clock would send one a second
/// forever -- so the page re-fetches the images on a timer instead, and only
/// while someone can see them.
function refreshPreviews() {
  const deckShowing = el("tab-deck").classList.contains("active");
  if (!deckShowing || document.visibilityState !== "visible" || !state.layout) return;
  const animated = state.layout.keys.some((k) => k.widget)
    || (state.layout.lcd ?? []).length > 0 || state.layout.background;
  if (!animated) return;
  previewVersion++;
  const tokenParam = encodeURIComponent(token);
  el("lcd").src = `/api/preview/lcd.jpg?v=${previewVersion}&token=${tokenParam}`;
  // Every key when a background moves behind them, else only the ones
  // with something changing on them.
  const behind = state.layout.background && state.layout.background.span !== "lcd";
  for (const img of document.querySelectorAll(".key img")) {
    const key = Number(img.closest(".key").dataset.key);
    if (!behind && !state.layout.keys.some((k) => k.key === key && k.widget)) continue;
    img.src = `/api/preview/key/${key}.jpg?v=${previewVersion}&token=${tokenParam}`;
  }
}

// ------------------------------------------------------------------ gallery

/// Every ready-made widget, sized for what it draws well at on the screen.
/// Each can go on the screen or on a key, but for the `keysOnly` ones:
/// a timer starts when its key is tapped, and the screen has no key.
const PRESETS = [
  { group: "Time", label: "Clock", note: "The time, large", size: [6, 2], widget: { kind: "clock" } },
  { group: "Time", label: "12-hour clock", note: "With AM and PM", size: [6, 2], widget: { kind: "clock", format: "%I:%M %p" } },
  { group: "Time", label: "Clock face", note: "Hands and ticks", size: [3, 3], widget: { kind: "clock", view: "analog", color: "#bf616a" } },
  { group: "Time", label: "Nixie clock", note: "Glowing tubes, to the second", size: [8, 2], widget: { kind: "clock", view: "nixie" } },
  { group: "Time", label: "World clock", note: "Another time zone", size: [4, 2], widget: { kind: "clock", timezone: "Asia/Tokyo", title: "Tokyo" } },
  { group: "Time", label: "Date", note: "Day and date", size: [6, 2], widget: { kind: "date" } },
  { group: "Time", label: "Uptime", note: "Time since boot", size: [4, 2], widget: { kind: "uptime", title: "Up" } },
  { group: "Media & web", label: "Now playing", note: "Spotify or any MPRIS player", size: [12, 3], widget: { kind: "media", color: "@accent" } },
  { group: "Media & web", label: "Volume", note: "The default output", size: [3, 3], widget: { kind: "volume", view: "gauge", color: "#88c0d0" } },
  { group: "Media & web", label: "Weather", note: "Now and the next few days", size: [6, 2], widget: { kind: "weather" }, locate: true },
  { group: "Media & web", label: "Command", note: "Any script's output", size: [6, 2], widget: { kind: "command", command: "uptime -p | cut -d' ' -f2-" } },
  { group: "System", label: "CPU graph", note: "Recent CPU use", size: [4, 2], widget: { kind: "cpu", view: "graph", color: "#bf616a" } },
  { group: "System", label: "CPU dial", note: "CPU use as a gauge", size: [3, 3], widget: { kind: "cpu", view: "gauge", color: "#bf616a" } },
  { group: "System", label: "CPU temperature", note: "From the CPU's sensor; amber at 80°, red at 90°", size: [4, 2], widget: { kind: "temperature", view: "graph", title: "CPU", color: "#d08770", warn: 80, critical: 90 } },
  { group: "System", label: "GPU graph", note: "Recent GPU use", size: [4, 2], widget: { kind: "gpu", view: "graph", color: "#a3be8c" } },
  { group: "System", label: "Memory", note: "Memory in use", size: [4, 2], widget: { kind: "memory", view: "bar", color: "#b48ead" } },
  { group: "System", label: "Fan", note: "The first fan's speed", size: [4, 2], widget: { kind: "fan", view: "graph", color: "#8fbcbb" } },
  { group: "System", label: "Load", note: "One-minute load average", size: [4, 2], widget: { kind: "load", view: "graph", color: "#ebcb8b" } },
  { group: "System", label: "Network", note: "Throughput, down and up", size: [4, 2], widget: { kind: "network", view: "graph", color: "#88c0d0" } },
  { group: "System", label: "Disk space", note: "How full / is", size: [4, 2], widget: { kind: "disk", view: "bar", color: "#ebcb8b" } },
  { group: "System", label: "Battery", note: "Charge, as a dial; amber at 20%, red at 10%", size: [3, 3], widget: { kind: "battery", view: "gauge", color: "#a3be8c", warn: 20, critical: 10 } },
  // Started and stopped by tapping their key, so for keys only; the size
  // is only the preview's.
  { group: "Timers", label: "Pomodoro", note: "25 minutes; tap to start", size: [2, 2], keysOnly: true, widget: { kind: "timer", duration: "25m", title: "Pomodoro" } },
  { group: "Timers", label: "Tea", note: "4 minutes; tap to start", size: [2, 2], keysOnly: true, widget: { kind: "timer", duration: "4m", title: "Tea" } },
  { group: "Timers", label: "Stopwatch", note: "Tap to start, hold to reset", size: [2, 2], keysOnly: true, widget: { kind: "stopwatch" } },
];

/// A preset's size on the current grid. Presets are sized for the default
/// 12 by 6, and keep their proportion of the screen on any other.
function presetSize(preset) {
  const grid = lcdGrid();
  return [
    Math.max(1, Math.min(grid.columns, Math.round((preset.size[0] * grid.columns) / 12))),
    Math.max(1, Math.min(grid.rows, Math.round((preset.size[1] * grid.rows) / 6))),
  ];
}

/// A preset's fields, typed for the protocol.
function typedFields(widget) {
  return Object.fromEntries(Object.entries(widget).map(([key, value]) => [key,
    typeof value === "number" ? (Number.isInteger(value) ? int(value) : float(value)) : str(value)]));
}

/// Draw the gallery. Previews are rendered by the daemon, with the current
/// theme and made-up readings, so they look exactly like the real thing.
async function renderGallery() {
  const list = el("gallery-list");
  const grid = lcdGrid();
  // Redrawn when the theme or the grid changes, since both change the
  // previews.
  const theme = `${state.status?.profile ?? ""} ${grid.columns}x${grid.rows} ${state.catalog ? "c" : ""}`
    + ` ${mixerProblem() ?? ""}`;
  if (list.dataset.theme === theme) return;
  list.dataset.theme = theme;
  let group = null;
  list.innerHTML = PRESETS.map((preset, index) => {
    const [columns, rows] = presetSize(preset);
    const heading = preset.group !== group ? `<li class="gallery-group">${escapeHtml(preset.group)}</li>` : "";
    group = preset.group;
    const where = preset.keysOnly ? "keys only" : `${columns}×${rows}`;
    return `${heading}
    <li class="preset" data-preset="${index}"
        title="${preset.keysOnly ? "Drag onto a key" : "Drag onto the screen or a key"}">
      <img alt="" draggable="false">
      <strong>${escapeHtml(preset.label)}</strong>
      <small>${escapeHtml(preset.note)} · ${where}</small>
    </li>`;
  }).join("") + actionCards();
  const cell = [grid.width / grid.columns, grid.height / grid.rows];
  await Promise.all(PRESETS.map(async (preset, index) => {
    const img = list.querySelector(`[data-preset="${index}"] img`);
    try {
      const reply = await call({
        cmd: "render_widget",
        fields: typedFields(preset.widget),
        width: Math.round(presetSize(preset)[0] * cell[0]),
        height: Math.round(presetSize(preset)[1] * cell[1]),
      });
      img.src = reply.url;
    } catch {
      img.alt = "no preview";
    }
  }));
}

/// Things to do rather than show, a key or knob at a time. The knob presets
/// come from the daemon's catalog; the key actions are the ones people put
/// on a deck first.
const KEY_ACTIONS = [
  ["play_pause", "Play / pause"], ["next_track", "Next track"], ["previous_track", "Previous track"],
  ["volume_mute", "Mute"], ["mic_mute", "Mic mute"], ["push_to_talk", "Push to talk"],
  ["volume_up", "Volume up"], ["volume_down", "Volume down"], ["next_page", "Next page"],
  ["previous_page", "Previous page"], ["home_page", "Home page"], ["next_profile", "Next profile"],
  ["next_output", "Next output"],
];

function actionCards() {
  if (!state.catalog) return "";
  const needs = (what) => (what && mixerProblem() ? `<small class="warn">needs PipeWire</small>` : "");
  const knobs = state.catalog.presets.map((p) => `
    <li class="preset action-card" data-knob="${escapeHtml(p.name)}" title="Drag onto a knob">
      <span class="glyph">⟳</span>
      <strong>${escapeHtml(p.title || p.name)}</strong>
      <small>${escapeHtml(p.label)}</small>
      ${needs(p.needs_mixer)}
    </li>`).join("");
  const keys = KEY_ACTIONS.map(([name, label]) => `
    <li class="preset action-card" data-action="${name}" title="Drag onto a key">
      <span class="glyph">⏺</span>
      <strong>${escapeHtml(label)}</strong>
      <small>${escapeHtml(builtInInfo(name)?.label ?? "")}</small>
      ${needs(builtInInfo(name)?.needs_mixer)}
    </li>`).join("");
  return `<li class="gallery-group">Knob presets</li>${knobs}<li class="gallery-group">Key actions</li>${keys}`;
}

/// Put a preset on a knob, for every page of this profile: a knob is set
/// once, like a keyboard's volume knob. Gestures, modes and a target
/// written beside the old preset at that level are cleared, so the new one
/// applies whole.
async function dropKnobPreset(encoder, name) {
  const info = state.layout.encoders.find((e) => e.encoder === encoder);
  const entry = layerEntry(info, "profile");
  const patches = [];
  let base = entry?.path;
  if (!base) {
    patches.push({ op: "append", path: "encoders", fields: { encoder: int(encoder) } });
    base = `encoders[${layerCount("profile")}]`;
  }
  patches.push({ op: "set", path: `${base}.preset`, value: str(name) });
  for (const slot of ["press", "cw", "ccw", "hold", "step", "modes", "target"]) {
    patches.push({ op: "remove", path: `${base}.${slot}` });
  }
  state.selected = { kind: "encoder", encoder };
  state.knobLayer = { ...(state.knobLayer ?? {}), [encoder]: "profile" };
  if (await apply(patches).catch((e) => (toast(e.message, true), false))) {
    const page = layerEntry(info, "page");
    const replaced = entry?.modes?.length ?? 0;
    if (page?.gestures?.cw || page?.preset || page?.modes?.length) {
      toast("set for the profile, but this page sets its own turn, which still wins here", true);
    } else {
      toast(`the ${encoder === 0 ? "left" : "right"} knob is now ${presetTitle(name)} on every page`
        + (replaced ? `, in place of its ${replaced} modes` : ""));
    }
  }
}

/// Put a built-in on a key's tap, adding the key if it is not configured.
async function dropKeyAction(key, name) {
  const info = state.layout.keys.find((k) => k.key === key);
  const page = state.layout.page_index;
  const label = KEY_ACTIONS.find(([n]) => n === name)?.[1] ?? name;
  const patches = [];
  let base;
  if (info) {
    base = keyPath(info.index);
    for (const field of ["exec", "page", "profile", "back"]) {
      patches.push({ op: "remove", path: `${base}.${field}` });
    }
  } else {
    base = `pages[${page}].keys[${state.layout.keys.length}]`;
    patches.push({ op: "append", path: `pages[${page}].keys`, fields: { key: int(key), label: str(label) } });
  }
  patches.push({ op: "set", path: `${base}.exec.action`, value: str(name) });
  state.selected = { kind: "key", key };
  await apply(patches).catch((e) => toast(e.message, true));
}

/// A preset's widget settings, asking the browser for a location first if
/// it needs one. `null` if it cannot be placed.
async function presetWidget(preset) {
  const widget = { ...preset.widget };
  if (preset.locate) {
    const coords = await locate();
    if (!coords) {
      toast("weather needs a location; allow it, or place another widget and switch it to weather", true);
      return null;
    }
    Object.assign(widget, coords);
  }
  return widget;
}

/// Put a preset on a key, replacing any widget it had. A key that is not
/// configured yet is added, labelled with the preset's name.
async function dropOnKey(key, preset) {
  const widget = await presetWidget(preset);
  if (!widget) return;
  const layout = state.layout;
  const page = layout.page_index;
  const info = layout.keys.find((k) => k.key === key);
  const patches = [];
  let base;
  // A timer starts when its key is tapped, which a tap bound to something
  // else would take over; that tap goes, and says so.
  const takesTap = TIMERS.has(widget.kind) && info
    && ((info.tap && info.tap.kind !== "implicit") || info.page || info.profile || info.back);
  if (info) {
    base = keyPath(info.index);
    patches.push({ op: "remove", path: `${base}.widget` });
    if (takesTap) {
      for (const field of ["exec", "page", "profile", "back"]) {
        patches.push({ op: "remove", path: `${base}.${field}` });
      }
    }
  } else {
    base = `pages[${page}].keys[${layout.keys.length}]`;
    patches.push({
      op: "append", path: `pages[${page}].keys`,
      fields: { key: int(key), label: str(preset.label) },
    });
  }
  for (const [name, value] of Object.entries(typedFields(widget))) {
    patches.push({ op: "set", path: `${base}.widget.${name}`, value });
  }
  const previous = state.selected;
  state.selected = { kind: "key", key };
  const saved = await apply(patches).catch((e) => (toast(e.message, true), false));
  if (!saved) select(previous ?? { kind: "key", key });
  else if (takesTap) toast("its tap now starts the timer, in place of what it did before");
}

/// Put a preset on the screen at `rect`.
async function dropOnScreen(rect, preset) {
  const widget = await presetWidget(preset);
  if (widget) await addTile(rect, widget);
}

/// Where a preset being dragged would land: a rectangle of screen cells,
/// centred on the pointer; a key; or nowhere. The screen refuses a timer,
/// which shows as a drop that cannot land.
function dropTargetAt(x, y, preset) {
  const under = document.elementFromPoint(x, y);
  const key = under?.closest(".key");
  if (key) return { type: "key", key: Number(key.dataset.key) };
  if (!under?.closest("#lcd-wrap")) return null;
  const grid = lcdGrid();
  const box = el("lcd-tiles").getBoundingClientRect();
  const [columns, rows] = presetSize(preset);
  const column = Math.floor(((x - box.left) / box.width) * grid.columns - columns / 2 + 0.5);
  const row = Math.floor(((y - box.top) / box.height) * grid.rows - rows / 2 + 0.5);
  const rect = {
    column: Math.max(0, Math.min(grid.columns - columns, column)),
    row: Math.max(0, Math.min(grid.rows - rows, row)),
    columns,
    rows,
  };
  const refused = !!preset.keysOnly;
  const invalid = refused || (state.layout?.lcd ?? []).some((t) => overlaps(rect, t));
  return { type: "screen", rect, invalid, refused };
}

/// Dragging a preset out of the gallery. A press that does not move adds
/// it to the first free space on the screen instead, or for a timer, says
/// to drag it onto a key.
function wireGallery() {
  const list = el("gallery-list");
  let drag = null;

  const clear = () => {
    document.querySelector("#lcd-tiles .tile.ghost")?.remove();
    el("lcd-wrap").classList.remove("dropping", "refusing");
    for (const node of document.querySelectorAll(".drop-target")) node.classList.remove("drop-target");
  };

  list.addEventListener("pointerdown", (event) => {
    const card = event.target.closest(".preset");
    if (!card || event.button !== 0 || !state.layout) return;
    const preset = card.dataset.preset !== undefined ? PRESETS[Number(card.dataset.preset)] : null;
    const knob = card.dataset.knob ?? null;
    const action = card.dataset.action ?? null;
    let ghost;
    if (preset) {
      ghost = document.createElement("img");
      ghost.src = card.querySelector("img").src;
    } else {
      ghost = document.createElement("div");
      ghost.textContent = card.querySelector("strong").textContent;
      ghost.classList.add("text-ghost");
    }
    ghost.classList.add("drag-ghost");
    ghost.hidden = true;
    document.body.append(ghost);
    drag = { preset, knob, action, ghost, x: event.clientX, y: event.clientY, moved: false, target: null };
    list.setPointerCapture(event.pointerId);
    event.preventDefault();
  });

  list.addEventListener("pointermove", (event) => {
    if (!drag) return;
    if (!drag.moved && Math.hypot(event.clientX - drag.x, event.clientY - drag.y) < 5) return;
    drag.moved = true;
    drag.ghost.hidden = false;
    drag.ghost.style.left = `${event.clientX}px`;
    drag.ghost.style.top = `${event.clientY}px`;
    clear();
    if (drag.preset) {
      drag.target = dropTargetAt(event.clientX, event.clientY, drag.preset);
    } else {
      // A knob preset lands on a knob, a key action on a key; nothing else.
      const under = document.elementFromPoint(event.clientX, event.clientY);
      const knob = drag.knob && under?.closest(".encoder");
      const key = drag.action && under?.closest(".key");
      drag.target = knob ? { type: "encoder", encoder: Number(knob.dataset.encoder) }
        : key ? { type: "key", key: Number(key.dataset.key) } : null;
    }
    if (drag.target?.type === "encoder") {
      document.querySelector(`.encoder[data-encoder="${drag.target.encoder}"]`)?.classList.add("drop-target");
    } else if (drag.target?.type === "key") {
      document.querySelector(`.key[data-key="${drag.target.key}"]`)?.classList.add("drop-target");
    } else if (drag.target?.type === "screen") {
      el("lcd-wrap").classList.add(drag.target.refused ? "refusing" : "dropping");
      const node = document.createElement("div");
      node.className = "tile ghost";
      node.classList.toggle("invalid", drag.target.invalid);
      placeTile(node, drag.target.rect);
      el("lcd-tiles").append(node);
    }
  });

  const finish = async (event, cancelled) => {
    if (!drag) return;
    const { preset, knob, action, ghost, moved, target } = drag;
    drag = null;
    ghost.remove();
    clear();
    if (list.hasPointerCapture(event.pointerId)) list.releasePointerCapture(event.pointerId);
    if (cancelled) return;
    if (knob || action) {
      if (!moved) toast(`drag it onto ${knob ? "a knob" : "a key"}`);
      else if (target?.type === "encoder") await dropKnobPreset(target.encoder, knob);
      else if (target?.type === "key") await dropKeyAction(target.key, action);
      return;
    }
    if (!moved && preset.keysOnly) {
      toast("drag it onto a key: tapping the key starts it");
    } else if (!moved) {
      const [columns, rows] = presetSize(preset);
      const rect = findSpace(columns, rows);
      if (!rect) {
        toast(`no free ${columns}×${rows} space on the screen; drag it onto a key, or make room`, true);
        return;
      }
      await dropOnScreen(rect, preset);
    } else if (target?.type === "key") {
      await dropOnKey(target.key, preset);
    } else if (target?.type === "screen") {
      if (target.refused) toast("the screen has no key to tap to start it: drag it onto a key", true);
      else if (target.invalid) toast("widgets cannot overlap", true);
      else await dropOnScreen(target.rect, preset);
    }
  };
  list.addEventListener("pointerup", (event) => finish(event, false));
  list.addEventListener("pointercancel", (event) => finish(event, true));
}

// --------------------------------------------------------------- background

const MOTIONS = [
  ["aurora", "aurora — soft curtains of light"],
  ["gradient", "gradient — slowly turning"],
  ["waves", "waves — rolling past"],
  ["plasma", "plasma — gently swirling"],
  ["starfield", "starfield — drifting stars"],
  ["rain", "rain — falling columns of light"],
  ["fire", "fire — flames from below"],
  ["bubbles", "bubbles — rising and wobbling"],
];

function backgroundForm(layout) {
  const bg = layout.background;
  const inherited = bg && bg.origin === "theme";
  const kind = !bg ? "none" : bg.image ? "image" : "animation";
  const span = bg?.span ?? "both";
  const option = (value, label, current) =>
    `<option value="${value}" ${current === value ? "selected" : ""}>${label}</option>`;
  const colours = [0, 1, 2, 3].map((i) => {
    const set = bg?.colors?.[i];
    const hex = bg?.colors_hex?.[i] ?? "#88c0d0";
    return `<input type="color" data-picks="f-bg-colour-${i}" value="${escapeHtml(hex)}">
            <input id="f-bg-colour-${i}" value="${escapeHtml(set ?? "")}" placeholder="theme">`;
  }).join("");
  const origin = !bg
    ? `<p class="empty">This page has no background.</p>`
    : `<p class="hint">Showing the background set on ${bg.origin === "theme"
        ? `the theme <code>${escapeHtml(bg.theme ?? "")}</code>. Saving here replaces it`
        : `this ${bg.origin}`}.</p>`;
  return `
    <h2>Background</h2>
    ${origin}
    <div class="field">
      <label for="f-bg-level">Set on</label>
      <select id="f-bg-level">
        ${option("page", "this page", bg?.origin === "profile" ? "profile" : "page")}
        ${option("profile", "every page in this profile", bg?.origin === "profile" ? "profile" : "page")}
      </select>
    </div>
    <div class="field">
      <label for="f-bg-span">Behind</label>
      <select id="f-bg-span">
        ${option("both", "the screen and the keys, as one picture", span)}
        ${option("lcd", "the screen", span)}
        ${option("keys", "the keys, as one picture", span)}
      </select>
    </div>
    <div class="field">
      <label for="f-bg-kind">Shows</label>
      <select id="f-bg-kind">
        ${option("none", "nothing", inherited ? "none" : kind)}
        ${option("image", "a picture or an animated GIF", inherited ? "none" : kind)}
        ${option("animation", "an animation", inherited ? "none" : kind)}
      </select>
    </div>
    <div data-bg="image" ${kind === "image" && !inherited ? "" : "hidden"}>
      ${pictureField('id="f-bg-image"', "Picture", bg?.image,
        "PNG, JPEG or GIF. A GIF plays. Scaled to cover, so the middle of it always shows.")}
    </div>
    <div data-bg="animation" ${kind === "animation" && !inherited ? "" : "hidden"}>
      <div class="field">
        <label for="f-bg-animation">Animation</label>
        <select id="f-bg-animation">
          ${MOTIONS.map(([value, label]) => option(value, label, bg?.animation ?? "aurora")).join("")}
        </select>
      </div>
      <div class="field">
        <label>Colours</label>
        <div class="colours">${colours}</div>
        <span class="hint">Empty ones come from the theme. The last is the darkest, behind the rest.</span>
      </div>
      <div class="field">
        <label for="f-bg-speed">Speed <output id="f-bg-speed-value">${bg?.speed ?? 1}×</output></label>
        <input id="f-bg-speed" type="range" min="0.2" max="3" step="0.1" value="${bg?.speed ?? 1}">
      </div>
    </div>
    <div data-bg="any" ${kind === "none" || inherited ? "hidden" : ""}>
      <div class="field" data-bg="moving">
        <label for="f-bg-fps">Frames per second <output id="f-bg-fps-value">${bg?.fps ?? 10}</output></label>
        <input id="f-bg-fps" type="range" min="1" max="20" step="1" value="${bg?.fps ?? 10}">
        <span class="hint">Each frame is a picture sent to every key it covers and the screen.
          Ten is smooth; lower is kinder to the USB link and the CPU.</span>
      </div>
      <div class="field">
        <label for="f-bg-dim">Darken <output id="f-bg-dim-value">${Math.round((bg?.dim ?? 0.25) * 100)}%</output></label>
        <input id="f-bg-dim" type="range" min="0" max="90" step="5" value="${Math.round((bg?.dim ?? 0.25) * 100)}">
        <span class="hint">So labels and widgets stay readable over it.</span>
      </div>
    </div>
    <div class="actions">
      <button id="f-bg-save" class="primary">Save</button>
    </div>`;
}

function wireBackground() {
  const kind = el("f-bg-kind");
  if (!kind) return;
  const update = () => {
    for (const part of document.querySelectorAll("[data-bg]")) {
      part.hidden = part.dataset.bg === "any" ? kind.value === "none"
        : part.dataset.bg === "moving" ? !backgroundMoves()
        : part.dataset.bg !== kind.value;
    }
  };
  kind.addEventListener("change", update);
  el("f-bg-image").addEventListener("input", update);
  update();
  for (const [id, format] of [
    ["f-bg-speed", (v) => `${v}×`], ["f-bg-fps", (v) => v], ["f-bg-dim", (v) => `${v}%`],
  ]) {
    const input = document.getElementById(id);
    input.addEventListener("input", () =>
      (document.getElementById(`${id}-value`).textContent = format(input.value)));
  }
  el("f-bg-save").addEventListener("click", saveBackground);
}

/// Whether the background being edited moves: an animation, or a GIF. A
/// frame rate means nothing to a still picture.
function backgroundMoves() {
  const kind = el("f-bg-kind").value;
  return kind === "animation" || (kind === "image" && /\.gif$/i.test(el("f-bg-image").value.trim()));
}

async function saveBackground() {
  const page = state.layout.page_index;
  const level = el("f-bg-level").value;
  const path = level === "page" ? `pages[${page}].background` : "background";
  // The other level is cleared, so there is one background to reason about
  // rather than a page one silently hiding a profile one.
  const other = level === "page" ? "background" : `pages[${page}].background`;
  const kind = el("f-bg-kind").value;
  const patches = [{ op: "remove", path }, { op: "remove", path: other }];
  if (kind !== "none") {
    const set = (name, value) => patches.push({ op: "set", path: `${path}.${name}`, value });
    set("span", str(el("f-bg-span").value));
    if (kind === "image") {
      const image = el("f-bg-image").value.trim();
      if (!image) {
        toast("choose a picture first", true);
        return;
      }
      set("image", str(image));
    } else {
      set("animation", str(el("f-bg-animation").value));
      const colours = [0, 1, 2, 3]
        .map((i) => document.getElementById(`f-bg-colour-${i}`).value.trim())
        .filter(Boolean);
      if (colours.length === 1) {
        toast("an animation needs two colours or more, or none to use the theme's", true);
        return;
      }
      if (colours.length) set("colors", { type: "array", value: colours.map(str) });
      set("speed", float(Number(el("f-bg-speed").value)));
    }
    if (backgroundMoves()) set("fps", int(Number(el("f-bg-fps").value)));
    set("dim", float(Number(el("f-bg-dim").value) / 100));
  }
  await apply(patches).catch((e) => toast(e.message, true));
}

// -------------------------------------------------------------------- theme

function renderTheme() {
  const node = el("theme-editor");
  const themes = (state.config?.files ?? []).filter((f) => f.name.startsWith("themes/"));
  if (themes.length === 0) {
    node.innerHTML = `<p class="empty">No themes yet. Add one under <code>themes/</code>.</p>`;
    return;
  }
  node.innerHTML = themes
    .map((file) => {
      const swatches = [...file.text.matchAll(/^\s*([A-Za-z0-9_-]+)\s*=\s*"(#[0-9a-fA-F]{6}|@[A-Za-z0-9_-]+)"/gm)]
        .map(([, name, value]) => {
          const colour = value.startsWith("@") ? "transparent" : value;
          return `<div class="swatch"><span class="dot" data-colour="${escapeHtml(colour)}"></span>
            <span>${escapeHtml(name)}</span><code>${escapeHtml(value)}</code></div>`;
        })
        .join("");
      return `<h2>${escapeHtml(file.name)}</h2><div class="swatches">${swatches}</div>`;
    })
    .join("");
  paintColours(node);
}

// -------------------------------------------------------------- calibration

/// Colours for the diagram. Deliberately the wizard's own: red is panel the
/// keycap is supposed to cover, so seeing it here means the same thing it
/// means on the hardware.
const CAL_ZONE = "#00e65a";
/// Amber, matching the wizard's own marking for a zone that was measured on
/// its own rather than derived from the grid.
const CAL_OVERRIDE = "#ebcb8b";
const CAL_SCREEN = "#88c0d0";
const CAL_BOUNDS = "#5a6172";
/// What a key image reaches. Deliberately not a zone colour: it is a
/// limitation being drawn, not geometry that was measured.
const CAL_COVER = "#bf8bd0";

function renderCalibration() {
  const cal = state.calibration;
  const badge = el("cal-source");
  if (!cal) {
    badge.innerHTML = `<p class="empty">This daemon does not report a calibration.
      It predates <code>get_calibration</code> \u2014 rebuild and restart it.</p>`;
    el("cal-diagram").replaceChildren();
    el("cal-legend").innerHTML = "";
    return;
  }

  // "file" is not a lesser state than "calibrated": the layout format does
  // not record how its numbers were arrived at, so everything that survives a
  // daemon restart reads back as a file. Only the template is a warning.
  const label = { template: "not calibrated", file: "calibrated", calibrated: "just calibrated" }[cal.source];
  const tone = cal.source === "template" ? "warn" : "ok";
  const problem = cal.problem
    ? `<p class="cal-problem">${escapeHtml(cal.problem)}</p>`
    : "";
  const uncalibrated =
    cal.source === "template"
      ? `<p class="cal-problem">These numbers are arithmetic, not measured on this
         unit. Run the wizard before trusting them.</p>`
      : "";
  badge.innerHTML = `
    <span class="chip ${tone}">${escapeHtml(label)}</span>
    ${cal.released ? `<span class="chip warn">device handed over</span>` : ""}
    <code class="cal-path">${escapeHtml(cal.path)}</code>
    ${problem}${uncalibrated}`;

  // Only refill the form when it is not being typed in, or every keystroke
  // would be overwritten by the SSE refresh it triggers.
  const form = el("cal-form");
  if (!form.contains(document.activeElement)) fillCalibrationForm(cal);

  drawCalibration(cal);
  el("cal-legend").innerHTML = calibrationSummary(cal) + `
    <ul class="cal-key">
      <li><i data-colour="${CAL_SCREEN}"></i>info screen</li>
      <li><i class="dashed" data-colour="${CAL_BOUNDS}"></i>grid boundary</li>
      <li><i data-colour="${CAL_ZONE}"></i>measured zone</li>
      <li><i data-colour="${CAL_OVERRIDE}"></i>zone measured on its own</li>
      <li><i class="dashed" data-colour="${CAL_COVER}"></i>key image reach (${cal.key_image_size}px, centring assumed)</li>
    </ul>`;
  paintColours(el("cal-legend"));
  el("cal-release").disabled = cal.released;
  el("cal-resume").disabled = !cal.released;
}

function fillCalibrationForm(cal) {
  const form = el("cal-form");
  const set = (name, value) => (form.elements[name].value = value);
  set("screen_x", cal.screen.x);
  set("screen_y", cal.screen.y);
  set("screen_width", cal.screen.width);
  set("screen_height", cal.screen.height);
  set("bounds_x", cal.bounds.x);
  set("bounds_y", cal.bounds.y);
  set("bounds_width", cal.bounds.width);
  set("bounds_height", cal.bounds.height);
  set("rows", cal.rows);
  set("columns", cal.columns);
  set("bleed_x", cal.bleed_x);
  set("bleed_y", cal.bleed_y);
}

function drawCalibration(cal) {
  const svg = el("cal-diagram");
  // A margin around the panel so strokes on the outermost edge are not
  // clipped in half by the viewBox.
  const pad = 12;
  svg.setAttribute(
    "viewBox",
    `${-pad} ${-pad} ${cal.panel_width + pad * 2} ${cal.panel_height + pad * 2}`);

  const box = (r, attrs) => {
    const pairs = Object.entries(attrs).map(([k, v]) => `${k}="${v}"`).join(" ");
    return `<rect x="${r.x}" y="${r.y}" width="${r.width}" height="${r.height}" ${pairs}/>`;
  };
  const label = (x, y, text, size, fill, weight = "normal") =>
    `<text x="${x}" y="${y}" fill="${fill}" font-size="${size}" font-weight="${weight}"
       text-anchor="middle" dominant-baseline="central"
       font-family="ui-monospace, monospace">${escapeHtml(text)}</text>`;

  const parts = [
    // The panel itself, so the zones are read against the thing they measure
    // rather than floating in space.
    box({ x: 0, y: 0, width: cal.panel_width, height: cal.panel_height },
        { fill: "#0a0c10", stroke: "#2c313c", "stroke-width": 2 }),
    box(cal.screen, { fill: "#141b24", stroke: CAL_SCREEN, "stroke-width": 3 }),
    label(cal.screen.x + cal.screen.width / 2, cal.screen.y + cal.screen.height / 2,
          `info screen  ${cal.screen.width}x${cal.screen.height}`, 34, CAL_SCREEN),
    // The grid boundary, dashed: it is a construction line, not a thing on
    // the panel. Every zone derives from it unless a band or an override
    // says otherwise.
    box(cal.bounds,
        { fill: "none", stroke: CAL_BOUNDS, "stroke-width": 2, "stroke-dasharray": "12 10" }),
  ];

  for (const zone of cal.zones) {
    const b = zone.bounds;
    const stroke = zone.overridden ? CAL_OVERRIDE : CAL_ZONE;
    parts.push(box(b, { fill: "#101a14", stroke, "stroke-width": 4 }));

    // What a key image can actually cover, drawn inside the measured zone.
    // The firmware blits this size and does not scale it, so the band left
    // between the two is panel the `02 07` path cannot reach -- the "boot
    // imagery around the edges" the probe was written to find. Centred is an
    // assumption; the report carries no placement, which is the whole reason
    // the size had to be measured by looking at the hardware.
    const k = cal.key_image_size;
    if (k < b.width || k < b.height) {
      const cover = {
        x: b.x + (b.width - k) / 2,
        y: b.y + (b.height - k) / 2,
        width: Math.min(k, b.width),
        height: Math.min(k, b.height),
      };
      parts.push(box(cover, {
        fill: "none",
        stroke: CAL_COVER,
        "stroke-width": 2,
        "stroke-dasharray": "8 6",
      }));
    }

    parts.push(
      label(b.x + b.width / 2, b.y + b.height / 2 - 16, String(zone.index), 46, "#e6e9ef", "bold"),
      label(b.x + b.width / 2, b.y + b.height / 2 + 26, `${b.width}x${b.height}`, 26, "#7d8799"));
  }

  svg.innerHTML = parts.join("");
}

/// A sentence about the calibration, above the legend.
///
/// Worth stating in words as well as pixels: "every zone is 176x176" is the
/// kind of thing that is obvious in a drawing only once you already know it.
function calibrationSummary(cal) {
  const sizes = new Set(cal.zones.map((z) => `${z.bounds.width}x${z.bounds.height}`));
  const overrides = cal.zones.filter((z) => z.overridden).length;
  const size = sizes.size === 1 ? [...sizes][0] : `${sizes.size} different sizes`;
  const shortfall = cal.zones.some(
    (z) => cal.key_image_size < z.bounds.width || cal.key_image_size < z.bounds.height);

  const bits = [`${cal.zones.length} zones, ${size}`];
  if (overrides > 0) bits.push(`${overrides} measured individually`);
  const note = shortfall
    ? `<p class="cal-note">A key image is ${cal.key_image_size}px and the firmware blits it
       without scaling, so the dashed inset is all the <code>02 07</code> path can reach.
       The gap around it is panel only the region path can fill.</p>`
    : "";
  return `<p class="cal-summary">${escapeHtml(bits.join(" \u00b7 "))}</p>${note}`;
}

function calibrationFromForm() {
  const f = el("cal-form").elements;
  const n = (name) => Number(f[name].value);
  return {
    cmd: "set_calibration",
    screen: { x: n("screen_x"), y: n("screen_y"), width: n("screen_width"), height: n("screen_height") },
    bounds: { x: n("bounds_x"), y: n("bounds_y"), width: n("bounds_width"), height: n("bounds_height") },
    rows: n("rows"),
    columns: n("columns"),
    bleed_x: n("bleed_x"),
    bleed_y: n("bleed_y"),
  };
}

function wireCalibration() {
  el("cal-save").addEventListener("click", async () => {
    try {
      await call(calibrationFromForm());
      await refreshCalibration();
      toast("calibration saved");
    } catch (e) {
      toast(e.message, true);
    }
  });

  el("cal-revert").addEventListener("click", async () => {
    try {
      await call({ cmd: "reload_calibration" });
      await refreshCalibration();
      fillCalibrationForm(state.calibration);
    } catch (e) {
      toast(e.message, true);
    }
  });

  // The daemon holds this request open until the hidraw handle is actually
  // closed, so the button is busy for as long as that takes and the wizard
  // can be started the moment it comes back.
  el("cal-release").addEventListener("click", async () => {
    try {
      await call({ cmd: "release_device" });
      toast("device handed over — run: galdeck calibrate");
    } catch (e) {
      toast(e.message, true);
    }
    await refresh();
  });

  // The probe answers a question the calibration cannot: the `02 07` key path
  // carries no geometry, so the only way to learn what size fills a physical
  // keycap is to draw one and look at it.
  el("cal-probe").addEventListener("click", async () => {
    const size = Number(el("cal-probe-size").value);
    try {
      await call({ cmd: "test_pattern", size });
      toast(`drew a ${size}x${size} pattern — border flush with the keycap means it fits`);
    } catch (e) {
      toast(e.message, true);
    }
  });

  // The other half of the same question: not how much of a key the firmware
  // can cover, but whether the region path can cover the rest.
  el("cal-probe-zones").addEventListener("click", async () => {
    try {
      await call({ cmd: "zone_pattern" });
      toast("filled every zone — borders flush with the keycaps means it works");
    } catch (e) {
      toast(e.message, true);
    }
  });

  el("cal-restore").addEventListener("click", async () => {
    try {
      await call({ cmd: "reload" });
      await refresh();
    } catch (e) {
      toast(e.message, true);
    }
  });

  el("cal-resume").addEventListener("click", async () => {
    try {
      await call({ cmd: "resume_device" });
    } catch (e) {
      toast(e.message, true);
    }
    await refresh();
  });
}

async function refreshCalibration() {
  state.calibration = await call({ cmd: "get_calibration" }).catch(() => null);
  renderCalibration();
}

function renderFiles() {
  const picker = el("file-picker");
  const files = state.config?.files ?? [];
  const current = picker.value || files[0]?.name;
  picker.innerHTML = files
    .map((f) => `<option value="${escapeHtml(f.name)}">${escapeHtml(f.name)}</option>`)
    .join("");
  picker.value = current ?? "";
  const file = files.find((f) => f.name === picker.value);
  el("file-text").value = file?.text ?? "";
}

// ------------------------------------------------------------------- wiring

async function refresh() {
  state.status = await call({ cmd: "status" });
  // What the daemon knows how to do; it does not change while it runs.
  state.catalog ??= await call({ cmd: "catalog" }).catch(() => null);
  state.layout = await call({ cmd: "get_layout" }).catch(() => null);
  state.config = await call({ cmd: "get_config" }).catch(() => null);
  state.calibration = await call({ cmd: "get_calibration" }).catch(() => null);
  previewVersion++;
  renderStatus();
  renderDeck();
  renderGallery();
  renderTheme();
  renderCalibration();
  renderFiles();
  showDiagnostics(state.config?.diagnostics);
}

function flashKey(key) {
  const node = document.querySelector(`.key[data-key="${key}"]`);
  if (!node) return;
  node.classList.add("pressed");
  setTimeout(() => node.classList.remove("pressed"), 180);
}

function listen() {
  const events = new EventSource(`/api/events?token=${encodeURIComponent(token)}`);
  const reload = () => refresh().catch((e) => toast(e.message, true));
  for (const name of ["page_changed", "profile_changed", "config_changed",
                      "device_connected", "device_disconnected",
                      "device_released", "device_resumed", "calibration_changed"]) {
    events.addEventListener(name, reload);
  }
  events.addEventListener("brightness_changed", (e) => {
    const { percent } = JSON.parse(e.data);
    el("brightness").value = percent;
    el("brightness-value").textContent = percent;
  });
  events.addEventListener("key_pressed", (e) => flashKey(JSON.parse(e.data).key));
  // A knob held into another mode: its label and ring here follow, and so
  // does its form unless it is being edited.
  events.addEventListener("mode_changed", () => refreshKnobs().catch((e) => toast(e.message, true)));
  events.addEventListener("timer_done", (e) => {
    const { profile, page, key } = JSON.parse(e.data);
    const here = profile === state.layout?.profile && page === state.layout?.page;
    const info = here ? state.layout.keys.find((k) => k.key === key) : null;
    const name = info?.widget?.title || info?.label || `key ${key}`;
    toast(here ? `timer done: ${name}` : `timer done: ${name} on ${page}`);
  });
  // EventSource reconnects on its own; this only reports it.
  events.onerror = () => el("device").classList.add("disconnected");
}

/// Re-read the layout for the knobs alone, when only they changed.
async function refreshKnobs() {
  state.layout = await call({ cmd: "get_layout" });
  renderEncoders();
  if (state.selected?.kind === "encoder") renderInspector();
}

function init() {
  for (const button of document.querySelectorAll(".tabs button")) {
    button.addEventListener("click", () => {
      for (const other of document.querySelectorAll(".tabs button")) {
        other.classList.toggle("active", other === button);
      }
      for (const tab of document.querySelectorAll(".tab")) {
        tab.classList.toggle("active", tab.id === `tab-${button.dataset.tab}`);
      }
    });
  }

  el("profile").addEventListener("change", async (e) => {
    await call({ cmd: "switch_profile", name: e.target.value }).catch((err) => toast(err.message, true));
    await refresh();
  });
  el("page").addEventListener("change", async (e) => {
    await call({ cmd: "switch_page", name: e.target.value }).catch((err) => toast(err.message, true));
    await refresh();
  });

  const brightness = el("brightness");
  brightness.addEventListener("input", () => {
    el("brightness-value").textContent = brightness.value;
  });
  brightness.addEventListener("change", async () => {
    await call({ cmd: "set_brightness", percent: Number(brightness.value), device: "all" })
      .catch((e) => toast(e.message, true));
  });

  wireCalibration();
  wireLcdEditor();
  wireGallery();
  el("background-edit").addEventListener("click", () => select({ kind: "background" }));
  el("outputs-edit").addEventListener("click", () => select({ kind: "outputs" }));
  el("page-new").addEventListener("click", () => select({ kind: "page" }));
  for (const type of ["input", "change"]) {
    el("inspector").addEventListener(type, () => {
      if (state.selected) state.draft = JSON.stringify(state.selected);
    });
  }
  setInterval(refreshPreviews, 1500);
  el("file-picker").addEventListener("change", renderFiles);

  refresh().then(listen).catch((e) => {
    const message =
      e instanceof StaleToken
        ? `This page's access token is no longer valid — the daemon has been
           restarted since it was opened. Open the address it printed at
           startup, which looks like
           <code>http://127.0.0.1:${location.port}/?token=…</code>, or run
           <code>galdeck ui</code> to print it again.`
        : `Could not reach the daemon: ${escapeHtml(e.message)}`;
    document.body.insertAdjacentHTML("afterbegin", `<p class="boot-error">${message}</p>`);
  });
}

init();
