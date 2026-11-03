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

function bytes(n) {
  if (n == null || isNaN(n)) return "–";
  const a = Math.abs(n);
  if (a >= 1e12) return (n / 1e12).toFixed(2) + " TB";
  if (a >= 1e9) return (n / 1e9).toFixed(a >= 1e11 ? 0 : 1) + " GB";
  if (a >= 1e6) return (n / 1e6).toFixed(a >= 1e8 ? 0 : 1) + " MB";
  if (a >= 1e3) return (n / 1e3).toFixed(0) + " kB";
  return n + " B";
}
function pct(x, digits = 0) { return x == null || isNaN(x) ? "–" : (x * 100).toFixed(digits) + "%"; }
function num(x, d = 2) { return x == null || isNaN(x) ? "–" : Number(x).toFixed(d); }
function hms(sec) {
  if (sec == null || isNaN(sec)) return "–";
  sec = Math.max(0, Math.round(sec));
  const hh = Math.floor(sec / 3600), mm = Math.floor(sec / 60) % 60, ss = sec % 60;
  return (hh ? hh + ":" + String(mm).padStart(2, "0") : mm) + ":" + String(ss).padStart(2, "0");
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
  es.onopen = () => { dot.classList.add("on"); dot.title = "Live updates connected"; };
  es.onerror = () => { dot.classList.remove("on"); dot.title = "Reconnecting…"; };
  es.onmessage = (m) => {
    let msg;
    try { msg = JSON.parse(m.data); } catch { return; }
    for (const fn of [...listeners]) fn(msg);
  };
}

// Queue badge: number of queued + running jobs.
async function refreshBadge() {
  try {
    const { jobs } = await api("/api/jobs/heads");
    const n = jobs.filter((j) => j.status === "queued" || j.status === "running").length;
    const b = document.getElementById("queue-badge");
    b.textContent = n;
    b.hidden = n === 0;
  } catch { /* offline */ }
}
onEvent((m) => { if (m.kind === "jobs") refreshBadge(); });

// ---------- state ----------

let STATE = null;
async function loadState() { STATE = await api("/api/state"); renderBanner(); return STATE; }

// Shown on every view while encodes cannot run (usually: ffmpeg without libvmaf).
function renderBanner() {
  const b = document.getElementById("banner");
  const p = STATE && STATE.tools && STATE.tools.problem;
  b.hidden = !p;
  if (p) b.replaceChildren("Encoding is off: " + p + ".", h("a", { href: "#/settings" }, "Settings"));
}

// ---------- folder / file picker ----------

// Resolves to an array of absolute paths, or null if cancelled.
// mode: "dir" picks one folder; "multi" picks any mix of folders and video files.
function pick({ title, mode = "dir", start }) {
  return new Promise((resolve) => {
    const selected = new Set();
    let cur = start || prefs.get("pickerDir", null) || (STATE && STATE.home) || "/";
    let hidden = false;
    const pathEl = h("div", { class: "picker-path" });
    const list = h("ul", { class: "picker-list" });
    const places = h("div", { class: "places" });
    const selInfo = h("span", { class: "dim grow" });
    const close = (v) => { bg.remove(); document.removeEventListener("keydown", onKey); resolve(v); };
    const onKey = (e) => { if (e.key === "Escape") close(null); };
    const choose = h("button", { class: "primary", onclick: () => {
      prefs.set("pickerDir", cur);
      if (mode === "dir") close([cur]);
      else close(selected.size ? [...selected] : [cur]);
    } }, mode === "dir" ? "Use this folder" : "Add selected");

    const updateSel = () => {
      if (mode === "dir") selInfo.textContent = "";
      else selInfo.textContent = selected.size ? `${selected.size} selected` : "nothing selected: adds the current folder";
    };

    async function go(p) {
      let r;
      try { r = await api("/api/browse?path=" + encodeURIComponent(p) + (hidden ? "&hidden=1" : "")); }
      catch (e) { toast(e.message, true); return; }
      cur = r.path;
      pathEl.textContent = r.path;
      places.replaceChildren(...r.places.map((pl) => h("button", { class: "small", onclick: () => go(pl.path) }, pl.name)));
      const join = (n) => (r.path === "/" ? "/" : r.path + "/") + n;
      const items = [];
      if (r.parent) items.push(h("li", { ondblclick: () => go(r.parent), onclick: () => go(r.parent) }, h("span", { class: "icon" }, "↰"), ".."));
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
        items.push(li);
      }
      for (const f of r.files) {
        const full = join(f.name);
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
        } else li.classList.add("dim");
        items.push(li);
      }
      if (!r.dirs.length && !r.files.length) items.push(h("li", { class: "dim" }, "(empty)"));
      list.replaceChildren(...items);
      updateSel();
    }

    const bg = h("div", { class: "modal-bg", onclick: (e) => { if (e.target === bg) close(null); } },
      h("div", { class: "modal" },
        h("div", { class: "head" }, h("strong", {}, title || "Pick a folder"), h("span", { class: "spacer" }),
          h("label", { class: "check dim" }, h("input", { type: "checkbox", onchange: (e) => { hidden = e.target.checked; go(cur); } }), "hidden")),
        h("div", { class: "body" }, places, h("div", { class: "row" }, pathEl), h("hr", { class: "dim" }), list),
        h("div", { class: "foot" }, selInfo, h("button", { onclick: () => close(null) }, "Cancel"), choose)));
    document.body.append(bg);
    document.addEventListener("keydown", onKey);
    go(cur);
  });
}

