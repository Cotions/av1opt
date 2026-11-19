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
const names = ["bytes", "pct", "floorPct", "sizeChange", "savedPct", "num", "rel", "times", "plural", "pathList", "leftOutOfList", "shownPath", "copiesLeft", "newestOnly"];
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

test("counts name one thing or several", () => {
  assert.equal(page.plural(1, "file"), "1 file");
  assert.equal(page.plural(2, "file"), "2 files");
  assert.equal(page.plural(0, "file"), "0 files");
});
