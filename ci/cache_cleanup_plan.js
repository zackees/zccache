"use strict";

const { createHash } = require("node:crypto");

// Retired producer identities and ordinary families eligible for one-per-shape
// retention. Keep OS, architecture, target triple, and feature dimensions in
// the shape functions below; retired patterns are deliberately exact.
const RETIRED_MAIN_PREFIXES = [
  "solo-toolchain-v3-",
  "setup-soldr-buildcache-v2-macos-arm64-6d40444a3fc5e4d0-",
  "setup-soldr-buildcache-v2-macos-arm64-032744c531163905-",
];

const RETIRED_MAIN_PATTERNS = [
  /^cook-base-v2-linux-x64-glibc-rustc1\.95\.0-f9e7e4902-l[0-9a-f]{16}-soldrv0\.9\.23$/i,
  /^cook-base-v2-windows-x64-msvc-rustc1\.95\.0-f9e7e4902-l[0-9a-f]{16}-soldrv0\.9\.23$/i,
  /^cook-base-v2-linux-x64-glibc-rustc1\.95\.0-f9e7e4902-l[0-9a-f]{16}-soldrv0\.9\.23-xdylint$/i,
  /^setup-soldr-buildcache-v2-linux-x64-032744c531163905-[0-9a-f]{16}$/i,
  /^setup-soldr-buildcache-v2-windows-x64-9cc0e23f450b04b3-[0-9a-f]{16}$/i,
  /^setup-soldr-buildcache-v2-windows-arm64-9cc0e23f450b04b3-[0-9a-f]{16}$/i,
];

const CACHE_PREFIXES = [
  ...RETIRED_MAIN_PREFIXES,
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
];

// setup-soldr Cargo-registry keys include both the root Cargo.lock identity
// and a toolchain-signature digest. Only a prior lock hash with a same-ref,
// same-format/platform/architecture/digest replacement for the checked-out
// lock is retired; digest profiles and unknown namespaces stay protected.

function cacheShape(key) {
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
  return (
    CACHE_PREFIXES.some((prefix) => key.startsWith(prefix)) ||
    RETIRED_MAIN_PATTERNS.some((pattern) => pattern.test(key))
  );
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

function cargoLockHashes(contents) {
  const text = Buffer.isBuffer(contents) ? contents.toString("utf8") : String(contents);
  const lfText = text.replace(/\r\n/g, "\n");
  const crlfText = lfText.replace(/\n/g, "\r\n");
  const sha16 = (value) =>
    createHash("sha256").update(value, "utf8").digest("hex").slice(0, 16);
  return { lf: sha16(lfText), crlf: sha16(crlfText) };
}

function setupSoldrCargoRegistryKeyParts(key) {
  // Production keys contain no validation namespace. Unknown formats and
  // namespaced keys are deliberately not eligible for automatic retirement.
  const match =
    /^setup-soldr-cargoregistry-(v[12])-(linux|macos|windows)-(x64|arm64)-([0-9a-f]{16})-([0-9a-f]{16})$/i.exec(
      key,
    );
  if (!match) return null;
  return {
    format: match[1].toLowerCase(),
    os: match[2].toLowerCase(),
    arch: match[3].toLowerCase(),
    lockHash: match[4].toLowerCase(),
    digest: match[5].toLowerCase(),
  };
}

function supersededCargoRegistryIds(caches, currentRootLockHashes) {
  if (!currentRootLockHashes) return new Set();
  const hashesForOs = (os) => {
    const configured = currentRootLockHashes[os];
    if (Array.isArray(configured)) {
      return new Set(configured.map((value) => String(value).toLowerCase()));
    }
    return configured ? new Set([String(configured).toLowerCase()]) : new Set();
  };
  const rows = caches
    .filter((cache) => cache.ref === "refs/heads/main" && typeof cache.key === "string")
    .map((cache) => ({ cache, parts: setupSoldrCargoRegistryKeyParts(cache.key) }))
    .filter((row) => row.parts);
  const currentShapes = new Set();
  for (const { parts } of rows) {
    if (hashesForOs(parts.os).has(parts.lockHash)) {
      currentShapes.add(`${parts.format}\u0000${parts.os}\u0000${parts.arch}\u0000${parts.digest}`);
    }
  }
  const stale = new Set();
  for (const { cache, parts } of rows) {
    const shape = `${parts.format}\u0000${parts.os}\u0000${parts.arch}\u0000${parts.digest}`;
    if (!hashesForOs(parts.os).has(parts.lockHash) && currentShapes.has(shape)) {
      stale.add(cache.id);
    }
  }
  return stale;
}

function planCountPrune(caches, keepPerShape = 1, currentRootLockHashes = null) {
  const shapes = new Map();
  const stale = [...supersededCargoRegistryIds(caches, currentRootLockHashes)]
    .map((id) => caches.find((cache) => cache.id === id))
    .filter(Boolean);
  for (const cache of caches) {
    if (!cache.key || !isEligible(cache.key)) continue;
    if (
      cache.ref === "refs/heads/main" &&
      (RETIRED_MAIN_PREFIXES.some((prefix) => cache.key.startsWith(prefix)) ||
        RETIRED_MAIN_PATTERNS.some((pattern) => pattern.test(cache.key)))
    ) {
      stale.push(cache);
      continue;
    }
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
  RETIRED_MAIN_PREFIXES,
  RETIRED_MAIN_PATTERNS,
  cacheShape,
  cargoLockHashes,
  effectiveCacheBytes,
  isEligible,
  planCountPrune,
  planHardCap,
  setupSoldrCargoRegistryKeyParts,
};
