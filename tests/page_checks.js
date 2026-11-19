// Runs parts of ui/app.js on a small stand-in for the browser, for what only the page decides:
// what it sends, what it remembers and what it shows. Started by tests/ui_api.rs.
"use strict";
const assert = require("assert");
const fs = require("fs");
const vm = require("vm");

class Node {}

class Text extends Node {
  constructor(text) { super(); this.text = text; this.parent = null; }
  get textContent() { return this.text; }
  remove() { if (this.parent) this.parent.children.splice(this.parent.children.indexOf(this), 1); this.parent = null; }
}

class Element extends Node {
  constructor(tag) {
    super();
    Object.assign(this, { tag, children: [], parent: null, attrs: {}, style: {}, dataset: {}, listeners: {}, value: "", className: "", hidden: false, disabled: false, checked: false, paused: true });
    const element = this;
    this.classList = {
      add: (...names) => { element.className = [...new Set([...element.names(), ...names])].join(" "); },
      remove: (...names) => { element.className = element.names().filter((n) => !names.includes(n)).join(" "); },
      contains: (name) => element.names().includes(name),
      toggle: (name, on = !element.classList.contains(name)) => { element.classList[on ? "add" : "remove"](name); return on; },
    };
  }
  names() { return this.className.split(/\s+/).filter(Boolean); }
  get tagName() { return this.tag.toUpperCase(); }
  setAttribute(k, v) { this.attrs[k] = String(v); if (k === "value") this.value = String(v); if (k === "checked" || k === "disabled" || k === "hidden") this[k] = true; }
  getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
  removeAttribute(k) { delete this.attrs[k]; }
  addEventListener(kind, fn) { (this.listeners[kind] = this.listeners[kind] || []).push(fn); }
  removeEventListener(kind, fn) { this.listeners[kind] = (this.listeners[kind] || []).filter((f) => f !== fn); }
  dispatch(kind) { for (const fn of [...(this.listeners[kind] || [])]) fn({ target: this, preventDefault() {}, stopPropagation() {} }); }
  click() { if (this.onclick) return this.onclick({ target: this }); for (const fn of this.listeners.click || []) return fn({ target: this, preventDefault() {}, stopPropagation() {} }); }
  focus() { this.ownerDocument.activeElement = this; }
  blur() {}
  pause() { if (!this.paused) { this.paused = true; this.dispatch("pause"); } }
  play() { if (this.paused) { this.paused = false; this.dispatch("play"); } return Promise.resolve(); }
  load() {}
  scrollIntoView() {}
  getBoundingClientRect() { return { left: 0, top: 0, right: 0, bottom: 0, width: 0, height: 0 }; }
  getContext() { return null; }
  adopt(kids) {
    for (const kid of kids) {
      const node = kid instanceof Node ? kid : new Text(String(kid));
      node.remove();
      node.parent = this;
      this.children.push(node);
    }
  }
  append(...kids) { this.adopt(kids); }
  appendChild(kid) { this.adopt([kid]); return kid; }
  prepend(...kids) { const old = this.children; this.children = []; this.adopt(kids); this.children.push(...old); }
  replaceChildren(...kids) { for (const kid of [...this.children]) kid.remove(); this.adopt(kids); }
  remove() { if (this.parent) this.parent.children.splice(this.parent.children.indexOf(this), 1); this.parent = null; }
  replaceWith(other) { const list = this.parent.children, at = list.indexOf(this); other.remove(); other.parent = this.parent; list[at] = other; this.parent = null; }
  contains(other) { for (let n = other; n; n = n.parent) if (n === this) return true; return false; }
  get firstChild() { return this.children[0] || null; }
  get firstElementChild() { return this.children.find((k) => k instanceof Element) || null; }
  get isConnected() { let n = this; while (n.parent) n = n.parent; return n === this.ownerDocument.body; }
  get textContent() { return this.children.map((k) => k.textContent).join(""); }
  set textContent(text) { this.replaceChildren(); if (text !== "" && text != null) this.adopt([String(text)]); }
  querySelector() { return null; }
  // Enough for the lists the page reads back: one class or one tag name.
  querySelectorAll(selector) {
    if (!/^\.?[a-z-]+$/.test(selector)) return [];
    return this.all().filter((e) => (selector[0] === "." ? e.classList.contains(selector.slice(1)) : e.tag === selector));
  }
  all() { return this.children.filter((k) => k instanceof Element).flatMap((k) => [k, ...k.all()]); }
  // The button or other element with exactly this text.
  named(tag, text) {
    const found = this.all().find((e) => e.tag === tag && e.textContent === text);
    assert(found, `no ${tag} "${text}" in: ${this.textContent.slice(0, 300)}`);
    return found;
  }
  // The input a form row labelled `label` holds.
  field(label) {
    const at = this.all().find((e) => e.tag === "label" && e.textContent.startsWith(label));
    assert(at, `no field "${label}"`);
    const own = at.all().find((e) => e.tag === "input");
    if (own) return own;
    const row = at.parent.children.indexOf(at);
    return at.parent.children[row + 1].all().find((e) => e.tag === "input");
  }
}

