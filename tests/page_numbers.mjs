// The parts of the page script that need no page: how numbers and file names are shown.
// Run with `node --test tests/page_numbers.mjs`. No packages and no build step: the part of
// ui/app.js between its two markers is evaluated on its own.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

const source = readFileSync(new URL("../ui/app.js", import.meta.url), "utf8");
const from = source.indexOf("// ---------- numbers and names ----------");
const to = source.indexOf("// ---------- end of the part that runs alone ----------");
assert.ok(from >= 0 && to > from, "the markers of the part that runs alone are in ui/app.js");
const names = ["bytes", "pct", "floorPct", "sizeChange", "savedPct", "num", "rel", "times", "plural", "pathList", "leftOutOfList", "shownPath", "copiesLeft", "newestOnly", "refusalText", "presetFrom", "NAME_ORDER", "rowOrder", "numberProblem", "RANGES", "sameKind", "rememberedForm", "rememberedOptions", "oneCrfText", "searchShift"];
const page = new Function(`"use strict";\n${source.slice(from, to)}\nreturn { ${names.join(", ")} };`)();

// Each case is [argument, what is shown]; arguments in an array are spread.
function cases(fn, list) {
  for (const [given, shown] of list) {
    const args = Array.isArray(given) ? given : [given];
    assert.equal(fn(...args), shown, `${fn.name}(${args.join(", ")})`);
  }
}

test("sizes are rounded before the unit and the sign are chosen", () => {
  cases(page.bytes, [
    [0, "0 B"],
    [512.3456, "512 B"],
    [-512.5, "−513 B"],
    [-0.4, "0 B"],
    [999.4, "999 B"],
    [999.6, "1 kB"],
    [1500, "2 kB"],
    [999499, "999 kB"],
    [999500, "1.0 MB"],
    [1.25e6, "1.3 MB"],
    [99.96e6, "100 MB"],
    [999.6e6, "1.0 GB"],
    [1.5e9, "1.5 GB"],
    [150e9, "150 GB"],
    [1.5e12, "1.50 TB"],
    [999.999e12, "1.00 PB"],
    [1500e12, "1.50 PB"],
    [-2.5e9, "−2.5 GB"],
    [5e18, "5000 PB"],
    [null, "–"],
    [undefined, "–"],
    [NaN, "–"],
    [Infinity, "–"],
  ]);
});

test("a change that rounds to nothing has no sign", () => {
  cases(page.sizeChange, [
    [0.12, "−12%"],
    [-0.12, "+12%"],
    [0, "0%"],
    [-0.004, "0%"],
    [0.004, "0%"],
    [[-0.004, 1], "+0.4%"],
    [null, "–"],
  ]);
  cases(page.savedPct, [
    [0.12, "12%"],
    [-0.12, "12% larger"],
    [-0.004, "0%"],
    [0, "0%"],
    [null, "–"],
    [NaN, "–"],
  ]);
  cases(page.pct, [
    [0.5, "50%"],
    [-0.001, "0%"],
    [Infinity, "–"],
    [[0.1234, 1], "12.3%"],
  ]);
  cases(page.rel, [
    [0.94, "−6.0%"],
    [1.06, "+6.0%"],
    [1, "0.0%"],
    [1.0001, "0.0%"],
    [Infinity, "–"],
  ]);
});

test("what is not complete never reads 100%", () => {
  cases(page.floorPct, [
    [0.996, "99%"],
    [0.29, "29%"],
    [1, "100%"],
    [1.2, "100%"],
    [0, "0%"],
    [-0.1, "0%"],
    [[0.9996, 1], "99.9%"],
    [[1, 1, false], "99.9%"],
    [[0.5, 1, false], "50.0%"],
    [[0.5, 1, true], "100.0%"],
    [null, "–"],
  ]);
});

test("a ratio with a base of zero is a dash", () => {
  cases(page.times, [
    [1.5, "1.50×"],
    [9.999, "10×"],
    [12.4, "12×"],
    [1 / 0, "–"],
    [0 / 0, "–"],
    [null, "–"],
  ]);
  cases(page.num, [
    [1.234, "1.23"],
    [[1.25, 1], "1.3"],
    [Infinity, "–"],
    [null, "–"],
  ]);
});

test("a list of paths cannot be given a line that is not a file", () => {
  const forged = "/videos/a\n/home/me/keep.mp4";
  const l = page.pathList(["/videos/b.mp4", forged, "/videos/c\td.mp4", "/videos/e\u2028f.mp4", "/videos/g.mp4"]);
  assert.equal(l.text, "/videos/b.mp4\n/videos/g.mp4\n");
  assert.deepEqual([l.listed, l.left], [2, 3]);
  assert.deepEqual({ ...page.pathList([forged]) }, { text: "", listed: 0, left: 1 });
  assert.equal(page.leftOutOfList(2), "2 files with line breaks or control characters in their names are not in this list");
  assert.equal(page.leftOutOfList(1), "1 file with line breaks or control characters in its name is not in this list");
  assert.equal(page.shownPath(forged), "/videos/a\u240a/home/me/keep.mp4");
  assert.equal(page.shownPath("/a\u007fb\u0085c\u2029d é"), "/a\u2421b\ufffdc\ufffdd é");
});

