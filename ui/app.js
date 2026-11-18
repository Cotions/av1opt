// av1opt web UI. Plain JS, no build step: the server embeds this file.
// Talks only to /api/* (JSON), /api/events (Server-Sent Events) and /media (file ranges).
"use strict";

// ---------- helpers ----------

function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else if (k === "text") el.textContent = v;
    else if (k === "style") Object.assign(el.style, v);
    else if (v === true) el.setAttribute(k, "");
    else el.setAttribute(k, v);
  }
  for (const kid of kids.flat(Infinity)) {
    if (kid == null || kid === false) continue;
    el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

const SVGNS = "http://www.w3.org/2000/svg";
function s(tag, attrs) {
  const el = document.createElementNS(SVGNS, tag);
  for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, v);
  return el;
}

async function api(path, body) {
  const opt = body === undefined ? {} : { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) };
  const r = await fetch(path, opt);
  let j = null;
  try { j = await r.json(); } catch { /* not JSON */ }
  if (!r.ok) throw new Error((j && j.error) || `${r.status} ${r.statusText}`);
  return j;
}

// ---------- numbers and names ----------
// Nothing from here to the end marker touches the page: tests/page_numbers.mjs runs this part alone.

function isNumber(x) { return x != null && isFinite(x); }
const BYTE_UNITS = [["B", 1, 0], ["kB", 1e3, 0], ["MB", 1e6, 1], ["GB", 1e9, 1], ["TB", 1e12, 2], ["PB", 1e15, 2]];
// The unit and the sign are chosen from the rounded number, so that 999.6 MB is "1.0 GB" and
// not "1000 MB", and half a byte below zero is "0 B".
function bytes(n) {
  if (!isNumber(n)) return "–";
  const size = Math.abs(n);
  for (const [k, [unit, scale, digits]] of BYTE_UNITS.entries()) {
    const v = size / scale;
    let text = v.toFixed(digits);
    if (Number(text) >= 100) text = v.toFixed(0);
    if (Number(text) < 1000 || k === BYTE_UNITS.length - 1) return (n < 0 && Number(text) !== 0 ? "−" : "") + text + " " + unit;
  }
}
function pct(x, digits = 0) {
  if (!isNumber(x)) return "–";
  const text = (x * 100).toFixed(digits);
  return (Number(text) === 0 ? text.replace("-", "") : text) + "%";
}
// For what must not look finished before it is: rounds down, and says 100% only when complete.
function floorPct(x, digits = 0, complete = x >= 1) {
  if (!isNumber(x)) return "–";
  if (complete) return (100).toFixed(digits) + "%";
  const scale = 10 ** digits;
  // The small addition keeps 0.29, which multiplies to just under 29, at 29.
  const steps = Math.min(Math.floor(x * 100 * scale + 1e-9), 100 * scale - 1);
  return (Math.max(steps, 0) / scale).toFixed(digits) + "%";
}
// What an encode saved, as a change in size: 0.12 is "−12%", and an output 12% larger than
// its source is "+12%". What rounds to nothing has no sign.
function sizeChange(x, digits = 0) {
  if (!isNumber(x)) return "–";
  const text = pct(Math.abs(x), digits);
  return (parseFloat(text) === 0 ? "" : x < 0 ? "+" : "−") + text;
}
// The same under a "Saved" heading: "12%", or "12% larger". Green is for what shrank.
function savedPct(x, digits = 0) {
  if (!isNumber(x)) return "–";
  const text = pct(Math.abs(x), digits);
  return x < 0 && parseFloat(text) !== 0 ? text + " larger" : text;
}
function num(x, d = 2) { return isNumber(x) ? Number(x).toFixed(d) : "–"; }
// A size or time ratio against the default as a change: 0.94 is "−6.0%".
function rel(x, d = 1) {
  if (!isNumber(x)) return "–";
  const v = (x - 1) * 100, t = Math.abs(v).toFixed(d);
  return (Number(t) === 0 ? "" : v > 0 ? "+" : "−") + t + "%";
}
// A ratio with nothing to compare against (a base of zero) has no value to show.
function times(x) {
  if (!isNumber(x)) return "–";
  const text = x.toFixed(2);
  return (Number(text) >= 10 ? x.toFixed(0) : text) + "×";
}
function plural(n, word) { return `${n} ${word}${n === 1 ? "" : "s"}`; }

// ---------- end of the part that runs alone ----------

function savedClass(x) { return x < 0 ? "warn" : "ok"; }
function hms(sec) {
  if (sec == null || isNaN(sec)) return "–";
  sec = Math.max(0, Math.round(sec));
  const hh = Math.floor(sec / 3600), mm = Math.floor(sec / 60) % 60, ss = sec % 60;
  return (hh ? hh + ":" + String(mm).padStart(2, "0") : mm) + ":" + String(ss).padStart(2, "0");
}
function span(sec) { // a length of time for people: minutes, hours, then days
  if (sec == null || isNaN(sec) || sec <= 0) return "–";
  if (sec < 60) return "<1 min";
  if (sec < 5400) return Math.round(sec / 60) + " min";
  if (sec < 172800) return (sec / 3600).toFixed(1) + " h";
  return (sec / 86400).toFixed(1) + " days";
}
function tc(sec) { // time with tenths, for the player
  if (sec == null || isNaN(sec)) return "–";
  const t = Math.max(0, sec);
  return hms(Math.floor(t)) + "." + Math.floor((t % 1) * 10);
}
function basename(p) { return String(p || "").split("/").pop(); }
function dirname(p) { const a = String(p || "").split("/"); a.pop(); return a.join("/") || "/"; }
function when(t) { return t ? new Date(t * 1000).toLocaleString() : "–"; }

let toastTimer;
function toast(msg, err) {
  const t = document.getElementById("toast");
  t.textContent = msg;
  t.className = "toast" + (err ? " err" : "");
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), err ? 7000 : 3000);
}

// The numbers in `fields` ([input, label, whole?] each), or null after marking the first that
// is empty or no number and saying which it is. Number("") is 0, which a field left empty must
// not be sent as, and a number input whose text is no number reads as empty. Whether a number
// is in range is the server's to say, with the same names.
function readNumbers(fields) {
  const numbers = [];
  for (const [input, label, whole] of fields) {
    const text = input.value.trim();
    const n = text === "" ? NaN : Number(text);
    if (!Number.isFinite(n) || (whole && !Number.isInteger(n))) {
      input.classList.add("invalid");
      input.addEventListener("input", () => input.classList.remove("invalid"), { once: true });
      input.focus();
      toast(`${label}: enter ${whole ? "a whole number" : "a number"}`, true);
      return null;
    }
    input.classList.remove("invalid");
    numbers.push(n);
  }
  return numbers;
}

// Keeps the keyboard in an open dialog: focus moves into it (to `first`, else to the dialog
// itself), Tab stays inside, and Escape calls `onEscape`. Only the dialog on top listens.
// Returns what to call when the dialog closes: focus goes back to where it was.
function holdFocus(bg, onEscape, first) {
  const opener = document.activeElement;
  const box = bg.firstElementChild;
  box.setAttribute("tabindex", "-1");
  const stops = () => [...box.querySelectorAll("a[href], button, input, select, textarea, summary, [tabindex='0']")].filter((el) => !el.disabled && el.offsetParent !== null);
  const onKey = (e) => {
    const open = document.querySelectorAll(".modal-bg");
    if (open[open.length - 1] !== bg) return;
    if (e.key === "Escape") { onEscape(); return; }
    if (e.key !== "Tab") return;
    const f = stops(), cur = document.activeElement;
    if (!f.length) { e.preventDefault(); box.focus(); return; }
    const from = e.shiftKey ? f[0] : f[f.length - 1], to = e.shiftKey ? f[f.length - 1] : f[0];
    if (cur === from || !box.contains(cur) || (e.shiftKey && cur === box)) { e.preventDefault(); to.focus(); }
  };
  document.addEventListener("keydown", onKey);
  (first || box).focus();
  let released = false;
  return () => {
    if (released) return;
    released = true;
    document.removeEventListener("keydown", onKey);
    if (opener && opener.isConnected && opener !== document.body) opener.focus();
  };
}

// The extensions the server takes for videos.
const VIDEO_EXTS = ["mp4", "mkv", "m4v", "mov", "webm", "avi", "wmv", "flv", "ts", "m2ts", "mpg", "mpeg", "3gp"];
// Whether `p` is a video file and not a folder. `files` is the set of paths known to be
// files, from whoever listed them; without it the name has to do.
function isVideoFile(p, files) {
  if (files) return files.has(p);
  const m = /\.([A-Za-z0-9]+)$/.exec(basename(p));
  return !!m && VIDEO_EXTS.includes(m[1].toLowerCase());
}