// The page in a world of its own. `answer(path, body)` stands for the server: what it returns
// is the reply, and a { refuse: text } is an error.
// `stored` is what the server remembers for the page, sent with the state as ui.json holds it:
// the folder lists in their own settings, the rest in `page`. What the page posts to
// /api/prefs is kept there as the server would keep it, so a page opened again with
// `page.stored` reads it back.
const FOLDER_FIELDS = { scanRoots: "scan_paths", dupRoots: "dup_paths", dupPrefer: "dup_prefer", benchRoots: "bench_paths" };
function stateWith(stored, extra = {}) {
  const settings = { ...state.settings, page: {} };
  for (const [k, v] of Object.entries(stored)) {
    if (k in FOLDER_FIELDS) settings[FOLDER_FIELDS[k]] = v;
    else settings.page[k] = v;
  }
  return { ...state, ...extra, settings };
}
function openPage(answer, stored = {}, extra = {}) {
  const page = { posts: [], toasts: [], stored: JSON.parse(JSON.stringify(stored)) };
  const named = {};
  const document = {
    activeElement: null,
    createElement: (tag) => { const e = new Element(tag); e.ownerDocument = document; return e; },
    createElementNS: (_, tag) => document.createElement(tag),
    createTextNode: (text) => new Text(text),
    getElementById: (id) => (named[id] = named[id] || document.createElement("div")),
    querySelector: () => null,
    querySelectorAll: () => [],
    addEventListener() {},
    removeEventListener() {},
  };
  document.body = document.createElement("body");
  const respond = async (path, options = {}) => {
    const body = options.body ? JSON.parse(options.body) : undefined;
    if (body) page.posts.push({ path, body });
    if (page.signedOut) return { ok: false, status: 401, statusText: "Unauthorized", headers: { get: () => "text/html" }, text: async () => "<p>Not signed in</p>" };
    if (path === "/api/prefs" && body) {
      if (page.prefsFail) return { ok: false, status: 500, statusText: "", headers: { get: () => "application/json" }, text: async () => JSON.stringify({ error: "cannot be saved" }) };
      if (body.value === null) delete page.stored[body.key];
      else page.stored[body.key] = JSON.parse(JSON.stringify(body.value));
      return { ok: true, status: 200, statusText: "", headers: { get: () => "application/json" }, text: async () => "{}" };
    }
    const reply = (path === "/api/state" ? stateWith(page.stored, extra) : await answer(path, body)) || {};
    return { ok: !reply.refuse, status: reply.refuse ? 400 : 200, statusText: "", headers: { get: () => "application/json" }, text: async () => JSON.stringify(reply.refuse ? { error: reply.refuse } : reply) };
  };
  const world = {
    Node, document, console, URLSearchParams, setTimeout, clearTimeout, setInterval: () => 0, clearInterval() {}, requestAnimationFrame() {}, cancelAnimationFrame() {}, performance,
    fetch: respond,
    location: { hash: "#/none", href: "", reload() {} },
    // The page keeps nothing in the browser: what it remembers goes to the server.
    localStorage: { getItem: () => { throw new Error("localStorage read"); }, setItem: () => { throw new Error("localStorage written"); }, removeItem: () => { throw new Error("localStorage written"); } },
    EventSource: class { static CLOSED = 2; constructor() { page.stream = this; } addEventListener() {} close() {} },
    ResizeObserver: class { observe() {} disconnect() {} },
    matchMedia: () => ({ matches: false, addEventListener() {} }),
    navigator: {},
    addEventListener() {},
    removeEventListener() {},
  };
  world.window = world;
  page.context = vm.createContext(world);
  vm.runInContext(fs.readFileSync(process.argv[2], "utf8"), page.context, { filename: "app.js" });
  page.run = (code) => vm.runInContext(code, page.context);
  world.__told = (message, bad) => page.toasts.push({ message, bad });
  page.run("var realToast = toast; toast = __told");
  page.document = document;
  page.remembered = (k) => page.stored[k];
  page.show = (view) => {
    const root = document.createElement("div");
    document.body.append(root);
    world.__root = root;
    page.run(`views.${view}(__root)`);
    return root;
  };
  return page;
}

const settled = async () => { for (let i = 0; i < 20; i++) await new Promise((done) => setImmediate(done)); };
const state = { settings: { scan_paths: ["/videos"], dup_paths: ["/videos"], bench_paths: ["/videos"] }, default_out_dir: "/out", default_svt_params: "tune=0", tuning: [], tools: {} };
const posted = (page, path) => page.posts.filter((p) => p.path === path);
const lastToast = (page) => (page.toasts[page.toasts.length - 1] || {}).message || "";
const benchReply = { defaults: { baseline: { preset: 5, params: "tune=0" }, presets: [4, 6], param_sets: ["tune=0"] }, run: null, report: null, tuning: [] };

// Each form: how to open it, the field to try, what its button posts to, and what it remembers.
const forms = [
  { name: "scan", view: "scan", button: "Scan", path: "/api/scans", field: "Target VMAF", key: "target_vmaf", good: "90", sent: 90, saved: ["scanVmaf", 90], reply: { id: 1 } },
  { name: "duplicates", view: "dups", button: "Find duplicates", path: "/api/dups", field: "Tolerance", key: "tolerance", good: "7", sent: 7, saved: ["dupTolerance", 7], reply: { id: 1 }, stored: { dupRoots: ["/videos"] } },
  { name: "benchmark", view: "bench", button: "Start benchmark", path: "/api/bench", field: "Videos", key: "max_videos", good: "4", sent: 4, saved: ["benchForm", 4, "videos"], reply: {}, stored: { benchRoots: ["/videos"] } },
];

const checks = {};

for (const form of forms) {
  const open = async (refusal) => {
    const page = openPage((path, body) => {
      if (body) return refusal ? { refuse: refusal } : form.reply;
      return path === "/api/bench" ? benchReply : { status: "running", phase: "probing", runs: [], jobs: [], queue: {} };
    }, { scanRoots: ["/videos"], ...form.stored });
    await settled();
    const root = page.show(form.view);
    await settled();
    return { page, root, input: root.field(form.field), button: root.named("button", form.button) };
  };
  const remembered = (page) => { const v = page.remembered(form.saved[0]); return form.saved[2] && v ? v[form.saved[2]] : v; };

  checks[`${form.name}: an empty or odd field is named and nothing is sent`] = async () => {
    for (const text of ["", "  ", "abc"]) {
      const { page, input, button } = await open();
      input.value = text;
      await button.click();
      await settled();
      assert.deepStrictEqual(posted(page, form.path), [], `sent for "${text}"`);
      assert(lastToast(page).startsWith(form.field), lastToast(page));
      assert(input.classList.contains("invalid"), "field not marked");
      assert.strictEqual(remembered(page), undefined);
      input.dispatch("input");
      assert(!input.classList.contains("invalid"), "mark stays after typing");
    }
  };
  checks[`${form.name}: a value the server refuses is not remembered`] = async () => {
    const { page, input, button } = await open("out of range");
    input.value = form.good;
    await button.click();
    await settled();
    assert.strictEqual(posted(page, form.path).length, 1);
    assert.strictEqual(lastToast(page), "out of range");
    assert.strictEqual(remembered(page), undefined);
  };
  checks[`${form.name}: an accepted value is sent and remembered`] = async () => {
    const { page, input, button } = await open();
    input.value = form.good;
    await button.click();
    await settled();
    assert.strictEqual(posted(page, form.path)[0].body[form.key], form.sent);
    assert.strictEqual(remembered(page), form.saved[1]);
  };
}