test("a pair of spare copies leaves its videos their copies", () => {
  const files = ["/keep/a", "/spare/a", "/keep/b", "/spare/b", "/alone/c"].map((path) => ({ path }));
  const groups = [
    { kind: "same", members: [0, 1] },
    { kind: "same", members: [2, 3] },
    { kind: "same_start", members: [1, 3] },
    { kind: "same_end", members: [1, 4] },
  ];
  const emptied = (marked, gone = []) => page.copiesLeft(groups, files, new Set(marked), new Set(gone)).emptied().length;
  assert.equal(emptied(["/spare/a", "/spare/b"]), 0);
  assert.equal(emptied(["/spare/a", "/alone/c"]), 0);
  assert.equal(emptied(["/keep/a", "/spare/a"]), 1, "its group; both pairs still have their other video");
  assert.equal(emptied(["/keep/a", "/spare/a", "/keep/b", "/spare/b"]), 3, "both groups and the pair between them");
  assert.equal(emptied(["/spare/a", "/spare/b"], ["/keep/a", "/keep/b"]), 3, "copies trashed earlier do not count");
});

test("only the answer to the latest question counts", () => {
  const ask = page.newestOnly();
  const first = ask(), second = ask();
  assert.equal(first(), false);
  assert.equal(second(), true);
});

test("a refusal is told in the server's words", () => {
  const said = (status, type, text, json = null) => page.refusalText(status, "Status", type, text, json);
  assert.equal(said(400, "application/json", "{}", { error: "no such file" }), "no such file");
  assert.equal(said(403, "text/plain", "forbidden: other pages\n"), "forbidden: other pages");
  assert.equal(said(415, "text/plain; charset=utf-8", "expected application/json"), "expected application/json");
  assert.equal(said(401, "text/html", "<p>Not signed in</p>"), "Not signed in: open the page again from the link av1opt printed");
  assert.equal(said(500, "text/html", "<p>oops</p>"), "500 Status");
  assert.equal(said(502, "text/plain", "x".repeat(301)), "502 Status");
  assert.equal(said(404, "text/plain", ""), "404 Status");
});

test("a remembered preset of 0 stays 0, and what is no number gives the usual one", () => {
  cases(page.presetFrom, [[0, 0], ["0", 0], [8, 8], ["8", 8], ["", 5], ["  ", 5], [null, 5], [undefined, 5], ["fast", 5], [2.5, 5]]);
});

test("names sort as people count, and rows without a value come last both ways", () => {
  assert.deepEqual(["a10.mkv", "B.mkv", "a9.mkv", "a.mkv"].sort(page.NAME_ORDER.compare), ["a.mkv", "a9.mkv", "a10.mkv", "B.mkv"]);
  const rows = [{ v: 2 }, { v: null }, { v: 1 }, {}, { v: 3 }];
  const sorted = (direction) => [...rows].sort(page.rowOrder((x) => x.v, direction)).map((x) => x.v ?? null);
  assert.deepEqual(sorted(1), [1, 2, 3, null, null]);
  assert.deepEqual(sorted(-1), [3, 2, 1, null, null]);
});

test("counts name one thing or several", () => {
  assert.equal(page.plural(1, "file"), "1 file");
  assert.equal(page.plural(2, "file"), "2 files");
  assert.equal(page.plural(0, "file"), "0 files");
});

test("a number out of a form's range is named in the page's words, with the server's range", () => {
  const { numberProblem: problem, RANGES } = page;
  for (const text of ["", " ", "abc", "120", "95.5", "-1", "Infinity"]) assert.equal(problem(text, RANGES.savingsPct), "enter a number from 0 to 95", text);
  for (const text of ["0", "20", "95", " 7.5 "]) assert.equal(problem(text, RANGES.savingsPct), null, text);
  assert.equal(problem("0", RANGES.vmaf), "enter a number above 0 and up to 100");
  assert.equal(problem("100", RANGES.vmaf), null);
  assert.equal(problem("0", RANGES.videos), "enter a whole number from 1 to 50");
  assert.equal(problem("2.5", RANGES.clips), "enter a whole number from 1 to 8");
  assert.equal(problem("-3", RANGES.clipSecs), "enter a number from 2 to 30");
  assert.equal(problem("2.5", RANGES.clipSecs), null);
  assert.equal(problem("14", RANGES.preset), "enter a whole number from 0 to 13");
  assert.equal(problem("0", RANGES.hours), "enter a number above 0 and up to 10000");
});

