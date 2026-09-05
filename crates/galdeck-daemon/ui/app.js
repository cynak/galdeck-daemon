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
const state = { status: null, layout: null, config: null, selected: null };

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
    button.innerHTML = info
      ? `<img alt="key ${index}" src="/api/preview/key/${index}.jpg?v=${previewVersion}&token=${encodeURIComponent(token)}">`
      : "";
    button.insertAdjacentHTML("beforeend", `<span class="index">${index}</span>`);
    button.addEventListener("click", () => select({ kind: "key", key: index }));
    grid.append(button);
  }

  el("lcd").src = `/api/preview/lcd.jpg?v=${previewVersion}&token=${encodeURIComponent(token)}`;
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
    if (state.selected?.kind === "encoder" && state.selected.encoder === index) {
      node.classList.add("selected");
    }
    const vars = segments.map((c, i) => `--s${i}:${c}`).join(";");
    node.innerHTML = `<div class="ring" style="${vars}"></div>
      <small>${index === 0 ? "left" : "right"}${info ? "" : " · unset"}</small>`;
    node.addEventListener("click", () => select({ kind: "encoder", encoder: index }));
    wrap.append(node);
  }
}

function select(what) {
  state.selected = what;
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

  if (selected.kind === "key") {
    const info = layout.keys.find((k) => k.key === selected.key);
    node.innerHTML = info ? keyForm(info) : emptyKeyForm(selected.key);
  } else {
    const info = layout.encoders.find((e) => e.encoder === selected.encoder);
    node.innerHTML = info ? encoderForm(info) : emptyEncoderForm(selected.encoder);
  }
  wireInspector();
}

function actionOf(info) {
  if (info.profile) return "profile";
  if (info.page) return "page";
  if (info.back) return "back";
  if (info.exec) return "exec";
  return "none";
}

function keyForm(info) {
  const action = actionOf(info);
  const pages = state.layout.pages
    .map((p) => `<option ${p === info.page ? "selected" : ""}>${escapeHtml(p)}</option>`)
    .join("");
  const profiles = (state.status?.profiles ?? [])
    .map((p) => `<option ${p === info.profile ? "selected" : ""}>${escapeHtml(p)}</option>`)
    .join("");
  return `
    <h2>Key ${info.key}</h2>
    ${info.widget
      ? `<div class="field"><label>Showing</label>
           <span class="hint"><code>${escapeHtml(info.widget)}</code> widget →
           ${escapeHtml(info.text ?? "—")}</span></div>`
      : ""}
    <div class="field">
      <label for="f-label">Label</label>
      <input id="f-label" value="${escapeHtml(info.label ?? "")}">
      ${info.widget ? `<span class="hint">Shown until the widget produces text.</span>` : ""}
    </div>
    <div class="field">
      <label for="f-icon">Icon</label>
      <input id="f-icon" value="${escapeHtml(info.icon ?? "")}" placeholder="/path/to/icon.png">
    </div>
    <div class="field">
      <label for="f-action">Does what</label>
      <select id="f-action">
        <option value="none" ${action === "none" ? "selected" : ""}>nothing</option>
        <option value="exec" ${action === "exec" ? "selected" : ""}>run a command</option>
        <option value="page" ${action === "page" ? "selected" : ""}>switch page</option>
        <option value="profile" ${action === "profile" ? "selected" : ""}>switch profile</option>
        <option value="back" ${action === "back" ? "selected" : ""}>go back</option>
      </select>
    </div>
    <div class="field" data-for="exec" ${action === "exec" ? "" : "hidden"}>
      <label for="f-exec">Command</label>
      <input id="f-exec" value="${escapeHtml(info.exec ?? "")}">
      <span class="hint">Run with <code>sh -c</code>.</span>
    </div>
    <div class="field" data-for="page" ${action === "page" ? "" : "hidden"}>
      <label for="f-page">Page</label>
      <select id="f-page">${pages}</select>
    </div>
    <div class="field" data-for="profile" ${action === "profile" ? "" : "hidden"}>
      <label for="f-profile">Profile</label>
      <select id="f-profile">${profiles}</select>
    </div>
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
    <div class="actions">
      <button id="save" class="primary">Save</button>
      <button id="remove" class="danger">Remove key</button>
    </div>`;
}

function emptyKeyForm(key) {
  return `
    <h2>Key ${key}</h2>
    <p class="empty">Nothing is bound here.</p>
    <div class="actions"><button id="add">Add this key</button></div>`;
}