// ---------- encode options + enqueue dialog ----------

function defaultOptions() {
  return Object.assign({ container: "mp4", target_vmaf: 93, preset: 6, scenes: false, workers: 1, min_savings: 0.15, force: false, retry_skipped: false }, prefs.get("encodeOptions", {}));
}

// Shows the encode dialog for `inputs` (paths). Resolves true if queued.
function encodeDialog(inputs, roots = []) {
  return new Promise((resolve) => {
    const o = defaultOptions();
    let outDir = prefs.get("outDir", null) || (STATE && STATE.default_out_dir) || "";
    const outEl = h("code", {}, outDir);
    const vmaf = h("input", { type: "number", step: "0.5", min: "50", max: "99.5", value: o.target_vmaf });
    const preset = h("input", { type: "number", min: "0", max: "13", value: o.preset });
    const container = h("select", {}, h("option", { value: "mp4" }, "MP4 (plays in browsers)"), h("option", { value: "mkv" }, "MKV (keeps image subtitles)"));
    container.value = o.container;
    const scenes = h("input", { type: "checkbox", checked: o.scenes });
    const workers = h("input", { type: "number", min: "1", max: "8", value: o.workers });
    const minSav = h("input", { type: "number", min: "0", max: "95", step: "1", value: Math.round(o.min_savings * 100) });
    const force = h("input", { type: "checkbox", checked: o.force });
    const retry = h("input", { type: "checkbox", checked: o.retry_skipped });
    const close = (v) => { bg.remove(); resolve(v); };
    const go = h("button", { class: "primary", onclick: async () => {
      const options = {
        container: container.value, target_vmaf: Number(vmaf.value), preset: Number(preset.value), scenes: scenes.checked,
        workers: Number(workers.value), min_savings: Number(minSav.value) / 100, force: force.checked, retry_skipped: retry.checked,
      };
      prefs.set("encodeOptions", options);
      prefs.set("outDir", outDir);
      go.disabled = true;
      try {
        const r = await api("/api/jobs", { inputs, out_dir: outDir, roots, options });
        toast(`Queued ${r.files} file${r.files === 1 ? "" : "s"}`);
        close(true);
        location.hash = "#/queue";
      } catch (e) { toast(e.message, true); go.disabled = false; }
    } }, "Add to queue");
    const list = inputs.length <= 6 ? inputs.map((p) => h("div", { class: "mono" }, p)) : [h("div", { class: "mono" }, inputs.slice(0, 4).join("\n")), h("div", { class: "dim" }, `… and ${inputs.length - 4} more`)];
    const bg = h("div", { class: "modal-bg", onclick: (e) => { if (e.target === bg) close(false); } },
      h("div", { class: "modal" },
        h("div", { class: "head" }, h("strong", {}, `Encode ${inputs.length} item${inputs.length === 1 ? "" : "s"}`)),
        h("div", { class: "body" },
          h("h3", {}, "Inputs"), list,
          h("h3", {}, "Output folder"),
          h("div", { class: "row" }, outEl, h("button", { class: "small", onclick: async () => {
            const p = await pick({ title: "Output folder (not on ZFS)", mode: "dir", start: outDir || undefined });
            if (p) { outDir = p[0]; outEl.textContent = outDir; }
          } }, "Change…")),
          h("p", { class: "dim" }, "Folders are mirrored below it. Inputs are never modified. Re-adding the same files resumes."),
          h("h3", {}, "Settings"),
          h("div", { class: "row" },
            h("label", { class: "field" }, "Target VMAF", vmaf),
            h("label", { class: "field" }, "Preset (lower = smaller, slower)", preset),
            h("label", { class: "field" }, "Container", container),
            h("label", { class: "field" }, "Min savings %", minSav)),
          h("div", { class: "row" },
            h("label", { class: "check" }, scenes, "Per-scene CRF (--scenes)"),
            h("label", { class: "field" }, "Chunk workers", workers),
            h("label", { class: "check" }, force, "Force (also AV1 / low gain)"),
            h("label", { class: "check" }, retry, "Retry earlier skips"))),
        h("div", { class: "foot" }, h("button", { onclick: () => close(false) }, "Cancel"), go)));
    document.body.append(bg);
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
  const minSav = h("input", { type: "number", min: "0", max: "95", value: prefs.get("scanMinSavings", 20) });
  const vmaf = h("input", { type: "number", step: "0.5", value: prefs.get("scanVmaf", 93) });
  const start = h("button", { class: "primary" }, "Scan");
  const progress = h("div", { class: "bar grow" }, h("div"));
  const progText = h("span", { class: "dim" });
  const out = h("div");
  let scanId = null;

  const drawChips = () => {
    chips.replaceChildren(...[...roots].map((r) => h("span", { class: "chip" }, h("code", {}, r), h("button", { title: "Remove", onclick: () => { roots.delete(r); prefs.set("scanRoots", [...roots]); drawChips(); } }, "×"))));
    if (!roots.size) chips.append(h("span", { class: "dim" }, "No folders yet."));
    start.disabled = !roots.size;
  };
  drawChips();

  start.onclick = async () => {
    prefs.set("scanMinSavings", Number(minSav.value));
    prefs.set("scanVmaf", Number(vmaf.value));
    start.disabled = true;
    try {
      const r = await api("/api/scans", { paths: [...roots], min_savings: Number(minSav.value) / 100, target_vmaf: Number(vmaf.value) });
      scanId = lastScanId = r.id;
      progText.textContent = "listing files…";
      progress.firstChild.style.width = "0%";
      out.replaceChildren();
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
    out);

  async function showResult(id) {
    let r;
    try { r = await api("/api/scans/" + id); } catch (e) { toast(e.message, true); return; }
    start.disabled = !roots.size;
    if (r.status === "running") {
      progText.textContent = r.total ? `probed ${r.done} / ${r.total}` : "listing files…";
      progress.firstChild.style.width = r.total ? (100 * r.done / r.total) + "%" : "0%";
      scanId = id; start.disabled = true;
      return;
    }
    progress.firstChild.style.width = "100%";
    if (r.status === "failed") { progText.textContent = "failed"; out.replaceChildren(h("div", { class: "panel bad" }, r.error)); return; }
    progText.textContent = `${r.rows.length} videos · estimates from ${r.model_samples ? r.model_samples + " past encodes" : "the built-in model (encode a few files to calibrate)"}`;
    renderScan(out, r);
  }

  const off = onEvent((m) => {
    if (m.kind === "scan" && m.id === scanId) {
      progText.textContent = m.total ? `probed ${m.done} / ${m.total}` : "listing files…";
      progress.firstChild.style.width = m.total ? (100 * m.done / m.total) + "%" : "0%";
    } else if (m.kind === "scan_done" && m.id === scanId) showResult(m.id);
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
  const rows = r.rows;
  const body = h("tbody");
  const selInfo = h("span", { class: "dim" });
  const encodeBtn = h("button", { class: "primary", disabled: true, onclick: () => encodeDialog([...selected], r.paths) }, "Encode selected…");
  const allBox = h("input", { type: "checkbox", title: "Select all shown" });

  const shown = () => {
    let v = rows.filter((x) => filter === "all" || (filter === "candidate" ? x.verdict === "Candidate" : x.verdict !== "Candidate"));
    const key = { saved: savedOf, size: (x) => x.size, name: (x) => x.rel, mbps: (x) => x.video_mbps || 0, dur: (x) => x.duration_s || 0 }[sortKey];
    v.sort((a, b) => { const ka = key(a), kb = key(b); return (ka < kb ? -1 : ka > kb ? 1 : 0) * sortDir; });
    return v;
  };
  const updateSel = () => {
    selInfo.textContent = selected.size ? `${selected.size} selected · ${bytes(rows.filter((x) => selected.has(x.path)).reduce((a, x) => a + savedOf(x), 0))} est. saved` : "";
    encodeBtn.disabled = !selected.size;
  };
  let limit = 500; // more rows make every sort and filter click slow
  const draw = () => {
    const v = shown();
    body.replaceChildren(...v.slice(0, limit).map((x) => {
      const cb = h("input", { type: "checkbox", checked: selected.has(x.path), onchange: () => { if (cb.checked) selected.add(x.path); else selected.delete(x.path); updateSel(); } });
      const cls = x.verdict === "Candidate" ? "ok" : x.verdict === "Unreadable" ? "bad" : "";
      return h("tr", {},
        h("td", {}, cb),
        h("td", { class: "path", title: x.path }, x.rel),
        h("td", {}, x.codec || "–", x.hdr ? h("span", { class: "pill warn" }, "HDR") : null),
        h("td", { class: "num" }, x.width ? `${x.width}×${x.height}` : "–"),
        h("td", { class: "num" }, hms(x.duration_s)),
        h("td", { class: "num" }, num(x.video_mbps, 1)),
        h("td", { class: "num" }, bytes(x.size)),
        h("td", { class: "num" }, x.verdict === "Candidate" ? `~${bytes(x.estimate.likely)}` : "–"),
        h("td", { class: "num" }, x.verdict === "Candidate" ? h("span", { title: `range ${bytes(x.size - x.estimate.high)} – ${bytes(x.size - x.estimate.low)}` }, `${bytes(savedOf(x))} (${pct(savedOf(x) / x.size)})`) : "–"),
        h("td", {}, h("span", { class: "pill " + cls, title: x.error || "" }, x.verdict_label)));
    }));
    if (v.length > limit) body.append(h("tr", {}, h("td", { colspan: 10, class: "dim" }, `${v.length - limit} more not shown. `,
      h("button", { class: "small", onclick: () => { limit = Infinity; draw(); } }, "Show all"))));
    if (!v.length) body.append(h("tr", {}, h("td", { colspan: 10, class: "empty" }, "Nothing here.")));
    allBox.checked = v.length > 0 && v.every((x) => selected.has(x.path));
  };
  allBox.onchange = () => { for (const x of shown()) { if (allBox.checked) selected.add(x.path); else selected.delete(x.path); } draw(); updateSel(); };
  const th = (label, key, cls) => h("th", { class: "sort " + (cls || ""), onclick: () => { if (sortKey === key) sortDir *= -1; else { sortKey = key; sortDir = key === "name" ? 1 : -1; } draw(); } }, label);
  const filt = h("select", { onchange: (e) => { filter = e.target.value; prefs.set("scanFilter", filter); draw(); } },
    h("option", { value: "candidate" }, "Candidates"), h("option", { value: "skip" }, "Skipped"), h("option", { value: "all" }, "All files"));
  filt.value = filter;

  out.replaceChildren(
    h("div", { class: "stats" },
      h("div", { class: "stat" }, h("div", { class: "v" }, t.files), h("div", { class: "l" }, `videos · ${bytes(t.bytes)}`)),
      h("div", { class: "stat" }, h("div", { class: "v" }, t.candidates), h("div", { class: "l" }, `candidates · ${bytes(t.candidate_bytes)}`)),
      h("div", { class: "stat" }, h("div", { class: "v ok" }, bytes(t.saved_likely)), h("div", { class: "l" }, `estimated savings (${bytes(t.saved_low)} – ${bytes(t.saved_high)})`)),
      h("div", { class: "stat" }, h("div", { class: "v" }, pct(t.bytes ? t.saved_likely / t.bytes : 0)), h("div", { class: "l" }, "of the whole library"))),
    h("div", { class: "panel" },
      h("div", { class: "row" }, filt, selInfo, h("span", { class: "spacer" }), encodeBtn),
      h("div", { class: "tablewrap" }, h("table", {},
        h("thead", {}, h("tr", {}, h("th", {}, allBox), th("File", "name"), h("th", {}, "Codec"), h("th", { class: "num" }, "Size px"), th("Length", "dur", "num"), th("Mbps", "mbps", "num"), th("Size", "size", "num"), h("th", { class: "num" }, "AV1 est."), th("Saves", "saved", "num"), h("th", {}, "Verdict"))),
        body))));
  draw();
  updateSel();
}

// ----- queue -----

const PHASES = { probing: "Probing", sampling: "Cutting samples", searching: "Searching CRF", scenes: "Finding scenes", chunks: "Encoding chunks", encoding: "Encoding", muxing: "Muxing", verifying: "Verifying" };
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

views.queue = (root) => {
  const list = h("div");
  root.append(
    h("div", { class: "row" }, h("h1", { class: "grow" }, "Encode queue"),
      h("button", { onclick: async () => { await api("/api/jobs/clear", {}); load(); } }, "Clear finished"),
      h("button", { class: "primary", onclick: async () => { const p = await pick({ title: "Videos or folders to encode", mode: "multi" }); if (p) encodeDialog(p); } }, "New encode…")),
    list);
  const fileEls = new Map(); // "job:file" -> { el, update(progress) } for running files
  // Rendered jobs. Only parts whose data changed are rebuilt, so buttons stay put while an
  // encode streams events.
  const jobEls = new Map(); // id -> { box, head, headSig, files, nodes[], sigs[], err }
  const empty = h("div", { class: "panel empty" }, "No encodes yet. Pick files in Scan, or use “New encode…”.");
  let jobs = [];
  let timer = null;
  const pending = new Map(); // job id -> timeout

  async function load() {
    try { jobs = (await api("/api/jobs")).jobs; } catch (e) { toast(e.message, true); return; }
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
    if (!jobs.length) { list.replaceChildren(empty); return; }
    empty.remove();
    for (const j of [...jobs].reverse()) list.append(patchJob(j)); // append moves nodes into order
  }

  const headSig = (j) => JSON.stringify([j.status, j.error, j.counts, j.saved]);
  const fileSig = (f) => JSON.stringify({ ...f, progress: null });

  function patchJob(j) {
    let e = jobEls.get(j.id);
    if (!e) {
      e = { box: h("div", { class: "job" }), head: null, headSig: "", files: h("div", { class: "files" }), nodes: [], sigs: [], err: null };
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
    if (j.error) { e.err = h("div", { class: "file bad" }, j.error); e.box.append(e.err); }
  }

  function patchFile(e, j, f, i) {
    const sg = fileSig(f);
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
      statusPill(j.status),
      h("strong", {}, `${j.total} file${j.total === 1 ? "" : "s"}`),
      h("span", { class: "dim" }, `→ ${j.out_dir}`),
      h("span", { class: "dim" }, `VMAF ${o.target_vmaf} · preset ${o.preset} · ${o.container}${o.scenes ? " · scenes×" + o.workers : ""}`),
      h("span", { class: "spacer" }),
      saved > 0 ? h("span", { class: "ok" }, `${bytes(saved)} saved`) : null,
      h("span", { class: "dim" }, Object.entries(counts).map(([k, v]) => `${v} ${(STATUS[k] || [k])[0].toLowerCase()}`).join(" · ")),
      j.status === "running" || j.status === "queued"
        ? h("button", { class: "danger small", onclick: async (e) => { e.target.disabled = true; try { await api(`/api/jobs/${j.id}/cancel`, {}); toast("Cancelling… (finished chunks are kept; re-queue to resume)"); } catch (er) { toast(er.message, true); } } }, "Cancel")
        : h("button", { class: "small", onclick: async () => { await api(`/api/jobs/${j.id}/remove`, {}); load(); } }, "Remove"));
    return head;
  }

  function drawFile(j, f, i) {
    const name = h("div", { class: "name" }, basename(f.input), h("div", { class: "dim mono" }, dirname(f.input)));
    const right = h("div", { class: "row" });
    if (f.status === "ok" || f.status === "done_before") {
      if (f.input_bytes && f.output_bytes) right.append(h("span", {}, `${bytes(f.input_bytes)} → ${bytes(f.output_bytes)} `, h("span", { class: "ok" }, `−${pct(1 - f.output_bytes / f.input_bytes)}`)));
      right.append(h("a", { class: "btn", href: "#/compare?report=" + encodeURIComponent(f.report) }, "Compare"));
    }
    const el = h("div", { class: "file" }, h("div", { class: "line1" }, statusPill(f.status), name, right));
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
      f.chosen ? h("div", {}, `CRF ${f.chosen.crf} → VMAF ${num(f.chosen.vmaf)}, predicted video savings `, h("span", { class: "ok" }, pct(f.chosen.predicted_savings))) : null,
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
      progText.textContent = `${(p.frac * 100).toFixed(1)}%${p.fps ? " · " + num(p.fps, 1) + " fps" : ""} · ETA ${hms(p.eta_secs)}`;
    };
    update(f.progress);
    return { el, update };
  }

  const off = onEvent((m) => {
    if (m.kind === "jobs") return reload();
    if (m.kind !== "job") return;
    if (m.event.type === "progress") {
      const live = fileEls.get(`${m.id}:${m.file}`);
      if (live) return live.update(m.event);
    }
    if (m.file == null) reload(); else reloadFile(m.id, m.file);
  });
  load();
  return () => { off(); clearTimeout(timer); for (const t of pending.values()) clearTimeout(t); };
};

// ----- history -----

views.history = (root) => {
  const out = h("div", { class: "empty" }, h("span", { class: "spin" }));
  root.append(h("div", { class: "row" }, h("h1", { class: "grow" }, "History"),
    h("button", { onclick: async () => { const p = await pick({ title: "Folder with av1opt outputs" }); if (p) { try { await api("/api/history/dirs", { path: p[0] }); load(); } catch (e) { toast(e.message, true); } } } }, "Add folder…")), out);

  async function load() {
    let r;
    try { r = await api("/api/history"); } catch (e) { toast(e.message, true); return; }
    const reps = r.reports;
    const tin = reps.reduce((a, x) => a + (x.source_bytes || 0), 0), tout = reps.reduce((a, x) => a + (x.output_bytes || 0), 0);
    const dirs = h("div", { class: "chips" }, r.dirs.map((d) => h("span", { class: "chip" }, h("code", {}, d), h("button", { title: "Forget folder", onclick: async () => { await api("/api/history/dirs/remove", { path: d }); load(); } }, "×"))));
    if (!r.dirs.length) dirs.append(h("span", { class: "dim" }, "No folders yet. Output folders of queued encodes are added automatically."));
    // Thousands of rows take a while to lay out; the newest come first, the rest on request.
    const LIMIT = 500;
    const row = (x) => h("tr", { class: "click", onclick: () => (location.hash = "#/compare?report=" + encodeURIComponent(x.report)) },
      h("td", { class: "dim" }, when(x.time)),
      h("td", { class: "path", title: x.input }, basename(x.input), x.input_exists ? null : h("span", { class: "pill warn", title: "source no longer at this path" }, "source gone"), x.output_exists ? null : h("span", { class: "pill bad" }, "output gone")),
      h("td", {}, x.mode === "scenes" ? `scenes (${x.chunks})` : "whole"),
      h("td", { class: "num" }, x.crf),
      h("td", { class: "num" }, x.target_vmaf),
      h("td", { class: "num" }, x.mean_vmaf != null ? num(x.mean_vmaf) : (x.verify_vmaf || []).map((v) => num(v, 1)).join(" / ")),
      h("td", { class: "num" }, bytes(x.source_bytes)),
      h("td", { class: "num" }, bytes(x.output_bytes)),
      h("td", { class: "num ok" }, pct(x.savings)),
      h("td", { class: "num dim" }, hms((x.search_secs || 0) + (x.encode_secs || 0))));
    const tbody = h("tbody", {}, reps.slice(0, LIMIT).map(row));
    if (reps.length > LIMIT) {
      const more = h("tr", {}, h("td", { colspan: 10, class: "dim" }, `${reps.length - LIMIT} older encodes not shown. `,
        h("button", { class: "small", onclick: () => { more.remove(); tbody.append(...reps.slice(LIMIT).map(row)); } }, "Show all")));
      tbody.append(more);
    }
    out.className = "";
    out.replaceChildren(
      h("div", { class: "panel" }, h("h3", {}, "Folders searched for reports"), dirs),
      h("div", { class: "stats" },
        h("div", { class: "stat" }, h("div", { class: "v" }, reps.length), h("div", { class: "l" }, "encodes")),
        h("div", { class: "stat" }, h("div", { class: "v" }, `${bytes(tin)} → ${bytes(tout)}`), h("div", { class: "l" }, "source → AV1")),
        h("div", { class: "stat" }, h("div", { class: "v ok" }, bytes(tin - tout)), h("div", { class: "l" }, `saved (${pct(tin ? 1 - tout / tin : 0)})`)),
        h("div", { class: "stat" }, h("div", { class: "v" }, STATE ? STATE.history_encodes : "–"), h("div", { class: "l" }, "encodes calibrating scan estimates"))),
      h("div", { class: "panel" }, reps.length
        ? h("div", { class: "tablewrap" }, h("table", {}, h("thead", {}, h("tr", {}, ["When", "File", "Mode", "CRF", "Target", "VMAF", "Source", "AV1", "Saved", "Time"].map((t, k) => h("th", { class: k >= 3 ? "num" : "" }, t)))), tbody))
        : h("div", { class: "empty" }, "No reports found in these folders.")));
  }
  load();
};

// ----- compare -----

views.compare = (root, params) => {
  const reportPath = params.get("report");
  if (!reportPath) {
    root.append(h("h1", {}, "Compare"), h("div", { class: "panel empty" }, "Pick an encode in ", h("a", { href: "#/history" }, "History"), " or the ", h("a", { href: "#/queue" }, "Queue"), "."));
    return;
  }
  const wrap = h("div", { class: "empty" }, h("span", { class: "spin" }), " loading report…");
  root.append(wrap);
  let destroy = () => {};
  let closed = false;
  api("/api/report?path=" + encodeURIComponent(reportPath))
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
  let offset = 0, segEnd = Infinity, srcMode = "direct";
  // Extra source shift in seconds, found by auto-align or set by hand: variable-frame-rate
  // sources are retimed by the encode, so frame k of the source isn't always at k/fps.
  let nudge = 0;
  const srcTime = (t) => Math.max(0, t - offset + nudge);
  let split = 0.5, zoom = 1, origin = "50% 50%";

  const layerS = h("div", { class: "layer" }, vs);
  const layerO = h("div", { class: "layer" }, vo);
  const divider = h("div", { class: "divider" });
  const tagL = h("div", { class: "tag l" }, `Source · ${ii.codec || "?"} · ${bytes(r.source_bytes)}`);
  const tagR = h("div", { class: "tag r" }, `AV1 · ${bytes(r.output_bytes)} (−${pct(r.savings)})`);
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
  const previewBtn = h("button", { class: "small", onclick: () => makePreview(vo.currentTime) }, "Preview source from here");
  const alignEl = h("span", { class: "mono" }, "0 frames");
  function setNudge(v, how) {
    nudge = v;
    const k = Math.round(nudge * fps);
    alignEl.textContent = `${k > 0 ? "+" : ""}${k} frame${Math.abs(k) === 1 ? "" : "s"}${how ? " (" + how + ")" : ""}`;
    vs.currentTime = srcTime(vo.currentTime);
  }
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
    if (aligning || srcMode === "loading" || !vs.videoWidth || !vo.videoWidth) return;
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
          await seekWait(vs, Math.max(0, t - offset + k / fps));
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
  // grid
  for (let v = Math.ceil(lo); v <= hi; v += Math.max(1, Math.round((hi - lo) / 5))) {
    tl.append(s("line", { x1: 0, x2: 1000, y1: y(v), y2: y(v), stroke: "#2c3136", "stroke-width": 1, "vector-effect": "non-scaling-stroke" }));
  }
  tl.append(s("line", { x1: 0, x2: 1000, y1: y(target), y2: y(target), stroke: "#8b939b", "stroke-dasharray": "4 4", "stroke-width": 1, "vector-effect": "non-scaling-stroke" }));
  for (const c of segs) {
    const rect = s("rect", { x: x(c.start_secs), width: Math.max(0.5, x(c.end) - x(c.start_secs) - 0.6), y: y(c.vmaf), height: Math.max(1, 96 - y(c.vmaf)), fill: vmafColor(c.vmaf, target), opacity: 0.85 });
    rect.dataset.tip = `chunk ${c.index + 1} · ${tc(c.start_secs)}–${tc(c.end)} · CRF ${c.crf}${c.corrected ? " (corrected)" : ""} · VMAF ${num(c.vmaf)} · ${bytes(c.bytes)} · ${num(c.bytes * 8 / (c.end - c.start_secs) / 1e6, 2)} Mbps`;
    tl.append(rect);
  }
  // Verification spot checks: 5 s windows at 30% and 70% of (duration − 5 s).
  (r.verify_vmaf || []).forEach((v, k) => {
    const secs = Math.min(5, duration), st = (duration - secs) * [0.3, 0.7][k];
    const c = s("circle", { cx: x(st + secs / 2), cy: y(v), r: 4, fill: "#6aa7e8", stroke: "#111", "vector-effect": "non-scaling-stroke" });
    c.dataset.tip = `verify spot check ${tc(st)}–${tc(st + secs)} · VMAF ${num(v)}`;
    tl.append(c);
  });
  const segRect = s("rect", { x: 0, width: 0, y: 0, height: 100, fill: "#6aa7e8", opacity: 0.12 });
  tl.append(segRect);
  const head = s("line", { x1: 0, x2: 0, y1: 0, y2: 100, stroke: "#fff", "stroke-width": 1.5, "vector-effect": "non-scaling-stroke" });
  tl.append(head);
  const tlLabel = h("div", { class: "row dim" },
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
  const side = h("div", {},
    h("div", { class: "panel" },
      h("h3", {}, "Encode"),
      h("div", { class: "kv" },
        kv("Source", h("span", { class: "mono", title: r.input_path }, basename(r.input_path))),
        kv("Output", h("span", { class: "mono", title: r.output_path }, basename(r.output_path))),
        kv("Size", `${bytes(r.source_bytes)} → ${bytes(r.output_bytes)}`),
        kv("Saved", h("span", { class: "ok" }, pct(r.savings, 1))),
        kv("Mode", r.mode === "scenes" ? `per-scene CRF, ${chunks.length} chunks` : "one CRF"),
        kv("CRF", r.mode === "scenes" && chunks.length ? `${Math.min(...chunks.map((c) => c.crf))}–${Math.max(...chunks.map((c) => c.crf))} (start ${r.crf})` : r.crf),
        kv("Preset", r.preset),
        kv("Target", `VMAF ${target} (${r.vmaf_model})`),
        r.mean_vmaf != null ? kv("Mean VMAF", num(r.mean_vmaf)) : null,
        kv("Spot checks", (r.verify_vmaf || []).map((v) => num(v)).join(", ") || "–"),
        kv("Time", `search ${hms(r.search_secs)}, encode ${hms(r.encode_secs)}`),
        kv("Source video", ii.codec ? `${ii.codec} ${ii.width}×${ii.height} ${num(ii.fps, 2)} fps${ii.hdr ? " HDR" : ""}` : (ii.error || "–")))),
    r.search && r.search.length ? h("div", { class: "panel" }, h("h3", {}, "CRF search (samples)"), h("table", { class: "trials" },
      h("tr", {}, h("th", {}, "CRF"), h("th", { class: "num" }, "VMAF"), h("th", { class: "num" }, "worst"), h("th", { class: "num" }, "size")),
      r.search.map((t) => h("tr", {}, h("td", {}, t.crf), h("td", { class: "num " + (t.mean_vmaf >= target ? "ok" : "bad") }, num(t.mean_vmaf)), h("td", { class: "num dim" }, num(t.min_vmaf)), h("td", { class: "num" }, pct(t.size_ratio)))))) : null,
    (r.notes || []).length ? h("div", { class: "panel" }, h("h3", {}, "Notes"), r.notes.map((n) => h("div", { class: "dim" }, n))) : null,
    h("div", { class: "panel" }, h("h3", {}, "Playback"), h("div", {}, srcStatus), h("div", { class: "row" }, previewBtn),
      h("h3", {}, "Frame alignment"),
      h("div", { class: "row" }, h("button", { class: "small", title: "Shift the source one frame earlier", onclick: () => setNudge(nudge - 1 / fps, "manual") }, "−1"),
        alignEl, h("button", { class: "small", title: "Shift the source one frame later", onclick: () => setNudge(nudge + 1 / fps, "manual") }, "+1"),
        h("button", { class: "small", onclick: () => autoAlign() }, "Auto")),
      h("p", { class: "dim" }, "Drag on the picture to move the split. Double-click zooms in at that point (4× → back to 1×); shift-click moves the zoom centre. Keys: space play, , and . step frames.")));

  root.append(h("div", { class: "row" }, h("h1", { class: "grow" }, basename(r.input_path)), h("a", { href: "#/history" }, "← History")),
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
    const start = Math.max(0, Math.min(at, Math.max(0, duration - 2)));
    const secs = Math.min(20, Math.max(2, duration - start));
    vo.pause();
    busy(`Making a ${Math.round(secs)} s near-lossless H.264 preview of the source from ${tc(start)}…`);
    previewBtn.disabled = true;
    const prev = { offset, segEnd, srcMode, src: vs.src };
    srcMode = "loading";
    try {
      const p = await api("/api/preview", { path: r.input_path, kind: "transcode", start, secs });
      await tryLoad(vs, p.url);
      offset = p.start; segEnd = p.start + p.secs; srcMode = "preview";
      segRect.setAttribute("x", x(offset)); segRect.setAttribute("width", x(segEnd) - x(offset));
      srcStatus.replaceChildren(h("span", { class: "warn" }, "Source: preview clip "), `${tc(offset)}–${tc(segEnd)}. The browser can't decode ${ii.codec || "this source"} directly, so this clip is re-encoded near-losslessly (H.264 CRF 12)${ii.hdr ? "; HDR shown without tone mapping" : ""}. Click the timeline outside the shaded part for a new clip.`);
      tagL.textContent = `Source preview · ${ii.codec || "?"} → H.264 near-lossless`;
      seekTo(offset);
      busy(null);
      await autoAlign();
      seekTo(offset);
    } catch (e) {
      ({ offset, segEnd, srcMode } = prev);
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
      busy("Your browser can't open this container; copying the AV1 stream into MP4…");
      try {
        const p = await api("/api/preview", { path: r.output_path, kind: "remux" });
        await tryLoad(vo, p.url);
      } catch (e) {
        overlay.replaceChildren(h("div", { class: "bad" }, "Cannot play the AV1 output here: " + e.message));
        return;
      }
    }
    if (ii.error) { busy(null); overlay.hidden = false; overlay.replaceChildren(h("div", { class: "bad" }, ii.error)); return; }
    try {
      await tryLoad(vs, mediaUrl(r.input_path));
      srcStatus.replaceChildren(h("span", { class: "ok" }, "Source: original file"), " played directly.");
      previewBtn.hidden = false;
      busy(null);
      autoAlign();
    } catch (e) {
      busy(null);
      await makePreview(0);
    }
  })();

  return () => {
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
      STATE.tools = tools;
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