test("a remembered value stands for its default only when it is of the same kind", () => {
  const { sameKind, rememberedForm, rememberedOptions } = page;
  assert.equal(sameKind(3, 1), true);
  assert.equal(sameKind("3", 1), false);
  assert.equal(sameKind({}, []), false);
  assert.equal(sameKind([], []), true);
  assert.equal(sameKind(null, 1), false);
  assert.deepEqual(rememberedForm({ vmaf: 93, videos: 6 }, { vmaf: "x", videos: 4, other: 1 }), { vmaf: 93, videos: 4 });
  assert.deepEqual(rememberedForm({ vmaf: 93 }, ["vmaf"]), { vmaf: 93 });
  const defaults = { preset: 5, container: "mp4", scenes: "auto", meta_tags: [] };
  assert.deepEqual(rememberedOptions(defaults, { preset: 0, container: "avi", scenes: false, meta_tags: [["a", "b"], ["c", 1], "d"] }),
    { preset: 0, container: "mp4", scenes: false, meta_tags: [["a", "b"]] });
});

test("a one-CRF encode says the CRF that made the file, after a second encode or a repair", () => {
  const { oneCrfText: text } = page;
  assert.deepEqual(text({ crf: 30 }), { mode: "one CRF", crf: "30", short: "30" });
  // Reports from before first_crf and video_secs were kept still read.
  assert.equal(text({ crf: 30, stretches: null }).short, "30");
  const second = text({ crf: 27, first_crf: 30 });
  assert.equal(second.mode, "one CRF, encoded a second time after the check");
  assert.equal(second.crf, "27, encoded a second time (the first time at 30)");
  const whole = text({ crf: 45.25, stretches: [{ crf: 38.25, secs: 19.98 }], video_secs: 20 });
  assert.equal(whole.mode, "one CRF, encoded again whole after the check");
  assert.equal(whole.short, "38.25");
  const some = text({ crf: 30, stretches: [{ crf: 26, secs: 4 }, { crf: 24, secs: 2 }], video_secs: 600 });
  assert.equal(some.mode, "one CRF, 2 stretches encoded again");
  assert.equal(some.short, "30 · 24–26");
  assert.equal(some.crf, "30; 24–26 in 2 stretches (0:06 of 10:00) after the check");
});

test("two copies of unknown offset are lined up however much was cut from the front, within the budget", async () => {
  const { searchShift } = page;
  // The left copy is the right one less its first `cut` seconds: right time t is left time
  // t - cut. Frames differ in proportion to how far apart they are, up to a point.
  const pair = (cut) => ({ diff: async (t, shift) => Math.min(100, Math.abs(shift + cut) * 200), leftAt: (t, shift) => t + shift });
  const common = { points: [10.25, 20, 29.75], leftSecs: 30, rightSecs: 40, fps: 25, wide: true };
  // Cut at the start only: the two ends meet (a "same" pair began at 0 and stayed there).
  assert.equal(await searchShift({ ...common, ...pair(10) }), -10);
  // Cut at both ends: found by the scan and the finer steps after it, to the frame.
  const heard = [];
  assert.equal(await searchShift({ ...common, ...pair(4.36), found: (s) => heard.push(s) }), -4.36);
  assert.equal(heard.at(-1), -4.36);
  // Each comparison taking 100 ms of a 1 s budget: what was found by then is what comes back.
  let clock = 0;
  const slow = pair(4.36), diff = async (t, s) => { clock += 100; return slow.diff(t, s); };
  const early = await searchShift({ ...common, ...slow, diff, budgetMs: 1000, now: () => clock });
  assert(clock <= 1300, `went on to ${clock} ms`);
  assert.notEqual(early, -4.36);
  // Stopped from outside, it ends at once.
  let asked = 0;
  await searchShift({ ...common, ...pair(4.36), diff: async () => { asked++; return 1; }, stopped: () => asked >= 3 });
  assert(asked <= 4, `${asked} comparisons after the stop`);
});

test("an encode's own files move only to a frame that is clearly better", async () => {
  const { searchShift } = page;
  const common = { points: [2, 5, 8], leftAt: (t, shift) => t + shift, leftSecs: 10, rightSecs: 10, fps: 25 };
  // 2% better two frames on: noise, so nothing moves.
  assert.equal(await searchShift({ ...common, diff: async (t, s) => (Math.round(s * 25) === 2 ? 98 : 100) }), 0);
  // Half the difference two frames on: that is where the source lines up.
  assert.equal(await searchShift({ ...common, diff: async (t, s) => (Math.round(s * 25) === 2 ? 50 : 100) }), 0.08);
  // Only the three frames to either side are looked at.
  const seen = new Set();
  await searchShift({ ...common, base: 0.4, diff: async (t, s) => { seen.add(Math.round(s * 25)); return 1; } });
  assert.deepEqual([...seen].sort((a, b) => a - b), [7, 8, 9, 10, 11, 12, 13]);
});
