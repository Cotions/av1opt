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
    Object.assign(this, { tag, children: [], parent: null, attrs: {}, style: {}, dataset: {}, listeners: {}, value: "", className: "", hidden: false, disabled: false, checked: false });
    const element = this;
    this.classList = {
      add: (...names) => { element.className = [...new Set([...element.names(), ...names])].join(" "); },
      remove: (...names) => { element.className = element.names().filter((n) => !names.includes(n)).join(" "); },
      contains: (name) => element.names().includes(name),
      toggle: (name, on = !element.classList.contains(name)) => { element.classList[on ? "add" : "remove"](name); return on; },
    };
  }
  names() { return this.className.split(/\s+/).filter(Boolean); }
  setAttribute(k, v) { this.attrs[k] = String(v); if (k === "value") this.value = String(v); if (k === "checked" || k === "disabled" || k === "hidden") this[k] = true; }
  getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
  removeAttribute(k) { delete this.attrs[k]; }
  addEventListener(kind, fn) { (this.listeners[kind] = this.listeners[kind] || []).push(fn); }
  removeEventListener(kind, fn) { this.listeners[kind] = (this.listeners[kind] || []).filter((f) => f !== fn); }
  dispatch(kind) { for (const fn of [...(this.listeners[kind] || [])]) fn({ target: this, preventDefault() {}, stopPropagation() {} }); }
  click() { if (this.onclick) return this.onclick({ target: this }); for (const fn of this.listeners.click || []) return fn({ target: this, preventDefault() {}, stopPropagation() {} }); }
  focus() { this.ownerDocument.activeElement = this; }
  blur() {}
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
  contains(other) { for (let n = other; n; n = n.parent) if (n === this) return true; return false; }
  get firstChild() { return this.children[0] || null; }
  get firstElementChild() { return this.children.find((k) => k instanceof Element) || null; }
  get isConnected() { let n = this; while (n.parent) n = n.parent; return n === this.ownerDocument.body; }
  get textContent() { return this.children.map((k) => k.textContent).join(""); }
  set textContent(text) { this.replaceChildren(); if (text !== "" && text != null) this.adopt([String(text)]); }
  querySelector() { return null; }
  querySelectorAll() { return []; }
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
function openPage(answer, stored = {}) {
  const page = { posts: [], toasts: [], stored: Object.fromEntries(Object.entries(stored).map(([k, v]) => ["av1opt." + k, JSON.stringify(v)])) };
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
    const reply = (path === "/api/state" ? state : await answer(path, body)) || {};
    return { ok: !reply.refuse, status: reply.refuse ? 400 : 200, statusText: "", json: async () => (reply.refuse ? { error: reply.refuse } : reply) };
  };
  const world = {
    Node, document, console, URLSearchParams, setTimeout, clearTimeout, setInterval: () => 0, clearInterval() {}, requestAnimationFrame() {}, performance,
    fetch: respond,
    location: { hash: "#/none", href: "", reload() {} },
    localStorage: { getItem: (k) => (k in page.stored ? page.stored[k] : null), setItem: (k, v) => { page.stored[k] = v; }, removeItem: (k) => { delete page.stored[k]; } },
    EventSource: class { addEventListener() {} close() {} },
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
  page.run("toast = __told");
  page.document = document;
  page.remembered = (k) => (("av1opt." + k) in page.stored ? JSON.parse(page.stored["av1opt." + k]) : undefined);
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
(async () => {
  let failed = 0;
  for (const [name, check] of Object.entries(checks)) {
    try { await check(); console.log(`ok: ${name}`); } catch (e) { failed++; console.log(`FAILED: ${name}\n${e.stack}`); }
  }
  process.exit(failed ? 1 : 0);
})();
