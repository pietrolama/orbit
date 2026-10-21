// Unit tests of the pure parts of the renderer (camera math and gravitational search session).
// Run with: node --test crates/orbit-app/tests/ui_gravity.test.js
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const path = require("node:path");
const R = require(path.join(__dirname, "..", "ui", "renderer.js"));

function original() {
  const m = new Map();
  m.set(1, { x: 10, y: 20, pinned: false });
  m.set(2, { x: -5, y: 7.25, pinned: false });
  m.set(3, { x: 100.125, y: -3, pinned: true });
  m.set(4, { x: 0.1, y: 0.2, pinned: false });
  m.set(5, { x: 42, y: 42, pinned: false });
  return m;
}
const results = [
  { entity_id: 1, relation_neighbors: [4, 5] },
  { entity_id: 3, relation_neighbors: [2] },
  { entity_id: 2, relation_neighbors: [] },
];

test("gravity targets are deterministic and keep pinned nodes in place", () => {
  const a = R.gravityTargets(results, original(), { x: 0, y: 0 }, 0.5);
  const b = R.gravityTargets(results, original(), { x: 0, y: 0 }, 0.5);
  assert.deepEqual([...a.entries()], [...b.entries()]);
  assert.deepEqual(a.get(3), { x: 100.125, y: -3 }, "pinned result does not move");
  assert.ok(a.has(4) && a.has(5), "neighbours are attracted");
  // Results are attracted towards the focal point.
  const d1 = Math.hypot(a.get(1).x, a.get(1).y);
  assert.ok(d1 < Math.hypot(10, 20));
});

test("restoring returns exactly the original coordinates", () => {
  const orig = original();
  const targets = R.gravityTargets(results, orig, { x: 0, y: 0 }, 0.5);
  const s = new R.GravitySession(orig, targets, 0, 600);
  const mid = s.positions(300);
  assert.notDeepEqual(mid.get(1), { x: 10, y: 20 });
  const held = s.positions(5000);
  assert.deepEqual(held.get(1), targets.get(1));
  s.release(6000);
  s.positions(6300);
  assert.equal(s.finished, false);
  const end = s.positions(6600);
  assert.equal(s.finished, true);
  for (const [id, p] of orig) {
    assert.equal(end.get(id).x, p.x, `x of ${id} restored bit-exactly`);
    assert.equal(end.get(id).y, p.y, `y of ${id} restored bit-exactly`);
  }
});

test("the session snapshot is isolated from later mutations of the input", () => {
  const orig = original();
  const s = new R.GravitySession(orig, new Map(), 0, 100);
  orig.get(1).x = 9999;
  s.release(0);
  const end = s.positions(1000);
  assert.equal(end.get(1).x, 10);
});

test("retargeting mid-animation is continuous", () => {
  const orig = original();
  const t1 = R.gravityTargets(results, orig, { x: 0, y: 0 }, 0.5);
  const s = new R.GravitySession(orig, t1, 0, 600);
  const before = s.positions(200).get(1);
  const t2 = R.gravityTargets(results.slice(1), orig, { x: 50, y: 50 }, 0.5);
  s.retarget(t2, 200);
  const after = s.positions(200).get(1);
  assert.ok(Math.abs(before.x - after.x) < 1e-9 && Math.abs(before.y - after.y) < 1e-9);
});

test("camera round trip and zoom around a fixed point", () => {
  const cam = R.makeCamera();
  cam.width = 800;
  cam.height = 600;
  cam.scale = 2;
  cam.cx = 10;
  const [sx, sy] = R.toScreen(cam, 33, -4);
  const [wx, wy] = R.toWorld(cam, sx, sy);
  assert.ok(Math.abs(wx - 33) < 1e-9 && Math.abs(wy + 4) < 1e-9);
  const before = R.toWorld(cam, 120, 80);
  R.zoomAt(cam, 120, 80, 3, 0.001, 1e6);
  const after = R.toWorld(cam, 120, 80);
  assert.ok(Math.abs(before[0] - after[0]) < 1e-9 && Math.abs(before[1] - after[1]) < 1e-9);
});
