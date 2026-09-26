"use strict";

// Cache families whose old generations are safe to retire after a newer
// generation for the same restore shape exists. Keep one generation per
// shape; the shape functions below intentionally retain OS, architecture,
// target triple, and feature dimensions.
const CACHE_PREFIXES = [
  "cook-delta-v2-",
  "zccache-Linux-X64-test-",
  "zccache-Linux-ARM64-test-",
  "zccache-Windows-X64-test-",
  "zccache-macos-arm64-test-", // historical lowercase spelling
  "zccache-macOS-ARM64-test-",
  "cargo-registry-Linux-X64-test-",
  "cargo-registry-Linux-ARM64-test-",
  "cargo-registry-Windows-X64-test-",
  "cargo-registry-macOS-ARM64-test-",
  "cargo-target-Linux-X64-bench-",
  "cargo-target-Windows-X64-bench-",
  "zccache-Linux-X64-bench-",
  "zccache-Windows-X64-bench-",
  "zccache-macos-arm64-bench-",
  "zccache-macOS-ARM64-bench-",
  "cargo-target-macos-arm64-bench-",
  "setup-soldr-cargoregistry-v1-",
];

function cacheShape(key) {
  if (key.startsWith("setup-soldr-cargoregistry-v1-")) {
    const match = key.match(
      /^(setup-soldr-cargoregistry-v1-(?:linux-x64|linux-arm64|macos-arm64|windows-x64|windows-arm64))-.*$/i,
    );
    return match ? match[1].toLowerCase() : null;
  }
  if (/^cargo-registry-.+-test-/.test(key)) {
    // The trailing digest changes with the registry snapshot, not the target.
    return key.replace(/-[0-9a-f]{16}$/i, "");
  }
  if (/^zccache-.+-test-/.test(key)) {
    // Keep the target triple in the shape; only the source commit is a
    // generation. This handles the historical macOS capitalization too.
    return key.replace(/-[0-9a-f]{40}$/i, "");
  }
  if (key.startsWith("cook-delta-v2-")) {
    return key.replace(/-g[0-9a-f]{8,40}$/i, "");
  }
  if (/-bench-/.test(key)) {
    return key.replace(/-[0-9a-f]{40}$/i, "");
  }
  return null;
}

function isEligible(key) {
  return CACHE_PREFIXES.some((prefix) => key.startsWith(prefix));
}

function newestFirst(a, b) {
  const aMain = a.ref === "refs/heads/main";
  const bMain = b.ref === "refs/heads/main";
  if (aMain !== bMain) return aMain ? -1 : 1;
  return Date.parse(b.created_at) - Date.parse(a.created_at);
}

function effectiveCacheBytes(usageBytes, listedBytes) {
  return Math.max(Number(usageBytes) || 0, Number(listedBytes) || 0);
}

function planCountPrune(caches, keepPerShape = 1) {
  const shapes = new Map();
  const stale = [];
  for (const cache of caches) {
    if (!cache.key || !isEligible(cache.key)) continue;
    // setup-soldr#528 disabled this layer at every first-party call site.
    // No current workflow restores it, so every remaining delta archive is
    // an obsolete generation rather than a reusable shape.
    if (cache.key.startsWith("cook-delta-v2-")) {
      stale.push(cache);
      continue;
    }
    const shape = cacheShape(cache.key);
    if (!shape) continue;
    // GitHub cache refs are isolation boundaries. Preserve one generation
    // per ref and restore shape; do not assume PR caches are restorable from
    // main or vice versa.
    const refShape = `${cache.ref || ""}\u0000${shape}`;
    const group = shapes.get(refShape) || [];
    group.push(cache);
    shapes.set(refShape, group);
  }

  const keep = [];
  for (const group of shapes.values()) {
    group.sort(newestFirst);
    keep.push(...group.slice(0, keepPerShape));
    stale.push(...group.slice(keepPerShape));
  }
  stale.sort((a, b) => Date.parse(a.created_at) - Date.parse(b.created_at));
  return { keep, stale };
}

function planHardCap(caches, currentBytes, targetBytes, alreadyPlannedIds = []) {
  const { keep, stale } = planCountPrune(caches);
  const plannedIds = new Set(alreadyPlannedIds);
  const selected = [];
  let projectedBytes = currentBytes;
  for (const cache of stale.slice().reverse()) {
    if (projectedBytes <= targetBytes) break;
    if (plannedIds.has(cache.id)) continue;
    plannedIds.add(cache.id);
    selected.push(cache);
    projectedBytes -= cache.size_in_bytes;
  }
  return {
    keep,
    selected,
    projectedBytes,
    withinBudget: projectedBytes <= targetBytes,
  };
}

module.exports = {
  CACHE_PREFIXES,
  cacheShape,
  effectiveCacheBytes,
  isEligible,
  planCountPrune,
  planHardCap,
};