for (const watch of [false, true]) {
  const name = watch ? "watch dialog" : "encode dialog";
  const open = async () => {
    const page = openPage((path, body) => (body ? { files: 1 } : {}));
    await settled();
    page.result = page.run(`encodeDialog(["/videos/a.mp4"], [], ${watch})`);
    await settled();
    const box = page.document.body;
    return { page, box, go: box.named("button", watch ? "Watch folder" : "Add to queue") };
  };
  const path = watch ? "/api/watch" : "/api/jobs";

  checks[`${name}: an empty field keeps it open and sends nothing`] = async () => {
    for (const label of ["Target VMAF", "Preset", "Skip if saving under"]) {
      const { page, box, go } = await open();
      const input = box.field(label);
      input.value = "";
      await go.click();
      await settled();
      assert.deepStrictEqual(posted(page, path), [], label);
      assert(lastToast(page).startsWith(label), lastToast(page));
      assert(input.classList.contains("invalid"), label);
      assert(go.isConnected, "dialog closed");
    }
  };
  checks[`${name}: each field is named by its label`] = async () => {
    const { box } = await open();
    const labels = box.all().filter((e) => e.tag === "label" && e.classList.contains("k"));
    assert(labels.length > 5, `${labels.length} labels`);
    const ids = new Map(box.all().filter((e) => e.id).map((e) => [e.id, e]));
    for (const label of ["Target VMAF", "Preset", "Skip if saving under"]) {
      const at = labels.find((e) => e.textContent.startsWith(label));
      assert(at && ids.get(at.getAttribute("for")) === box.field(label), label);
    }
    for (const at of labels) {
      const control = at.parent.children[at.parent.children.indexOf(at) + 1];
      const field = control.all().find((e) => ["input", "select", "textarea"].includes(e.tag));
      if (field) assert.strictEqual(ids.get(at.getAttribute("for")), field, at.textContent);
    }
  };
  checks[`${name}: scenes at once counts only while it is in use`] = async () => {
    const { page, box, go } = await open();
    const scenes = box.field("Per-scene CRF");
    assert(scenes.disabled, "greyed out while scenes are off");
    scenes.value = "";
    await go.click();
    await settled();
    assert.strictEqual(posted(page, path)[0].body.options.workers, 1);

    const second = await open();
    const select = second.box.all().find((e) => e.tag === "select" && e.all().some((o) => o.textContent === "Always"));
    select.value = "always";
    select.dispatch("change");
    const workers = second.box.field("Per-scene CRF");
    assert(!workers.disabled);
    workers.value = "";
    await second.go.click();
    await settled();
    assert.deepStrictEqual(posted(second.page, path), []);
    assert(lastToast(second.page).startsWith("Scenes at once"), lastToast(second.page));
  };
}

// What the server can hand the page that is empty, very long or holds HTML characters.
const odd = '<img src=x onerror="alert(1)">&amp;.mkv';
const holdReply = { hold: null, paused: false, schedule: { on: false, from: "22:00", to: "07:00" } };
const openQueue = async (jobsReply) => {
  const page = openPage((path, body) => {
    if (body) return {};
    if (path === "/api/jobs") return { jobs: [], unrestored: [], queue: holdReply, ...jobsReply };
    return path === "/api/watch" ? { watches: [] } : {};
  });
  await settled();
  const root = page.show("queue");
  await settled();
  return root;
};
// No element comes of text, whatever it holds.
const noElementFromText = (root) => assert(!root.all().some((e) => e.tag === "img"), "text became an element");

checks["queue: saved jobs that could not come back are listed in a bounded way"] = async () => {
  const inputs = Array.from({ length: 300 }, (_, i) => `/videos/${i}-${odd}`);
  const unrestored = Array.from({ length: 500 }, () => ({ inputs, out_dir: `/out/${odd}`, error: `gone: ${odd}` }));
  const root = await openQueue({ unrestored });
  const rows = root.all().filter((e) => e.tag === "div" && e.classList.contains("long-text"));
  assert.strictEqual(rows.length, 20);
  assert(rows[0].textContent.includes(odd) && rows[0].textContent.includes("and 295 more"), rows[0].textContent.slice(0, 200));
  assert(!rows[0].textContent.includes("/videos/5-"), "more inputs than the cut allows");
  assert(root.textContent.includes("and 480 more jobs"), root.textContent.slice(0, 200));
  assert(root.textContent.includes("500 saved jobs"));
  noElementFromText(root);
};
checks["queue: the heading of saved jobs says what each kind waits for, as its rows do"] = async () => {
  const away = { inputs: ["/v/a.mp4"], out_dir: "/out", error: "they were not there when the rest of their job ran; they are queued when they are back" };
  const stopped = { inputs: ["/v/b.mp4"], out_dir: "/out", only_by_hand: true, error: "they were left out of a job you cancelled or removed; they start only if you choose Try again" };
  const heading = (root) => root.all().find((e) => e.tag === "strong" && e.textContent.includes("not in the queue")).textContent;
  const buttons = (root) => root.all().filter((e) => e.tag === "button" && e.textContent === "Try again").length;
  let root = await openQueue({ unrestored: [stopped] });
  assert.strictEqual(heading(root), "1 saved job not in the queue, left out of a job you stopped: it starts only if you choose Try again");
  assert.strictEqual(buttons(root), 1, "only the row's own button");
  root = await openQueue({ unrestored: [away, away] });
  assert.strictEqual(heading(root), "2 saved jobs not in the queue, kept for when their files are back");
  assert.strictEqual(buttons(root), 3);
  root = await openQueue({ unrestored: [away, stopped] });
  assert.strictEqual(heading(root), "2 saved jobs not in the queue: 1 kept for when their files are back, 1 left out of a job you stopped, which start only if you choose Try again on their row");
};
checks["queue: a saved job without a list of videos or an output folder still reads"] = async () => {
  const root = await openQueue({ unrestored: [{ out_dir: null, error: "cannot be read" }] });
  assert(root.textContent.includes("(its list of videos cannot be read) → (no output folder): cannot be read"), root.textContent.slice(0, 300));
  assert(!root.textContent.includes("undefined"));
  const none = await openQueue({ unrestored: [] });
  assert(!none.textContent.includes("saved job"));
};
checks["queue: jobs still coming back are counted, and none means no line"] = async () => {
  for (const [restoring, text] of [[1, "1 job to go"], [40, "40 jobs to go"]]) assert((await openQueue({ restoring })).textContent.includes(text));
  for (const restoring of [0, undefined]) assert(!(await openQueue({ restoring })).textContent.includes("Bringing back"));
};
checks["queue: the note of a job is shown as text"] = async () => {
  const job = { id: 1, status: "queued", total: 0, out_dir: "/out", options: { target_vmaf: 93, preset: 5, container: "mp4" }, counts: {}, files: [], note: `Left out because they are gone: ${odd} and 295 more.` };
  const root = await openQueue({ jobs: [job] });
  assert(root.textContent.includes(job.note), root.textContent.slice(0, 300));
  noElementFromText(root);
};
checks["banner: a notice is text, and goes when dismissed"] = async () => {
  const page = openPage(() => ({}));
  await settled();
  page.context.__notices = [`the saved queue could not be read; it was kept as /data/${odd}`];
  page.run("STATE = { notices: __notices, tools: {} }; renderBanner()");
  const banner = page.document.getElementById("banner");
  assert(!banner.hidden && banner.textContent.includes(odd), banner.textContent);
  noElementFromText(banner);
  banner.named("button", "Dismiss").click();
  assert(banner.hidden);
  page.run("STATE = { notices: [], tools: {} }; renderBanner()");
  assert(banner.hidden && banner.textContent === "");
};
checks["scan: folders that could not be read are counted and the first few named"] = async () => {
  const paths = [`/videos/${odd}`, "/videos/b"];
  const result = { id: 1, status: "done", rows: [], totals: {}, paths: ["/videos"], unreadable_folders: 300, unreadable_paths: paths };
  const page = openPage((path, body) => (body ? { id: 1 } : path === "/api/scans/1" ? result : {}), { scanRoots: ["/videos"] });
  await settled();
  const root = page.show("scan");
  await settled();
  await root.named("button", "Scan").click();
  await settled();
  assert(root.textContent.includes("300 folders could not be read"), root.textContent.slice(0, 300));
  assert(root.textContent.includes(paths[0]) && root.textContent.includes("and 298 more"), root.textContent.slice(0, 300));
  noElementFromText(root);
};

