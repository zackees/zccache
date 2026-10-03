"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { planLockTransitionPrePrune } = require(process.argv[2]);
const CURRENT = "55c1323f88b8e77c";
const OLD = "01fdce755221f8f4";
const MEASURED = 1_056_628_741;
const RESERVED = 1_200_000_000;
function integration(id, lock, size = MEASURED) {
  return {
    id, key: `setup-soldr-buildcache-v2-linux-x64-63942eb6ab326045-integration-${lock}`,
    ref: "refs/heads/main", size_in_bytes: size, created_at: "2026-10-03T00:00:00Z",
  };
}
function plan(entries) {
  const caches = [{
    id: 99, key: `cook-base-v2-linux-x64-glibc-rustc1.95.0-f9e7e4902-l${CURRENT}-soldrv0.9.27`,
    ref: "refs/heads/main", size_in_bytes: 1, created_at: "2026-10-03T00:00:00Z",
  }, ...entries];
  const total = caches.reduce((sum, entry) => sum + entry.size_in_bytes, 0);
  return planLockTransitionPrePrune(caches,
    { linux: CURRENT, macos: CURRENT, windows: [CURRENT] }, total, total);
}
const old = plan([integration(1, OLD)]);
assert.equal(old.ok, true);
assert(old.deleteIds.includes(1), "retire old Integration before re-seeding");
assert.equal(old.estimatedNewBytes, RESERVED, "one measured reservation");
const current = plan([integration(2, CURRENT)]);
assert.equal(current.ok, true);
assert(!current.deleteIds.includes(2), "retain current Integration");
assert.equal(current.estimatedNewBytes, 0, "current generation needs no reservation");
const both = plan([integration(1, OLD), integration(2, CURRENT)]);
assert.equal(both.ok, true);
assert(both.deleteIds.includes(1));
assert(!both.deleteIds.includes(2));
assert.equal(both.estimatedNewBytes, 0, "do not charge a second replacement");
const larger = plan([integration(1, OLD, 1_400_000_000)]);
assert.equal(larger.ok, true);
assert.equal(larger.estimatedNewBytes, 1_400_000_000, "respect growth beyond the floor");
const bootstrap = plan([]);
assert.equal(bootstrap.ok, true);
assert.equal(bootstrap.estimatedNewBytes, RESERVED, "reserve a first writer before its archive exists");
const oversized = plan([{
  id: 3, key: "unclassified-live-entry", ref: "refs/heads/main",
  size_in_bytes: 8_500_000_000, created_at: "2026-10-03T00:00:00Z",
}]);
assert.equal(oversized.ok, false, "refuse writes when the first Integration archive cannot fit");
console.log("Integration retirement, replacement and bootstrap accounting: passed");

// The complete captured listing fits the unchanged production target after
// replacing the old Test store, even before Integration has any entry.
const inventory = JSON.parse(fs.readFileSync(
  path.join(__dirname, "fixtures/cache_inventory_integration_1885.json"), "utf8",
));
const live = planLockTransitionPrePrune(inventory.actions_caches,
  inventory.lock_hashes, inventory.listed_bytes, inventory.listed_bytes);
assert.equal(live.ok, true);
assert.equal(live.targetBytes, 9_200_000_000);
assert.equal(live.projectedPeakBytes, 9_100_758_346);
const oldTest = inventory.actions_caches.find((entry) =>
  entry.key === "setup-soldr-buildcache-v2-linux-x64-63942eb6ab326045-test-01fdce755221f8f4",
);
assert(oldTest);
assert.deepEqual(live.deleteIds, [oldTest.id]);