function encoderForm(info) {
  return `
    <h2>Encoder ${info.encoder} (${info.encoder === 0 ? "left" : "right"})</h2>
    <div class="field">
      <label for="f-press">Press</label>
      <input id="f-press" value="${escapeHtml(info.press ?? "")}">
    </div>
    <div class="field">
      <label for="f-cw">Clockwise</label>
      <input id="f-cw" value="${escapeHtml(info.cw ?? "")}">
    </div>
    <div class="field">
      <label for="f-ccw">Anticlockwise</label>
      <input id="f-ccw" value="${escapeHtml(info.ccw ?? "")}">
      <span class="hint"><code>GALDECK_DELTA</code> carries the step count.</span>
    </div>
    <div class="field">
      <label for="f-ring">Ring colour</label>
      <input id="f-ring" type="color" value="${escapeHtml(info.ring)}">
      <span class="hint">${info.ring_is_own
        ? "Set on this encoder."
        : "Inherited from the theme."}</span>
    </div>
    <div class="actions">
      <button id="save" class="primary">Save</button>
      <button id="remove" class="danger">Remove encoder</button>
    </div>`;
}

function emptyEncoderForm(encoder) {
  return `
    <h2>Encoder ${encoder}</h2>
    <p class="empty">Nothing is bound here.</p>
    <div class="actions"><button id="add">Add this encoder</button></div>`;
}

function wireInspector() {
  const action = el("f-action");
  if (action) {
    action.addEventListener("change", () => {
      for (const field of document.querySelectorAll("[data-for]")) {
        field.hidden = field.dataset.for !== action.value;
      }
    });
  }
  el("save")?.addEventListener("click", save);
  el("add")?.addEventListener("click", add);
  el("remove")?.addEventListener("click", remove);
  el("f-bg-clear")?.addEventListener("click", clearBackground);
}

// ------------------------------------------------------------------ editing

const str = (v) => ({ type: "string", value: v });
const int = (v) => ({ type: "integer", value: v });
const bool = (v) => ({ type: "boolean", value: v });

function keyPath(index) {
  return `pages[${state.layout.page_index}].keys[${index}]`;
}

function encoderPath(index) {
  return `pages[${state.layout.page_index}].encoders[${index}]`;
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

  // Exactly one action, so the others are cleared rather than left to argue.
  const action = el("f-action").value;
  setOrRemove(patches, `${base}.exec`, action === "exec" ? el("f-exec").value.trim() : "");
  setOrRemove(patches, `${base}.page`, action === "page" ? el("f-page").value : "");
  setOrRemove(patches, `${base}.profile`, action === "profile" ? el("f-profile").value : "");
  if (action === "back") {
    patches.push({ op: "set", path: `${base}.back`, value: bool(true) });
  } else {
    patches.push({ op: "remove", path: `${base}.back` });
  }

  const bg = el("f-bg").value;
  if (info.background_is_own || bg.toLowerCase() !== info.background.toLowerCase()) {
    patches.push({ op: "set", path: `${base}.style.key_bg`, value: str(bg) });
  }
  return patches;
}

function collectEncoderPatches(info) {
  const base = encoderPath(info.index);
  const patches = [];
  setOrRemove(patches, `${base}.press`, el("f-press").value.trim());
  setOrRemove(patches, `${base}.cw`, el("f-cw").value.trim());
  setOrRemove(patches, `${base}.ccw`, el("f-ccw").value.trim());
  const ring = el("f-ring").value;
  if (info.ring_is_own || ring.toLowerCase() !== info.ring.toLowerCase()) {
    patches.push({ op: "set", path: `${base}.style.ring`, value: str(ring) });
  }
  return patches;
}

async function apply(patches) {
  const file = state.layout.file;
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
  await refresh();
  return true;
}

async function save() {
  try {
    const selected = state.selected;
    const patches =
      selected.kind === "key"
        ? collectKeyPatches(state.layout.keys.find((k) => k.key === selected.key))
        : collectEncoderPatches(
            state.layout.encoders.find((e) => e.encoder === selected.encoder));
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
          return `<div class="swatch"><span class="dot" style="background:${escapeHtml(colour)}"></span>
            <span>${escapeHtml(name)}</span><code>${escapeHtml(value)}</code></div>`;
        })
        .join("");
      return `<h2>${escapeHtml(file.name)}</h2><div class="swatches">${swatches}</div>`;
    })
    .join("");
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
  state.layout = await call({ cmd: "get_layout" }).catch(() => null);
  state.config = await call({ cmd: "get_config" }).catch(() => null);
  previewVersion++;
  renderStatus();
  renderDeck();
  renderTheme();
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
                      "device_connected", "device_disconnected"]) {
    events.addEventListener(name, reload);
  }
  events.addEventListener("brightness_changed", (e) => {
    const { percent } = JSON.parse(e.data);
    el("brightness").value = percent;
    el("brightness-value").textContent = percent;
  });
  events.addEventListener("key_pressed", (e) => flashKey(JSON.parse(e.data).key));
  // EventSource reconnects on its own; this only reports it.
  events.onerror = () => el("device").classList.add("disconnected");
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
    await call({ cmd: "set_brightness", percent: Number(brightness.value) })
      .catch((e) => toast(e.message, true));
  });

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
    document.body.insertAdjacentHTML(
      "afterbegin",
      `<p style="padding:1rem 1.25rem;margin:0;color:#ebcb8b;border-bottom:1px solid #2c313c">${message}</p>`);
  });
}

init();