checks["duplicates: the trash dialog says which marks are not on screen and lists every path apart"] = async () => {
  const page = openPage(() => ({}));
  await settled();
  const file = (path) => ({ path, size: 1000, codec: "h264", width: 1280, height: 720, picture_width: 1280, picture_height: 720, duration_s: 60, video_mbps: 1, sound_tracks: 1, subtitle_tracks: 0 });
  const forged = "/v/a\n/home/me/keep.mp4";
  page.context.__run = { id: 1, trashed: [], report: {
    stats: { same_groups: 2, partial_groups: 0, spare_files: 2, spare_bytes: 2000 }, unreadable: [],
    files: [file("/v/k1.mp4"), file(forged), file("/v/k2.mp4"), file("/v/s2.mp4")],
    groups: [{ kind: "same", score: 1, members: [0, 1] }, { kind: "same", score: 1, members: [2, 3] }],
  } };
  page.context.__out = page.document.createElement("div");
  page.document.body.append(page.context.__out);
  page.run(`window.scrollY = 0; dupMarks.add(__run.report.files[1].path); dupMarks.add("/v/s2.mp4"); renderDups(__out, __run, () => {}, { limit: 1, keepScroll: false })`);
  const out = page.context.__out;
  assert(out.textContent.includes("1 not on screen"), out.textContent.slice(0, 300));
  assert(out.textContent.includes("1 file with line breaks or control characters in its name is not in this list"), out.textContent.slice(0, 500));
  out.named("button", "Move to trash…").click();
  await settled();
  const dialog = page.document.body.all().find((e) => e.classList.contains("modal-bg"));
  assert(dialog.textContent.includes("1 marked file is not on screen"), dialog.textContent);
  const lines = dialog.all().filter((e) => e.classList.contains("trash-list")).flatMap((e) => e.children.map((k) => k.textContent));
  assert.deepStrictEqual(lines, ["/v/a\u240a/home/me/keep.mp4", "/v/s2.mp4"]);
};

checks["sections: with the length unknown an end can stay open, and a typed one is kept"] = async () => {
  const page = openPage((path) => (path === "/api/source" ? { error: "cannot read it" } : {}));
  await settled();
  page.context.__cuts = page.run(`sectionsEditor("/videos/a.mp4")`);
  await settled();
  const box = page.context.__cuts.el;
  page.document.body.append(box);
  assert(box.textContent.includes("You can still type the times"), box.textContent.slice(0, 300));
  for (const [start, end] of [["1:30", ""], ["0:10", "END"], ["0:01", "0:02"]]) {
    box.named("button", "+ Section").click();
    const inputs = box.querySelectorAll(".cut-row").pop().querySelectorAll("input");
    inputs[0].value = start;
    inputs[1].value = end;
  }
  assert.deepStrictEqual(JSON.parse(page.run("JSON.stringify(__cuts.get())")), [[1, 2], [10, null], [90, null]]);
  const last = box.querySelectorAll(".cut-row").pop().querySelectorAll("input");
  last[1].value = "0:01";
  assert.throws(() => page.run("__cuts.get()"), /not a valid range/);
};

const hello = (page, server) => page.stream.onmessage({ data: JSON.stringify({ kind: "hello", server }) });