const prefs = {
  get(k, d) { try { const v = localStorage.getItem("av1opt." + k); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { localStorage.setItem("av1opt." + k, JSON.stringify(v)); },
};

// ---------- live events ----------

const listeners = new Set();
function onEvent(fn) { listeners.add(fn); return () => listeners.delete(fn); }

function connect() {
  const es = new EventSource("/api/events");
  const dot = document.getElementById("conn");
  // Events sent while the connection was down are lost: views are told when it is back.
  let dropped = false;
  es.onopen = () => {
    dot.classList.add("on"); dot.title = "Live updates connected";
    if (dropped) { dropped = false; for (const fn of [...listeners]) fn({ kind: "reopened" }); }
  };
  es.onerror = () => { dot.classList.remove("on"); dot.title = "Reconnecting…"; dropped = true; };
  es.onmessage = (m) => {
    let msg;
    try { msg = JSON.parse(m.data); } catch { return; }
    for (const fn of [...listeners]) fn(msg);
  };
}

// Queue badge: number of queued + running jobs.
async function refreshBadge() {
  try {
    const { jobs, queue } = await api("/api/jobs/heads");
    const n = jobs.filter((j) => j.status === "queued" || j.status === "running").length;
    const b = document.getElementById("queue-badge");
    b.textContent = n;
    b.hidden = n === 0;
    // Amber while the queue is paused or outside its hours.
    b.classList.toggle("held", !!queue.hold);
    b.title = !queue.hold ? "" : queue.hold.why === "paused" ? "Paused" : "Outside encoding hours";
  } catch { /* offline */ }
}
onEvent((m) => { if (m.kind === "jobs" || m.kind === "reopened") refreshBadge(); });
// Desktop app: tell the user when a job ends while the window is in the background.
onEvent((m) => {
  if (m.kind !== "job_done" || !desktop) return;
  const j = m.job, c = j.counts || {};
  const parts = [];
  if (c.ok) parts.push(`${c.ok} encoded`);
  if (c.skip) parts.push(`${c.skip} skipped`);
  if (c.error) parts.push(`${c.error} failed`);
  if (j.saved) parts.push(`${bytes(j.saved)} saved`);
  const title = j.status === "done" ? "Encode finished" : j.status === "failed" ? "Encode failed" : "Encode cancelled";
  desktop.notify(title, parts.join(" · ") || j.error || "");
});

// ---------- state ----------

let STATE = null;
async function loadState() { STATE = await api("/api/state"); renderBanner(); return STATE; }

// Shown on every view while encodes cannot run (usually: ffmpeg without libvmaf), and for what
// went wrong at start (a settings file or saved queue that could not be read) until dismissed.
const noticesRead = new Set();
function renderBanner() {
  const b = document.getElementById("banner");
  const p = STATE && STATE.tools && STATE.tools.problem;
  const notices = ((STATE && STATE.notices) || []).filter((n) => !noticesRead.has(n));
  b.hidden = !p && !notices.length;
  b.replaceChildren(
    ...(p ? [h("div", {}, "Encoding is off: " + p + ".", h("a", { href: "#/settings" }, "Settings"))] : []),
    ...notices.map((n) => h("div", {}, n + " ", h("button", { class: "small", onclick: () => { noticesRead.add(n); renderBanner(); } }, "Dismiss"))));
}

// ---------- folder / file picker ----------

// Resolves to an array of absolute paths, or null if cancelled.
// mode: "dir" picks one folder; "multi" picks any mix of folders and video files, and the
// array then says which of them are files in its `files` (a Set).
// In the desktop app, folders come from the system's own dialog. Files and folders mixed
// ("multi") stay with the in-page picker, which can select both at once.
const desktop = window.av1optDesktop || null;

function pick({ title, mode = "dir", start }) {
  if (desktop && mode === "dir") {
    return desktop.pickFolder(title, start || prefs.get("pickerDir", null) || (STATE && STATE.home) || "").then((p) => {
      if (!p) return null;
      prefs.set("pickerDir", p);
      return [p];
    }).catch(() => null);
  }
  return new Promise((resolve) => {
    const selected = new Set();
    const files = new Set();
    let cur = start || prefs.get("pickerDir", null) || (STATE && STATE.home) || "/";
    let hidden = false;
    const pathEl = h("div", { class: "picker-path" });
    const list = h("ul", { class: "picker-list" });
    const places = h("div", { class: "places" });
    const selInfo = h("span", { class: "dim grow" });
    const close = (v) => { bg.remove(); release(); resolve(v); };
    const choose = h("button", { class: "primary", onclick: () => {
      prefs.set("pickerDir", cur);
      if (mode === "dir") { close([cur]); return; }
      const out = selected.size ? [...selected] : [cur];
      out.files = new Set(out.filter((p) => files.has(p)));
      close(out);
    } }, mode === "dir" ? "Use this folder" : "Add selected");

    const updateSel = () => {
      if (mode === "dir") selInfo.textContent = "";
      else selInfo.textContent = selected.size ? `${selected.size} selected` : "nothing selected: adds the current folder";
    };

    // The folder it opens on may be gone by now: home is listed instead (an empty path).
    async function go(p, opening) {
      let r;
      try { r = await api("/api/browse?path=" + encodeURIComponent(p) + (hidden ? "&hidden=1" : "")); }
      catch (e) { if (opening) return go(""); toast(e.message, true); return; }
      cur = r.path;
      pathEl.textContent = r.path;
      places.replaceChildren(...r.places.map((pl) => h("button", { class: "small", onclick: () => go(pl.path) }, pl.name)));
      const join = (n) => (r.path === "/" ? "/" : r.path + "/") + n;
      const items = [];
      // A row is reached with Tab and acts on Enter or space, as on a click.
      const keyed = (li) => {
        li.tabIndex = 0;
        li.addEventListener("keydown", (e) => {
          if (e.target !== li || (e.key !== "Enter" && e.key !== " ")) return;
          e.preventDefault();
          li.click();
        });
        return li;
      };
      if (r.parent) items.push(keyed(h("li", { onclick: () => go(r.parent) }, h("span", { class: "icon" }, "↰"), "..")));
      if (r.not_listed) items.push(h("li", { class: "dim" }, `${plural(r.not_listed, "item")} not shown: the name is not valid UTF-8`));
      for (const d of r.dirs) {
        const full = join(d);
        const li = h("li", { class: selected.has(full) ? "sel" : "" }, h("span", { class: "icon" }, "▸"), h("span", { class: "grow" }, d));
        if (mode === "multi") {
          const cb = h("input", { type: "checkbox", checked: selected.has(full), onclick: (e) => {
            e.stopPropagation();
            if (cb.checked) selected.add(full); else selected.delete(full);
            li.classList.toggle("sel", cb.checked); updateSel();
          } });
          li.prepend(cb);
        }
        li.addEventListener("click", () => go(full));
        items.push(keyed(li));
      }
      for (const f of r.files) {
        const full = join(f.name);
        files.add(full);
        const li = h("li", { class: selected.has(full) ? "sel" : "" }, h("span", { class: "icon" }, "▶"), h("span", { class: "grow" }, f.name), h("span", { class: "dim" }, bytes(f.size)));
        if (mode === "multi") {
          const cb = h("input", { type: "checkbox", checked: selected.has(full) });
          const toggle = () => {
            if (selected.has(full)) selected.delete(full); else selected.add(full);
            cb.checked = selected.has(full); li.classList.toggle("sel", cb.checked); updateSel();
          };
          cb.addEventListener("click", (e) => { e.stopPropagation(); cb.checked = !cb.checked; toggle(); });
          li.addEventListener("click", toggle);
          li.prepend(cb);
          keyed(li);
        } else li.classList.add("dim");
        items.push(li);
      }
      if (!r.dirs.length && !r.files.length) items.push(h("li", { class: "dim" }, "(empty)"));
      // Opened with the keyboard, a folder leaves the focus on its first row.
      const had = list.contains(document.activeElement);
      list.replaceChildren(...items);
      if (had) { const first = list.querySelector("li[tabindex]"); if (first) first.focus(); }
      updateSel();
    }

    const bg = h("div", { class: "modal-bg", onclick: (e) => { if (e.target === bg) close(null); } },
      h("div", { class: "modal" },
        h("div", { class: "head" }, h("strong", {}, title || "Pick a folder"), h("span", { class: "spacer" }),
          h("label", { class: "check dim" }, h("input", { type: "checkbox", onchange: (e) => { hidden = e.target.checked; go(cur); } }), "hidden")),
        h("div", { class: "body" }, places, h("div", { class: "row" }, pathEl), h("hr", { class: "dim" }), list),
        h("div", { class: "foot" }, selInfo, h("button", { onclick: () => close(null) }, "Cancel"), choose)));
    document.body.append(bg);
    const release = holdFocus(bg, () => close(null));
    go(cur, true);
  });
}

// ---------- encode options + enqueue dialog ----------

function metaLabel(o) {
  const parts = [];
  if (o.keep_metadata === false) parts.push("tags stripped");
  if (o.meta_tags && o.meta_tags.length) {
    const p = ((STATE && STATE.meta_presets) || []).find((x) => x.id === o.meta_preset);
    parts.push(p ? `${p.name} tags` : `${o.meta_tags.length} tag${o.meta_tags.length === 1 ? "" : "s"}`);
  }
  if (o.keep_dates) parts.push("dates kept");
  if (o.shrink_audio) parts.push("audio shrunk");
  if (o.remove && o.remove.length) parts.push(`${o.remove.length} section${o.remove.length === 1 ? "" : "s"} removed`);
  return parts.length ? " · " + parts.join(" · ") : "";
}

// The settings of a job or a watched folder, in one line.
function optionsLabel(o) {
  const sc = scenesMode(o.scenes);
  return `VMAF ${o.target_vmaf} · preset ${o.preset}${o.svt_params && o.svt_params !== "tune=0" ? " · " + o.svt_params : ""}${o.tuned ? " · tuned by size" : ""} · ${o.container}${sc === "off" ? "" : (sc === "auto" ? " · scenes where uneven×" : " · scenes×") + o.workers}${metaLabel(o)}`;
}

// Settings and jobs saved before "auto" existed hold true or false.
function scenesMode(v) {
  return v === true ? "always" : v === "auto" || v === "always" ? v : "off";
}

function defaultOptions() {
  return Object.assign({
    container: "mp4", target_vmaf: 93, preset: 5, svt_params: "", tuned: false, scenes: "off", workers: 1, min_savings: 0.15, force: false, retry_skipped: false,
    keep_metadata: true, keep_dates: false, info_json: true, meta_preset: "", meta_tags: [], shrink_audio: false,
  }, prefs.get("encodeOptions", {}));
}

// Tag editor for the encode dialog: preset picker, name/template rows, and (for one file)
// a preview of what would be written. `one` is that file, or null for several inputs or a
// folder. `get()` returns the current option fields.
function metaEditor(o, one) {
  const presets = (STATE && STATE.meta_presets) || [];
  const keep = h("input", { type: "checkbox", checked: o.keep_metadata });
  const dates = h("input", { type: "checkbox", checked: o.keep_dates });
  const infoJson = h("input", { type: "checkbox", checked: o.info_json });
  const presetSel = h("select", {}, h("option", { value: "" }, "Custom (add your own below)"), presets.map((p) => h("option", { value: p.id, title: p.about }, p.name)));
  presetSel.value = presets.some((p) => p.id === o.meta_preset) ? o.meta_preset : "";
  const about = h("p", { class: "dim about" });
  const rows = h("div", { class: "tags" });
  const out = h("div", { class: "tag-preview" });
  const showAbout = () => { const p = presets.find((x) => x.id === presetSel.value); about.textContent = p ? p.about : ""; };
  function addRow(k = "", v = "") {
    const key = h("input", { type: "text", class: "tag-key", value: k, placeholder: "tag", spellcheck: "false" });
    const val = h("input", { type: "text", class: "tag-val", value: v, placeholder: "value or {field}", spellcheck: "false" });
    const row = h("div", { class: "tag-row" }, key, val, h("button", { class: "small", title: "Remove", onclick: () => row.remove() }, "×"));
    rows.append(row);
    return key;
  }
  const setTags = (tags) => { rows.replaceChildren(); for (const [k, v] of tags) addRow(k, v); };
  setTags(o.meta_tags || []);
  showAbout();
  presetSel.addEventListener("change", () => {
    const p = presets.find((x) => x.id === presetSel.value);
    setTags(p ? p.tags : []);
    showAbout();
    out.replaceChildren();
  });
  function get() {
    const meta_tags = [...rows.querySelectorAll(".tag-row")]
      .map((r) => [r.querySelector(".tag-key").value.trim(), r.querySelector(".tag-val").value])
      .filter(([k, v]) => k && v.trim());
    return { keep_metadata: keep.checked, keep_dates: dates.checked, info_json: infoJson.checked, meta_preset: presetSel.value || null, meta_tags };
  }
  const previewBtn = one && h("button", { class: "small", onclick: async () => {
    previewBtn.disabled = true;
    try {
      const r = await api("/api/meta/preview", { input: one, options: get() });
      const set = new Map(r.tags.map(([k, v]) => [k.toLowerCase(), v]));
      const kept = r.keep ? Object.entries(r.source).filter(([k]) => !set.has(k.toLowerCase()) && !/^(encoder|major_brand|minor_version|compatible_brands|creation_time)$/i.test(k)) : [];
      const short = (v) => (v.length > 160 ? v.slice(0, 160) + "…" : v);
      out.replaceChildren(
        h("div", { class: "dim" }, r.info_json ? `Filled from ${basename(r.info_json)}` : "No .info.json next to this file; templates use its own tags"),
        r.tags.length || kept.length
          ? h("div", { class: "kv" },
              r.tags.map(([k, v]) => [h("span", { class: "k" }, k), h("span", { class: "mono" }, short(v))]),
              kept.map(([k, v]) => [h("span", { class: "k" }, k), h("span", { class: "mono dim", title: "kept from the source" }, short(v))]))
          : h("div", { class: "dim" }, "No tags would be written."),
        !r.keep && Object.keys(r.source).length ? h("div", { class: "dim" }, `${Object.keys(r.source).length} source tags dropped`) : null);
    } catch (e) { toast(e.message, true); }
    previewBtn.disabled = false;
  } }, "Preview tags");
  const el = h("div", {},
    h("div", { class: "checks-col" },
      h("label", { class: "check", title: "Title, description, dates, chapters… Stream languages and track names are always kept." }, keep, "Keep source metadata and chapters"),
      h("label", { class: "check", title: "Set the output's modification time to the source's" }, dates, "Keep file date"),
      h("label", { class: "check", title: "Fill tags from <name>.info.json (yt-dlp --write-info-json) next to each video" }, infoJson, "Read yt-dlp .info.json next to each video")),
    h("div", { class: "form-grid" }, setting(one ? "Tags for this file" : "Tags for these files", presetSel, null)),
    about,
    rows,
    h("div", { class: "row" }, h("button", { class: "small", onclick: () => addRow().focus() }, "+ Tag"), previewBtn),
    h("p", { class: "dim" }, "{field} comes from the video's .info.json, else its own tags; {a|b} takes the first with a value; {file} is the file name. Tags that come out empty are skipped. MP4 stores only the common tags (title, artist, date, description, comment, genre, show, episode…); MKV keeps any."),
    out);
  return { el, get };
}

// ---------- removing sections ----------

// "75.5", "1:15.5" or "1:02:03" → seconds; null if it isn't a time.
function parseTime(str) {
  const parts = String(str).trim().split(":");
  if (parts.length > 3 || parts.some((p) => p === "")) return null;
  const last = parts.pop();
  if (!/^\d+(\.\d+)?$|^\.\d+$/.test(last) || parts.some((p) => !/^\d+$/.test(p))) return null;
  const sec = Number(last);
  if (parts.length && sec >= 60) return null;
  return parts.reduce((t, p) => t * 60 + Number(p), 0) * 60 + sec;
}
// Seconds → "1:02:03.25" (hundredths, trailing zeros dropped).
function fmtTime(t) {
  const cs = Math.round(Math.max(0, t) * 100);
  const hh = Math.floor(cs / 360000), mm = Math.floor(cs / 6000) % 60, ss = Math.floor(cs / 100) % 60, f = cs % 100;
  let out = (hh ? hh + ":" + String(mm).padStart(2, "0") : mm) + ":" + String(ss).padStart(2, "0");
  if (f) out += "." + String(f).padStart(2, "0").replace(/0$/, "");
  return out;
}
// Removed (start, end) sections → the kept parts, as the encoder works them out: sorted,
// merged, clipped to the video, and kept slivers under 0.25 s dropped too.
const MIN_KEEP = 0.25;
function keptParts(removed, duration) {
  const r = removed.map(([a, b]) => [a, Math.min(b, duration)]).filter(([a, b]) => a < duration && b > a).sort((x, y) => x[0] - y[0]);
  const keep = [];
  let t = 0;
  for (const [a, b] of r) { if (a - t >= MIN_KEEP) keep.push([t, a]); t = Math.max(t, b); }
  if (duration - t >= MIN_KEEP) keep.push([t, duration]);
  return keep;
}
// Output time → source time for an output made from `keep` parts.
function outToSrc(keep, t) {
  let base = 0;
  for (const [a, b] of keep) { if (t < base + (b - a)) return a + Math.max(0, t - base); base += b - a; }
  return keep.length ? keep[keep.length - 1][1] : t;
}

// Section picker for the encode dialog (one file): a player with a bar showing what goes,
// "mark start / mark end" from the playhead, and editable rows. `get()` returns
// [[start, end], ...] in seconds or throws with a message for the user.
function sectionsEditor(path) {
  let info = null, duration = 0, fps = 25;
  // Source time of the player's 0: non-zero when playing a preview clip.
  let offset = 0, clipEnd = Infinity;
  let pending = null;
  // Set when the dialog closes; a preview asked for before that is then dropped, and so is
  // one that a later request has overtaken.
  let closed = false, clipRun = 0;
  const v = h("video", { preload: "metadata", playsinline: true, controls: true });
  const stage = h("div", { class: "cut-stage" }, v);
  const status = h("div", { class: "dim" }, "Loading the video…");
  const bar = h("div", { class: "cut-bar", title: "Click to seek" });
  const playhead = h("div", { class: "cut-head" });
  const pendingEl = h("div", { class: "cut-pending", hidden: true });
  const timeEl = h("span", { class: "mono" }, "–");
  const rows = h("div", { class: "cut-rows" });
  const summary = h("div", { class: "dim" });
  const skip = h("input", { type: "checkbox", checked: true });
  const now = () => offset + (v.currentTime || 0);
  const seek = (t) => {
    t = Math.max(0, Math.min(duration || t, t));
    if (t < offset || t > clipEnd) { makeClip(t); return; }
    v.currentTime = t - offset;
  };

  function readRows(strict) {
    const out = [];
    for (const r of rows.querySelectorAll(".cut-row")) {
      const [si, ei] = r.querySelectorAll("input");
      const a = parseTime(si.value), b = ei.value.trim() === "" || /^end$/i.test(ei.value.trim()) ? duration : parseTime(ei.value);
      const bad = a == null || b == null || b <= a || (duration && a >= duration);
      r.classList.toggle("invalid", bad);
      if (bad) { if (strict) throw new Error(`Section "${si.value} – ${ei.value}" is not a valid range`); continue; }
      out.push([a, b]);
    }
    return out;
  }
  function draw() {
    const removed = readRows(false);
    const pctOf = (t) => (duration ? (t / duration) * 100 : 0) + "%";
    bar.replaceChildren(...removed.map(([a, b]) => h("div", { class: "cut-span", style: { left: pctOf(a), width: `calc(${pctOf(Math.min(b, duration) - a)} + 1px)` } })), pendingEl, playhead);
    if (offset > 0 || clipEnd < Infinity) bar.prepend(h("div", { class: "cut-clip", style: { left: pctOf(offset), width: pctOf(Math.min(clipEnd, duration) - offset) } }));
    if (!duration) { summary.textContent = ""; return; }
    const keep = keptParts(removed, duration);
    const kept = keep.reduce((t, [a, b]) => t + b - a, 0);
    summary.textContent = removed.length
      ? `Output ${fmtTime(kept)} of ${fmtTime(duration)}: ${fmtTime(duration - kept)} removed in ${removed.length} section${removed.length === 1 ? "" : "s"}. Cuts are frame-accurate; audio is re-encoded to Opus.`
      : "Nothing removed.";
  }
  function addRow(a, b, focus) {
    const si = h("input", { type: "text", class: "mono", value: a == null ? "" : fmtTime(a), placeholder: "start", spellcheck: "false" });
    const ei = h("input", { type: "text", class: "mono", value: b == null ? "" : b >= duration && duration ? "end" : fmtTime(b), placeholder: "end", spellcheck: "false" });
    const go = (inp) => () => { const t = /^end$/i.test(inp.value.trim()) ? duration : parseTime(inp.value); if (t != null) seek(t); };
    const row = h("div", { class: "cut-row" },
      h("button", { class: "small", title: "Go to the start", onclick: go(si) }, "▶"), si, h("span", { class: "dim" }, "to"), ei,
      h("button", { class: "small", title: "Go to the end", onclick: go(ei) }, "▶"),
      h("button", { class: "small", title: "Set the end to the playhead", onclick: () => { ei.value = fmtTime(now()); draw(); } }, "end = here"),
      h("button", { class: "small", title: "Keep this part", onclick: () => { row.remove(); draw(); } }, "×"));
    for (const i of [si, ei]) i.addEventListener("input", draw);
    rows.append(row);
    draw();
    if (focus) si.focus();
  }
  function markStart() {
    pending = now();
    pendingEl.hidden = false;
    pendingEl.style.left = (duration ? (pending / duration) * 100 : 0) + "%";
    markEndBtn.disabled = false;
  }
  function markEnd() {
    if (pending == null) return;
    const [a, b] = [pending, now()].sort((x, y) => x - y);
    pending = null;
    pendingEl.hidden = true;
    markEndBtn.disabled = true;
    if (b - a > 0.01) addRow(a, b);
  }
  const step = (n) => { v.pause(); seek(now() + n / fps); };
  const markStartBtn = h("button", { class: "small", title: "Start a section at the playhead (key I)", onclick: markStart }, "Mark start");
  const markEndBtn = h("button", { class: "small", title: "End the section at the playhead and add it (key O)", disabled: true, onclick: markEnd }, "Mark end");
  const clipBtn = h("button", { class: "small", hidden: true, onclick: () => makeClip(now()) }, "Preview from here");

  bar.addEventListener("click", (e) => { const b = bar.getBoundingClientRect(); seek(((e.clientX - b.left) / b.width) * duration); });
  // Plays the output as it will be: jumps over removed sections.
  let raf = 0;
  const tick = () => {
    if (!v.isConnected) return; // dialog closed
    raf = requestAnimationFrame(tick);
    const t = now();
    timeEl.textContent = `${fmtTime(t)} / ${fmtTime(duration)}`;
    playhead.style.left = (duration ? (t / duration) * 100 : 0) + "%";
    if (skip.checked && !v.paused) {
      const hit = readRows(false).find(([a, b]) => t >= a && t < b - 0.05);
      if (hit) seek(hit[1]);
    }
  };
  const onKey = (e) => {
    if (!v.isConnected) { document.removeEventListener("keydown", onKey); return; }
    if (["INPUT", "SELECT", "TEXTAREA"].includes(e.target.tagName)) return;
    // A dialog opened over this one has the keys.
    const open = document.querySelectorAll(".modal-bg");
    if (open.length && !open[open.length - 1].contains(v)) return;
    if (e.key === "i" || e.key === "I") markStart();
    else if (e.key === "o" || e.key === "O") markEnd();
    else if (e.key === ",") step(-1);
    else if (e.key === ".") step(1);
  };
  document.addEventListener("keydown", onKey);

  function load(url) {
    return new Promise((resolve, reject) => {
      const ok = () => { cleanup(); v.videoWidth ? resolve() : reject(new Error("no video track the browser can show")); };
      const bad = () => { cleanup(); reject(new Error((v.error && v.error.message) || "cannot play this format")); };
      const cleanup = () => { v.removeEventListener("loadeddata", ok); v.removeEventListener("error", bad); };
      v.addEventListener("loadeddata", ok);
      v.addEventListener("error", bad);
      v.src = url;
    });
  }
  // For sources the browser can't decode: a 2-minute near-lossless H.264 clip from `at`.
  async function makeClip(at) {
    if (!info || closed) return;
    const run = ++clipRun;
    const late = () => closed || run !== clipRun;
    const start = Math.max(0, Math.min(at, duration - 2));
    v.pause();
    clipBtn.disabled = true;
    status.textContent = `Making a playable 2-minute preview from ${fmtTime(start)}…`;
    try {
      const p = await api("/api/preview", { path, kind: "transcode", start, secs: 120 });
      if (late()) return;
      await load(p.url);
      if (late()) return;
      offset = p.start; clipEnd = p.start + p.secs;
      status.textContent = `Preview of ${fmtTime(offset)}–${fmtTime(Math.min(clipEnd, duration))} (this browser can't play ${info.codec} directly). Click the bar outside it for another part.`;
      v.currentTime = Math.max(0, at - offset);
    } catch (e) {
      if (late()) return;
      status.textContent = "Could not make a preview: " + e.message + ". You can still type the times.";
    }
    clipBtn.disabled = false;
    draw();
  }

  (async () => {
    try {
      const got = await api("/api/source", { path });
      if (closed) return;
      info = got;
      if (info.error) throw new Error(info.error);
      duration = info.duration_s || 0;
      fps = info.fps || 25;
      draw();
      try {
        await load(info.url);
        if (closed) return;
        status.textContent = "";
      } catch {
        if (closed) return;
        clipBtn.hidden = false;
        await makeClip(0);
      }
    } catch (e) {
      status.textContent = "Cannot open the video: " + e.message + ". You can still type the times.";
    }
    if (closed) return;
    raf = requestAnimationFrame(tick);
  })();

  const el = h("div", { class: "cut" },
    h("p", { class: "dim" }, "Leave parts out of the output: ads, intros, mistakes. Mark start / Mark end at the playhead (keys I and O; , and . step frames), or type the times (1:23.5, or \"end\")."),
    stage, status, bar,
    h("div", { class: "row" },
      h("button", { class: "small", title: "Previous frame (,)", onclick: () => step(-1) }, "◀ frame"),
      h("button", { class: "small", title: "Next frame (.)", onclick: () => step(1) }, "frame ▶"),
      timeEl, h("span", { class: "spacer" }), markStartBtn, markEndBtn, clipBtn),
    rows,
    h("div", { class: "row" },
      h("button", { class: "small", onclick: () => addRow(null, null, true) }, "+ Section"),
      h("label", { class: "check", title: "While playing here, jump over the sections marked for removal" }, skip, "Skip removed parts while playing")),
    summary);
  return { el, get: () => readRows(true).sort((x, y) => x[0] - y[0]), stop: () => { closed = true; cancelAnimationFrame(raf); document.removeEventListener("keydown", onKey); v.pause(); v.removeAttribute("src"); v.load(); } };
}

// One label / control / hint line of a .form-grid.
function setting(label, control, hint) {
  return [h("label", { class: "k" }, label), h("div", { class: "ctl" }, control, hint ? h("span", { class: "dim hint" }, hint) : null)];
}

// Shows the encode dialog for `inputs` (paths). Resolves true if queued. With `watch`, the
// one input is a folder to keep an eye on: its settings are asked for, and nothing is queued now.
// `files` is the set of inputs known to be files, where the caller knows (see isVideoFile).
function encodeDialog(inputs, roots = [], watch = false, files = null) {
  return new Promise((resolve) => {
    const o = defaultOptions();
    let outDir = prefs.get("outDir", null) || (STATE && STATE.default_out_dir) || "";
    const outEl = h("code", {}, outDir);
    const vmaf = h("input", { type: "number", step: "0.5", min: "50", max: "99.5", value: o.target_vmaf });
    const preset = h("input", { type: "number", min: "0", max: "13", value: o.preset });
    const dflParams = (STATE && STATE.default_svt_params) || "tune=0";
    const svtParams = h("input", { type: "text", class: "mono", spellcheck: "false", value: o.svt_params || dflParams, placeholder: dflParams });
    const tuning = (STATE && STATE.tuning) || [];
    const tuned = h("input", { type: "checkbox", checked: o.tuned });
    const container = h("select", {}, h("option", { value: "mp4" }, "MP4 (plays in browsers)"), h("option", { value: "mkv" }, "MKV (keeps image subtitles)"));
    container.value = o.container;
    const scenes = h("select", {}, h("option", { value: "off" }, "Off: one CRF for the whole video"), h("option", { value: "auto" }, "Where quality is uneven"), h("option", { value: "always" }, "Always"));
    scenes.value = scenesMode(o.scenes);
    const workers = h("input", { type: "number", min: "1", max: "8", value: o.workers });
    const minSav = h("input", { type: "number", min: "0", max: "95", step: "1", value: Math.round(o.min_savings * 100) });
    const force = h("input", { type: "checkbox", checked: o.force });
    const retry = h("input", { type: "checkbox", checked: o.retry_skipped });
    const existing = h("input", { type: "checkbox" });
    const shrinkAudio = h("input", { type: "checkbox", checked: o.shrink_audio });
    const one = !watch && inputs.length === 1 && isVideoFile(inputs[0], files) ? inputs[0] : null;
    const meta = metaEditor(o, one);
    // Only built when opened, so the dialog doesn't load the video for nothing.
    let cuts = null;
    const cutsBox = h("div");
    const cutsBtn = one && h("button", { class: "small", onclick: () => {
      cuts = sectionsEditor(one);
      cutsBox.replaceChildren(cuts.el);
      cutsBtn.remove();
    } }, "Choose sections to remove…");
    const syncWorkers = () => { workers.disabled = scenes.value === "off"; };
    scenes.addEventListener("change", syncWorkers);
    syncWorkers();
    let busy = false;
    const close = (v) => { if (cuts) cuts.stop(); bg.remove(); release(); resolve(v); };
    const go = h("button", { class: "primary", onclick: async () => {
      const numbers = readNumbers([[vmaf, "Target VMAF"], [preset, "Preset", true], [minSav, "Skip if saving under"]]);
      if (!numbers) return;
      const [targetVmaf, presetNumber, minSavings] = numbers;
      // Scenes at once is greyed out and means nothing with per-scene CRF off: what it holds
      // then is no reason to refuse, and is not sent if it could not be taken.
      let sceneWorkers = Number.isInteger(o.workers) && o.workers >= 1 && o.workers <= 8 ? o.workers : 1;
      if (!workers.disabled) {
        const read = readNumbers([[workers, "Scenes at once", true]]);
        if (!read) return;
        [sceneWorkers] = read;
      }
      const options = {
        container: container.value, target_vmaf: targetVmaf, preset: presetNumber, svt_params: svtParams.value.trim(),
        tuned: tuning.length > 0 && tuned.checked, scenes: scenes.value,
        workers: sceneWorkers, min_savings: minSavings / 100, force: force.checked, retry_skipped: !watch && retry.checked,
        shrink_audio: shrinkAudio.checked,
        ...meta.get(),
      };
      if (watch) {
        // Neither the settings nor the folder become the defaults of encodes queued by hand.
        go.disabled = busy = true;
        try {
          await api("/api/watch", { path: inputs[0], out_dir: outDir, options, existing: existing.checked });
          toast(`Watching ${inputs[0]}`);
          close(true);
        } catch (e) { toast(e.message, true); } finally { go.disabled = busy = false; }
        return;
      }
      // The tick is remembered also while there are no tuned settings to use.
      const remembered = { ...options, tuned: tuning.length ? tuned.checked : o.tuned };
      // Sections belong to this one video, so they aren't remembered with the settings.
      try { if (cuts) options.remove = cuts.get(); } catch (e) { toast(e.message, true); return; }
      go.disabled = busy = true;
      try {
        const r = await api("/api/jobs", { inputs, out_dir: outDir, roots, options });
        prefs.set("encodeOptions", remembered);
        prefs.set("outDir", outDir);
        toast(`Queued ${r.files} file${r.files === 1 ? "" : "s"}`);
        close(true);
        location.hash = "#/queue";
      } catch (e) { toast(e.message, true); } finally { go.disabled = busy = false; }
    } }, watch ? "Watch folder" : "Add to queue");
    const list = inputs.length <= 6 ? inputs.map((p) => h("div", { class: "mono" }, p)) : [h("div", { class: "mono" }, inputs.slice(0, 4).join("\n")), h("div", { class: "dim" }, `… and ${inputs.length - 4} more`)];
    // While the request is on its way the dialog stays: what it asked for is being queued.
    const dismiss = () => { if (!busy) close(false); };
    const bg = h("div", { class: "modal-bg", onclick: (e) => { if (e.target === bg) dismiss(); } },
      h("div", { class: "modal" },
        h("div", { class: "head" }, h("strong", {}, watch ? "Watch a folder" : `Encode ${inputs.length} item${inputs.length === 1 ? "" : "s"}`)),
        h("div", { class: "body" },
          h("h3", {}, watch ? "Folder" : "Inputs"), list,
          watch ? [
            h("p", { class: "dim" }, "Videos that turn up in this folder, or anywhere below it, are added to the queue on their own while av1opt is open. The folder is looked at every few minutes; a video is queued once it has stopped changing (a download still running is left alone) and a scan would call it worth encoding. The queue’s pause and hours decide when it is encoded."),
            h("label", { class: "check" }, existing, "Also queue the videos that are in it now"),
          ] : null,
          h("h3", {}, "Output folder"),
          h("div", { class: "row" }, outEl, h("button", { class: "small", onclick: async () => {
            const p = await pick({ title: "Output folder (not on ZFS)", mode: "dir", start: outDir || undefined });
            if (p) { outDir = p[0]; outEl.textContent = outDir; }
          } }, "Change…")),
          h("p", { class: "dim" }, watch ? "Folders are mirrored below it. The videos in the watched folder are never modified." : "Folders are mirrored below it. Inputs are never modified. Re-adding the same files skips finished ones and reuses what was already measured."),
          h("h3", {}, "Settings"),
          h("div", { class: "form-grid" },
            setting("Target VMAF", vmaf, "Quality to match: 93 looks like the source, higher keeps more and saves less"),
            setting("Preset", preset, "0–13: lower makes smaller files but encodes slower. Measured at the same quality, against 5: 6 is about 6% larger for a fifth less processor time, 4 about 4% smaller for a quarter more, 3 about 10% smaller and 2.5 times slower"),
            setting("Encoder parameters", svtParams, "SVT-AV1 options as name=value:name=value; the Benchmark page finds good ones"),
            tuning.length ? setting("Tuned settings", h("label", { class: "check" }, tuned, "Use the benchmark's settings by video size"),
              tuning.map((x) => `${x.label}: ${configLabel(x.choice)}`).join("; ") + ". Other sizes use the preset and parameters above") : null,
            setting("Container", container, null),
            setting("Per-scene CRF", h("span", { class: "unit" }, scenes, workers, "at once"),
              "Splits at scene changes and picks a CRF for each: even quality, about three times the time. “Where quality is uneven” does it only for videos whose samples score far apart at one CRF"),
            setting("Skip if saving under", h("span", { class: "unit" }, minSav, "%"), "Files that wouldn't shrink this much are left alone")),
          h("div", { class: "checks-col" },
            h("label", { class: "check", title: "AAC, Opus and MP3 sound is normally copied untouched. With this, a track far above what Opus needs (stereo above 144 kbps) is re-encoded to Opus (stereo at 96 kbps): smaller, but lossy a second time" }, shrinkAudio, "Shrink heavy sound tracks to Opus (stereo above 144 kbps becomes 96 kbps)"),
            h("label", { class: "check" }, force, watch ? "Force: also queue videos that are too short or have little to gain (AV1 files are left alone all the same)" : "Force: also re-encode AV1 files and low-gain ones"),
            watch ? null : h("label", { class: "check" }, retry, "Retry files skipped in earlier runs")),
          h("h3", {}, "Metadata"),
          meta.el,
          one ? [h("h3", {}, "Remove sections"), cutsBtn, cutsBox] : null),
        h("div", { class: "foot" }, h("button", { onclick: dismiss }, "Cancel"), go)));
    document.body.append(bg);
    const release = holdFocus(bg, dismiss);
  });
}

// ---------- views ----------

const views = {};
let cleanup = null;

function route() {
  if (cleanup) { cleanup(); cleanup = null; }
  const hash = location.hash || "#/scan";
  const [path, qs] = hash.slice(2).split("?");
  const params = new URLSearchParams(qs || "");
  const name = views[path] ? path : "scan";
  for (const a of document.querySelectorAll("nav a")) a.classList.toggle("active", a.dataset.view === name);
  const root = document.getElementById("view");
  root.replaceChildren();
  cleanup = views[name](root, params) || null;
}

// ----- scan -----

let lastScanId = null;

views.scan = (root) => {
  const saved = (STATE && STATE.settings.scan_paths) || [];
  const roots = new Set(prefs.get("scanRoots", saved));
  const chips = h("div", { class: "chips" });
  const minSav = h("input", { type: "number", min: "0", max: "95", value: prefs.get("scanMinSavings", 15) });
  const vmaf = h("input", { type: "number", step: "0.5", value: prefs.get("scanVmaf", 93) });
  const start = h("button", { class: "primary" }, "Scan");
  const progress = h("div", { class: "bar grow" }, h("div"));
  const progText = h("span", { class: "dim" });
  const out = h("div");
  const unreadable = h("div");
  let scanId = null;
  let scanStatus = null; // as last read; null until then

  const drawChips = () => {
    chips.replaceChildren(...[...roots].map((r) => h("span", { class: "chip" }, h("code", {}, r), h("button", { title: "Remove", onclick: () => { roots.delete(r); prefs.set("scanRoots", [...roots]); drawChips(); } }, "×"))));
    if (!roots.size) chips.append(h("span", { class: "dim" }, "No folders yet."));
    start.disabled = !roots.size;
  };
  drawChips();

  start.onclick = async () => {
    const numbers = readNumbers([[minSav, "Min savings %"], [vmaf, "Target VMAF"]]);
    if (!numbers) return;
    const [minSavings, targetVmaf] = numbers;
    start.disabled = true;
    try {
      const r = await api("/api/scans", { paths: [...roots], min_savings: minSavings / 100, target_vmaf: targetVmaf, preset: Number(defaultOptions().preset) || 5 });
      // Only now: values the server refused would come back at the next visit.
      prefs.set("scanMinSavings", minSavings);
      prefs.set("scanVmaf", targetVmaf);
      scanId = lastScanId = r.id;
      scanStatus = null;
      progText.textContent = "listing files…";
      progress.firstChild.style.width = "0%";
      out.replaceChildren();
      unreadable.replaceChildren();
      // A short scan can be over before its id is known here: its last event went by unseen.
      showResult(r.id);
    } catch (e) { toast(e.message, true); start.disabled = false; }
  };

  root.append(
    h("h1", {}, "Scan a library"),
    h("div", { class: "panel" },
      h("div", { class: "row" }, chips, h("span", { class: "spacer" }),
        h("button", { onclick: async () => { const p = await pick({ title: "Folder to scan" }); if (p) { roots.add(p[0]); prefs.set("scanRoots", [...roots]); drawChips(); } } }, "Add folder…")),
      h("div", { class: "row" }, h("p", { class: "dim grow" }, "Read-only: probes every video (cached), estimates the AV1 size from metadata and past encodes. Nothing is written to the scanned folders."),
        h("label", { class: "field" }, "Min savings %", minSav), h("label", { class: "field" }, "Target VMAF", vmaf), start),
      h("div", { class: "row" }, progress, progText)),
    unreadable, out);

  // The server names the first few and counts them all.
  const drawUnreadable = (count, paths) => {
    unreadable.replaceChildren(...(count ? [h("div", { class: "panel" },
      h("strong", { class: "warn" }, `${plural(count, "folder")} could not be read, so the videos in ${count === 1 ? "it" : "them"} are not counted here`),
      paths.map((p) => h("div", { class: "dim mono long-text" }, p)),
      count > paths.length ? h("div", { class: "dim" }, `and ${count - paths.length} more`) : null)] : []));
  };

  async function showResult(id) {
    let r;
    try { r = await api("/api/scans/" + id); } catch (e) { toast(e.message, true); if (id === scanId) start.disabled = !roots.size; return; }
    if (id !== scanId) return; // another scan was started meanwhile
    scanStatus = r.status;
    start.disabled = !roots.size;
    if (r.status === "running") {
      progText.textContent = r.total ? `probed ${r.done} / ${r.total}` : "listing files…";
      progress.firstChild.style.width = r.total ? (100 * r.done / r.total) + "%" : "0%";
      scanId = id; start.disabled = true;
      return;
    }
    progress.firstChild.style.width = "100%";
    drawUnreadable(r.unreadable_folders || 0, r.unreadable_paths || []);
    if (r.status === "failed") { progText.textContent = "failed"; out.replaceChildren(h("div", { class: "panel bad" }, r.error)); return; }
    progText.textContent = `${r.rows.length} videos · estimates from ${r.model_samples ? r.model_samples + " past encodes" : "the built-in model (encode a few files to calibrate)"}${r.not_listed ? ` · ${plural(r.not_listed, "video")} left out: the path is not valid UTF-8` : ""}`;
    renderScan(out, r);
  }

  const off = onEvent((m) => {
    if (m.kind === "scan" && m.id === scanId) {
      progText.textContent = m.total ? `probed ${m.done} / ${m.total}` : "listing files…";
      progress.firstChild.style.width = m.total ? (100 * m.done / m.total) + "%" : "0%";
    } else if (m.kind === "scan_done" && m.id === scanId) showResult(m.id);
    // A finished scan cannot have changed, and drawing it again would lose the selection.
    else if (m.kind === "reopened" && scanId != null && (scanStatus == null || scanStatus === "running")) showResult(scanId);
  });
  if (lastScanId) { scanId = lastScanId; showResult(lastScanId); }
  return off;
};

function renderScan(out, r) {
  const t = r.totals;
  let filter = prefs.get("scanFilter", "candidate");
  let sortKey = "saved", sortDir = -1;
  const selected = new Set();
  const savedOf = (row) => row.size - row.estimate.likely;
  const tried = (row) => row.verdict === "Candidate" || row.verdict === "Maybe";
  // Bytes saved per hour of encoding: what to start with when time is short.
  const rateOf = (row) => tried(row) && row.est_secs ? savedOf(row) / row.est_secs * 3600 : 0;
  const timeBasis = `Search and encode at preset ${r.preset}, ${r.time_samples ? "going by " + plural(r.time_samples, "past encode") + " on this machine" : "a rough guess until a few files are encoded on this machine"}. A CRF per scene takes about ${num(r.scenes_factor || 3, 1)} times as long${r.scene_samples ? " (" + plural(r.scene_samples, "such encode") + ")" : ""}; one only where quality is uneven about ${num(r.auto_factor || 1.7, 1)} times on average${r.auto_samples ? " (" + Math.round((r.auto_share || 0) * 100) + "% of " + plural(r.auto_samples, "encode") + " made that way went per scene)" : ""}`;
  const rows = r.rows;
  const body = h("tbody");
  const selInfo = h("span", { class: "dim" });
  // Files are queued in the order of the table: sort by "Per hour" to encode the best value first.
  const inOrder = () => { const seen = shown().map((x) => x.path).filter((p) => selected.has(p)); const s = new Set(seen); return seen.concat([...selected].filter((p) => !s.has(p))); };
  const encodeBtn = h("button", { class: "primary", disabled: true, title: "Files are queued in the order of the table", onclick: () => encodeDialog(inOrder(), r.paths, false, selected) }, "Encode selected…");
  const allBox = h("input", { type: "checkbox", title: "Select all shown" });

  const shown = () => {
    let v = rows.filter((x) => filter === "all" || (filter === "candidate" ? tried(x) : !tried(x)));
    const key = { saved: savedOf, size: (x) => x.size, name: (x) => x.rel, mbps: (x) => x.video_mbps || 0, dur: (x) => x.duration_s || 0, time: (x) => x.est_secs || 0, rate: (x) => rateOf(x) * (x.order_weight ?? 1) }[sortKey];
    v.sort((a, b) => { const ka = key(a), kb = key(b); return (ka < kb ? -1 : ka > kb ? 1 : 0) * sortDir; });
    return v;
  };
  const updateSel = () => {
    const sel = rows.filter((x) => selected.has(x.path));
    const secs = sel.reduce((a, x) => a + (x.est_secs || 0), 0);
    selInfo.textContent = sel.length ? `${sel.length} selected · ${bytes(sel.reduce((a, x) => a + savedOf(x), 0))} est. saved${secs ? " · about " + span(secs) + " to encode" : ""}` : "";
    encodeBtn.disabled = !selected.size;
  };
  // "What is worth encoding tonight": the candidates that save the most in so many hours.
  const hoursIn = h("input", { type: "number", min: "0.5", max: "999", step: "0.5", value: prefs.get("pickHours", 8), title: "Hours of encoding to fill" });
  const pickBtn = h("button", { title: "Selects the candidates that save the most together in this many hours of encoding. Not simply the best per hour first: a long video can leave a gap that shorter ones fill better", onclick: async () => {
    const hours = Number(hoursIn.value);
    if (!(hours > 0)) { toast("Give a number of hours", true); return; }
    pickBtn.disabled = true;
    try {
      const p = await api(`/api/scans/${r.id}/pick`, { hours });
      prefs.set("pickHours", hours);
      if (!p.paths.length) { toast("No candidate can be encoded in that time", true); return; }
      selected.clear();
      for (const x of p.paths) selected.add(x);
      // Shown the way they will be queued: best value first.
      filter = "candidate"; filt.value = filter; sortKey = "rate"; sortDir = -1;
      draw(); updateSel();
      toast(`${plural(p.paths.length, "video")} picked: about ${span(p.secs)} to encode, ${bytes(p.saved)} est. saved`);
    } catch (e) { toast(e.message, true); } finally { pickBtn.disabled = false; }
  } }, "Pick");
  // With encoding hours set, a night is as long as they are.
  if (prefs.get("pickHours", null) == null) {
    api("/api/jobs/heads").then(({ queue }) => {
      const sch = queue && queue.schedule;
      if (!sch || !sch.on) return;
      const mins = (t) => { const [a, b] = String(t).split(":").map(Number); return a * 60 + b; };
      const len = ((mins(sch.to) - mins(sch.from) + 1440) % 1440) / 60;
      if (len > 0) hoursIn.value = Math.round(len * 2) / 2;
    }).catch(() => {});
  }
  let limit = 500; // more rows make every sort and filter click slow
  const draw = () => {
    const v = shown();
    body.replaceChildren(...v.slice(0, limit).map((x) => {
      const cb = h("input", { type: "checkbox", checked: selected.has(x.path), onchange: () => { if (cb.checked) selected.add(x.path); else selected.delete(x.path); updateSel(); } });
      const cls = x.verdict === "Candidate" ? "ok" : x.verdict === "Maybe" ? "warn" : x.verdict === "Unreadable" ? "bad" : "";
      return h("tr", {},
        h("td", {}, cb),
        h("td", { class: "path", title: x.path }, x.rel),
        h("td", {}, x.codec || "–", x.hdr ? h("span", { class: "pill warn" }, "HDR") : null),
        h("td", { class: "num" }, x.width ? `${x.width}×${x.height}` : "–"),
        h("td", { class: "num" }, hms(x.duration_s)),
        h("td", { class: "num" }, num(x.video_mbps, 1)),
        h("td", { class: "num" }, bytes(x.size)),
        h("td", { class: "num" }, tried(x) ? `~${bytes(x.estimate.likely)}` : "–"),
        h("td", { class: "num" }, tried(x) ? h("span", { title: `range ${bytes(x.size - x.estimate.high)} – ${bytes(x.size - x.estimate.low)}` }, `${bytes(savedOf(x))} (${pct(savedOf(x) / x.size)})`) : "–"),
        h("td", { class: "num" }, tried(x) ? span(x.est_secs) : "–"),
        h("td", { class: "num" }, rateOf(x) ? bytes(rateOf(x)) + "/h" : "–"),
        h("td", {}, h("span", { class: "pill " + cls, title: x.error || "" }, x.verdict_label)));
    }));
    if (v.length > limit) body.append(h("tr", {}, h("td", { colspan: 12, class: "dim" }, `${v.length - limit} more not shown. `,
      h("button", { class: "small", onclick: () => { limit = Infinity; draw(); } }, "Show all"))));
    if (!v.length) body.append(h("tr", {}, h("td", { colspan: 12, class: "empty" }, "Nothing here.")));
    allBox.checked = v.length > 0 && v.every((x) => selected.has(x.path));
  };
  allBox.onchange = () => { for (const x of shown()) { if (allBox.checked) selected.add(x.path); else selected.delete(x.path); } draw(); updateSel(); };
  const th = (label, key, cls, title) => h("th", { class: "sort " + (cls || ""), title, onclick: () => { if (sortKey === key) sortDir *= -1; else { sortKey = key; sortDir = key === "name" ? 1 : -1; } draw(); } }, label);
  const filt = h("select", { onchange: (e) => { filter = e.target.value; prefs.set("scanFilter", filter); draw(); } },
    h("option", { value: "candidate" }, "Candidates"), h("option", { value: "skip" }, "Skipped"), h("option", { value: "all" }, "All files"));
  filt.value = filter;

  out.replaceChildren(
    h("div", { class: "stats" },
      h("div", { class: "stat" }, h("div", { class: "v" }, t.files), h("div", { class: "l" }, `videos · ${bytes(t.bytes)}`)),
      h("div", { class: "stat" }, h("div", { class: "v" }, t.candidates), h("div", { class: "l" }, `candidates · ${bytes(t.candidate_bytes)}`)),
      h("div", { class: "stat" }, h("div", { class: "v ok" }, bytes(t.saved_likely)), h("div", { class: "l" }, `estimated savings (${bytes(t.saved_low)} – ${bytes(t.saved_high)})`)),
      h("div", { class: "stat" }, h("div", { class: "v" }, pct(t.bytes ? t.saved_likely / t.bytes : 0)), h("div", { class: "l" }, "of the whole library")),
      h("div", { class: "stat", title: timeBasis }, h("div", { class: "v" }, t.encode_secs ? "≈ " + span(t.encode_secs) : "–"), h("div", { class: "l" }, `to encode the candidates${r.time_samples ? "" : " (rough guess)"}`))),
    h("div", { class: "panel" },
      h("div", { class: "row" }, filt, selInfo, h("span", { class: "spacer" }),
        t.candidates ? h("span", { class: "unit pick" }, "Most saved in", hoursIn, "h", pickBtn) : null, encodeBtn),
      h("div", { class: "tablewrap" }, h("table", {},
        h("thead", {}, h("tr", {}, h("th", {}, allBox), th("File", "name"), h("th", {}, "Codec"), h("th", { class: "num" }, "Size px"), th("Length", "dur", "num"), th("Mbps", "mbps", "num"), th("Size", "size", "num"), h("th", { class: "num" }, "AV1 est."), th("Saves", "saved", "num"), th("Time", "time", "num", timeBasis), th("Per hour", "rate", "num", "Estimated saving per hour of encoding: sort by this to start where the time pays most"), h("th", {}, "Verdict"))),
        body))));
  draw();
  updateSel();
}

// ----- duplicates -----

let lastDupId = null;
// Paths ticked for removal. Kept while moving between views; a new search starts empty.
const dupMarks = new Set();
// Files a trash request could not move, shown once with the next drawing of the result.
let pendingTrashFailures = null;
const DUP_KIND = { same: ["Same video", "ok"], same_start: ["Same start", "warn"], same_end: ["Same ending", "warn"] };

views.dups = (root) => {
  const st = (STATE && STATE.settings) || {};
  const roots = new Set(prefs.get("dupRoots", st.dup_paths && st.dup_paths.length ? st.dup_paths : st.scan_paths || []));
  let prefer = prefs.get("dupPrefer", st.dup_prefer || []);
  const chips = h("div", { class: "chips" });
  const preferChips = h("div", { class: "chips" });
  const avoid = h("input", { type: "text", class: "grow", value: prefs.get("dupAvoid", (st.dup_avoid || []).join(", ")), placeholder: "e.g. trash, tmp", spellcheck: "false" });
  const tol = h("input", { type: "number", min: "0", max: "20", value: prefs.get("dupTolerance", 10) });
  const refresh = h("input", { type: "checkbox" });
  const start = h("button", { class: "primary" }, "Find duplicates");
  const cancel = h("button", { hidden: true }, "Stop");
  const progress = h("div", { class: "bar grow" }, h("div"));
  const progText = h("span", { class: "dim" });
  const out = h("div");
  let runId = null;
  let dupStatus = null; // of the search shown, as last read; null until then
  // For the time-left estimate of the slow step.
  let fpStart = null;

  const drawChips = () => {
    chips.replaceChildren(...[...roots].map((r) => h("span", { class: "chip" }, h("code", {}, r), h("button", { title: "Remove", onclick: () => { roots.delete(r); prefs.set("dupRoots", [...roots]); drawChips(); } }, "×"))));
    if (!roots.size) chips.append(h("span", { class: "dim" }, "No folders yet."));
    start.disabled = !roots.size || runId != null;
  };
  const drawPrefer = () => {
    preferChips.replaceChildren(...prefer.map((p, i) => h("span", { class: "chip" }, h("span", { class: "dim" }, `${i + 1}.`), h("code", {}, p),
      h("button", { title: "Remove", onclick: () => { prefer = prefer.filter((x) => x !== p); prefs.set("dupPrefer", prefer); drawPrefer(); } }, "×"))));
    if (!prefer.length) preferChips.append(h("span", { class: "dim" }, "None: quality alone decides."));
  };
  drawChips();
  drawPrefer();

  const setRunning = (on) => {
    cancel.hidden = !on;
    start.disabled = on || !roots.size;
  };
  const showProgress = (m) => {
    const frac = m.total ? m.done / m.total : 0;
    progress.firstChild.style.width = (m.phase === "comparing" ? 100 : frac * 100) + "%";
    if (m.phase === "probing") progText.textContent = m.total ? `probing ${m.done} / ${m.total}` : "listing files…";
    else if (m.phase === "fingerprinting") {
      if (!fpStart || m.done < fpStart.done) fpStart = { t: performance.now(), done: m.done };
      const el = (performance.now() - fpStart.t) / 1000, n = m.done - fpStart.done;
      const eta = n >= 5 && el > 3 ? ` · about ${hms((m.total - m.done) * el / n)} left` : "";
      progText.textContent = `reading frames ${m.done} / ${m.total}${eta}`;
    } else progText.textContent = `comparing ${m.total} videos…`;
  };

  start.onclick = async () => {
    const avoidList = avoid.value.split(",").map((x) => x.trim()).filter(Boolean);
    const numbers = readNumbers([[tol, "Tolerance", true]]);
    if (!numbers) return;
    const [tolerance] = numbers;
    start.disabled = true;
    try {
      const r = await api("/api/dups", { paths: [...roots], prefer, avoid: avoidList, tolerance, refresh: refresh.checked });
      prefs.set("dupAvoid", avoid.value);
      prefs.set("dupTolerance", tolerance);
      runId = lastDupId = r.id;
      dupStatus = null;
      dupMarks.clear();
      fpStart = null;
      refresh.checked = false;
      progText.textContent = "listing files…";
      progress.firstChild.style.width = "0%";
      out.replaceChildren();
      setRunning(true);
      // A short search can be over before its id is known here: its last event went by unseen.
      showResult(r.id);
    } catch (e) { toast(e.message, true); start.disabled = !roots.size; }
  };
  cancel.onclick = async () => {
    if (runId == null) return;
    cancel.disabled = true;
    try { await api(`/api/dups/${runId}/cancel`, {}); } catch (e) { toast(e.message, true); }
    cancel.disabled = false;
  };

  root.append(
    h("h1", {}, "Find duplicate videos"),
    h("div", { class: "panel" },
      h("div", { class: "row" }, chips, h("span", { class: "spacer" }),
        h("button", { onclick: async () => { const p = await pick({ title: "Folder to search" }); if (p) { roots.add(p[0]); prefs.set("dupRoots", [...roots]); drawChips(); } } }, "Add folder…")),
      h("p", { class: "dim" }, "Finds the same video stored more than once: another resolution, bitrate, codec or container, with or without black bars, and copies cut at the start or the end. Read-only: a few frames of every video are decoded and compared; nothing is written to these folders and nothing is deleted. The first search of a big library takes hours; the frames are remembered, so later searches only read new files."),
      h("details", { class: "more" }, h("summary", {}, "Which copy to keep, and how strict"),
        h("div", { class: "form-grid" },
          setting("Prefer copies in", h("div", { class: "row grow" }, preferChips, h("span", { class: "spacer" }),
            h("button", { class: "small", onclick: async () => { const p = await pick({ title: "Folder to prefer" }); if (p && !prefer.includes(p[0])) { prefer = [...prefer, p[0]]; prefs.set("dupPrefer", prefer); drawPrefer(); } } }, "Add folder…")),
            "Between equally good copies, the one under the first folder listed is suggested"),
          setting("Never keep paths with", avoid, "Comma-separated texts; a copy whose path contains one is never the suggestion while another copy exists"),
          setting("Tolerance", tol, "0–20: how different two frames may be and still count as the same picture. Lower finds fewer, surer matches"),
          setting("Read again", h("label", { class: "check" }, refresh, "Ignore the remembered frames and read every video again"), null))),
      h("div", { class: "row" }, progress, progText, cancel, start)),
    out);

  async function showResult(id) {
    let r;
    try { r = await api("/api/dups/" + id); } catch (e) { if (id === lastDupId) { runId = null; setRunning(false); } return; }
    if (id !== lastDupId) return; // another search was started meanwhile
    dupStatus = r.status;
    if (r.status === "running") { runId = id; setRunning(true); showProgress(r); return; }
    runId = null;
    setRunning(false);
    progress.firstChild.style.width = r.status === "done" ? "100%" : "0%";
    if (r.status === "failed") { progText.textContent = "failed"; out.replaceChildren(h("div", { class: "panel bad" }, r.error)); return; }
    if (r.status === "cancelled") { progText.textContent = "stopped (frames read so far are remembered)"; out.replaceChildren(); return; }
    const s = r.report.stats;
    progText.textContent = `${s.compared} of ${s.videos} videos compared · ${s.fresh} read now, the rest remembered${s.not_text ? ` · ${plural(s.not_text, "video")} left out: the path is not valid UTF-8` : ""}`;
    renderDups(out, r, () => showResult(id));
  }

  const off = onEvent((m) => {
    if (m.kind === "dups" && m.id === runId) showProgress(m);
    else if (m.kind === "dups_done" && (m.id === runId || m.id === lastDupId)) showResult(m.id);
    // A finished search cannot have changed, and drawing it again would lose the sort and the rows shown.
    else if (m.kind === "reopened" && lastDupId != null && (dupStatus == null || dupStatus === "running")) showResult(lastDupId);
  });
  (async () => {
    // After a reload the page has forgotten the run; the server hasn't.
    if (lastDupId == null) {
      try { const { runs } = await api("/api/dups"); if (runs.length) lastDupId = runs[runs.length - 1].id; } catch { /* offline */ }
    }
    if (lastDupId != null) showResult(lastDupId);
  })();
  return off;
};

// Browsers play some codecs and not others; the comparison player needs its right-hand
// (master) file playable, so the likelier one goes there.
function playRank(codec) { return { av1: 0, h264: 1, vp9: 1, vp8: 1, hevc: 3 }[codec] ?? 4; }

// Asks before something that can't be taken back with one click. Resolves true on "yes".
function confirmDialog({ title, body, action }) {
  return new Promise((resolve) => {
    const close = (v) => { bg.remove(); release(); resolve(v); };
    const cancel = h("button", { onclick: () => close(false) }, "Cancel");
    const bg = h("div", { class: "modal-bg", onclick: (e) => { if (e.target === bg) close(false); } },
      h("div", { class: "modal narrow" },
        h("div", { class: "head" }, h("strong", {}, title)),
        h("div", { class: "body" }, body),
        h("div", { class: "foot" }, cancel, h("button", { class: "danger", onclick: () => close(true) }, action))));
    document.body.append(bg);
    // Enter on opening does the harmless thing.
    const release = holdFocus(bg, () => close(false), cancel);
  });
}

function renderDups(out, run, reload) {
  const rep = run.report, files = rep.files;
  let filter = prefs.get("dupFilter", "same");
  let limit = 60;
  // Files moved to the trash since the search are out of the picture; a group needs two copies.
  const gone = new Set(run.trashed || []);
  for (const p of gone) dupMarks.delete(p);
  const groups = rep.groups.map((g) => ({ ...g, members: g.members.filter((i) => !gone.has(files[i].path)), keepGone: gone.has(files[g.members[0]].path) }))
    .filter((g) => g.members.length > 1);
  // A group that needs a look first has no spare copies to count or to mark in one go.
  const spares = groups.filter((g) => g.kind === "same" && !g.look_first).flatMap((g) => g.members.slice(1));
  const s = gone.size ? {
    same_groups: groups.filter((g) => g.kind === "same").length, partial_groups: groups.filter((g) => g.kind !== "same").length,
    spare_files: spares.length, spare_bytes: spares.reduce((a, i) => a + files[i].size, 0),
  } : rep.stats;
  const sizeOf = new Map(files.filter((f) => !gone.has(f.path)).map((f) => [f.path, f.size]));
  const list = h("div");
  const markInfo = h("span", { class: "dim" });
  const marked = () => [...dupMarks].filter((p) => sizeOf.has(p)).sort();
  const copyBtn = h("button", { onclick: async () => {
    try { await navigator.clipboard.writeText(marked().join("\n") + "\n"); toast(`Copied ${marked().length} paths`); }
    catch (e) { toast("Could not copy: " + e.message, true); }
  } }, "Copy paths");
  const saveBtn = h("button", { onclick: () => {
    const url = URL.createObjectURL(new Blob([marked().join("\n") + "\n"], { type: "text/plain" }));
    const a = h("a", { href: url, download: "av1opt-duplicates.txt" });
    document.body.append(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 10000);
  } }, "Save list…");
  const clearBtn = h("button", { onclick: () => { dupMarks.clear(); draw(); } }, "Clear marks");
  // Groups where every copy left is marked: the server refuses those too.
  const emptied = () => groups.filter((g) => g.members.every((i) => dupMarks.has(files[i].path)));
  const failures = h("div");
  let trashing = false;
  const trashBtn = h("button", { class: "danger", onclick: async () => {
    const m = marked(), total = m.reduce((a, p) => a + sizeOf.get(p), 0);
    const ok = await confirmDialog({
      title: `Move ${plural(m.length, "file")} to the trash?`,
      action: "Move to trash",
      body: [
        h("p", {}, `${bytes(total)} in ${plural(m.length, "file")}. Each goes to the trash of the drive it is on, where your file manager can put it back; the space is freed when you empty the trash. Another copy of every video stays where it is.`),
        h("div", { class: "mono trash-list" }, m.slice(0, 12).join("\n")),
        m.length > 12 ? h("p", { class: "dim" }, `… and ${m.length - 12} more`) : null],
    });
    if (!ok) return;
    trashBtn.disabled = trashing = true;
    try {
      let r = await api(`/api/dups/${run.id}/trash`, { paths: m });
      if (r.needs_zfs) {
        const z = r.needs_zfs;
        const yes = await confirmDialog({
          title: `${plural(z.length, "file")} on ZFS`,
          action: "Move to trash on ZFS too",
          body: [
            h("p", { class: "warn" }, `${plural(z.length, "file")} of these ${z.length === 1 ? "is" : "are"} stored on ZFS, which av1opt writes to only when asked. The trash of that drive is on it too. Nothing has been moved yet.`),
            h("div", { class: "mono trash-list" }, z.slice(0, 12).join("\n")),
            z.length > 12 ? h("p", { class: "dim" }, `… and ${z.length - 12} more`) : null],
        });
        if (!yes) return;
        r = await api(`/api/dups/${run.id}/trash`, { paths: m, allow_zfs: true });
      }
      for (const p of r.trashed) dupMarks.delete(p);
      toast(`${plural(r.trashed.length, "file")} moved to the trash (${bytes(r.bytes)})`, r.failed.length > 0);
      if (r.failed.length) pendingTrashFailures = r.failed;
      reload();
    } catch (e) { toast(e.message, true); } finally { trashing = false; updateMarks(); }
  } }, "Move to trash…");
  if (pendingTrashFailures) {
    failures.append(h("div", { class: "panel" }, h("strong", { class: "bad" }, `${plural(pendingTrashFailures.length, "file")} could not be moved to the trash and are still where they were`),
      pendingTrashFailures.slice(0, 50).map((f) => h("div", { class: "dim mono" }, `${f.path}: ${f.error}`))));
    pendingTrashFailures = null;
  }
  const shown = () => groups.filter((g) => filter === "all" || (filter === "same" ? g.kind === "same" : g.kind !== "same"));
  const updateMarks = () => {
    const m = marked();
    const e = emptied().length;
    markInfo.textContent = m.length ? `${m.length} file${m.length === 1 ? "" : "s"} marked · ${bytes(m.reduce((a, p) => a + sizeOf.get(p), 0))}` : "Nothing marked";
    copyBtn.disabled = saveBtn.disabled = clearBtn.disabled = !m.length;
    trashBtn.disabled = trashing || !m.length || e > 0;
    trashBtn.title = e ? `${plural(e, "video")} would have no copy left: untick one copy first` : "";
  };

  const card = (g) => {
    const members = g.members.map((i) => files[i]);
    const [label, cls] = DUP_KIND[g.kind] || [g.kind, ""];
    // The same moment in every copy, so the thumbnails can be compared at a glance: the middle
    // of the shortest, because videos that share an opening look alike at its start.
    const at = Math.min(...members.map((m) => m.duration_s)) * 0.5;
    const thumbAt = (m) => (g.kind === "same_end" ? Math.max(0, m.duration_s - at) : at);
    const spare = g.kind === "same" && !g.look_first ? members.slice(1).reduce((a, m) => a + m.size, 0) : 0;
    const allMarked = h("span", { class: "bad", hidden: true }, "Every copy is marked: nothing would be left");
    const sync = () => { allMarked.hidden = !members.every((m) => dupMarks.has(m.path)); };
    const rows = members.map((m, k) => {
      const row = h("div", { class: "dup-file" + (dupMarks.has(m.path) ? " marked" : "") });
      const cb = h("input", { type: "checkbox", checked: dupMarks.has(m.path), title: "Mark this file for removal", onchange: () => {
        if (cb.checked) dupMarks.add(m.path); else dupMarks.delete(m.path);
        row.classList.toggle("marked", cb.checked); sync(); updateMarks();
      } });
      let compare = null;
      if (k > 0) {
        const keep = members[0];
        const [left, right] = playRank(keep.codec) <= playRank(m.codec) ? [m, keep] : [keep, m];
        compare = h("a", { class: "btn small", href: `#/compare?left=${encodeURIComponent(left.path)}&right=${encodeURIComponent(right.path)}&kind=${g.kind}` }, "Compare");
      }
      // The first copy of a cut pair is not always the longer one: an avoided path goes second.
      const longer = g.kind !== "same" && members.length > 1 && members.every((o) => o === m || m.duration_s > o.duration_s + 1);
      const img = h("img", { class: "dup-thumb", loading: "lazy", alt: "", src: `/thumb?path=${encodeURIComponent(m.path)}&t=${thumbAt(m).toFixed(2)}` });
      img.addEventListener("error", () => img.classList.add("missing"));
      row.append(...[cb, img,
        h("div", { class: "dup-info" },
          h("div", { class: "name", title: m.path }, h("span", { class: "dim" }, dirname(m.path) + "/"), basename(m.path)),
          h("div", { class: "dim" }, `${m.codec} · ${m.width}×${m.height}${m.picture_width !== m.width || m.picture_height !== m.height ? ` (picture ${m.picture_width}×${m.picture_height})` : ""} · ${num(m.video_mbps, 1)} Mbps · ${hms(m.duration_s)} · ${bytes(m.size)} · ${plural(m.sound_tracks, "sound track")}${m.subtitle_tracks ? ` · ${plural(m.subtitle_tracks, "subtitle track")}` : ""}`)),
        g.kind === "same" && k === 0 && !g.keepGone ? h("span", { class: "pill ok", title: g.reason }, "Suggested keep") : null,
        longer ? h("span", { class: "pill ok", title: k === 0 ? g.reason : "The longest of these copies" }, "Longer") : null,
        m.identical != null ? h("span", { class: "pill", title: "Same size and the same bytes at the start, middle and end" }, "Identical bytes") : null,
        m.damaged ? h("span", { class: "pill bad", title: "Frames of this file could not be read: it is damaged, or stops before its stated length" }, "Damaged or incomplete") : null,
        compare].filter(Boolean));
      return row;
    });
    sync();
    return h("div", { class: "dup-group" },
      h("div", { class: "head" }, h("span", { class: "pill " + cls }, label), h("span", {}, `${floorPct(g.score)} of the compared frames match`),
        g.kind === "same" && g.reason && !g.keepGone ? h("span", { class: "dim" }, `keep: ${g.reason}`) : null, allMarked,
        h("span", { class: "spacer" }), spare ? h("span", { class: "ok" }, `frees ${bytes(spare)}`) : null),
      g.look_first ? h("div", { class: "warn" }, "Look before removing any of these: they are not marked as spare copies") : null,
      (g.differences || []).map((d) => h("div", { class: "dim" }, d)),
      rows);
  };
  const draw = () => {
    const v = shown();
    list.replaceChildren(...v.slice(0, limit).map(card));
    if (v.length > limit) list.append(h("div", { class: "panel dim" }, `${v.length - limit} more groups not shown. `, h("button", { class: "small", onclick: () => { limit += 200; draw(); } }, "Show more")));
    if (!v.length) list.append(h("div", { class: "panel empty" }, groups.length ? "Nothing in this view." : gone.size ? "No duplicates left." : "No duplicates found."));
    updateMarks();
  };
  const filt = h("select", { onchange: (e) => { filter = e.target.value; prefs.set("dupFilter", filter); limit = 60; draw(); } },
    h("option", { value: "same" }, `Same video (${s.same_groups})`), h("option", { value: "partial" }, `Same start or ending (${s.partial_groups})`), h("option", { value: "all" }, "All groups"));
  filt.value = filter;
  const markSpares = h("button", { title: "Marks every copy except the suggested one, in the groups of the same video", onclick: () => {
    for (const g of groups) if (g.kind === "same" && !g.look_first) for (const i of g.members.slice(1)) dupMarks.add(files[i].path);
    draw();
  } }, "Mark all spare copies");

  out.replaceChildren(...[
    h("div", { class: "stats" },
      h("div", { class: "stat" }, h("div", { class: "v" }, s.same_groups), h("div", { class: "l" }, "videos stored more than once")),
      h("div", { class: "stat" }, h("div", { class: "v" }, s.spare_files), h("div", { class: "l" }, "spare copies")),
      h("div", { class: "stat" }, h("div", { class: "v ok" }, bytes(s.spare_bytes)), h("div", { class: "l" }, "freed by removing the spares")),
      h("div", { class: "stat" }, h("div", { class: "v" }, s.partial_groups), h("div", { class: "l" }, "pairs sharing only a start or an ending"))),
    h("div", { class: "panel" },
      h("div", { class: "row" }, filt, markSpares, h("span", { class: "spacer" }), markInfo, clearBtn, copyBtn, saveBtn, trashBtn),
      h("p", { class: "dim" }, "Tick the files you want gone, then move them to the trash, or copy or save the list of their paths. av1opt deletes nothing: trashed files can be put back from your file manager until you empty the trash, and the last copy of a video is never trashed. “Same start / ending” pairs have different lengths: one is cut, or two videos share an intro or credits, so look before you tick."),
      gone.size ? h("p", {}, `${plural(gone.size, "file")} of this result ${gone.size === 1 ? "is" : "are"} in the trash now (${bytes(files.filter((f) => gone.has(f.path)).reduce((a, f) => a + f.size, 0))}).`) : null),
    failures,
    list,
    rep.unreadable.length ? h("details", { class: "panel more" }, h("summary", {}, `${rep.unreadable.length} video${rep.unreadable.length === 1 ? "" : "s"} could not be compared`),
      h("div", { class: "tablewrap" }, h("table", {}, h("tbody", {}, rep.unreadable.slice(0, 500).map((u) => h("tr", {}, h("td", { class: "path" }, u.path), h("td", { class: "dim" }, u.error))))))) : null].filter(Boolean));
  draw();
}

// ----- benchmark -----

const CLASS_LABEL = { sd: "720p and below", hd: "1080p", uhd: "above 1080p" };
function configLabel(c) { return c ? `preset ${c.preset} · ${c.params || "SVT-AV1 defaults"}` : "–"; }
views.bench = (root) => {
  const st = (STATE && STATE.settings) || {};
  const roots = new Set(prefs.get("benchRoots", st.bench_paths && st.bench_paths.length ? st.bench_paths : st.scan_paths || []));
  const f = Object.assign({ presets: null, params: null, vmaf: 93, videos: 6, samples: 2, secs: 6 }, prefs.get("benchForm", {}));
  const chips = h("div", { class: "chips" });
  const presetBoxes = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12].map((p) => [p, h("input", { type: "checkbox" })]);
  const params = h("textarea", { rows: "5", spellcheck: "false", class: "mono" });
  const vmaf = h("input", { type: "number", step: "0.5", min: "50", max: "99.5", value: f.vmaf });
  const videos = h("input", { type: "number", min: "1", max: "50", value: f.videos });
  const samples = h("input", { type: "number", min: "1", max: "8", value: f.samples });
  const secs = h("input", { type: "number", min: "2", max: "30", value: f.secs });
  const refresh = h("input", { type: "checkbox" });
  const planText = h("p", { class: "dim" });
  const start = h("button", { class: "primary" }, "Start benchmark");
  const cancel = h("button", { hidden: true }, "Stop");
  const progress = h("div", { class: "bar grow" }, h("div"));
  const progText = h("span", { class: "dim" });
  const out = h("div");
  const ui = { tab: prefs.get("benchTab", "all"), video: 0, sort: "size", dir: 1, stopChart: null };
  let base = { preset: 5, params: "tune=0" };
  let defaults = null;
  let running = false;
  let seenVideos = -1;
  let etaStart = null;
  let gone = false;

  const drawChips = () => {
    chips.replaceChildren(...[...roots].map((r) => h("span", { class: "chip" }, h("code", {}, r), h("button", { title: "Remove", onclick: () => { roots.delete(r); prefs.set("benchRoots", [...roots]); drawChips(); } }, "×"))));
    if (!roots.size) chips.append(h("span", { class: "dim" }, "No folders or videos yet."));
    start.disabled = !roots.size || running;
  };
  drawChips();

  const form = () => ({
    presets: presetBoxes.filter(([, b]) => b.checked).map(([p]) => p),
    param_sets: params.value.split("\n").map((x) => x.trim()).filter(Boolean),
  });
  // The server builds the same list: the default setting, then every preset with every set.
  // It takes its own parameter sets when none is given, and refuses to start without a preset.
  const plan = () => {
    const { presets, param_sets } = form();
    if (!presets.length) { planText.textContent = "No preset is ticked: tick at least one under “What to measure”."; return; }
    const sets = param_sets.length ? param_sets : (defaults ? defaults.param_sets : []);
    const seen = new Set([`${base.preset} ${base.params}`]);
    for (const p of presets) for (const x of sets) seen.add(`${p} ${x}`);
    const n = seen.size, v = Number(videos.value) || 0;
    const which = presets.length > 1 ? `presets ${presets.slice(0, -1).join(", ")} and ${presets[presets.length - 1]}` : `preset ${presets[0]}`;
    planText.textContent = `${plural(n, "setting")}: ${which} with ${plural(sets.length, "parameter set")}${presets.length > 1 ? " each" : ""}, and the default (preset ${base.preset} · ${base.params}), which is always one. With at most ${plural(v, "video")} that is up to around ${n * v * 4} sample encodes of ${samples.value} × ${secs.value} s. Slow presets and big videos take the longest.`;
  };
  for (const el of [params, videos, samples, secs, ...presetBoxes.map(([, b]) => b)]) el.addEventListener("input", plan);

  const showRun = (run) => {
    running = !!run && run.status === "running";
    cancel.hidden = !running;
    start.disabled = running || !roots.size;
    if (!run) { progText.textContent = ""; return; }
    if (!running) {
      etaStart = null;
      progress.firstChild.style.width = run.status === "done" ? "100%" : "0%";
      progText.textContent = run.status === "done" ? "finished" : run.status === "failed" ? "failed" : "stopped: what was measured is remembered, and starting again carries on from there";
      return;
    }
    const c = run.current || {}, total = run.cells_total;
    progress.firstChild.style.width = (total ? 100 * run.cells_done / total : 0) + "%";
    if (run.phase === "probing") { progText.textContent = c.total ? `reading videos ${c.done} / ${c.total}` : "listing files…"; return; }
    if (!etaStart || run.cells_done < etaStart.done) etaStart = { t: performance.now(), done: run.cells_done };
    const el = (performance.now() - etaStart.t) / 1000, n = run.cells_done - etaStart.done;
    const eta = n >= 2 && el > 15 ? ` · about ${hms((total - run.cells_done) * el / n)} left` : "";
    const vid = c.video != null ? `video ${c.video + 1} of ${run.videos_total}` : "starting";
    let now = vid;
    if (run.phase === "sampling" && c.path) now = `${vid}: cutting sample clips of ${basename(c.path)}`;
    else if (c.config != null) now = `${vid} · ${configLabel(run.configs[c.config])} · CRF ${c.crf} gave VMAF ${num(c.vmaf, 1)}${c.cached ? " (remembered)" : ""}`;
    progText.textContent = `${now} · ${run.cells_done} / ${total} done${eta}`;
  };

  async function load() {
    let d;
    try { d = await api("/api/bench"); } catch (e) { toast(e.message, true); return; }
    if (gone) return;
    if (!defaults) {
      defaults = d.defaults;
      base = defaults.baseline;
      const on = new Set(f.presets || defaults.presets);
      for (const [p, b] of presetBoxes) b.checked = on.has(p);
      params.value = f.params != null ? f.params : defaults.param_sets.join("\n");
      plan();
    }
    seenVideos = d.run ? d.run.videos_done : -1;
    showRun(d.run);
    renderBench(out, d, ui);
  }

  start.onclick = async () => {
    const { presets, param_sets } = form();
    const numbers = readNumbers([[vmaf, "Target VMAF"], [videos, "Videos", true], [samples, "Clips per video", true], [secs, "Seconds per clip"]]);
    if (!numbers) return;
    const [targetVmaf, maxVideos, clips, clipSecs] = numbers;
    start.disabled = true;
    try {
      await api("/api/bench", { paths: [...roots], presets, param_sets, target_vmaf: targetVmaf, max_videos: maxVideos,
        samples: clips, sample_secs: clipSecs, refresh: refresh.checked });
      prefs.set("benchForm", { presets, params: params.value, vmaf: targetVmaf, videos: maxVideos, samples: clips, secs: clipSecs });
      refresh.checked = false;
      etaStart = null;
      progress.firstChild.style.width = "0%";
      progText.textContent = "listing files…";
      await load();
    } catch (e) { toast(e.message, true); start.disabled = !roots.size || running; }
  };
  cancel.onclick = async () => {
    cancel.disabled = true;
    try { await api("/api/bench/cancel", {}); } catch (e) { toast(e.message, true); }
    cancel.disabled = false;
  };

  root.append(
    h("h1", {}, "Benchmark encoder settings"),
    h("div", { class: "panel" },
      h("div", { class: "row" }, chips, h("span", { class: "spacer" }),
        h("button", { onclick: async () => {
          const p = await pick({ title: "Folders or videos to measure on", mode: "multi" });
          if (p) { for (const x of p) roots.add(x); prefs.set("benchRoots", [...roots]); drawChips(); }
        } }, "Add folders or videos…")),
      h("p", { class: "dim" }, "Finds out which SVT-AV1 preset and parameters give the smallest files for the encode time on your own videos. A few videos are picked across the folders and sizes; short clips of each are encoded under every setting until the target VMAF is met, and the size and time needed are compared with the default. Read-only for these folders. Encodes in the queue wait while it runs, so the times mean something; best run on an otherwise idle machine."),
      h("details", { class: "more" }, h("summary", {}, "What to measure"),
        h("div", { class: "form-grid" },
          setting("Presets", h("div", { class: "checks-row" }, presetBoxes.map(([p, b]) => h("label", { class: "check" }, b, String(p)))), "Lower is slower and smaller"),
          setting("Parameter sets", params, "One per line, SVT-AV1 options as name=value:name=value. Every preset is tried with every line"),
          setting("Target VMAF", vmaf, "Settings are compared at this quality; use the one you encode at"),
          setting("Videos", videos, "At most this many, spread over the folders and sizes"),
          setting("Clips per video", h("span", { class: "unit" }, samples, "of", secs, "seconds"), "More and longer clips are closer to a whole encode and take longer"),
          setting("Measure again", h("label", { class: "check" }, refresh, "Ignore remembered sample encodes"), "Only needed when the machine was busy during the last run"))),
      planText,
      h("div", { class: "row" }, progress, progText, cancel, start)),
    out);

  const off = onEvent((m) => {
    if (m.kind === "bench") {
      showRun(m.run);
      // A video is finished: its numbers can be shown.
      if (m.run.videos_done !== seenVideos) { seenVideos = m.run.videos_done; load(); }
    } else if (m.kind === "bench_done" || m.kind === "reopened") load();
  });
  load();
  return () => { gone = true; off(); if (ui.stopChart) ui.stopChart(); };
};

// Settings as dots: left is faster, down is smaller. One colour says "worth considering"
// (no other setting is both smaller and faster); the default is the diamond.
function benchChart(sum, rep, onHover) {
  const wrap = h("div", { class: "bench-chart" });
  const tip = h("div", { class: "tooltip", hidden: true });
  const pts = sum.rows.filter((r) => r.size > 0 && r.time > 0);
  const marks = new Map();
  let width = 0;

  const tipFor = (r) => [
    h("strong", {}, configLabel(rep.configs[r.config])),
    h("div", {}, `size ${rel(r.size)}${sum.videos > 1 ? ` (per video ${rel(r.size_best, 0)} to ${rel(r.size_worst, 0)})` : ""}`),
    h("div", {}, `encode time ${times(r.time)} · CRF ${num(r.crf, 1)}`),
    h("div", { class: "dim" }, [r.config === rep.baseline ? "the default" : null,
      r.missing ? `no result on ${plural(r.missing, "video")}` : r.pareto ? "nothing is both smaller and faster" : null].filter(Boolean).join(" · ")),
  ];

  const draw = () => {
    const W = width = Math.max(360, Math.floor(wrap.clientWidth) || 900), H = 380;
    const m = { l: 50, r: 22, t: 30, b: 46 };
    const pw = W - m.l - m.r, ph = H - m.t - m.b;
    const ts = pts.map((r) => r.time).concat(1), ss = pts.map((r) => r.size).concat(1);
    let t0 = Math.min(...ts), t1 = Math.max(...ts);
    const padT = Math.max(1.12, Math.pow(t1 / t0, 0.08));
    t0 /= padT; t1 *= padT;
    let s0 = Math.min(...ss), s1 = Math.max(...ss);
    const padS = Math.max(s1 - s0, 0.04) * 0.12 + Math.max(0, 0.04 - (s1 - s0)) / 2;
    s0 -= padS; s1 += padS;
    const x = (t) => m.l + Math.log(t / t0) / Math.log(t1 / t0) * pw;
    const y = (v) => m.t + (s1 - v) / (s1 - s0) * ph;
    const svg = s("svg", { width: W, height: H, viewBox: `0 0 ${W} ${H}`, role: "img",
      "aria-label": `File size against encode time for ${pts.length} encoder settings, both relative to the default. The table below has the same numbers.` });
    const text = (str, attrs) => { const t = s("text", attrs); t.textContent = str; svg.append(t); return t; };

    // Size axis: a step that gives at most 7 lines.
    const step = [0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1].find((d) => (s1 - s0) / d <= 7) || 2;
    for (let v = Math.ceil(s0 / step) * step; v <= s1 + 1e-9; v += step) {
      svg.append(s("line", { class: "grid", x1: m.l, x2: m.l + pw, y1: y(v), y2: y(v) }));
      text(rel(v, step < 0.01 ? 1 : 0), { x: m.l - 8, y: y(v) + 4, "text-anchor": "end" });
    }
    // Time axis (log): the coarsest family of round factors that gives a few lines.
    const families = [
      [0.03125, 0.0625, 0.125, 0.25, 0.5, 1, 2, 4, 8, 16, 32, 64],
      [0.125, 0.18, 0.25, 0.35, 0.5, 0.7, 1, 1.4, 2, 2.8, 4, 5.6, 8],
      Array.from({ length: 40 }, (_, i) => (i + 1) / 10),
      Array.from({ length: 60 }, (_, i) => (i + 1) / 20),
    ];
    const inside = (fam) => fam.filter((v) => v >= t0 && v <= t1);
    const ticks = [1];
    for (const v of inside(families.find((fam) => inside(fam).length >= 4) || families[3])) {
      if (!ticks.some((u) => Math.abs(x(u) - x(v)) < 44)) ticks.push(v);
    }
    for (const v of ticks) {
      svg.append(s("line", { class: "grid", x1: x(v), x2: x(v), y1: m.t, y2: m.t + ph }));
      text(v + "×", { x: x(v), y: m.t + ph + 16, "text-anchor": "middle" });
    }
    svg.append(s("line", { class: "base", x1: x(1), x2: x(1), y1: m.t, y2: m.t + ph }), s("line", { class: "base", x1: m.l, x2: m.l + pw, y1: y(1), y2: y(1) }));
    text("File size against the default (lower is smaller)", { x: 6, y: 14 });
    text("Encode time against the default, log scale (left is faster)", { x: m.l + pw, y: H - 8, "text-anchor": "end" });

    // The front as steps: everything above and right of it is beaten by a setting on it.
    const front = pts.filter((r) => r.pareto).sort((a, b) => a.time - b.time);
    if (front.length) {
      let d = `M${x(front[0].time)},${m.t}V${y(front[0].size)}`;
      for (const r of front.slice(1)) d += `H${x(r.time)}V${y(r.size)}`;
      svg.append(s("path", { class: "front", d: d + `H${m.l + pw}` }));
    }

    marks.clear();
    const order = [...pts].sort((a, b) => (a.pareto ? 1 : 0) - (b.pareto ? 1 : 0) || (a.config === rep.baseline ? 1 : 0) - (b.config === rep.baseline ? 1 : 0));
    for (const r of order) {
      const isBase = r.config === rep.baseline;
      const g = s("g", { class: "pt" + (r.missing ? " part" : r.pareto ? " on" : ""), transform: `translate(${x(r.time)},${y(r.size)})`, tabindex: 0,
        "aria-label": `${configLabel(rep.configs[r.config])}: size ${rel(r.size)}, encode time ${times(r.time)}` });
      g.append(s("circle", { class: "hit", r: 14 }), isBase ? s("path", { class: "m", d: "M0,-7L7,0L0,7L-7,0Z" }) : s("circle", { class: "m", r: r.pareto ? 5 : 4 }));
      const show = () => {
        tip.replaceChildren(...tipFor(r));
        tip.hidden = false;
        const px = x(r.time), py = y(r.size), half = tip.offsetWidth / 2;
        tip.style.left = Math.min(Math.max(px, half + 2), Math.max(half + 2, width - half - 2)) + "px";
        tip.style.top = py + "px";
        tip.style.transform = py < 110 ? "translate(-50%, 14px)" : "translate(-50%, calc(-100% - 12px))";
        onHover(r.config);
      };
      const hide = () => { tip.hidden = true; onHover(null); };
      g.addEventListener("pointerenter", show);
      g.addEventListener("pointerleave", hide);
      g.addEventListener("focus", show);
      g.addEventListener("blur", hide);
      svg.append(g);
      marks.set(r.config, g);
    }

    // Names for the settings worth a look, where they fit without covering anything.
    const taken = pts.map((r) => ({ x0: x(r.time) - 8, x1: x(r.time) + 8, y0: y(r.size) - 8, y1: y(r.size) + 8, of: r.config }));
    const named = pts.filter((r) => r.pareto || r.config === rep.baseline).sort((a, b) => (b.config === rep.baseline) - (a.config === rep.baseline) || a.size - b.size);
    for (const r of named) {
      const label = r.config === rep.baseline ? `default (${configLabel(rep.configs[r.config])})` : configLabel(rep.configs[r.config]);
      const w = label.length * 6.1 + 4, px = x(r.time), py = y(r.size);
      // Left and below first: the front's line leaves a point upwards and to the right.
      const spots = [
        { x0: px - 11 - w, y0: py - 7, anchor: "end", tx: px - 11 },
        { x0: px - w / 2, y0: py + 11, anchor: "middle", tx: px },
        { x0: px + 11, y0: py - 7, anchor: "start", tx: px + 11 },
        { x0: px - w / 2, y0: py - 24, anchor: "middle", tx: px },
      ];
      for (const sp of spots) {
        const box = { x0: sp.x0, x1: sp.x0 + w, y0: sp.y0, y1: sp.y0 + 14 };
        if (box.x0 < m.l + 2 || box.x1 > W - 2 || box.y0 < m.t || box.y1 > m.t + ph) continue;
        if (taken.some((o) => o.of !== r.config && box.x0 < o.x1 && box.x1 > o.x0 && box.y0 < o.y1 && box.y1 > o.y0)) continue;
        taken.push({ ...box, of: -1 });
        text(label, { class: "lbl", x: sp.tx, y: sp.y0 + 11, "text-anchor": sp.anchor });
        break;
      }
    }
    wrap.replaceChildren(svg, tip);
  };

  const ro = new ResizeObserver(() => { const w = Math.floor(wrap.clientWidth); if (w && Math.max(360, w) !== width) draw(); });
  ro.observe(wrap);
  return {
    el: wrap,
    highlight: (config) => { for (const [c, g] of marks) g.classList.toggle("hi", c === config); },
    stop: () => ro.disconnect(),
  };
}

function renderBench(out, data, ui) {
  if (ui.stopChart) { ui.stopChart(); ui.stopChart = null; }
  const rep = data.report, run = data.run, tuning = data.tuning || [];
  const kids = [];
  if (run && run.status === "failed") kids.push(h("div", { class: "panel bad" }, run.error || "The benchmark failed."));
  if (run && run.note) kids.push(h("div", { class: "panel warn" }, run.note));

  const setTuning = async (classes, config, done) => {
    try {
      const r = await api("/api/bench/tuning", { classes, config });
      data.tuning = r.tuning;
      if (STATE) STATE.tuning = r.tuning;
      renderBench(out, data, ui);
      toast(done);
    } catch (e) { toast(e.message, true); }
  };
  const tunedPanel = h("div", { class: "panel" },
    h("div", { class: "row" }, h("h3", { class: "flat" }, "Tuned settings"), h("span", { class: "spacer" }),
      tuning.length > 1 ? h("button", { class: "small", onclick: () => setTuning([], null, "Tuned settings forgotten") }, "Forget all") : null),
    tuning.map((t) => {
      const c = t.choice;
      return h("div", { class: "tuned-row" },
        h("strong", {}, t.label), h("code", {}, configLabel(c)),
        h("span", { class: "dim grow" }, c.videos ? `${rel(c.size)} size, ${times(c.time)} encode time against the default · measured on ${plural(c.videos, "video")} at VMAF ${c.target_vmaf} · ${when(c.made)}` : "set by hand"),
        h("button", { class: "small", onclick: () => setTuning([t.class], null, `Tuned settings for ${t.label} forgotten`) }, "Forget"));
    }),
    h("p", { class: "dim" }, tuning.length
      ? "Encodes with “Use tuned settings” ticked take the preset and parameters from here by the video's size. Sizes not listed use what the encode dialog says."
      : "None yet. When a result is in, “Use for…” on a setting makes it the one for videos of that size; encodes with “Use tuned settings” ticked then take it."));

  if (!rep) {
    kids.push(tunedPanel);
    if (!run || run.status !== "running") kids.push(h("div", { class: "panel empty" }, "No result yet. Add a folder and start: the default settings take a while but need no choices."));
    out.replaceChildren(...kids);
    return;
  }

  const vids = rep.videos || [], configs = rep.configs, base = rep.baseline;
  const meta = rep.partial
    ? `So far: ${plural(vids.length, "video")}${run && run.videos_total ? ` of ${run.videos_total}` : ""} measured · ${plural(configs.length, "setting")} · target VMAF ${rep.target_vmaf}`
    : `Measured ${when(rep.made)} · ${plural(vids.length, "video")} · ${plural(configs.length, "setting")} · target VMAF ${rep.target_vmaf} · ${rep.fresh + rep.cached} sample encodes (${rep.cached} remembered) in ${hms(rep.elapsed_secs)}`;
  kids.push(h("p", { class: "dim", title: rep.encoder || "" }, meta));

  // With one size class, "all" and that class are the same numbers.
  let sums = rep.summaries || [];
  if (sums.length === 2) sums = [sums[1]];
  const keyOf = (x) => x.class || "all";
  const sum = sums.find((x) => keyOf(x) === ui.tab) || sums[0];
  if (!sum) {
    kids.push(h("div", { class: "panel" }, "Nothing to compare yet: the default setting has no result on any video measured. The table per video below says why."));
  } else {
    const tabs = sums.length > 1 ? h("div", { class: "tabs" }, sums.map((x) => h("button", {
      class: x === sum ? "active" : "", onclick: () => { ui.tab = keyOf(x); prefs.set("benchTab", ui.tab); renderBench(out, data, ui); },
    }, `${x.class ? CLASS_LABEL[x.class] : "All sizes"} · ${plural(x.videos, "video")}`))) : null;
    const rowOf = (i) => sum.rows.find((r) => r.config === i);
    const tile = (name, i, byTime) => {
      const r = rowOf(i), better = r && i !== base;
      return h("div", { class: "stat" },
        h("div", { class: "v" }, better ? (byTime ? times(r.time) : rel(r.size)) : "–"),
        h("div", { class: "l" }, better
          ? [name, ": ", h("code", {}, configLabel(configs[i])), byTime ? ` · ${rel(r.size)} size` : ` · ${times(r.time)} encode time`]
          : `${name}: nothing measured beats the default`));
    };
    const pickNames = [["smallest", "smallest"], ["balanced", "balanced"], ["fastest", "fastest"]];
    const bodyRows = new Map();
    const chart = benchChart(sum, rep, (config) => { for (const [c, tr] of bodyRows) tr.classList.toggle("hi", c === config); });
    ui.stopChart = chart.stop;

    const classes = sum.class ? [sum.class] : sums.filter((x) => x.class).map((x) => x.class);
    const inUse = (c) => classes.length > 0 && classes.every((k) => { const t = tuning.find((x) => x.class === k); return t && t.choice.preset === c.preset && t.choice.params === c.params; });
    const useLabel = sum.class ? `Use for ${CLASS_LABEL[sum.class]}` : "Use for all sizes";
    const sorted = [...sum.rows].sort((a, b) => {
      const ka = ui.sort === "setting" ? a.config : a[ui.sort], kb = ui.sort === "setting" ? b.config : b[ui.sort];
      if (ka == null || kb == null) return (ka == null) - (kb == null);
      return (ka - kb) * ui.dir || a.config - b.config;
    });
    const th = (label, key, title) => h("th", { class: "sort" + (key === "setting" ? "" : " num"), title, onclick: () => {
      if (ui.sort === key) ui.dir *= -1; else { ui.sort = key; ui.dir = 1; }
      renderBench(out, data, ui);
    } }, label + (ui.sort === key ? (ui.dir > 0 ? " ↑" : " ↓") : ""));
    const body = h("tbody", {}, sorted.map((r) => {
      const c = configs[r.config];
      const picks = pickNames.filter(([k]) => sum.picks[k] === r.config && r.config !== base).map(([, n]) => h("span", { class: "pill ok" }, n));
      const tr = h("tr", { onmouseenter: () => chart.highlight(r.config), onmouseleave: () => chart.highlight(null) },
        h("td", {}, h("span", { class: "dot" + (r.missing ? " part" : r.pareto ? " on" : ""), title: r.missing ? "" : r.pareto ? "Nothing is both smaller and faster" : "Another setting is both smaller and faster" }),
          h("code", {}, configLabel(c)), r.config === base ? h("span", { class: "pill" }, "default") : null, picks),
        h("td", { class: "num" }, rel(r.size)),
        h("td", { class: "num dim" }, sum.videos > 1 && r.size != null ? `${rel(r.size_best, 0)} to ${rel(r.size_worst, 0)}` : "–"),
        h("td", { class: "num" }, times(r.time)),
        h("td", { class: "num" }, r.speed ? num(r.speed, 2) : "–"),
        h("td", { class: "num" }, r.crf ? num(r.crf, 1) : "–"),
        h("td", {}, r.missing ? h("span", { class: "pill warn", title: "It failed there or could not reach the target; the numbers cover the other videos" }, `no result on ${plural(r.missing, "video")}`)
          : inUse(c) ? h("span", { class: "pill ok" }, "in use")
          : rep.partial ? null
          : h("button", { class: "small", onclick: () => setTuning(sum.class ? [sum.class] : [], r.config, `${configLabel(c)} is now used for ${sum.class ? CLASS_LABEL[sum.class] : "all sizes measured"}`) }, useLabel)));
      bodyRows.set(r.config, tr);
      return tr;
    }));
    const swatch = (cls, diamond) => { const v = s("svg", { viewBox: "-8 -8 16 16", class: "pt " + cls }); v.append(diamond ? s("path", { class: "m", d: "M0,-7L7,0L0,7L-7,0Z" }) : s("circle", { class: "m", r: 5 })); return v; };
    kids.push(h("div", { class: "panel bench-fig" },
      tabs,
      [sum.picks.smallest, sum.picks.balanced, sum.picks.fastest].every((i) => i === base)
        ? h("p", {}, "Nothing measured here beats the default, ", h("code", {}, configLabel(configs[base])), ": no setting is smaller without being slower, or faster without being bigger.")
        : h("div", { class: "stats" }, tile("Smallest files", sum.picks.smallest, false), tile("Smallest at the default's speed", sum.picks.balanced, false), tile("Fastest at the default's size", sum.picks.fastest, true)),
      h("div", { class: "legend" },
        h("span", {}, swatch("on"), "nothing is both smaller and faster"),
        h("span", {}, swatch(""), "beaten by another setting"),
        h("span", {}, swatch("", true), "the default"),
        sum.rows.some((r) => r.missing) ? h("span", {}, swatch("part"), "no result on some videos") : null),
      chart.el,
      sum.videos < 3 ? h("p", { class: "warn" }, `Only ${plural(sum.videos, "video")} here: one odd video can decide the order. More videos make it trustworthy.`) : null,
      sum.left_out ? h("p", { class: "warn" }, `${plural(sum.left_out, "video")} left out: the default setting failed or could not reach the target there.`) : null,
      h("div", { class: "tablewrap" }, h("table", {},
        h("thead", {}, h("tr", {}, th("Setting", "setting"), th("Size", "size", "Typical file size against the default at the target quality"),
          h("th", { class: "num", title: "The videos where it did best and worst" }, "Per video"), th("Encode time", "time", "Typical encode time against the default"),
          h("th", { class: "num", title: "Seconds of video encoded per second on this machine" }, "× real time"), h("th", { class: "num", title: "Mean CRF that meets the target" }, "CRF"), h("th", {}, ""))),
        body)),
      rep.partial ? h("p", { class: "dim" }, "A setting can be put to use once every video is measured.") : null,
      h("p", { class: "dim" }, "VMAF is the judge here. Settings tuned for metrics (tune=1, tune=2) tend to win on it; tune=0 aims at how the picture looks to people, which VMAF only partly sees. Before switching for a few percent, encode a clip both ways and look at them in Compare.")));
  }
  kids.push(tunedPanel);

  if (vids.length) {
    if (ui.video >= vids.length) ui.video = 0;
    const info = h("p", { class: "dim" });
    const vbody = h("tbody");
    const drawVideo = () => {
      const v = vids[ui.video];
      info.textContent = `${v.path} · ${v.codec} ${v.width}×${v.height} · ${num(v.fps, 2)} fps${v.hdr ? " · HDR" : ""} · ${hms(v.duration_s)}${v.source_bitrate ? ` · ${num(v.source_bitrate / 1e6, 2)} Mbps` : ""} · ${CLASS_LABEL[v.class] || v.class}`;
      if (v.error) { vbody.replaceChildren(h("tr", {}, h("td", { colspan: 8, class: "bad" }, v.error))); return; }
      const b = v.cells[base] && v.cells[base].point;
      vbody.replaceChildren(...configs.map((c, i) => {
        const cell = v.cells[i] || { trials: [] }, p = cell.point;
        const note = cell.error ? h("span", { class: "pill bad", title: cell.error }, "failed")
          : !p ? h("span", { class: "pill warn" }, rep.min_crf != null ? `below VMAF ${rep.target_vmaf} even at CRF ${rep.min_crf}` : "target out of reach")
          : p.capped ? h("span", { class: "pill", title: "The file could be smaller still; a higher CRF isn't tried" }, `VMAF ${num(p.vmaf, 1)} at the highest CRF`) : null;
        return h("tr", {},
          h("td", {}, h("code", {}, configLabel(c)), i === base ? h("span", { class: "pill" }, "default") : null),
          h("td", { class: "num" }, p ? num(p.crf, 1) : "–"),
          h("td", { class: "num" }, p ? num(p.bitrate / 1e6, 2) : "–"),
          h("td", { class: "num" }, p && b ? rel(p.bitrate / b.bitrate) : "–"),
          h("td", { class: "num" }, p ? num(p.cost, 2) : "–"),
          h("td", { class: "num" }, p && b ? times(p.cost / b.cost) : "–"),
          h("td", { class: "num" }, cell.trials.length),
          h("td", {}, note));
      }));
    };
    const sel = h("select", { onchange: (e) => { ui.video = Number(e.target.value); drawVideo(); } }, vids.map((v, i) => h("option", { value: i }, `${basename(v.path)} · ${v.width}×${v.height}`)));
    sel.value = ui.video;
    drawVideo();
    kids.push(h("div", { class: "panel" },
      h("details", { class: "more", open: ui.videoOpen, ontoggle: (e) => { ui.videoOpen = e.target.open; } }, h("summary", {}, "Each video on its own"),
        h("div", { class: "row" }, sel), info,
        h("div", { class: "tablewrap" }, h("table", {},
          h("thead", {}, h("tr", {}, h("th", {}, "Setting"), h("th", { class: "num" }, "CRF"), h("th", { class: "num" }, "Mbps"), h("th", { class: "num" }, "Size"),
            h("th", { class: "num", title: "Encode seconds per second of video" }, "Encode s/s"), h("th", { class: "num" }, "Encode time"), h("th", { class: "num", title: "Sample encodes it took to find the CRF" }, "Tries"), h("th", {}, ""))),
          vbody))),
      (rep.skipped || []).length ? h("details", { class: "more" }, h("summary", {}, `${plural(rep.skipped.length, "video")} found but not used`),
        h("div", { class: "tablewrap" }, h("table", {}, h("tbody", {}, rep.skipped.map((x) => h("tr", {}, h("td", { class: "path" }, x.path), h("td", { class: "dim" }, x.reason))))))) : null));
  }
  out.replaceChildren(...kids);
}

// ----- queue -----

const PHASES = { probing: "Probing", sampling: "Cutting samples", searching: "Searching CRF", scenes: "Finding scenes", chunks: "Encoding chunks", encoding: "Encoding", muxing: "Muxing", verifying: "Verifying", repairing: "Encoding weak stretches again" };
const STATUS = {
  pending: ["Waiting", ""], running: ["Running", "run"], ok: ["Done", "ok"], skip: ["Skipped", "warn"], error: ["Failed", "bad"],
  done_before: ["Done earlier", "ok"], cancelled: ["Cancelled", ""], queued: ["Queued", ""], done: ["Finished", "ok"], failed: ["Failed", "bad"],
};
function statusPill(st) { const [l, c] = STATUS[st] || [st, ""]; return h("span", { class: "pill " + c }, l); }

function vmafColor(v, target) {
  if (v >= target) return "#4fc3a1";
  if (v >= target - 1) return "#e0a84a";
  return "#e06666";
}

// A list that can run to hundreds of entries, as `show` makes them, cut after `max` with what
// `rest` makes of the number left out.
function cutList(items, max, show, rest) {
  return [...items.slice(0, max).map(show), ...(items.length > max ? [rest(items.length - max)] : [])];
}
const UNRESTORED_SHOWN = 20, UNRESTORED_INPUTS_SHOWN = 5;

views.queue = (root) => {
  const list = h("div");
  // Estimated time left: files not yet probed have no estimate and are counted apart.
  const leftText = (secs, unknown) => !secs ? "" : `${secs < 60 ? "under a minute" : "about " + span(secs)} left${unknown ? ` + ${plural(unknown, "file")} not estimated yet` : ""}`;
  const leftAll = h("span", { class: "dim", title: "Estimated from frame sizes and lengths, and from how long past encodes took on this machine" });
  const drawLeft = () => {
    const live = jobs.filter((j) => j.status === "queued" || j.status === "running");
    leftAll.textContent = leftText(live.reduce((a, j) => a + (j.left_secs || 0), 0), live.reduce((a, j) => a + (j.left_unknown || 0), 0));
  };
  const missing = h("div"); // saved jobs whose files were not there at start
  const held = h("div", { class: "panel warn", hidden: true }, "A benchmark is running: encodes wait until it ends, so that its timings mean something. ", h("a", { href: "#/bench" }, "Benchmark"));
  // Pause switch and the hours encodes may run in: { hold, paused, schedule }.
  let queue = null;
  const holdBox = h("div");
  const setQueue = async (path, body) => { try { queue = await api(path, body); drawHold(); } catch (e) { toast(e.message, true); } };
  const pauseBtn = h("button", { hidden: true, onclick: () => setQueue("/api/queue/pause", { paused: !queue.paused }) });
  const hoursBtn = h("button", { hidden: true, title: "Hours of the day encodes may run in", onclick: async () => { const q = await hoursDialog(queue.schedule); if (q) { queue = q; drawHold(); } } });
  const clockTime = (t) => new Date(t * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  function drawHold() {
    if (!queue) return;
    const sch = queue.schedule, hd = queue.hold;
    pauseBtn.hidden = hoursBtn.hidden = false;
    pauseBtn.textContent = queue.paused ? "Resume" : "Pause";
    pauseBtn.title = queue.paused ? "Encodes go on from where they stopped" : "Holds the running encode where it is and starts no new one, until you resume";
    hoursBtn.textContent = sch.on ? `Hours: ${sch.from}–${sch.to}` : "Hours…";
    if (!hd) { holdBox.replaceChildren(); return; }
    const working = jobs.some((j) => j.status === "running");
    holdBox.replaceChildren(h("div", { class: "panel hold" },
      hd.why === "paused"
        ? [h("span", { class: "grow" }, h("strong", {}, "Paused. "), working ? "The running encode is held where it is (it keeps its memory) and goes on when you resume." : "Encodes wait until you resume."),
          h("button", { class: "primary", onclick: () => setQueue("/api/queue/pause", { paused: false }) }, "Resume")]
        : [h("span", { class: "grow" }, h("strong", {}, `Outside encoding hours (${sch.from}–${sch.to}). `), working ? `The running encode is held where it is and goes on at ${clockTime(hd.opens)}.` : `Encodes start at ${clockTime(hd.opens)}.`),
          h("button", { title: `Encode now; the hours count again from ${sch.from}`, onclick: () => setQueue("/api/queue/run-now", {}) }, "Run now")]));
  }
  // Watched folders: what turns up in them is queued without being asked for.
  const watchBox = h("div");
  const restoringBox = h("div");
  let watched = null, watchTick = null;
  // A path too long for its place loses its beginning, not its end: the mark keeps the
  // slashes in order where the text runs right to left for that.
  const lrm = "\u200e";
  const ago = (t) => { const d = Date.now() / 1000 - t; return d < 90 ? "a moment ago" : span(d) + " ago"; };
  const watchAct = (path, body) => async (e) => {
    e.target.disabled = true;
    try { watched = await api(path, body); } catch (er) { toast(er.message, true); }
    drawWatched();
  };
  async function loadWatched() {
    try { watched = await api("/api/watch"); } catch { return; }
    drawWatched();
  }
  function drawWatched() {
    const ws = (watched && watched.watches) || [];
    if (!ws.length) { watchBox.replaceChildren(); return; }
    const row = (w) => {
      const counts = [
        w.queued ? `${w.queued} queued` : null,
        w.fresh ? { text: `${w.fresh} new`, title: `Seen, not queued yet: a video is left alone until it has not changed for ${span(watched.settled_secs)} and two looks` } : null,
        w.unread ? { text: `${w.unread} not read yet`, title: "Their probe did not get through. They are tried again, less and less often, at least once a day" } : null,
        w.left ? { text: `${w.left} left alone`, title: "AV1 already, too short, too little to gain, or unreadable" } : null,
        w.before ? { text: `${w.before} there before`, title: "In the folder when it was added; only new videos were asked for" } : null,
      ].filter(Boolean);
      const unread = (w.unread_files || []).map((u) => h("div", { class: "warn sub", title: u.path },
        `${basename(u.path)}: not read after ${u.tries} ${u.tries === 1 ? "try" : "tries"}${u.why ? ` (${u.why})` : ""}`,
        w.enabled ? `; next try ${u.next_secs < 45 ? "in a moment" : "in " + span(u.next_secs)}` : ""));
      if (w.unread > unread.length) unread.push(h("div", { class: "warn sub" }, `and ${w.unread - unread.length} more`));
      const state = !w.enabled ? h("span", { class: "pill warn" }, "Not watching") : w.error ? h("span", { class: "pill bad" }, "Problem") : h("span", { class: "pill ok" }, "Watching");
      return h("div", { class: "watch" },
        h("div", { class: "row" },
          state,
          h("code", { class: "path", title: w.path }, lrm + w.path), h("span", { class: "dim" }, "→"), h("code", { class: "path", title: w.out_dir }, lrm + w.out_dir),
          h("span", { class: "spacer" }),
          h("button", { class: "small", title: "Look at the folder now instead of waiting for the next look", onclick: watchAct(`/api/watch/${w.id}/look`, {}) }, "Look now"),
          h("button", { class: "small", onclick: watchAct(`/api/watch/${w.id}/enabled`, { enabled: !w.enabled }) }, w.enabled ? "Stop watching" : "Watch again"),
          h("button", { class: "small", title: "Forget this folder. Jobs it queued stay in the queue", onclick: async () => {
            const sure = await confirmDialog({ title: "Forget this watched folder?", action: "Forget",
              body: [h("p", { class: "mono" }, w.path), h("p", { class: "dim" }, "What it queued stays in the queue, and no file is touched. Watched again later, the videos in it then count as there before.")] });
            if (!sure) return;
            try { watched = await api(`/api/watch/${w.id}/remove`, {}); } catch (er) { toast(er.message, true); }
            drawWatched();
          } }, "Forget")),
        h("div", { class: "dim sub" },
          optionsLabel(w.options), " · ",
          w.looked ? `looked ${ago(w.looked)}` : "not looked at yet",
          w.enabled && w.next_secs != null ? `, again ${w.next_secs < 45 ? "in a moment" : "in " + span(w.next_secs)}` : "",
          counts.map((c) => [" · ", typeof c === "string" ? c : h("span", { title: c.title }, c.text)])),
        unread,
        w.error ? h("div", { class: "bad sub" }, w.error) : null);
    };
    watchBox.replaceChildren(h("div", { class: "panel watched" }, h("h3", {}, "Watched folders"), ws.map(row)));
  }
  const watchBtn = h("button", { title: "Queue the videos that turn up in a folder, on their own", onclick: async () => {
    const p = await pick({ title: "Folder to watch", mode: "dir" });
    if (p && await encodeDialog([p[0]], [], true)) loadWatched();
  } }, "Watch folder…");
  root.append(
    h("div", { class: "row" }, h("h1", {}, "Encode queue"), leftAll, h("span", { class: "spacer" }),
      pauseBtn, hoursBtn, watchBtn,
      h("button", { onclick: async () => { try { await api("/api/jobs/clear", {}); load(); } catch (e) { toast(e.message, true); } } }, "Clear finished"),
      h("button", { class: "primary", onclick: async () => { const p = await pick({ title: "Videos or folders to encode", mode: "multi" }); if (p) encodeDialog(p, [], false, p.files); } }, "New encode…")),
    held, holdBox, watchBox, restoringBox, missing, list);
  // The saved queue comes back while the page is already up.
  const drawRestoring = (n) => {
    restoringBox.replaceChildren(...(n ? [h("div", { class: "panel dim" }, `Bringing back the saved queue: ${n} job${n === 1 ? "" : "s"} to go…`)] : []));
  };
  const drawMissing = (un) => {
    if (!un || !un.length) { missing.replaceChildren(); return; }
    const act = (what) => async () => {
      try {
        const r = await api(`/api/jobs/unrestored/${what}`, {});
        if (what === "retry") toast(r.restored ? `Re-queued ${r.restored} job${r.restored === 1 ? "" : "s"}` : "Files still missing", !r.restored);
        load();
      } catch (e) { toast(e.message, true); }
    };
    missing.replaceChildren(h("div", { class: "panel" },
      h("div", { class: "row" }, h("strong", { class: "warn grow" }, `${un.length} saved job${un.length === 1 ? "" : "s"} not in the queue, kept for when ${un.length === 1 ? "its files are" : "their files are"} back`),
        h("button", { onclick: act("retry") }, "Try again"), h("button", { class: "danger", onclick: act("forget") }, "Forget")),
      cutList(un, UNRESTORED_SHOWN, (u) => {
        const inputs = Array.isArray(u.inputs) ? cutList(u.inputs, UNRESTORED_INPUTS_SHOWN, (p) => p, (n) => `and ${n} more`).join(", ") : "(its list of videos cannot be read)";
        return h("div", { class: "dim mono long-text" }, `${inputs} → ${u.out_dir ?? "(no output folder)"}: ${u.error}`);
      }, (n) => h("div", { class: "dim" }, `and ${plural(n, "more job")}`))));
  };
  const fileEls = new Map(); // "job:file" -> { el, update(progress) } for running files
  // Rendered jobs. Only parts whose data changed are rebuilt, so buttons stay put while an
  // encode streams events.
  const jobEls = new Map(); // id -> { box, head, headSig, files, nodes[], sigs[], err }
  const empty = h("div", { class: "panel empty" }, "No encodes yet. Pick files in Scan, or use “New encode…”.");
  let jobs = [];
  let timer = null;
  const pending = new Map(); // job id -> timeout

  async function load() {
    let r;
    try { r = await api("/api/jobs"); } catch (e) { toast(e.message, true); return; }
    jobs = r.jobs;
    held.hidden = !r.benchmark;
    queue = r.queue;
    drawHold();
    drawRestoring(r.restoring);
    drawMissing(r.unrestored);
    draw();
  }
  // A live event touches one file: fetch just that file and the job's header.
  async function loadFile(id, i) {
    let r;
    try { r = await api(`/api/jobs/${id}/files/${i}`); } catch { return load(); }
    const j = jobs.find((x) => x.id === id);
    const e = jobEls.get(id);
    if (!j || !e || !e.nodes[i]) return load();
    Object.assign(j, r.head);
    j.files[i] = r.file;
    drawLeft();
    patchHead(e, j);
    patchFile(e, j, r.file, i);
  }
  const reload = () => { clearTimeout(timer); timer = setTimeout(load, 150); };
  const reloadFile = (id, i) => {
    const key = `${id}:${i}`;
    clearTimeout(pending.get(key));
    pending.set(key, setTimeout(() => { pending.delete(key); loadFile(id, i); }, 120));
  };

  function draw() {
    const ids = new Set(jobs.map((j) => j.id));
    for (const [id, e] of jobEls) if (!ids.has(id)) { e.box.remove(); jobEls.delete(id); }
    if (!jobs.length) { list.replaceChildren(empty); drawLeft(); return; }
    empty.remove();
    for (const j of [...jobs].reverse()) list.append(patchJob(j)); // append moves nodes into order
    drawLeft();
  }

  // What holds a running job, if anything: the pause switch or the hours.
  const heldBy = (j) => (j.paused ? queue?.hold?.why ?? "paused" : null);
  // The waiting job that starts next: the oldest one, unless another was moved ahead of it.
  const isNext = (j) => j.status === "queued" && jobs.find((x) => x.status === "queued") === j;
  const headSig = (j) => JSON.stringify([j.status, isNext(j), heldBy(j), j.error, j.note, j.counts, j.saved, leftText(j.left_secs, j.left_unknown)]);
  const fileSig = (j, f) => JSON.stringify({ ...f, progress: null, held: f.status === "running" ? heldBy(j) : null });
  const pausedPill = (j) => h("span", { class: "pill warn" }, heldBy(j) === "hours" ? "On hold" : "Paused");

  function patchJob(j) {
    let e = jobEls.get(j.id);
    if (!e) {
      e = { box: h("div", { class: "job" }), head: null, headSig: "", files: h("div", { class: "files" }), nodes: [], sigs: [], err: null, note: null };
      e.box.append(e.files);
      jobEls.set(j.id, e);
    }
    patchHead(e, j);
    j.files.forEach((f, i) => patchFile(e, j, f, i));
    return e.box;
  }

  function patchHead(e, j) {
    const hs = headSig(j);
    if (hs === e.headSig) return;
    const nh = drawHead(j);
    if (e.head) e.head.replaceWith(nh); else e.box.prepend(nh);
    e.head = nh; e.headSig = hs;
    if (e.err) { e.err.remove(); e.err = null; }
    if (e.note) { e.note.remove(); e.note = null; }
    if (j.error) { e.err = h("div", { class: "file bad" }, j.error); e.box.append(e.err); }
    if (j.note) { e.note = h("div", { class: "file warn" }, j.note); e.box.append(e.note); }
  }

  function patchFile(e, j, f, i) {
    const sg = fileSig(j, f);
    if (sg === e.sigs[i]) return;
    fileEls.delete(`${j.id}:${i}`);
    const n = drawFile(j, f, i);
    if (e.nodes[i]) e.nodes[i].replaceWith(n); else e.files.append(n);
    e.nodes[i] = n; e.sigs[i] = sg;
  }

  function drawHead(j) {
    const o = j.options;
    const counts = j.counts || {};
    const saved = j.saved || 0;
    const head = h("div", { class: "head" },
      j.paused ? pausedPill(j) : statusPill(j.status),
      h("strong", {}, `${j.total} file${j.total === 1 ? "" : "s"}`),
      h("span", { class: "dim" }, `→ ${j.out_dir}`),
      h("span", { class: "dim" }, optionsLabel(o)),
      j.watch ? h("span", { class: "pill", title: `Queued on its own: these turned up in the watched folder ${j.watch}` }, "watched folder") : null,
      h("span", { class: "spacer" }),
      saved > 0 ? h("span", { class: "ok" }, `${bytes(saved)} saved`) : null,
      j.left_secs ? h("span", {}, leftText(j.left_secs, j.left_unknown)) : null,
      h("span", { class: "dim" }, Object.entries(counts).map(([k, v]) => `${v} ${(STATUS[k] || [k])[0].toLowerCase()}`).join(" · ")),
      j.status === "queued" && !isNext(j)
        ? h("button", { class: "small", title: "Start this job before the other waiting ones", onclick: async (e) => { const b = e.target; b.disabled = true; try { await api(`/api/jobs/${j.id}/first`, {}); } catch (er) { toast(er.message, true); } finally { b.disabled = false; } load(); } }, "Do next")
        : null,
      j.status === "cancelled" || j.status === "failed"
        ? h("button", { class: "small", title: "Queue these files again with the same settings. Finished files are skipped and scene chunks resume.", onclick: async (e) => { const b = e.target; b.disabled = true; try { await api(`/api/jobs/${j.id}/again`, {}); toast("Queued again"); } catch (er) { toast(er.message, true); } finally { b.disabled = false; } load(); } }, "Queue again")
        : null,
      j.status === "running" || j.status === "queued"
        ? h("button", { class: "danger small", onclick: async (e) => { const b = e.target; b.disabled = true; try { await api(`/api/jobs/${j.id}/cancel`, {}); toast("Cancelling… (finished chunks are kept; re-queue to resume)"); } catch (er) { toast(er.message, true); } finally { b.disabled = false; } } }, "Cancel")
        : h("button", { class: "small", onclick: async (e) => { const b = e.target; b.disabled = true; try { await api(`/api/jobs/${j.id}/remove`, {}); load(); } catch (er) { toast(er.message, true); } finally { b.disabled = false; } } }, "Remove"));
    return head;
  }

  function drawFile(j, f, i) {
    const name = h("div", { class: "name" }, basename(f.input), h("div", { class: "dim mono" }, dirname(f.input)));
    const right = h("div", { class: "row" });
    if (f.status === "ok" || f.status === "done_before") {
      if (f.input_bytes && f.output_bytes) right.append(h("span", {}, `${bytes(f.input_bytes)} → ${bytes(f.output_bytes)} `, h("span", { class: savedClass(f.input_bytes - f.output_bytes) }, sizeChange(1 - f.output_bytes / f.input_bytes))));
      right.append(h("a", { class: "btn", href: "#/compare?report=" + encodeURIComponent(f.report) }, "Compare"));
    }
    const el = h("div", { class: "file" }, h("div", { class: "line1" }, j.paused && f.status === "running" ? pausedPill(j) : statusPill(f.status), name, right));
    if (f.detail && (f.status === "skip" || f.status === "error")) el.append(h("div", { class: f.status === "error" ? "bad mono" : "warn" }, f.detail));
    if (f.status === "running") {
      const live = drawLive(j, f);
      el.append(live.el);
      fileEls.set(`${j.id}:${i}`, live);
    }
    return el;
  }

  function drawLive(j, f) {
    const target = j.options.target_vmaf;
    const bar = h("div", { class: "bar" }, h("div"));
    const progText = h("span", { class: "dim" });
    const src = f.source;
    const left = h("div", {},
      h("div", { class: "row" }, h("span", { class: "phase" }, PHASES[f.phase] || f.phase || "Starting"), h("span", { class: "dim" }, f.phase_detail)),
      h("div", { class: "row" }, h("div", { class: "grow" }, bar), progText),
      src ? h("div", { class: "dim" }, `${src.codec} ${src.width}×${src.height} · ${num(src.fps, 2)} fps · ${hms(src.duration_s)} · ${num(src.video_mbps, 1)} Mbps${src.hdr ? " · HDR" : ""}`) : null,
      f.chosen ? h("div", {}, `CRF ${f.chosen.crf} → VMAF ${num(f.chosen.vmaf)}, predicted video savings `, h("span", { class: savedClass(f.chosen.predicted_savings) }, savedPct(f.chosen.predicted_savings))) : null,
      f.spot_checks.length ? h("div", {}, "Spot checks: ", f.spot_checks.map((c) => `${hms(c.at_secs)} → ${num(c.vmaf)}`).join(", ")) : null,
      f.notes.length ? h("div", { class: "dim" }, f.notes.join(" · ")) : null);
    const rightCol = h("div", {});
    if (f.trials.length) {
      rightCol.append(h("h3", {}, "CRF search"), h("table", { class: "trials" },
        h("tr", {}, h("th", {}, "CRF"), h("th", { class: "num" }, "VMAF"), h("th", { class: "num" }, "worst"), h("th", { class: "num" }, "size")),
        f.trials.map((t) => h("tr", {}, h("td", {}, t.crf), h("td", { class: "num " + (t.mean_vmaf >= target ? "ok" : "bad") }, num(t.mean_vmaf)), h("td", { class: "num dim" }, num(t.min_vmaf)), h("td", { class: "num" }, pct(t.size_ratio))))));
    }
    if (f.chunks_total) {
      const strip = h("div", { class: "chunkstrip", title: "One block per chunk, coloured by VMAF vs target" });
      const done = new Map(f.chunks.map((c) => [c.index, c]));
      for (let k = 0; k < f.chunks_total; k++) {
        const c = done.get(k);
        const d = h("div", { class: "grow" });
        d.style.background = c ? vmafColor(c.vmaf, target) : k < f.chunks_resumed ? "#2c6e5c" : "#2c3136";
        if (c) d.title = `chunk ${k + 1}: CRF ${c.crf}, VMAF ${num(c.vmaf)}, ${bytes(c.bytes)}${c.corrected ? " (corrected)" : ""}`;
        else if (k < f.chunks_resumed) d.title = `chunk ${k + 1}: done in an earlier run`;
        strip.append(d);
      }
      const vm = f.chunks.length ? f.chunks.reduce((a, c) => a + c.vmaf * c.frames, 0) / f.chunks.reduce((a, c) => a + c.frames, 0) : null;
      rightCol.append(h("h3", {}, `Chunks ${f.chunks.length + f.chunks_resumed} / ${f.chunks_total}`), strip);
      if (vm) rightCol.append(h("div", { class: "dim" }, `mean VMAF so far ${num(vm)}`));
    }
    const el = h("div", { class: "live" }, left, rightCol);
    const update = (p) => {
      if (!p) { bar.firstChild.style.width = "0%"; progText.textContent = ""; return; }
      bar.firstChild.style.width = (p.frac * 100).toFixed(1) + "%";
      // A running file is not done, however close its last frames are.
      progText.textContent = `${floorPct(p.frac, 1, false)}${p.fps ? " · " + num(p.fps, 1) + " fps" : ""} · ETA ${hms(p.eta_secs)}`;
    };
    update(f.progress);
    return { el, update };
  }

  const off = onEvent((m) => {
    if (m.kind === "jobs") return reload();
    if (m.kind === "reopened") { reload(); loadWatched(); return; }
    if (m.kind === "watch") return loadWatched();
    if (m.kind === "bench") { held.hidden = m.run.status !== "running"; return; }
    if (m.kind === "bench_done") { held.hidden = true; return; }
    if (m.kind !== "job") return;
    if (m.event.type === "progress") {
      const live = fileEls.get(`${m.id}:${m.file}`);
      if (live) return live.update(m.event);
    }
    if (m.file == null) reload(); else reloadFile(m.id, m.file);
  });
  load();
  loadWatched();
  // "Looked 3 min ago" goes on counting between two looks.
  watchTick = setInterval(() => { if (watched && watched.watches.length) loadWatched(); }, 30000);
  return () => { off(); clearTimeout(timer); clearInterval(watchTick); for (const t of pending.values()) clearTimeout(t); };
};

// Hours of the day encodes may run in. Resolves to the queue's new state, or null.
function hoursDialog(sch) {
  return new Promise((resolve) => {
    let busy = false;
    const close = (v) => { bg.remove(); release(); resolve(v); };
    // While a save is on its way the dialog stays: its answer is what closes it.
    const cancel = () => { if (!busy) close(null); };
    const on = h("input", { type: "checkbox" });
    on.checked = sch.on;
    const from = h("input", { type: "time", value: sch.from, required: true }), to = h("input", { type: "time", value: sch.to, required: true });
    const note = h("p", { class: "dim" });
    const sync = () => {
      from.disabled = to.disabled = !on.checked;
      const a = from.value, b = to.value;
      note.textContent = !on.checked ? "Encodes run whenever there is something in the queue."
        : !a || !b ? "Give both times."
        : a === b ? "The hours begin and end at the same time."
        : `Encodes run from ${a} to ${b}${b < a ? " the next day" : ""}. Outside these hours nothing new starts and a running encode is held where it is; it goes on when the hours begin again.`;
      save.disabled = busy || (on.checked && (!a || !b || a === b));
    };
    const save = h("button", { class: "primary", onclick: async () => {
      busy = true; sync();
      try { close(await api("/api/queue/schedule", { on: on.checked, from: from.value || sch.from, to: to.value || sch.to })); } catch (e) { toast(e.message, true); } finally { busy = false; sync(); }
    } }, "Save");
    for (const el of [on, from, to]) el.addEventListener("input", sync);
    const bg = h("div", { class: "modal-bg", onclick: (e) => { if (e.target === bg) cancel(); } },
      h("div", { class: "modal narrow" },
        h("div", { class: "head" }, h("strong", {}, "Encoding hours")),
        h("div", { class: "body" },
          h("label", { class: "check" }, on, "Encode only during these hours"),
          h("div", { class: "row hours" }, "from", from, "to", to),
          note),
        h("div", { class: "foot" }, h("button", { onclick: cancel }, "Cancel"), save)));
    sync();
    document.body.append(bg);
    const release = holdFocus(bg, cancel, on);
  });
}

// ----- history -----

// The encodes of each day, oldest first, with the space saved up to and including that day.
function savedByDay(reps) {
  const rows = reps.filter((x) => x.time && x.source_bytes > 0 && x.output_bytes > 0).sort((a, b) => a.time - b.time);
  const days = [];
  let total = 0;
  for (const x of rows) {
    const d = new Date(x.time * 1000);
    d.setHours(0, 0, 0, 0);
    let last = days[days.length - 1];
    if (!last || last.day !== d.getTime()) days.push(last = { day: d.getTime(), n: 0, saved: 0, total });
    last.n++;
    last.saved += x.source_bytes - x.output_bytes;
    total += x.source_bytes - x.output_bytes;
    last.total = total;
  }
  return days;
}

// Space saved so far, as it grew: one line that steps up on the days something was encoded
// and stays level in between. Hover, or the arrow keys, read a day; the table below has
// every encode.
function savedChart(days) {
  const wrap = h("div", { class: "saved-chart" });
  const tip = h("div", { class: "tooltip", hidden: true });
  const DAY = 86400000;
  const date = (t, year) => new Date(t).toLocaleDateString([], year ? { day: "numeric", month: "short", year: "numeric" } : { day: "numeric", month: "short" });
  let width = 0;

  const draw = () => {
    const W = width = Math.max(320, Math.floor(wrap.clientWidth) || 900), H = 210;
    const m = { l: 58, r: 18, t: 22, b: 26 };
    const pw = W - m.l - m.r, ph = H - m.t - m.b;
    const t0 = days[0].day, t1 = Math.max(Date.now(), days[days.length - 1].day + DAY);
    const top = Math.max(0, ...days.map((d) => d.total)), low = Math.min(0, ...days.map((d) => d.total));
    // Lines at round amounts, four or so, over all that is drawn: the total can be below
    // zero where outputs came out larger than their sources.
    const range = top - low > 0 && isFinite(top - low) ? top - low : 1;
    const mag = Math.pow(10, Math.floor(Math.log10(range / 4)));
    const step = [1, 2, 5, 10].map((k) => k * mag).find((v) => range / v <= 4) || 10 * mag;
    const y0 = Math.floor(low / step) * step || 0, y1 = Math.max(Math.ceil(top / step) * step, y0 + step);
    const x = (t) => m.l + (t - t0) / (t1 - t0) * pw;
    const y = (v) => m.t + (y1 - v) / (y1 - y0) * ph;
    // A day's encodes are drawn at its middle.
    const at = (d) => x(Math.min(d.day + DAY / 2, t1));
    const last = days[days.length - 1];
    const svg = s("svg", { width: W, height: H, viewBox: `0 0 ${W} ${H}`, role: "img", tabindex: 0,
      "aria-label": `Space saved over time: ${bytes(last.total)} in all since ${date(t0, true)}, over ${plural(days.length, "day")} with encodes. Left and right arrows read a day; the table below lists every encode.` });
    const text = (str, attrs) => { const t = s("text", attrs); t.textContent = str; svg.append(t); return t; };

    for (let k = 0; y0 + k * step <= y1 + step / 2; k++) {
      const v = y0 + k * step;
      svg.append(s("line", { class: "grid", x1: m.l, x2: m.l + pw, y1: y(v), y2: y(v) }));
      text(Math.abs(v) < step / 1e6 ? "0" : bytes(v).replace(/\.0 /, " "), { x: m.l - 8, y: y(v) + 4, "text-anchor": "end" });
    }
    // Dates: a few, at the start of days (of months, over a long stretch), none on top of another.
    const long = t1 - t0 > 100 * DAY, years = new Date(t0).getFullYear() !== new Date(t1).getFullYear();
    let lastX = -Infinity;
    for (const d = new Date(t0); d.getTime() <= t1; long ? d.setMonth(d.getMonth() + 1, 1) : d.setDate(d.getDate() + 1)) {
      const px = x(d.getTime());
      if (px - lastX < (years ? 86 : 64) || px > m.l + pw - 20) continue;
      lastX = px;
      svg.append(s("line", { class: "tick", x1: px, x2: px, y1: m.t + ph, y2: m.t + ph + 4 }));
      text(long ? d.toLocaleDateString([], years ? { month: "short", year: "numeric" } : { month: "short" }) : date(d.getTime(), years), { x: px, y: m.t + ph + 17, "text-anchor": "middle" });
    }

    let line = `M${x(t0)},${y(0)}`;
    for (const d of days) line += `H${at(d)}V${y(d.total)}`;
    line += `H${x(t1)}`;
    svg.append(s("path", { class: "area", d: `${line}V${y(0)}Z` }), s("path", { class: "line", d: line }));
    // The amount it stands at, by the end of the line.
    svg.append(s("circle", { class: "dot", cx: x(t1), cy: y(last.total), r: 4 }));
    text(bytes(last.total), { class: "lbl", x: x(t1) - 8, y: Math.max(y(last.total) - 8, 12), "text-anchor": "end" });

    const cross = s("line", { class: "cross", y1: m.t, y2: m.t + ph, visibility: "hidden" });
    const mark = s("circle", { class: "dot", r: 4, visibility: "hidden" });
    svg.append(cross, mark);
    let cur = -1;
    const show = (i) => {
      cur = i;
      const d = days[i], px = at(d), py = y(d.total);
      cross.setAttribute("x1", px); cross.setAttribute("x2", px);
      mark.setAttribute("cx", px); mark.setAttribute("cy", py);
      cross.setAttribute("visibility", "visible"); mark.setAttribute("visibility", "visible");
      tip.replaceChildren(
        h("div", {}, h("strong", {}, bytes(d.total)), ` saved by ${date(d.day, true)}`),
        h("div", { class: "dim" }, `${d.saved < 0 ? "−" : "+"}${bytes(Math.abs(d.saved))} that day, ${plural(d.n, "encode")}`));
      tip.hidden = false;
      const half = tip.offsetWidth / 2;
      tip.style.left = Math.min(Math.max(px, half + 2), Math.max(half + 2, width - half - 2)) + "px";
      tip.style.top = py + "px";
      tip.style.transform = py < 70 ? "translate(-50%, 14px)" : "translate(-50%, calc(-100% - 12px))";
    };
    const hide = () => { cur = -1; tip.hidden = true; cross.setAttribute("visibility", "hidden"); mark.setAttribute("visibility", "hidden"); };
    // The pointer only has to be nearest to a day, anywhere over the plot.
    svg.addEventListener("pointermove", (e) => {
      const px = e.clientX - svg.getBoundingClientRect().left;
      let best = 0;
      for (let i = 1; i < days.length; i++) if (Math.abs(at(days[i]) - px) < Math.abs(at(days[best]) - px)) best = i;
      if (best !== cur) show(best);
    });
    svg.addEventListener("pointerleave", hide);
    svg.addEventListener("focus", () => { if (cur < 0) show(days.length - 1); });
    svg.addEventListener("blur", hide);
    svg.addEventListener("keydown", (e) => {
      const to = { ArrowLeft: cur - 1, ArrowRight: cur + 1, Home: 0, End: days.length - 1 }[e.key];
      if (to == null) return;
      e.preventDefault();
      show(Math.min(Math.max(to, 0), days.length - 1));
    });
    wrap.replaceChildren(svg, tip);
  };

  const ro = new ResizeObserver(() => { const w = Math.floor(wrap.clientWidth); if (w && Math.max(320, w) !== width) draw(); });
  ro.observe(wrap);
  return { el: wrap, stop: () => ro.disconnect() };
}

views.history = (root) => {
  const out = h("div", { class: "empty" }, h("span", { class: "spin" }));
  let ready = [], chart = null;
  const replaceBtn = h("button", { disabled: true, onclick: () => replaceDialog(ready, load) }, "Replace originals…");
  root.append(h("div", { class: "row" }, h("h1", { class: "grow" }, "History"), replaceBtn,
    h("button", { onclick: async () => { const p = await pick({ title: "Folder with av1opt outputs" }); if (p) { try { await api("/api/history/dirs", { path: p[0] }); load(); } catch (e) { toast(e.message, true); } } } }, "Add folder…")), out);

  async function load() {
    let r;
    try { r = await api("/api/history"); } catch (e) { toast(e.message, true); return; }
    const reps = r.reports;
    const stateOf = (x) => (x.replace || {}).state;
    ready = reps.filter((x) => stateOf(x) === "ready");
    const replaced = reps.filter((x) => stateOf(x) === "replaced").length;
    replaceBtn.disabled = !ready.length;
    replaceBtn.title = ready.length ? `${plural(ready.length, "output")} can take the place of their original` : "No output here can take its original's place (already replaced, or a file is gone or changed)";
    const tin = reps.reduce((a, x) => a + (x.source_bytes || 0), 0), tout = reps.reduce((a, x) => a + (x.output_bytes || 0), 0);
    const dirs = h("div", { class: "chips" }, r.dirs.map((d) => h("span", { class: "chip" }, h("code", {}, d), h("button", { title: "Forget folder", onclick: async () => { try { await api("/api/history/dirs/remove", { path: d }); load(); } catch (e) { toast(e.message, true); } } }, "×"))));
    if (!r.dirs.length) dirs.append(h("span", { class: "dim" }, "No folders yet. Output folders of queued encodes are added automatically."));
    // Thousands of rows take a while to lay out; the newest come first, the rest on request.
    const LIMIT = 500;
    const row = (x) => h("tr", { class: "click", onclick: () => (location.hash = "#/compare?report=" + encodeURIComponent(x.report)) },
      h("td", { class: "dim" }, when(x.time)),
      h("td", { class: "path", title: x.input }, h("span", {}, basename(x.input)),
        stateOf(x) === "replaced" ? h("span", { class: "pill ok", title: `The AV1 file is where the original was: ${x.output}\n${x.input_exists ? "The original is in the trash" : "The original is no longer in the trash"}` }, "replaced")
          : x.input_exists === false ? h("span", { class: "pill warn", title: "source no longer at this path" }, "source gone") : null,
        x.output_exists === false ? h("span", { class: "pill bad" }, "output gone") : null,
        stateOf(x) === "not_recorded" ? h("span", { class: "pill", title: x.replace.why }, "not made here") : null),
      h("td", {}, x.mode === "scenes" ? `scenes (${x.chunks})` : "whole"),
      h("td", { class: "num" }, x.crf),
      h("td", { class: "num" }, x.target_vmaf),
      h("td", { class: "num" }, x.mean_vmaf != null ? num(x.mean_vmaf) : (x.verify_vmaf || []).map((v) => num(v, 1)).join(" / ")),
      h("td", { class: "num" }, bytes(x.source_bytes)),
      h("td", { class: "num" }, bytes(x.output_bytes)),
      h("td", { class: "num " + savedClass(x.savings) }, savedPct(x.savings)),
      h("td", { class: "num dim" }, hms((x.search_secs || 0) + (x.encode_secs || 0))));
    const tbody = h("tbody", {}, reps.slice(0, LIMIT).map(row));
    if (reps.length > LIMIT) {
      const more = h("tr", {}, h("td", { colspan: 10, class: "dim" }, `${reps.length - LIMIT} older encodes not shown. `,
        h("button", { class: "small", onclick: () => { more.remove(); tbody.append(...reps.slice(LIMIT).map(row)); } }, "Show all")));
      tbody.append(more);
    }
    // A line needs somewhere to go: with the encodes of one day only, the numbers say it all.
    const days = savedByDay(reps);
    if (chart) chart.stop();
    chart = days.length >= 2 ? savedChart(days) : null;
    out.className = "";
    out.replaceChildren(
      h("div", { class: "panel" }, h("h3", {}, "Folders searched for reports"), dirs),
      h("div", { class: "stats" },
        h("div", { class: "stat" }, h("div", { class: "v" }, reps.length), h("div", { class: "l" }, replaced ? `encodes · ${replaced} in place of their original` : "encodes")),
        h("div", { class: "stat" }, h("div", { class: "v" }, `${bytes(tin)} → ${bytes(tout)}`), h("div", { class: "l" }, "source → AV1")),
        h("div", { class: "stat" }, h("div", { class: "v " + savedClass(tin - tout) }, bytes(tin - tout)), h("div", { class: "l" }, `saved (${savedPct(tin ? 1 - tout / tin : 0)})`)),
        h("div", { class: "stat" }, h("div", { class: "v" }, STATE ? STATE.history_encodes : "–"), h("div", { class: "l" }, "encodes calibrating scan estimates"))),
      chart ? h("div", { class: "panel" }, h("h3", {}, "Space saved over time"), chart.el) : null,
      h("div", { class: "panel" }, reps.length
        ? h("div", { class: "tablewrap" }, h("table", {}, h("thead", {}, h("tr", {}, ["When", "File", "Mode", "CRF", "Target", "VMAF", "Source", "AV1", "Saved", "Time"].map((t, k) => h("th", { class: k >= 3 ? "num" : "" }, t)))), tbody))
        : h("div", { class: "empty" }, "No reports found in these folders.")));
  }
  load();
  return () => { if (chart) chart.stop(); };
};

// Outputs take the place of their originals: one request per file, so the dialog can say
// which one it is on and which ones failed.
function replaceDialog(rows, reload) {
  const picked = new Set(rows.map((x) => x.report));
  const anyZfs = rows.some((x) => x.replace.zfs);
  let allowZfs = false, busy = false, finished = false;
  const usable = (x) => !x.replace.zfs || allowZfs;
  const chosen = () => rows.filter((x) => picked.has(x.report) && usable(x));
  const list = h("div", { class: "replace-list" });
  const total = h("span", { class: "dim grow" });
  const cancel = h("button", { onclick: () => close() }, "Cancel");
  const go = h("button", { class: "danger" });
  const close = () => { if (busy) return; bg.remove(); release(); };
  const sum = () => {
    const c = chosen();
    go.textContent = c.length ? `Replace ${plural(c.length, "original")}` : "Replace";
    go.disabled = !c.length || busy;
    total.textContent = c.length ? `${bytes(c.reduce((a, x) => a + x.source_bytes, 0))} to the trash · ${bytes(c.reduce((a, x) => a + x.output_bytes, 0))} in their place` : "Nothing picked";
  };
  const draw = () => {
    list.replaceChildren(...rows.map((x) => {
      const cb = h("input", { type: "checkbox", onchange: () => { if (cb.checked) picked.add(x.report); else picked.delete(x.report); sum(); } });
      cb.checked = picked.has(x.report) && usable(x);
      cb.disabled = !usable(x) || busy;
      const notes = [x.replace.copy ? "copied from another drive" : null, x.replace.removed ? `${plural(x.replace.removed, "section")} left out` : null, x.replace.zfs ? "on ZFS" : null].filter(Boolean);
      const newName = basename(x.replace.dest);
      return h("label", { class: "replace-row" + (usable(x) ? "" : " off") }, cb,
        h("span", { class: "grow path", title: `${x.input}\n→ ${x.replace.dest}` }, basename(x.input),
          newName !== basename(x.input) ? h("span", { class: "dim" }, ` → ${newName}`) : null, notes.map((n) => h("span", { class: "pill" }, n))),
        h("span", { class: "num dim" }, `${bytes(x.source_bytes)} → ${bytes(x.output_bytes)}`));
    }));
    sum();
  };
  const all = h("button", { class: "small", onclick: () => { const on = chosen().length < rows.filter(usable).length; for (const x of rows) { if (on) picked.add(x.report); else picked.delete(x.report); } draw(); } }, "All / none");
  const zfs = h("input", { type: "checkbox", onchange: () => { allowZfs = zfs.checked; draw(); } });
  go.onclick = async () => {
    const c = chosen();
    busy = true; cancel.disabled = true; all.disabled = true; zfs.disabled = true;
    draw();
    let done = 0, moved = 0;
    const problems = [];
    for (const [i, x] of c.entries()) {
      total.textContent = `Replacing ${i + 1} of ${c.length}: ${basename(x.input)}`;
      try {
        const r = await api("/api/replace", { report: x.report, allow_zfs: allowZfs });
        done++; moved += r.source_bytes;
        for (const w of r.warnings || []) problems.push({ path: x.input, text: w, warn: true });
      } catch (e) { problems.push({ path: x.input, text: e.message }); }
    }
    busy = false; finished = true; cancel.disabled = false;
    const failed = problems.filter((q) => !q.warn).length;
    toast(`${plural(done, "original")} replaced; ${bytes(moved)} in the trash until it is emptied`, failed > 0);
    reload();
    if (!problems.length) { close(); return; }
    // What went wrong stays on screen; the files concerned are where they were.
    go.remove(); all.remove();
    cancel.textContent = "Close";
    total.textContent = `${plural(done, "original")} replaced`;
    list.replaceChildren(
      failed ? h("p", {}, h("strong", { class: "bad" }, `${plural(failed, "original")} not replaced; ${failed === 1 ? "it is" : "they are"} still where ${failed === 1 ? "it was" : "they were"}`)) : null,
      ...problems.map((q) => h("div", { class: "dim mono" }, `${q.path}: ${q.text}`)));
  };
  const bg = h("div", { class: "modal-bg", onclick: (e) => { if (e.target === bg && !finished) close(); } },
    h("div", { class: "modal" },
      h("div", { class: "head" }, h("strong", { class: "grow" }, "Replace originals"), all),
      h("div", { class: "body" },
        h("p", {}, "Each original moves to the trash of the drive it is on, and its AV1 file takes its name in its folder. Nothing is deleted or written over: the space is freed when you empty the trash, and until then an original can be put back from its page in Compare (or with ", h("code", {}, "av1opt replace --undo"), ")."),
        h("p", { class: "dim" }, "Listed are the outputs whose report still matches both files. Subtitles, covers and other files next to an original stay where they are."),
        anyZfs ? h("label", { class: "check" }, zfs, "Also replace originals stored on ZFS") : null,
        list),
      h("div", { class: "foot" }, total, cancel, go)));
  document.body.append(bg);
  const release = holdFocus(bg, close);
  draw();
}

// ----- compare -----

views.compare = (root, params) => {
  const reportPath = params.get("report");
  const left = params.get("left"), right = params.get("right");
  if (!reportPath && !(left && right)) {
    root.append(h("h1", {}, "Compare"), h("div", { class: "panel empty" }, "Pick an encode in ", h("a", { href: "#/history" }, "History"), " or the ", h("a", { href: "#/queue" }, "Queue"), ", or two copies in ", h("a", { href: "#/dups" }, "Duplicates"), "."));
    return;
  }
  const wrap = h("div", { class: "empty" }, h("span", { class: "spin" }), reportPath ? " loading report…" : " loading…");
  root.append(wrap);
  let destroy = () => {};
  let closed = false;
  // Two copies from a duplicate search stand in for source (left) and output (right).
  const load = reportPath ? api("/api/report?path=" + encodeURIComponent(reportPath)) : api("/api/dups/pair", { left, right }).then((r) => {
    // Copies that share only their ending line up from the end.
    const dl = (r.input_info || {}).duration_s, dr = (r.output_info || {}).duration_s;
    if (params.get("kind") === "same_end" && dl && dr) r.shift = dl - dr;
    return r;
  });
  load
    .then((r) => { if (closed) return; wrap.remove(); destroy = comparePlayer(root, r); })
    .catch((e) => { wrap.className = "panel bad"; wrap.textContent = e.message; });
  return () => { closed = true; destroy(); };
};

function mediaUrl(p) { return "/media?path=" + encodeURIComponent(p); }

function comparePlayer(root, r) {
  const ii = r.input_info || {}, oi = r.output_info || {};
  const fps = oi.fps || ii.fps || 25;
  const duration = oi.duration_s || ii.duration_s || 0;
  const target = r.target_vmaf;

  // Output (AV1) is the master clock and carries the audio; the source follows it.
  // In preview mode the source is a short clip starting at `offset` seconds of the output.
  const vo = h("video", { preload: "auto", playsinline: true });
  const vs = h("video", { preload: "auto", playsinline: true, muted: true });
  vs.muted = true;
  // In preview mode the source clip covers output times offset..segEnd and starts at
  // source time srcOff.
  let offset = 0, segEnd = Infinity, srcOff = 0, srcMode = "direct";
  // Extra source shift in seconds, found by auto-align or set by hand: variable-frame-rate
  // sources are retimed by the encode, so frame k of the source isn't always at k/fps.
  let nudge = r.shift || 0;
  // What the two sides are called: source and AV1 output of an encode, or two copies.
  const leftName = r.pair ? "Left" : "Source";
  const desc = (i) => `${i.codec || "?"} ${i.width || "?"}×${i.height || "?"}`;
  // Outputs with sections removed: output time t shows source time toSrc(t).
  const removed = r.removed || [];
  const keep = removed.length && ii.duration_s ? keptParts(removed, ii.duration_s) : null;
  const toSrc = (t) => (keep ? outToSrc(keep, t) : t);
  const srcTime = (t) => Math.max(0, toSrc(t) - srcOff + nudge);
  let split = 0.5, zoom = 1, origin = "50% 50%";
  // Set when the view is left: what was still loading or being made then is dropped.
  let closed = false;

  const layerS = h("div", { class: "layer" }, vs);
  const layerO = h("div", { class: "layer" }, vo);
  const divider = h("div", { class: "divider" });
  const tagL = h("div", { class: "tag l" }, r.pair ? `Left · ${desc(ii)} · ${bytes(r.source_bytes)}` : `Source · ${ii.codec || "?"} · ${bytes(r.source_bytes)}`);
  const tagR = h("div", { class: "tag r" }, r.pair ? `Right · ${desc(oi)} · ${bytes(r.output_bytes)}` : `AV1 · ${bytes(r.output_bytes)} (${sizeChange(r.savings)})`);
  const overlay = h("div", { class: "overlay", hidden: true });
  const stage = h("div", { class: "stage" }, layerS, layerO, divider, tagL, tagR, overlay);
  if (oi.width && oi.height) stage.style.aspectRatio = `${oi.width} / ${oi.height}`;

  const applySplit = () => {
    layerO.style.clipPath = `inset(0 0 0 ${split * 100}%)`;
    divider.style.left = `calc(${split * 100}% - 1px)`;
  };
  const applyZoom = () => {
    for (const v of [vo, vs]) { v.style.transform = `scale(${zoom})`; v.style.transformOrigin = origin; }
  };
  applySplit();

  let dragging = false;
  const setFromEvent = (e) => {
    const b = stage.getBoundingClientRect();
    split = Math.min(1, Math.max(0, (e.clientX - b.left) / b.width));
    applySplit();
  };
  stage.addEventListener("pointerdown", (e) => {
    if (e.target === overlay || overlay.contains(e.target)) return;
    dragging = true; stage.setPointerCapture(e.pointerId); setFromEvent(e);
    if (zoom > 1 && e.shiftKey) {
      const b = stage.getBoundingClientRect();
      origin = `${((e.clientX - b.left) / b.width * 100).toFixed(1)}% ${((e.clientY - b.top) / b.height * 100).toFixed(1)}%`;
      applyZoom();
    }
  });
  stage.addEventListener("pointermove", (e) => { if (dragging) setFromEvent(e); });
  stage.addEventListener("pointerup", () => (dragging = false));
  stage.addEventListener("dblclick", (e) => {
    const b = stage.getBoundingClientRect();
    origin = `${((e.clientX - b.left) / b.width * 100).toFixed(1)}% ${((e.clientY - b.top) / b.height * 100).toFixed(1)}%`;
    zoom = zoom >= 4 ? 1 : zoom * 2;
    zoomSel.value = String(zoom);
    applyZoom();
  });

  // ---- controls ----
  const playBtn = h("button", { class: "primary", onclick: () => togglePlay() }, "Play");
  const seek = h("input", { type: "range", min: "0", max: String(duration), step: "0.01", value: "0" });
  const timeEl = h("span", { class: "mono" }, "0:00.0");
  const speed = h("select", { onchange: () => { vo.playbackRate = Number(speed.value); } }, ["0.25", "0.5", "1", "1.5", "2"].map((x) => h("option", { value: x }, x + "×")));
  speed.value = "1";
  const zoomSel = h("select", { onchange: () => { zoom = Number(zoomSel.value); applyZoom(); } }, ["1", "2", "4", "8"].map((x) => h("option", { value: x }, x + "× zoom")));
  const mute = h("button", { onclick: () => { vo.muted = !vo.muted; mute.textContent = vo.muted ? "Unmute" : "Mute"; } }, "Mute");
  const step = (n) => { vo.pause(); seekTo(vo.currentTime + n / fps); };
  const srcStatus = h("span", { class: "dim" });
  const previewBtn = h("button", { class: "small", onclick: () => makePreview(vo.currentTime) }, `Preview ${leftName.toLowerCase()} from here`);
  const alignEl = h("span", { class: "mono" }, "0 frames");
  function setNudge(v, how) {
    nudge = v;
    const k = Math.round(nudge * fps);
    alignEl.textContent = `${k > 0 ? "+" : ""}${k} frame${Math.abs(k) === 1 ? "" : "s"}${how ? " (" + how + ")" : ""}`;
    vs.currentTime = srcTime(vo.currentTime);
  }
  if (nudge) setNudge(nudge, "from the lengths");
  // Picks the source shift (−3..+3 frames) whose picture differs least from the output at a
  // few paused positions. Ties keep the current shift, so static scenes change nothing.
  const seekWait = (v, t) => new Promise((res) => {
    if (Math.abs(v.currentTime - t) < 1e-4 && !v.seeking) return res();
    const done = () => { clearTimeout(tm); res(); };
    const tm = setTimeout(done, 3000);
    v.addEventListener("seeked", done, { once: true });
    v.currentTime = t;
  });
  const grab = (v) => {
    const c = document.createElement("canvas"); c.width = 128; c.height = 72;
    const g = c.getContext("2d", { willReadFrequently: true }); g.drawImage(v, 0, 0, 128, 72);
    return g.getImageData(0, 0, 128, 72).data;
  };
  const pixDiff = (a, b) => { let d = 0; for (let i = 0; i < a.length; i += 4) d += Math.abs(a[i] - b[i]) + Math.abs(a[i + 1] - b[i + 1]) + Math.abs(a[i + 2] - b[i + 2]); return d; };
  let aligning = false;
  async function autoAlign() {
    if (closed || aligning || srcMode === "loading" || !vs.videoWidth || !vo.videoWidth) return;
    aligning = true;
    const was = vo.currentTime;
    vo.pause();
    alignEl.textContent = "aligning…";
    try {
      const lo = srcMode === "preview" ? offset : 0, hi = srcMode === "preview" ? segEnd : duration;
      const span = Math.max(0, hi - lo - 1);
      const points = [0.25, 0.5, 0.75].map((f) => lo + 0.5 + span * f);
      const score = new Map();
      const base = Math.round(nudge * fps);
      for (const t of points) {
        await seekWait(vo, t);
        const o = grab(vo);
        for (let k = base - 3; k <= base + 3; k++) {
          await seekWait(vs, Math.max(0, toSrc(t) - srcOff + k / fps));
          score.set(k, (score.get(k) || 0) + pixDiff(o, grab(vs)));
        }
      }
      let best = base;
      for (const [k, v] of score) if (v < score.get(best)) best = k;
      // Only move for a clear win (3% less difference), so noise doesn't shift the source.
      if (score.get(best) >= score.get(base) * 0.97) best = base;
      setNudge(best / fps, "auto");
    } catch (e) {
      alignEl.textContent = "align failed";
    } finally {
      aligning = false;
      await seekWait(vo, was);
      vs.currentTime = srcTime(was);
    }
  }

  const controls = h("div", { class: "controls" },
    playBtn, h("button", { title: "Previous frame (,)", onclick: () => step(-1) }, "◀ frame"), h("button", { title: "Next frame (.)", onclick: () => step(1) }, "frame ▶"),
    seek, timeEl, speed, zoomSel, mute);

  // ---- timeline ----
  const tl = s("svg", { viewBox: "0 0 1000 100", preserveAspectRatio: "none" });
  const tip = h("div", { class: "tooltip", hidden: true });
  const timeline = h("div", { class: "timeline" }, tl, tip);
  const chunks = r.chunks || [];
  const segs = chunks.map((c) => ({ ...c, end: c.start_secs + c.frames / fps }));
  const vs_ = segs.map((c) => c.vmaf).concat(r.verify_vmaf || [], [target]);
  const lo = Math.floor(Math.min(...vs_, target - 2) - 0.5), hi = Math.ceil(Math.max(...vs_, target + 2) + 0.5);
  const y = (v) => 92 - (v - lo) / (hi - lo) * 84;
  const x = (t) => (duration ? t / duration * 1000 : 0);
  // Two copies have no scores to draw: the timeline is just a seek bar.
  if (r.pair) tl.classList.add("plain");
  // grid
  if (!r.pair) for (let v = Math.ceil(lo); v <= hi; v += Math.max(1, Math.round((hi - lo) / 5))) {
    tl.append(s("line", { x1: 0, x2: 1000, y1: y(v), y2: y(v), stroke: "#2c3136", "stroke-width": 1, "vector-effect": "non-scaling-stroke" }));
  }
  if (!r.pair) tl.append(s("line", { x1: 0, x2: 1000, y1: y(target), y2: y(target), stroke: "#8b939b", "stroke-dasharray": "4 4", "stroke-width": 1, "vector-effect": "non-scaling-stroke" }));
  for (const c of segs) {
    const rect = s("rect", { x: x(c.start_secs), width: Math.max(0.5, x(c.end) - x(c.start_secs) - 0.6), y: y(c.vmaf), height: Math.max(1, 96 - y(c.vmaf)), fill: vmafColor(c.vmaf, target), opacity: 0.85 });
    rect.dataset.tip = `chunk ${c.index + 1} · ${tc(c.start_secs)}–${tc(c.end)} · CRF ${c.crf}${c.corrected ? " (corrected)" : ""} · VMAF ${num(c.vmaf)}${c.out_of_reach ? " (target out of reach at any CRF)" : ""} · ${bytes(c.bytes)} · ${num(c.bytes * 8 / (c.end - c.start_secs) / 1e6, 2)} Mbps`;
    tl.append(rect);
  }
  // Verification spot checks: 5 s windows at 30% and 70% of (duration − 5 s), spread the
  // same way where a long video has more than two, or one check of all of a video of 10 s
  // or less.
  (r.verify_vmaf || []).forEach((v, k, all) => {
    const secs = all.length === 1 ? duration : Math.min(5, duration), st = all.length === 1 ? 0 : (duration - secs) * (k + 0.75) / (all.length + 0.5);
    const c = s("circle", { cx: x(st + secs / 2), cy: y(v), r: 4, fill: "#6aa7e8", stroke: "#111", "vector-effect": "non-scaling-stroke" });
    c.dataset.tip = `verify spot check ${tc(st)}–${tc(st + secs)} · VMAF ${num(v)}`;
    tl.append(c);
  });
  // Where removed sections were joined.
  if (keep) {
    let at = 0;
    keep.forEach(([a, b], k) => {
      const gapBefore = k === 0 ? a : a - keep[k - 1][1];
      if (gapBefore > 0) {
        const m = s("rect", { x: x(at) - 2, width: 4, y: 0, height: 100, fill: "#e06666", opacity: 0.8 });
        m.dataset.tip = `cut at ${tc(at)}: source ${tc(a - gapBefore)}–${tc(a)} removed`;
        tl.append(m);
      }
      at += b - a;
    });
  }
  const segRect = s("rect", { x: 0, width: 0, y: 0, height: 100, fill: "#6aa7e8", opacity: 0.12 });
  tl.append(segRect);
  const head = s("line", { x1: 0, x2: 0, y1: 0, y2: 100, stroke: "#fff", "stroke-width": 1.5, "vector-effect": "non-scaling-stroke" });
  tl.append(head);
  const tlLabel = r.pair ? h("div", { class: "row dim" }, "Click the bar to seek.") : h("div", { class: "row dim" },
    h("span", {}, chunks.length ? `Per-chunk VMAF (${chunks.length} chunks, ${lo}–${hi}); dashed line = target ${target}. Click to seek.` : "Whole-file encode: one CRF, no per-chunk scores (use per-scene CRF for a timeline). Dots = verification spot checks."),
    h("span", { class: "spacer" }), h("span", { class: "ok" }, "■ ≥ target"), h("span", { class: "warn" }, "■ within 1"), h("span", { class: "bad" }, "■ below"));
  tl.addEventListener("mousemove", (e) => {
    const t = e.target.dataset && e.target.dataset.tip;
    const b = timeline.getBoundingClientRect();
    if (t) { tip.hidden = false; tip.textContent = t; tip.style.left = (e.clientX - b.left) + "px"; tip.style.top = (e.clientY - b.top) + "px"; }
    else tip.hidden = true;
  });
  tl.addEventListener("mouseleave", () => (tip.hidden = true));
  tl.addEventListener("click", (e) => {
    const b = tl.getBoundingClientRect();
    const t = (e.clientX - b.left) / b.width * duration;
    if (srcMode === "preview" && (t < offset || t > segEnd)) makePreview(Math.max(0, t - 1));
    else seekTo(t);
  });

  // ---- side panel ----
  const kv = (k, v) => [h("div", { class: "k" }, k), h("div", {}, v)];
  const fileKv = (name, path, i, size) => [
    kv(name, h("span", { class: "mono", title: path }, basename(path))),
    kv("", h("span", { class: "dim" }, dirname(path))),
    kv("", i.codec ? `${desc(i)} · ${num(i.fps, 2)} fps${i.hdr ? " HDR" : ""} · ${hms(i.duration_s)} · ${bytes(size)}` : (i.error || "–"))];
  // ---- the original's place ----
  const rp = r.replace || {};
  const act = async (path, body, said) => {
    try { await api(path, body); toast(said); route(); } catch (e) { toast(e.message, true); }
  };
  const replacePanel = r.pair || !rp.state ? null : h("div", { class: "panel" }, h("h3", {}, "Original"),
    rp.state === "ready" ? [
      h("p", { class: "dim" }, "Happy with it? The AV1 file can take the original's place: the original moves to the trash, the AV1 file gets its name in its folder."),
      h("div", { class: "row" }, h("button", { class: "danger", onclick: async (e) => {
        const btn = e.currentTarget;
        const ok = await confirmDialog({
          title: "Replace the original?",
          action: "Replace original",
          body: [
            h("p", {}, `The original (${bytes(r.source_bytes)}) moves to the trash of its drive and the AV1 file (${bytes(r.output_bytes)}) becomes:`),
            h("div", { class: "mono trash-list" }, rp.dest),
            rp.copy ? h("p", { class: "dim" }, "The AV1 file is on another drive: it is copied there first, then removed from the output folder.") : null,
            rp.removed ? h("p", { class: "dim" }, `The AV1 file leaves out ${plural(rp.removed, "section")} of the original.`) : null,
            rp.zfs ? h("p", { class: "warn" }, "The original is on ZFS, which av1opt writes to only when asked. Replacing it here is asking.") : null,
            h("p", { class: "dim" }, "The space is freed when you empty the trash. Until then this page can put the original back.")],
        });
        if (!ok) return;
        btn.disabled = true;
        btn.textContent = "Replacing…";
        try { await act("/api/replace", { report: r.report_path, allow_zfs: !!rp.zfs }, "Original replaced; it is in the trash"); }
        finally { btn.disabled = false; btn.textContent = "Replace original…"; }
      } }, "Replace original…"))]
    : rp.state === "replaced" ? [
      h("p", { class: "dim" }, "The AV1 file is where the original was. ", rp.can_undo ? "The original is in the trash." : "The original is no longer in the trash (or one of the two files changed), so this can't be undone here."),
      rp.can_undo ? h("div", { class: "row" }, h("button", { title: `Original back to ${r.input}\nAV1 file back to ${rp.from}`, onclick: async (e) => {
        const btn = e.currentTarget;
        if (rp.zfs && !await confirmDialog({
          title: "Put the original back on ZFS?",
          action: "Put original back",
          body: [h("p", { class: "warn" }, "The original's folder is on ZFS, which av1opt writes to only when asked. Putting it back here is asking."),
            h("div", { class: "mono trash-list" }, r.input)],
        })) return;
        btn.disabled = true;
        await act("/api/replace/undo", { report: r.report_path, allow_zfs: !!rp.zfs }, "Original back in its place");
        btn.disabled = false;
      } }, "Put original back")) : null]
    : rp.state === "not_recorded" ? h("p", { class: "dim" }, `${rp.why}.`)
    : h("p", { class: "dim" }, `The AV1 file can't take the original's place: ${rp.why}.`));

  const side = h("div", {},
    r.pair ? h("div", { class: "panel" },
      h("h3", {}, "Copies"),
      h("div", { class: "kv" }, fileKv("Left", r.input_path, ii, r.source_bytes), fileKv("Right", r.output_path, oi, r.output_bytes)),
      h("p", { class: "dim" }, "Sound comes from the right-hand file.")) :
    h("div", { class: "panel" },
      h("h3", {}, "Encode"),
      h("div", { class: "kv" },
        kv("Source", [h("span", { class: "mono", title: r.input_path }, basename(r.input || r.input_path)), rp.state === "replaced" && rp.can_undo ? h("span", { class: "pill" }, "in the trash") : null]),
        kv("Output", h("span", { class: "mono", title: r.output_path }, basename(r.output_path))),
        kv("Size", `${bytes(r.source_bytes)} → ${bytes(r.output_bytes)}`),
        removed.length ? kv("Removed", removed.map(([a, b]) => `${fmtTime(a)}–${fmtTime(b)}`).join(", ")) : null,
        kv("Saved", h("span", { class: savedClass(r.savings) }, savedPct(r.savings, 1))),
        kv("Mode", r.mode === "scenes" ? `per-scene CRF, ${chunks.length} chunks` : "one CRF"),
        kv("CRF", r.mode === "scenes" && chunks.length ? `${Math.min(...chunks.map((c) => c.crf))}–${Math.max(...chunks.map((c) => c.crf))} (start ${r.crf})` : r.crf),
        kv("Encoder", `preset ${r.preset}${r.svt_params ? " · " + r.svt_params : ""}`),
        kv("Target", `VMAF ${target} (${r.vmaf_model})`),
        r.mean_vmaf != null ? kv("Mean VMAF", num(r.mean_vmaf)) : null,
        r.whole ? kv("Lowest 5 s", `VMAF ${num(r.whole.lowest_vmaf)}${r.whole.stretches.length ? ` (${num(r.whole.lowest_before)} before the repair)` : ""}`) : null,
        r.whole && r.whole.stretches.length ? kv("Encoded again", h("span", { title: r.whole.stretches.map((x) => `${hms(x.start_secs)} +${num(x.secs, 1)} s: CRF ${x.crf}, VMAF ${num(x.vmaf_before)} → ${num(x.vmaf)}`).join("\n") },
          `${r.whole.stretches.length} stretch${r.whole.stretches.length > 1 ? "es" : ""}, ${hms(r.whole.stretches.reduce((a, x) => a + x.secs, 0))} in all, +${bytes(Math.max(0, r.whole.stretches.reduce((a, x) => a + x.bytes - x.bytes_before, 0)))}`)) : null,
        kv("Spot checks", (r.verify_vmaf || []).map((v) => num(v)).join(", ") || "–"),
        kv("Time", `search ${hms(r.search_secs)}, encode ${hms(r.encode_secs)}`),
        kv("Source video", ii.codec ? `${ii.codec} ${ii.width}×${ii.height} ${num(ii.fps, 2)} fps${ii.hdr ? " HDR" : ""}` : (ii.error || "–")))),
    replacePanel,
    r.search && r.search.length ? h("div", { class: "panel" }, h("h3", {}, "CRF search (samples)"), h("table", { class: "trials" },
      h("tr", {}, h("th", {}, "CRF"), h("th", { class: "num" }, "VMAF"), h("th", { class: "num" }, "worst"), h("th", { class: "num" }, "size")),
      r.search.map((t) => h("tr", {}, h("td", {}, t.crf), h("td", { class: "num " + (t.mean_vmaf >= target ? "ok" : "bad") }, num(t.mean_vmaf)), h("td", { class: "num dim" }, num(t.min_vmaf)), h("td", { class: "num" }, pct(t.size_ratio)))))) : null,
    r.tags && Object.keys(r.tags).length ? h("div", { class: "panel" }, h("h3", {}, "Tags written"),
      r.info_json ? h("div", { class: "dim" }, `from ${basename(r.info_json)}`) : null,
      h("div", { class: "kv" }, Object.entries(r.tags).map(([k, v]) => kv(k, h("span", { class: "mono", title: v }, v.length > 120 ? v.slice(0, 120) + "…" : v))))) : null,
    (r.notes || []).length ? h("div", { class: "panel" }, h("h3", {}, "Notes"), r.notes.map((n) => h("div", { class: "dim" }, n))) : null,
    h("div", { class: "panel" }, h("h3", {}, "Playback"), h("div", {}, srcStatus), h("div", { class: "row" }, previewBtn),
      h("h3", {}, "Frame alignment"),
      h("div", { class: "row" }, h("button", { class: "small", title: "Shift the source one frame earlier", onclick: () => setNudge(nudge - 1 / fps, "manual") }, "−1"),
        alignEl, h("button", { class: "small", title: "Shift the source one frame later", onclick: () => setNudge(nudge + 1 / fps, "manual") }, "+1"),
        h("button", { class: "small", onclick: () => autoAlign() }, "Auto")),
      h("p", { class: "dim" }, "Drag on the picture to move the split. Double-click zooms in at that point (4× → back to 1×); shift-click moves the zoom centre. Keys: space play, , and . step frames.")));

  root.append(h("div", { class: "row" }, h("h1", { class: "grow" }, basename(r.pair ? r.input_path : (r.input || r.input_path))), r.pair ? h("a", { href: "#/dups" }, "← Duplicates") : h("a", { href: "#/history" }, "← History")),
    h("div", { class: "cmp-grid" }, h("div", {}, stage, controls, timeline, tlLabel), side));

  // ---- sync ----
  function seekTo(t) {
    t = Math.max(0, Math.min(duration || t, t));
    if (srcMode === "preview") t = Math.max(offset, Math.min(segEnd - 0.05, t));
    vo.currentTime = t;
    vs.currentTime = srcTime(t);
  }
  function togglePlay() {
    if (vo.paused) {
      if (srcMode === "preview" && vo.currentTime >= segEnd - 0.1) seekTo(offset);
      vo.play().catch((e) => toast("Cannot play: " + e.message, true));
    } else vo.pause();
  }
  vo.addEventListener("play", () => { playBtn.textContent = "Pause"; vs.currentTime = srcTime(vo.currentTime); vs.play().catch(() => {}); });
  vo.addEventListener("pause", () => { playBtn.textContent = "Play"; vs.pause(); vs.currentTime = srcTime(vo.currentTime); });
  vo.addEventListener("seeking", () => { vs.currentTime = srcTime(vo.currentTime); });
  vo.addEventListener("ratechange", () => { vs.playbackRate = vo.playbackRate; });
  seek.addEventListener("input", () => seekTo(Number(seek.value)));
  // After a drag the slider lets go of the keyboard, so space and the frame keys reach the
  // player again. Moved with the arrow keys, it keeps it.
  let seekByPointer = false;
  seek.addEventListener("pointerdown", () => { seekByPointer = true; });
  seek.addEventListener("keydown", () => { seekByPointer = false; });
  seek.addEventListener("change", () => { if (seekByPointer) { seekByPointer = false; seek.blur(); } });

  let raf = 0;
  const tick = () => {
    raf = requestAnimationFrame(tick);
    const t = vo.currentTime;
    if (document.activeElement !== seek) seek.value = String(t);
    timeEl.textContent = `${tc(t)} / ${tc(duration)}`;
    head.setAttribute("x1", x(t)); head.setAttribute("x2", x(t));
    if (srcMode === "preview" && !vo.paused && t >= segEnd) { vo.pause(); seekTo(segEnd - 0.05); }
    const srcT = srcTime(t);
    const srcDone = srcT >= (vs.duration || Infinity) - 0.05;
    if (!vo.paused && srcMode !== "loading" && !srcDone && !vs.seeking && vs.readyState >= 2) {
      if (vs.paused) vs.play().catch(() => {});
      const drift = vs.currentTime - srcTime(t);
      if (Math.abs(drift) > 0.25) vs.currentTime = srcTime(t);
      else vs.playbackRate = vo.playbackRate * (1 - Math.max(-0.1, Math.min(0.1, drift * 0.8)));
    }
  };
  raf = requestAnimationFrame(tick);

  const onKey = (e) => {
    if (e.target.tagName === "INPUT" || e.target.tagName === "SELECT") return;
    // A dialog over the player has the keys.
    if (document.querySelector(".modal-bg")) return;
    if (e.key === " ") { e.preventDefault(); togglePlay(); }
    else if (e.key === ",") step(-1);
    else if (e.key === ".") step(1);
  };
  document.addEventListener("keydown", onKey);

  // ---- loading, with fallbacks for files the browser can't decode ----
  // Resolves when `v` shows picture, rejects if it can't (error, no video track, or timeout).
  function tryLoad(v, url) {
    return new Promise((resolve, reject) => {
      let done = false;
      const finish = (ok, why) => { if (done) return; done = true; clearTimeout(timer); v.removeEventListener("loadeddata", onData); v.removeEventListener("error", onErr); ok ? resolve() : reject(new Error(why)); };
      const onData = () => (v.videoWidth > 0 ? finish(true) : finish(false, "no decodable video track"));
      const onErr = () => finish(false, (v.error && v.error.message) || "cannot play this format");
      const timer = setTimeout(() => finish(false, "timed out"), 30000);
      v.addEventListener("loadeddata", onData);
      v.addEventListener("error", onErr);
      v.src = url;
      v.load();
    });
  }

  function busy(msg) {
    overlay.hidden = !msg;
    overlay.replaceChildren(msg ? h("div", {}, h("span", { class: "spin" }), " ", msg) : "");
  }

  async function makePreview(at) {
    if (closed) return;
    let start = Math.max(0, Math.min(at, Math.max(0, duration - 2)));
    let secs = Math.min(20, Math.max(2, duration - start));
    if (keep) {
      // Keep the clip inside one kept part, where output and source run in step.
      let base = 0;
      for (const [a, b] of keep) {
        const len = b - a;
        if (start < base + len || b === keep[keep.length - 1][1]) {
          secs = Math.min(20, len);
          start = Math.max(base, Math.min(start, base + len - secs));
          break;
        }
        base += len;
      }
    }
    vo.pause();
    busy(`Making a ${Math.round(secs)} s near-lossless H.264 preview of the source from ${tc(start)}…`);
    previewBtn.disabled = true;
    const prev = { offset, segEnd, srcOff, srcMode, src: vs.src };
    srcMode = "loading";
    try {
      // The clip starts where the source lines up with the output, shift included.
      const p = await api("/api/preview", { path: r.input_path, kind: "transcode", start: Math.max(0, toSrc(start) + nudge), secs });
      if (closed) return;
      await tryLoad(vs, p.url);
      if (closed) return;
      offset = start; segEnd = start + p.secs; srcOff = p.start; srcMode = "preview";
      segRect.setAttribute("x", x(offset)); segRect.setAttribute("width", x(segEnd) - x(offset));
      srcStatus.replaceChildren(h("span", { class: "warn" }, `${leftName}: preview clip `), `${tc(srcOff)}–${tc(srcOff + p.secs)}. The browser can't decode ${ii.codec || "this file"} directly, so this clip is re-encoded near-losslessly (H.264 CRF 12)${ii.hdr ? "; HDR shown without tone mapping" : ""}. Click the timeline outside the shaded part for a new clip.`);
      tagL.textContent = `${leftName} preview · ${ii.codec || "?"} → H.264 near-lossless`;
      seekTo(offset);
      busy(null);
      await autoAlign();
      seekTo(offset);
    } catch (e) {
      if (closed) return;
      ({ offset, segEnd, srcOff, srcMode } = prev);
      if (prev.src && srcMode !== "loading") { vs.src = prev.src; vs.currentTime = srcTime(vo.currentTime); }
      busy(null);
      overlay.hidden = false;
      overlay.replaceChildren(h("div", { class: "bad" }, "Could not make a source preview: " + e.message));
    } finally { previewBtn.disabled = false; }
  }

  (async () => {
    if (oi.error || ii.error) {
      overlay.hidden = false;
      overlay.replaceChildren(h("div", { class: "bad" }, oi.error || ii.error));
      if (oi.error) return;
    }
    busy("Loading…");
    try {
      await tryLoad(vo, mediaUrl(r.output_path));
    } catch {
      if (closed) return;
      busy("Your browser can't open this container; copying the video stream into MP4…");
      try {
        const p = await api("/api/preview", { path: r.output_path, kind: "remux" });
        if (closed) return;
        await tryLoad(vo, p.url);
      } catch (e) {
        overlay.replaceChildren(h("div", { class: "bad" }, (r.pair ? `Cannot play the right-hand file (${oi.codec || "?"}) in this browser: ` : "Cannot play the AV1 output here: ") + e.message));
        return;
      }
    }
    if (closed) return;
    if (ii.error) { busy(null); overlay.hidden = false; overlay.replaceChildren(h("div", { class: "bad" }, ii.error)); return; }
    try {
      await tryLoad(vs, mediaUrl(r.input_path));
      if (closed) return;
      srcStatus.replaceChildren(h("span", { class: "ok" }, `${leftName}: original file`), " played directly.");
      previewBtn.hidden = false;
      busy(null);
      autoAlign();
    } catch (e) {
      if (closed) return;
      busy(null);
      await makePreview(0);
    }
  })();

  return () => {
    closed = true;
    cancelAnimationFrame(raf);
    document.removeEventListener("keydown", onKey);
    for (const v of [vo, vs]) { v.pause(); v.removeAttribute("src"); v.load(); }
  };
}

// ---------- settings ----------

views.settings = (root) => {
  const t = (STATE && STATE.tools) || {};
  const ffmpeg = h("input", { type: "text", value: t.ffmpeg === t.default_ffmpeg ? "" : t.ffmpeg || "", placeholder: t.default_ffmpeg || "ffmpeg", spellcheck: "false" });
  const ffprobe = h("input", { type: "text", value: t.ffprobe === t.default_ffprobe ? "" : t.ffprobe || "", placeholder: t.default_ffprobe || "ffprobe", spellcheck: "false" });
  const result = h("div");
  const save = h("button", { class: "primary" }, "Save and check");

  const show = (tools) => {
    const c = tools.capabilities || {};
    const item = (ok, text) => h("li", { class: ok ? "ok" : "bad" }, (ok ? "✓ " : "✗ ") + text);
    result.replaceChildren(
      h("ul", { class: "checks" },
        item(!!c.ffmpeg_version, c.ffmpeg_version || `ffmpeg does not run (${tools.ffmpeg})`),
        item(c.svtav1, "libsvtav1 encoder"),
        item(c.vmaf, "libvmaf filter"),
        item(c.ffprobe_ok, c.ffprobe_ok ? `ffprobe (${tools.ffprobe})` : `ffprobe does not run (${tools.ffprobe})`)),
      h("p", { class: tools.problem ? "warn" : "ok" }, tools.problem ? tools.problem : "Ready to encode."));
  };
  save.onclick = async () => {
    save.disabled = true;
    try {
      const tools = await api("/api/settings/tools", { ffmpeg: ffmpeg.value, ffprobe: ffprobe.value });
      // The page may have opened while av1opt could not be reached: there is no state yet.
      if (!STATE) await loadState().catch(() => {});
      if (STATE) STATE.tools = tools;
      renderBanner();
      show(tools);
      refreshBadge();
    } catch (e) { toast(e.message, true); }
    save.disabled = false;
  };
  for (const i of [ffmpeg, ffprobe]) i.addEventListener("keydown", (e) => { if (e.key === "Enter") save.click(); });

  root.append(
    h("h1", {}, "Settings"),
    h("div", { class: "panel" },
      h("h3", {}, "Tools"),
      h("p", { class: "dim" }, "av1opt needs an ffmpeg with the libsvtav1 encoder and the libvmaf filter. Leave a field empty to use the default from the command line or environment (shown greyed). Give an absolute path or a program name on PATH."),
      h("div", { class: "form" }, h("label", {}, "ffmpeg"), ffmpeg, h("label", {}, "ffprobe"), ffprobe),
      h("div", { class: "row gap-top" }, save),
      result),
    desktop && desktop.openLog ? h("div", { class: "panel" },
      h("h3", {}, "Log"),
      h("p", { class: "dim" }, "Messages from av1opt while the desktop app runs, with times. Useful when an encode failed overnight."),
      h("div", { class: "row" },
        h("button", { onclick: () => desktop.openLog(false).then((ok) => ok || toast("Could not open the log", true), () => toast("Could not open the log", true)) }, "Open log"),
        h("button", { onclick: () => desktop.openLog(true).catch(() => toast("Could not open the log", true)) }, "Show in folder"))) : null,
    h("div", { class: "panel" },
      h("h3", {}, "About"),
      h("div", { class: "kv" },
        h("span", { class: "k" }, "Version"), h("span", {}, STATE ? STATE.version : "–"),
        h("span", { class: "k" }, "Encodes behind scan estimates"), h("span", {}, STATE ? STATE.history_encodes : "–"))));
  show(t);
};

// ---------- start ----------

window.addEventListener("hashchange", route);
(async () => {
  try { await loadState(); } catch (e) { toast("Cannot reach av1opt: " + e.message, true); }
  connect();
  refreshBadge();
  route();
})();