checks["restart: what the page holds from an earlier run of the server is dropped"] = async () => {
  const page = openPage(() => ({ runs: [] }));
  await settled();
  hello(page, "first");
  await settled();
  page.run(`lastScanId = 4; lastDupId = 7; dupMarks.add("/v/a.mp4"); watchBusy.add("1:look")`);
  page.context.__closed = page.run("confirmDialog({ title: 'Sure?', body: [], action: 'Yes' })");
  hello(page, "first");
  assert.strictEqual(page.run("lastDupId"), 7, "the same run keeps them");
  assert.strictEqual(page.run("openDialogs.size"), 1);
  hello(page, "second");
  await settled();
  assert.deepStrictEqual(JSON.parse(page.run("JSON.stringify([lastScanId, lastDupId, dupMarks.size, watchBusy.size, openDialogs.size])")), [null, null, 0, 0, 0]);
  assert.strictEqual(await page.context.__closed, false);
  assert(lastToast(page).includes("started again"), lastToast(page));
};
checks["reconnecting: views are told, and the state is read again"] = async () => {
  const page = openPage(() => ({}));
  await settled();
  hello(page, "one");
  page.run(`STATE = { stale: true, tools: {} }; var told = []; onEvent((m) => told.push(m.kind))`);
  page.stream.onerror();
  hello(page, "one");
  await settled();
  assert.deepStrictEqual(Array.from(page.run("told")), ["reopened"]);
  assert.strictEqual(page.run("STATE.stale"), undefined);
};
checks["sign-in lost: the stream refused with 401 says so and is not tried again"] = async () => {
  const page = openPage(() => ({}));
  await settled();
  const stream = page.stream;
  page.signedOut = true;
  stream.readyState = 2;
  stream.onerror();
  await settled();
  const banner = page.document.getElementById("banner");
  assert(!banner.hidden && banner.textContent.includes("link av1opt printed"), banner.textContent);
  assert.strictEqual(page.document.getElementById("conn").title, "Not signed in");
  assert.strictEqual(page.stream, stream);
};
checks["routes: names every object inherits are no views"] = async () => {
  const page = openPage(() => ({}));
  await settled();
  for (const name of ["constructor", "valueOf", "toString", "__proto__"]) page.run(`location.hash = "#/${name}"; route(); route()`);
};
checks["remembering: a value that cannot be saved stops nothing and is still used here"] = async () => {
  const page = openPage(() => ({}));
  await settled();
  page.prefsFail = true;
  page.run(`prefs.set("benchTab", "one")`);
  await settled();
  assert.strictEqual(page.run(`prefs.get("benchTab", "all")`), "one");
  assert.deepStrictEqual(page.toasts, []);
};
checks["remembering: what a form remembers is there when the page is opened again"] = async () => {
  const answer = (path, body) => (body ? { id: 1 } : { id: 1, status: "running" });
  const first = openPage(answer);
  await settled();
  const root = first.show("scan");
  await settled();
  root.field("Target VMAF").value = "91";
  await root.named("button", "Scan").click();
  await settled();
  // Each value is sent on its own, so that another tab's values are not written over.
  assert(posted(first, "/api/prefs").every((p) => Object.keys(p.body).sort().join() === "key,value"));
  assert.strictEqual(first.remembered("scanVmaf"), 91);
  const again = openPage(answer, first.stored);
  await settled();
  assert.strictEqual(again.show("scan").field("Target VMAF").value, "91");
};
checks["remembering: a remembered value of the wrong kind gives way to the usual one"] = async () => {
  const page = openPage(() => ({ id: 1, status: "running" }), { scanVmaf: "<b>", scanFilter: "everything", encodeOptions: { preset: "x", container: "avi", meta_tags: [["a", 1]] } });
  await settled();
  assert.strictEqual(page.show("scan").field("Target VMAF").value, "93");
  assert.strictEqual(page.run(`prefs.get("scanVmaf", 93)`), 93);
  const o = page.run("defaultOptions()");
  assert.strictEqual(o.preset, 5);
  assert.strictEqual(o.container, "mp4");
  assert.strictEqual(JSON.stringify(o.meta_tags || []), "[]");
};
checks["banner: a dismissed notice stays dismissed when the page is opened again"] = async () => {
  const notices = { notices: ["ui.json could not be read; it was kept as ui.json.damaged"] };
  const first = openPage(() => ({}), {}, notices);
  await settled();
  const banner = first.document.getElementById("banner");
  assert(!banner.hidden && banner.textContent.includes("ui.json"), banner.textContent);
  banner.named("button", "Dismiss").click();
  await settled();
  assert(banner.hidden);
  const again = openPage(() => ({}), first.stored, notices);
  await settled();
  assert(again.document.getElementById("banner").hidden, again.document.getElementById("banner").textContent);
  // Another notice, or the same file damaged again under a new name, is shown.
  const other = openPage(() => ({}), first.stored, { notices: ["queue.json could not be read"] });
  await settled();
  assert(!other.document.getElementById("banner").hidden);
};
checks["no view shows the word null where a part is left out"] = async () => {
  const page = openPage((path) => (path === "/api/history" ? { reports: [], dirs: [], history_encodes: 3 } : {}));
  await settled();
  const shown = [];
  for (const view of ["history", "settings"]) {
    const root = page.show(view);
    await settled();
    if (/null/.test(root.textContent)) shown.push(`${view}: ${root.textContent}`);
  }
  assert.deepStrictEqual(shown, []);
};
checks["history: the count of encodes behind scan estimates is the one just read"] = async () => {
  const page = openPage((path) => (path === "/api/history" ? { reports: [], dirs: [], history_encodes: 12 } : {}), {}, { history_encodes: 4 });
  await settled();
  const root = page.show("history");
  await settled();
  assert(root.textContent.includes("12encodes calibrating"), root.textContent.slice(0, 400));
  assert(page.show("settings").textContent.includes("Encodes behind scan estimates12"));
};
checks["scan: a share of savings out of range is refused in the page's own words"] = async () => {
  const page = openPage((path, body) => (body ? { id: 1 } : { id: 1, status: "running" }));
  await settled();
  const root = page.show("scan");
  await settled();
  for (const text of ["120", "96", "-1"]) {
    root.field("Min savings %").value = text;
    await root.named("button", "Scan").click();
    await settled();
    assert.deepStrictEqual(posted(page, "/api/scans"), [], text);
    assert.strictEqual(lastToast(page), "Min savings %: enter a number from 0 to 95");
  }
  root.field("Min savings %").value = "95";
  await root.named("button", "Scan").click();
  await settled();
  assert.strictEqual(posted(page, "/api/scans")[0].body.min_savings, 0.95);
};
checks["benchmark: counts and lengths out of range are refused, and the plan says why"] = async () => {
  const page = openPage((path, body) => (body ? {} : path === "/api/bench" ? benchReply : {}), { benchRoots: ["/videos"] });
  await settled();
  const root = page.show("bench");
  await settled();
  const plan = () => root.all().find((e) => e.tag === "p" && /^(Videos|Seconds|Clips|No preset|\d+ settings?:)/.test(e.textContent)).textContent;
  // The clip length sits in the row of the clip count, after it.
  const clipRow = root.field("Clips per video").parent.all().filter((e) => e.tag === "input");
  const inputs = { Videos: root.field("Videos"), "Clips per video": clipRow[0], "Seconds per clip": clipRow[1] };
  for (const [field, text, said] of [["Videos", "0", "Videos: enter a whole number from 1 to 50"], ["Seconds per clip", "-3", "Seconds per clip: enter a number from 2 to 30"], ["Clips per video", "2.5", "Clips per video: enter a whole number from 1 to 8"]]) {
    const input = inputs[field], was = input.value;
    input.value = text;
    input.dispatch("input");
    assert(plan().startsWith(said), plan());
    await root.named("button", "Start benchmark").click();
    await settled();
    assert.deepStrictEqual(posted(page, "/api/bench"), [], `${field} ${text}`);
    assert.strictEqual(lastToast(page), said);
    input.value = was;
    input.dispatch("input");
  }
  assert(/setting/.test(plan()), plan());
};
checks["sections: removing all of a video, or all but a moment, is refused before it is queued"] = async () => {
  const page = openPage((path) => (path === "/api/source" ? { duration_s: 10, fps: 25, url: "/media/x" } : {}));
  await settled();
  page.run(`var cuts = sectionsEditor("/v/a.mp4")`);
  await settled();
  const el = page.run("cuts.el");
  const add = async (a, b) => {
    el.named("button", "+ Section").click();
    const row = el.querySelectorAll(".cut-row").pop();
    const [start, end] = row.querySelectorAll("input");
    start.value = a; end.value = b;
    start.dispatch("input");
    return row;
  };
  const refusal = () => { try { page.run("cuts.get()"); return ""; } catch (e) { return e.message; } };
  const row = await add("0", "end");
  assert.strictEqual(refusal(), "These sections remove the whole video: nothing would be left to encode");
  row.querySelectorAll("input")[0].value = "0:09.5";
  row.querySelectorAll("input")[0].dispatch("input");
  assert.strictEqual(refusal(), "");
  row.querySelectorAll("input")[0].value = "0:00.5";
  row.querySelectorAll("input")[0].dispatch("input");
  assert(refusal().endsWith("at least 1 s has to stay"), refusal());
  assert(el.textContent.includes("At least 1 s has to stay"), el.textContent);
  row.querySelectorAll("input")[0].value = "0:01";
  row.querySelectorAll("input")[0].dispatch("input");
  assert.strictEqual(refusal(), "");
  page.run("cuts.stop()");
};

checks["queue: forgetting saved jobs asks first"] = async () => {
  const page = openPage((path, body) => {
    if (body) return { restored: 0, unrestored: [] };
    if (path === "/api/jobs") return { jobs: [], unrestored: [{ inputs: ["/v/a.mp4"], out_dir: "/out", error: "gone" }], queue: holdReply };
    return path === "/api/watch" ? { watches: [] } : {};
  });
  await settled();
  const root = page.show("queue");
  await settled();
  const answer = async (button) => {
    root.named("button", "Forget").click();
    await settled();
    page.document.body.all().filter((e) => e.classList.contains("modal-bg")).pop().named("button", button).click();
    await settled();
  };
  await answer("Cancel");
  assert.deepStrictEqual(posted(page, "/api/jobs/unrestored/forget"), []);
  await answer("Forget");
  assert.strictEqual(posted(page, "/api/jobs/unrestored/forget").length, 1);
};
checks["scan: a remembered preset of 0 is the one asked for"] = async () => {
  const page = openPage((path, body) => (body ? { id: 1 } : { id: 1, status: "running" }), { scanRoots: ["/videos"], encodeOptions: { preset: 0 } });
  await settled();
  const root = page.show("scan");
  await settled();
  await root.named("button", "Scan").click();
  await settled();
  assert.strictEqual(posted(page, "/api/scans")[0].body.preset, 0);
};
checks["remembered values out of their field's range are not used: Scan starts with the defaults"] = async () => {
  const stored = { scanRoots: ["/videos"], dupRoots: ["/videos"], encodeOptions: { preset: 99, target_vmaf: 1e308, min_savings: -500, workers: 0, container: "avi", scenes: "sometimes" },
    scanVmaf: 1e308, scanMinSavings: -500, dupTolerance: 99999, scanFilter: "everything", benchForm: { vmaf: -1, videos: 1e9, samples: 0, secs: 1000, presets: [99, 4] } };
  const page = openPage((path, body) => (body ? { id: 1 } : path === "/api/bench" ? { defaults: { presets: [5], param_sets: ["tune=0"], baseline: { preset: 5, params: "tune=0" } } } : { id: 1, status: "running" }), stored);
  await settled();
  const scan = page.show("scan");
  await settled();
  assert.strictEqual(scan.field("Min savings").value, "15");
  assert.strictEqual(scan.field("Target VMAF").value, "93");
  await scan.named("button", "Scan").click();
  await settled();
  const body = (posted(page, "/api/scans")[0] || {}).body;
  assert(body, `no scan started: ${lastToast(page)}`);
  assert.deepStrictEqual([body.preset, body.target_vmaf, body.min_savings], [5, 93, 0.15]);
  assert.strictEqual(page.show("dups").field("Tolerance").value, "10");
  page.run("var __o = defaultOptions()");
  const o = page.context.__o;
  assert.deepStrictEqual([o.preset, o.target_vmaf, o.min_savings, o.workers, o.container, o.scenes], [5, 93, 0.15, 1, "mp4", "off"]);
  const bench = page.show("bench");
  await settled();
  assert.deepStrictEqual(["Target VMAF", "Videos", "Clips per video"].map((l) => bench.field(l).value), ["93", "6", "2"]);
  const ticked = bench.all().filter((e) => e.tag === "input" && e.attrs.type === "checkbox" && e.checked);
  assert.strictEqual(ticked.length, 1, "the remembered presets that have a box are not the ones ticked");
};
checks["form numbers are remembered when they change to one their field takes, and not otherwise"] = async () => {
  const page = openPage((path) => (path === "/api/bench" ? { defaults: { presets: [5], param_sets: ["tune=0"], baseline: { preset: 5, params: "tune=0" } } } : {}), { scanRoots: ["/videos"] });
  await settled();
  const change = (input, value) => { input.value = value; input.dispatch("change"); };
  const scan = page.show("scan");
  await settled();
  change(scan.field("Min savings"), "20");
  change(scan.field("Target VMAF"), "95.5");
  await settled();
  assert.deepStrictEqual([page.remembered("scanMinSavings"), page.remembered("scanVmaf")], [20, 95.5]);
  change(scan.field("Min savings"), "200");
  change(scan.field("Target VMAF"), "");
  await settled();
  assert.deepStrictEqual([page.remembered("scanMinSavings"), page.remembered("scanVmaf")], [20, 95.5]);
  const dups = page.show("dups");
  change(dups.field("Tolerance"), "4");
  await settled();
  change(dups.field("Tolerance"), "4.5");
  await settled();
  assert.strictEqual(page.remembered("dupTolerance"), 4);
  const bench = page.show("bench");
  await settled();
  change(bench.field("Target VMAF"), "90");
  change(bench.field("Videos"), "0");
  await settled();
  assert.strictEqual(page.remembered("benchForm").vmaf, 90);
  // The 6 the field held when the target changed stays: 0 is no number of videos.
  assert.strictEqual(page.remembered("benchForm").videos, 6);
  change(bench.field("Videos"), "3");
  const box = bench.all().filter((e) => e.tag === "input" && e.attrs.type === "checkbox")[0];
  box.checked = true;
  box.dispatch("change");
  await settled();
  const kept = page.remembered("benchForm");
  assert.deepStrictEqual([kept.vmaf, kept.videos, kept.presets, kept.params], [90, 3, [2, 5], "tune=0"]);
};
checks["scan table: dashes sort last, names as people count, and select all stops at what is drawn"] = async () => {
  const page = openPage(() => ({}), { scanFilter: "all" });
  await settled();
  const row = (rel, verdict, likely) => ({ path: "/v/" + rel, rel, verdict, verdict_label: verdict, size: 1000, estimate: { likely, low: likely, high: likely }, est_secs: 10, duration_s: 60, video_mbps: 1 });
  const table = (rows) => {
    page.context.__out = page.document.createElement("div");
    page.context.__scan = { id: 1, preset: 5, rows, paths: [], totals: {} };
    page.run("renderScan(__out, __scan)");
    return page.context.__out;
  };
  const out = table([row("a10", "Candidate", 900), row("B", "Skip", 0), row("a9", "Candidate", 700)]);
  const order = () => out.querySelectorAll(".path").map((e) => e.textContent);
  assert.deepStrictEqual(order(), ["a9", "a10", "B"]);
  out.named("th", "Saves").click();
  assert.deepStrictEqual(order(), ["a10", "a9", "B"]);
  out.named("th", "File").click();
  assert.deepStrictEqual(order(), ["a9", "a10", "B"]);
  out.named("th", "File").click();
  assert.deepStrictEqual(order(), ["B", "a10", "a9"]);

  const many = table(Array.from({ length: 502 }, (_, i) => row(`v${i}`, "Candidate", 500)));
  const all = many.all().find((e) => e.tag === "input" && e.attrs.title === "Select all shown");
  all.checked = true;
  all.onchange();
  assert(many.textContent.includes("500 selected"), many.textContent.slice(0, 300));
  assert(all.checked);
  many.named("button", "Show all").click();
  assert(!all.checked, "two rows on screen are not ticked");
};
checks["compare: a failed load and a player that cannot start both say why"] = async () => {
  const refused = openPage(() => ({ refuse: "no such report" }));
  await settled();
  refused.context.__root = refused.document.createElement("div");
  refused.run(`views.compare(__root, new URLSearchParams("report=/r.json"))`);
  await settled();
  assert(refused.context.__root.textContent.includes("no such report"));
  assert.strictEqual(lastToast(refused), "no such report");
  const broken = openPage(() => ({}));
  await settled();
  broken.context.__root = broken.document.createElement("div");
  broken.run(`comparePlayer = () => { throw new Error("cannot draw it"); }; views.compare(__root, new URLSearchParams("report=/r.json"))`);
  await settled();
  assert(broken.context.__root.textContent.includes("cannot draw it"));
  assert.strictEqual(lastToast(broken), "cannot draw it");
};

checks["compare: the output does not play before its source is there to play beside it"] = async () => {
  const page = openPage(() => ({}));
  await settled();
  page.context.__root = page.document.createElement("div");
  page.run(`var leave = comparePlayer(__root, { output_path: "/o.mp4", input_path: "/i.mp4", output_info: { fps: 25, duration_s: 10 }, input_info: { fps: 25, duration_s: 10 }, target_vmaf: 93, verify_vmaf: [94] })`);
  await settled();
  const root = page.context.__root;
  const videos = root.all().filter((e) => e.tag === "video");
  const vs = videos.find((v) => "muted" in v.attrs), vo = videos.find((v) => !("muted" in v.attrs));
  const play = root.named("button", "Play");
  // Loaded, as the browser says it; with no picture size after, so that no alignment starts.
  const loaded = (v) => { v.videoWidth = 64; v.dispatch("loadeddata"); v.videoWidth = 0; };
  assert(play.disabled);
  loaded(vo);
  await settled();
  assert(play.disabled, "Play is on with the output alone");
  play.click();
  assert(vo.paused, "the output plays before the source");
  loaded(vs);
  await settled();
  assert(!play.disabled);
  play.click();
  assert(!vo.paused && !vs.paused);
  page.run("leave()");
};

checks["compare: Play is there as soon as both sides can play, while they are still being lined up"] = async () => {
  const page = openPage(() => ({}));
  await settled();
  page.context.__root = page.document.createElement("div");
  page.run(`var leave = comparePlayer(__root, { output_path: "/o.mp4", input_path: "/i.mp4", output_info: { fps: 25, duration_s: 10 }, input_info: { fps: 25, duration_s: 10 }, target_vmaf: 93, verify_vmaf: [94] })`);
  await settled();
  const root = page.context.__root;
  const videos = root.all().filter((e) => e.tag === "video");
  const vs = videos.find((v) => "muted" in v.attrs), vo = videos.find((v) => !("muted" in v.attrs));
  const play = root.named("button", "Play");
  // With a picture size the alignment starts, and here it never ends: its own copies of the
  // two videos never load.
  for (const v of [vo, vs]) { v.videoWidth = 64; v.dispatch("loadeddata"); await settled(); }
  assert(root.all().some((e) => e.textContent === "aligning…"), "no alignment started");
  assert(!play.disabled, "Play waits for the alignment");
  play.click();
  assert(!vo.paused && !vs.paused);
  page.run("leave()");
};

checks["toast: a long message scrolls in its own box, beside a Close button"] = async () => {
  const page = openPage(() => ({}));
  await settled();
  page.context.__long = "cannot open /" + "x/".repeat(6000) + "last.mkv";
  page.run("realToast(__long, true)");
  const t = page.document.getElementById("toast");
  assert(!t.hidden);
  const text = t.all().find((e) => e.className === "toast-text");
  assert(text && text.textContent === page.context.__long, "the message is not in a box of its own");
  const close = t.all().find((e) => e.tag === "button");
  assert(close && close.attrs["aria-label"], "no Close button");
  assert(!text.contains(close), "the Close button scrolls away with the text");
  close.click();
  assert(t.hidden, "Close leaves the message");
};

checks["replace originals: an output with sections left out starts unticked, and says so beside any name"] = async () => {
  const page = openPage(() => ({}));
  await settled();
  const long = "a".repeat(300) + ".mkv";
  const row = (name, removed) => ({ report: `/out/${name}.json`, input: `/v/${name}`, source_bytes: 2e9, output_bytes: 1e9, replace: { dest: `/v/${name}`, removed, copy: false, zfs: false } });
  page.context.__rows = [row(long, 2), row("whole.mkv", 0)];
  page.run("replaceDialog(__rows, () => {})");
  const body = page.document.body;
  const rows = body.all().filter((e) => e.tag === "label" && e.names().includes("replace-row"));
  const box = (r) => r.all().find((e) => e.tag === "input");
  const [cut, whole] = rows;
  assert(!box(cut).checked, "the row with sections left out starts ticked");
  assert(box(whole).checked);
  const pill = cut.all().find((e) => e.names().includes("pill") && e.textContent === "2 sections left out");
  assert(pill, "no pill for the sections left out");
  const name = cut.all().find((e) => e.names().includes("path"));
  assert(name.textContent.startsWith("aaa") && !name.contains(pill), "the pill shares the name's line");
  assert(body.all().some((e) => e.tag === "p" && e.names().includes("warn") && e.textContent.startsWith("1 output leaves sections")), "no word about it above the list");
  const all = body.named("button", "All / none");
  const go = () => body.all().find((e) => e.tag === "button" && e.names().includes("danger"));
  // All leaves it alone, both ways; its own tick puts it in.
  const now = () => body.all().filter((e) => e.tag === "label" && e.names().includes("replace-row"));
  all.click();
  assert(now().every((r) => !box(r).checked));
  all.click();
  const [cut2, whole2] = now();
  assert(!box(cut2).checked && box(whole2).checked);
  assert.strictEqual(go().textContent, "Replace 1 original");
  box(cut2).checked = true;
  box(cut2).dispatch("change");
  assert.strictEqual(go().textContent, "Replace 2 originals");
};

// Answers held back until the check lets them go, so that they can arrive in any order.
const heldAnswers = () => {
  const waiting = [];
  return { waiting, answer: (reply) => new Promise((done) => waiting.push(() => done(reply))) };
};
const tell = (page, message) => { page.context.__message = message; page.run("for (const fn of [...listeners]) fn(__message)"); };

checks["scan: a running scan keeps Scan off when folders change, and a late answer does not undo the end"] = async () => {
  const held = heldAnswers();
  const running = { id: 1, status: "running", done: 0, total: 2 };
  const finished = { id: 1, status: "done", rows: [], totals: {}, paths: ["/videos"] };
  let asked = 0;
  const page = openPage((path, body) => (body ? { id: 1 } : path === "/api/scans/1" ? held.answer(++asked === 1 ? running : finished) : {}), { scanRoots: ["/videos", "/more"] });
  await settled();
  const root = page.show("scan");
  await settled();
  const start = root.named("button", "Scan");
  await start.click();
  await settled();
  root.all().find((e) => e.tag === "button" && e.textContent === "×").click();
  assert(start.disabled, "taking a folder away made Scan available");
  tell(page, { kind: "scan_done", id: 1 });
  await settled();
  const [saysRunning, saysDone] = held.waiting.splice(0);
  saysDone();
  await settled();
  assert(!start.disabled, "still off after the end");
  saysRunning();
  await settled();
  assert(!start.disabled, "the older answer turned it off again");
};
checks["watched folders: rows stay, a button stays busy, and a late list does not bring a forgotten folder back"] = async () => {
  const held = heldAnswers();
  const watch = (id) => ({ id, path: `/w/${id}`, out_dir: "/out", enabled: true, options: { target_vmaf: 93, preset: 5, container: "mp4" } });
  let list = { watches: [watch(1), watch(2)], settled_secs: 60 };
  let hold = false;
  const page = openPage((path, body) => {
    if (path === "/api/jobs") return { jobs: [], unrestored: [], queue: holdReply };
    if (path === "/api/watch") return hold ? held.answer(list) : list;
    if (path === "/api/watch/1/look") return held.answer(list);
    if (path === "/api/watch/2/remove") { list = { ...list, watches: [watch(1)] }; return list; }
    return {};
  });
  await settled();
  const root = page.show("queue");
  await settled();
  const rows = root.querySelectorAll(".watch");
  assert.strictEqual(rows.length, 2);
  const look = rows[0].named("button", "Look now");
  look.click();
  await settled();
  assert(look.disabled, "not busy");
  tell(page, { kind: "watch" });
  await settled();
  assert.strictEqual(root.querySelectorAll(".watch")[0], rows[0], "the row was built again");
  assert.strictEqual(rows[0].named("button", "Look now"), look, "the button was built again");
  assert(look.disabled, "a new drawing forgot the request on its way");
  held.waiting.splice(0).forEach((go) => go());
  await settled();
  assert(!look.disabled, "still busy after the answer");

  hold = true;
  tell(page, { kind: "watch" });
  await settled();
  const [late] = held.waiting.splice(0);
  hold = false;
  rows[1].named("button", "Forget").click();
  await settled();
  page.document.body.all().filter((e) => e.classList.contains("modal-bg")).pop().named("button", "Forget").click();
  await settled();
  assert.strictEqual(root.querySelectorAll(".watch").length, 1, "not gone after the answer");
  late();
  await settled();
  assert.strictEqual(root.querySelectorAll(".watch").length, 1, "a late list brought it back");
};
checks["queue: an older list does not replace a newer one, and a file read meanwhile is read again"] = async () => {
  const held = heldAnswers();
  const file = { input: "/v/a.mp4", status: "pending", spot_checks: [], notes: [], trials: [], chunks: [] };
  const list = (note) => ({ jobs: [{ id: 1, status: "running", total: 1, out_dir: "/out", note, options: { target_vmaf: 93, preset: 5, container: "mp4" }, counts: {}, files: [file] }], unrestored: [], queue: holdReply });
  const notes = ["asked first", "asked second", "asked third"];
  let holdLists = false, fileReads = 0;
  const page = openPage((path) => {
    if (path === "/api/jobs") return holdLists ? held.answer(list(notes.shift())) : list("first");
    if (path === "/api/watch") return { watches: [] };
    if (path === "/api/jobs/1/files/0") { fileReads++; return { head: {}, file: { ...file, status: "skip", detail: "too little to gain" } }; }
    return {};
  });
  const wait = (ms) => new Promise((done) => setTimeout(done, ms));
  await settled();
  const root = page.show("queue");
  await settled();
  holdLists = true;
  for (let i = 0; i < 2; i++) { tell(page, { kind: "jobs" }); await wait(170); }
  const [older, newer] = held.waiting.splice(0);
  newer();
  await settled();
  older();
  await settled();
  assert(root.textContent.includes("asked second") && !root.textContent.includes("asked first"), root.textContent.slice(0, 300));

  tell(page, { kind: "jobs" });
  await wait(170);
  const [third] = held.waiting.splice(0);
  tell(page, { kind: "job", id: 1, file: 0, event: { type: "phase" } });
  await wait(150);
  assert.strictEqual(fileReads, 1);
  third();
  await settled();
  assert(!root.textContent.includes("too little to gain"), "the list was to show its own older state");
  await wait(150);
  assert.strictEqual(fileReads, 2, "the file was not read again after the list");
  assert(root.textContent.includes("too little to gain"), root.textContent.slice(0, 200));
};
checks["history: a chart is made only while the view is open"] = async () => {
  const held = heldAnswers();
  const page = openPage((path) => (path === "/api/history" ? held.answer({ reports: [], dirs: [] }) : {}));
  await settled();
  page.run(`var charts = 0; savedByDay = () => [1, 2]; savedChart = () => { charts++; return { stop() {}, el: document.createElement("div") }; }`);
  page.context.__root = page.document.createElement("div");
  page.run("var leave = views.history(__root)");
  await settled();
  held.waiting.splice(0)[0]();
  await settled();
  assert.strictEqual(page.run("charts"), 1);
  page.run("leave = views.history(__root); leave()");
  await settled();
  held.waiting.splice(0)[0]();
  await settled();
  assert.strictEqual(page.run("charts"), 1);
};

(async () => {
  let failed = 0;
  for (const [name, check] of Object.entries(checks)) {
    try { await check(); console.log(`ok: ${name}`); } catch (e) { failed++; console.log(`FAILED: ${name}\n${e.stack}`); }
  }
  process.exit(failed ? 1 : 0);
})();
