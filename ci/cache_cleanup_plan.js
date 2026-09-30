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
  // Cook bases run on Linux only (zackees/ci.yml#5, RUST-010; zccache#1758).
  // The macOS and Windows generations (~3 GB) pushed a full lock-transition
  // re-seed past the target, so the pre-prune failed closed on every push.
  /^cook-base-v2-(?:macos|windows)-/i,
  // #1852: the #1838 test-deps cook runs on Linux x64 only; the arm64 leg
  // (277 MB) pushed the transition forecast over the target and is retired.
  /^cook-base-v2-linux-arm64-glibc-rustc[0-9.]+-f6cafa616-l[0-9a-f]{16}-soldrv?[0-9.]+$/i,
  /^cook-base-v2-windows-x64-msvc-rustc1\.95\.0-f9e7e4902-l[0-9a-f]{16}-soldrv0\.9\.23$/i,
  /^cook-base-v2-linux-x64-glibc-rustc1\.95\.0-f9e7e4902-l[0-9a-f]{16}-soldrv0\.9\.23-xdylint$/i,
];

const LOCK_TRANSITION_TARGET_BYTES = 9_200_000_000;
// Latest measured native-Python release cook archive was 1,093,942,322 B.
// Reserve 1,120,000,000 B so small payload growth does not make the forecast
// depend on that one archive being exactly repeatable.
const NATIVE_PYTHON_F9_RESERVE_BYTES = 1_120_000_000;
// #1822: floor for what still-running older main writers can add before they
// finish. Per-commit `zccache-<OS>-test-*-<sha>` action entries dominate at
// about 1.6 GB per generation; the floor applies when the live inventory
// carries no such entry to measure.
const IN_FLIGHT_WRITER_MIN_RESERVE_BYTES = 1_600_000_000;

// Old-lock build-cache fallbacks deliberately retired at a lock transition.
// #1850: selected by (os, arch) only. The toolchain digest in the key moves
// with every toolchain/config change, so the original digest-pinned list went
// inert (nothing selected, 1.65 GB of old-lock builds kept next to their
// re-seeds). The main (no suffix) and `dylint` profiles of these platforms are
// retired; cross-target `check-*` profiles are the 100%-hit musl caches and
// other suffixes are unmeasured, so both stay.
const TRANSITION_BUILD_CACHE_FALLBACKS = [
  { os: "linux", arch: "x64" },
  { os: "windows", arch: "x64" },
  { os: "linux", arch: "arm64" },
];
const RETIRED_BUILD_CACHE_SUFFIXES = new Set(["", "dylint"]);
const TRANSITION_BUILD_CACHE_PROFILES = [
  { os: "linux", arch: "x64", digest: "6d40444a3fc5e4d0", suffix: "", minimumBytes: 695_272_185 },
  { os: "windows", arch: "x64", digest: "0a12db972fd789a0", suffix: "", minimumBytes: 563_360_821 },
  { os: "linux", arch: "arm64", digest: "6d40444a3fc5e4d0", suffix: "", minimumBytes: 326_918_007 },
  // Perf Guard both restores this active build-cache family and recreates it
  // after cleanup; reserve a larger-than-observed payload even if the current
  // listing is temporarily missing it.
  { os: "linux", arch: "x64", digest: "032744c531163905", suffix: "", minimumBytes: 400_000_000 },
  { os: "linux", arch: "x64", digest: "67ddfadb5b3c0042", suffix: "check-linux-x86-musl", minimumBytes: 222_818_750 },
  { os: "linux", arch: "x64", digest: "f19152cfbd9e4599", suffix: "check-linux-arm-musl", minimumBytes: 214_577_067 },
];

// Top-level workflow names which can persist cache data on a main push.
// Keep this list in sync with the writer/event contract tests. Reusable
// workflows run as part of their top-level caller and are represented by the
// caller's name here.
const MAIN_CACHE_WRITER_WORKFLOW_NAMES = [
  "CI",
  "Linux",
  "macOS",
  "Windows",
  "Wrapper end-to-end",
  "Clippy",
  "Integration",
  "Coverage",
  "Python Tests",
  "Perf Guard",
  "Filesystem Matrix",
  "Soldr Broker Stress",
  "Test zccache-action",
  "Feature Matrix Check",
  "Auto-Release",
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

function isRetiredMainKey(key) {
  return RETIRED_MAIN_PREFIXES.some((prefix) => key.startsWith(prefix)) ||
    RETIRED_MAIN_PATTERNS.some((pattern) => pattern.test(key));
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

function parseCookBaseKey(key) {
  // Treat every dimension around the Cargo.lock hash as opaque profile data.
  // The regex is intentionally strict about the known v2 layout; unknown key
  // formats stay protected until their restore semantics are reviewed.
  // The in-action cook writes `-soldrv0.9.25`; the standalone
  // `setup-soldr/cook` sub-action writes `-soldr0.9.25` (no `v`). Both name the
  // same release, so `v` is optional and the captured version excludes it.
  const match =
    /^cook-base-v2-(linux|macos|windows)-(x64|arm64)-([a-z0-9]+)-rustc([0-9.]+)-f([a-z0-9]+)-l([0-9a-f]{16})-soldrv?([0-9.]+)(?:-([a-z0-9][a-z0-9._-]*))?$/i.exec(
      key,
    );
  if (!match) return null;
  return {
    os: match[1].toLowerCase(),
    arch: match[2].toLowerCase(),
    libc: match[3].toLowerCase(),
    rustc: match[4],
    flags: match[5].toLowerCase(),
    lockHash: match[6].toLowerCase(),
    soldr: match[7],
    suffix: match[8] || "",
  };
}

function currentLockHashesByOs(currentRootLockHashes) {
  if (!currentRootLockHashes) return null;
  const result = new Map();
  for (const os of ["linux", "macos", "windows"]) {
    const configured = currentRootLockHashes[os];
    const values = Array.isArray(configured) ? configured : [configured];
    const hashes = values
      .filter((value) => typeof value === "string" && /^[0-9a-f]{16}$/i.test(value))
      .map((value) => value.toLowerCase());
    if (hashes.length === 0) return null;
    result.set(os, new Set(hashes));
  }
  return result;
}

function planCookBasePrune(caches, currentRootLockHashes) {
  const currentHashes = currentLockHashesByOs(currentRootLockHashes);
  if (!currentHashes) return { keep: caches.slice(), stale: [] };
  const keep = [];
  const stale = [];
  for (const cache of caches) {
    const parts = typeof cache.key === "string" ? parseCookBaseKey(cache.key) : null;
    const supportedMainCook =
      cache.ref === "refs/heads/main" && parts &&
      currentHashes.has(parts.os);
    if (supportedMainCook && !currentHashes.get(parts.os).has(parts.lockHash)) {
      stale.push(cache);
    } else {
      keep.push(cache);
    }
  }
  const uniqueStale = [...new Map(stale.map((cache) => [cache.id, cache])).values()];
  uniqueStale.sort((a, b) => Date.parse(a.created_at) - Date.parse(b.created_at));
  return { keep, stale: uniqueStale };
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

function setupSoldrBuildCacheKeyParts(key) {
  // The suffix is optional but remains part of the restore shape. The final
  // 16-hex component is the Cargo.lock identity; unknown formats stay held.
  const match =
    /^setup-soldr-buildcache-v2-(linux|macos|windows)-(x64|arm64)-([0-9a-f]{16})(?:-(.+?))?-([0-9a-f]{16})$/i.exec(
      key,
    );
  if (!match) return null;
  return {
    os: match[1].toLowerCase(),
    arch: match[2].toLowerCase(),
    digest: match[3].toLowerCase(),
    suffix: match[4] || "",
    lockHash: match[5].toLowerCase(),
  };
}

function buildCacheShape(parts) {
  return `${parts.os}\u0000${parts.arch}\u0000${parts.digest}\u0000${parts.suffix}`;
}

function buildCacheFamily(parts) {
  return `${parts.os}\u0000${parts.arch}\u0000${parts.suffix}`;
}

function profileBytes(rows, parseParts, currentHashes, shapeFor, requiredProfiles = []) {
  const groups = new Map();
  // A required minimum only applies to a shape that is still produced, i.e.
  // listed now. A hardcoded digest that no listed cache carries is an
  // orphan from an older toolchain/config and will never be re-seeded.
  const listedShapes = new Set(rows
    .map((cache) => parseParts(cache.key))
    .filter(Boolean)
    .map(shapeFor));
  for (const profile of requiredProfiles) {
    const shape = shapeFor(profile);
    if (!listedShapes.has(shape)) continue;
    groups.set(shape, { current: false, maxBytes: profile.minimumBytes });
  }
  for (const cache of rows) {
    const parts = parseParts(cache.key);
    if (!parts) continue;
    const shape = shapeFor(parts);
    const group = groups.get(shape) || { current: false, maxBytes: 0 };
    group.current ||= currentHashes.get(parts.os).has(parts.lockHash);
    group.maxBytes = Math.max(group.maxBytes, Number(cache.size_in_bytes) || 0);
    groups.set(shape, group);
  }
  let bytes = 0;
  for (const group of groups.values()) {
    if (!group.current) bytes += group.maxBytes;
  }
  return bytes;
}

// #1850: dotted-version compare ("1.95.0" vs "1.100.0"); missing parts are 0.
function compareDotted(a, b) {
  const pa = String(a).split(".").map((n) => Number.parseInt(n, 10) || 0);
  const pb = String(b).split(".").map((n) => Number.parseInt(n, 10) || 0);
  for (let i = 0; i < Math.max(pa.length, pb.length); i += 1) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d !== 0) return d;
  }
  return 0;
}

// #1850: main cook bases whose (os, arch, libc, flags, suffix) family also has
// a strictly newer (rustc, soldr) generation. Producers write the newest
// toolchain only, so a superseded generation is never re-seeded: it is dead
// weight that neither needs a forecast reserve nor a forecast to be deleted.
function supersededCookBaseIds(caches) {
  const families = new Map();
  const rows = [];
  for (const cache of caches) {
    if (cache.ref !== "refs/heads/main" || typeof cache.key !== "string") continue;
    const parts = parseCookBaseKey(cache.key);
    if (!parts || isRetiredMainKey(cache.key)) continue;
    const family = [parts.os, parts.arch, parts.libc, parts.flags, parts.suffix].join("\u0000");
    rows.push({ cache, parts, family });
    const best = families.get(family);
    if (!best || compareDotted(parts.rustc, best.rustc) > 0 ||
        (compareDotted(parts.rustc, best.rustc) === 0 && compareDotted(parts.soldr, best.soldr) > 0)) {
      families.set(family, { rustc: parts.rustc, soldr: parts.soldr });
    }
  }
  const superseded = new Set();
  for (const { cache, parts, family } of rows) {
    const best = families.get(family);
    const cmp = compareDotted(parts.rustc, best.rustc) || compareDotted(parts.soldr, best.soldr);
    if (cmp < 0) superseded.add(cache.id);
  }
  return superseded;
}

// #1852: lock-independent producer generations that are superseded within
// their own family. Neither is re-seeded (producers write the newest only), so
// like superseded cook bases they are retire-first and carry no forecast
// reserve. `soldr-mini-v2-<os>-<arch>-<libc>-v<soldr>`: the higher soldr wins.
// `setup-soldr-prepare-v3-...-s<signature>...`: the signature is opaque, so the
// newest entry of the otherwise identical shape wins.
function supersededGenerationIds(caches) {
  const groups = new Map();
  for (const cache of caches) {
    if (cache.ref !== "refs/heads/main" || typeof cache.key !== "string") continue;
    let shape = null;
    const mini = /^(soldr-mini-v2-[a-z0-9]+-[a-z0-9]+-[a-z0-9]+)-v([0-9.]+)$/i.exec(cache.key);
    const prepare = /^(setup-soldr-prepare-v3-.+?-)s[0-9a-f]{16}((?:-.+)?)$/i.exec(cache.key);
    if (mini) {
      shape = mini[1];
    } else if (prepare) {
      shape = prepare[1] + "\u0000" + prepare[2];
    } else {
      continue;
    }
    const group = groups.get(shape) || [];
    group.push({ cache, version: mini ? mini[2] : null });
    groups.set(shape, group);
  }
  const superseded = new Set();
  for (const group of groups.values()) {
    group.sort((a, b) =>
      a.version !== null
        ? compareDotted(b.version, a.version)
        : Date.parse(b.cache.created_at) - Date.parse(a.cache.created_at));
    for (const row of group.slice(1)) superseded.add(row.cache.id);
  }
  return superseded;
}

function planLockTransitionPrePrune(
  caches,
  currentRootLockHashes,
  usageBytes,
  listedBytes,
  targetBytes = LOCK_TRANSITION_TARGET_BYTES,
) {
  const currentHashes = currentLockHashesByOs(currentRootLockHashes);
  const currentBytes = effectiveCacheBytes(usageBytes, listedBytes);
  const fail = (reason) => ({
    ok: false, reason, deleteIds: [], selectedBuildCacheIds: [], retireFirstIds: [],
  });
  if (!currentHashes) return fail("current Cargo.lock hashes unavailable");
  if (!Number.isFinite(currentBytes) || currentBytes <= 0) return fail("cache inventory bytes unavailable");
  if (!Number.isFinite(targetBytes) || targetBytes <= 0) return fail("invalid transition target");

  const mainRows = caches.filter((cache) => cache.ref === "refs/heads/main" && typeof cache.key === "string");
  for (const cache of mainRows) {
    if (cache.key.startsWith("cook-base-v2-") && !parseCookBaseKey(cache.key)) {
      return fail(`unknown cook-base key format: ${cache.key}`);
    }
    if (cache.key.startsWith("setup-soldr-cargoregistry-") && !setupSoldrCargoRegistryKeyParts(cache.key)) {
      return fail(`unknown Cargo registry key format: ${cache.key}`);
    }
    if (cache.key.startsWith("setup-soldr-buildcache-") && !setupSoldrBuildCacheKeyParts(cache.key)) {
      return fail(`unknown build-cache key format: ${cache.key}`);
    }
  }

  // Root-lock changes invalidate exact cook keys. Prior-lock registry rows
  // also cannot restore into the new main lock; retiring them can remove a
  // warm seed for an older open PR, a bounded cache-performance tradeoff only
  // (the PR remains correct and can rebuild its registry snapshot).
  const staleCooks = planCookBasePrune(mainRows, currentRootLockHashes).stale;
  const registryRows = mainRows
    .filter((cache) => cache.key.startsWith("setup-soldr-cargoregistry-"));
  const staleRegistries = registryRows.filter((cache) => {
    const parts = setupSoldrCargoRegistryKeyParts(cache.key);
    return !currentHashes.get(parts.os).has(parts.lockHash);
  });
  const buildRows = mainRows
    .filter((cache) => cache.key.startsWith("setup-soldr-buildcache-"));
  const staleBuilds = buildRows.filter((cache) => {
    const parts = setupSoldrBuildCacheKeyParts(cache.key);
    return !currentHashes.get(parts.os).has(parts.lockHash);
  });

  const selectedBuildCacheIds = staleBuilds
    .filter((cache) => {
      const parts = setupSoldrBuildCacheKeyParts(cache.key);
      return RETIRED_BUILD_CACHE_SUFFIXES.has(parts.suffix) &&
        TRANSITION_BUILD_CACHE_FALLBACKS.some((candidate) =>
          candidate.os === parts.os && candidate.arch === parts.arch,
        );
    })
    .map((cache) => cache.id)
    .sort((a, b) => a - b);
  const selectedBuilds = staleBuilds.filter((cache) => selectedBuildCacheIds.includes(cache.id));
  const retiredBuilds = staleBuilds.filter((cache) => isRetiredMainKey(cache.key));
  // An old-lock build cache whose family (os, arch, suffix) already has a
  // current-lock generation under any digest is orphaned: its producer now
  // writes the current shape. Delete it rather than forecasting a re-seed.
  const currentBuildFamilies = new Set(buildRows
    .map((cache) => setupSoldrBuildCacheKeyParts(cache.key))
    .filter((parts) => currentHashes.get(parts.os).has(parts.lockHash))
    .map(buildCacheFamily));
  const orphanedBuilds = staleBuilds.filter((cache) =>
    currentBuildFamilies.has(buildCacheFamily(setupSoldrBuildCacheKeyParts(cache.key))),
  );
  const orphanedBuildIds = new Set(orphanedBuilds.map((cache) => cache.id));

  // Retire-first: superseded old-lock cook generations. They are already in
  // the stale set, but unlike the rest of it they are not re-seeded, so the
  // workflow may delete them before (and regardless of) the forecast verdict.
  const supersededCooks = supersededCookBaseIds(mainRows);
  const supersededOthers = supersededGenerationIds(mainRows);
  const supersededOtherRows = mainRows.filter((cache) => supersededOthers.has(cache.id));
  const retireFirstIds = [...new Set([
    ...staleCooks
      .filter((cache) => supersededCooks.has(cache.id) || isRetiredMainKey(cache.key))
      .map((cache) => cache.id),
    ...supersededOtherRows.map((cache) => cache.id),
  ])].sort((a, b) => a - b);
  const cookEstimate = profileBytes(
    mainRows.filter((cache) =>
      cache.key.startsWith("cook-base-v2-") && !isRetiredMainKey(cache.key) &&
      !supersededCooks.has(cache.id)),
    parseCookBaseKey,
    currentHashes,
    (parts) => `${parts.os}\u0000${parts.arch}\u0000${parts.libc}\u0000${parts.rustc}\u0000${parts.flags}\u0000${parts.soldr}\u0000${parts.suffix}`,
  );
  const registryEstimate = profileBytes(
    registryRows,
    setupSoldrCargoRegistryKeyParts,
    currentHashes,
    (parts) => `${parts.format}\u0000${parts.os}\u0000${parts.arch}\u0000${parts.digest}`,
  );
  const buildEstimate = profileBytes(
    buildRows.filter((cache) => !isRetiredMainKey(cache.key) && !orphanedBuildIds.has(cache.id)),
    setupSoldrBuildCacheKeyParts,
    currentHashes,
    buildCacheShape,
    TRANSITION_BUILD_CACHE_PROFILES,
  );
  const nativePythonF9Rows = mainRows.filter((cache) => {
    const parts = parseCookBaseKey(cache.key);
    return parts && parts.os === "linux" && parts.arch === "x64" &&
      parts.flags === "9e7e4902" && parts.suffix === "";
  });
  const hasCurrentNativePythonF9 = nativePythonF9Rows.some((cache) =>
    currentHashes.get("linux").has(parseCookBaseKey(cache.key).lockHash),
  );
  const nativePythonObservedBytes = Math.max(
    0,
    ...nativePythonF9Rows.map((cache) => Number(cache.size_in_bytes) || 0),
  );
  // The f9 profile is already counted in cookEstimate when an old generation
  // exists. Add only the reserve beyond that observed size, not a duplicate.
  const nativePythonReserve = hasCurrentNativePythonF9
    ? 0
    : Math.max(0, NATIVE_PYTHON_F9_RESERVE_BYTES - nativePythonObservedBytes);
  const deletedCaches = [...new Map([
    ...staleCooks, ...staleRegistries, ...selectedBuilds, ...retiredBuilds, ...orphanedBuilds,
    ...supersededOtherRows,
  ].map((cache) => [cache.id, cache])).values()];
  const deletedBytes = deletedCaches
    .reduce((sum, cache) => sum + (Number(cache.size_in_bytes) || 0), 0);
  const newBytes = cookEstimate + registryEstimate + buildEstimate + nativePythonReserve;
  const projectedPeakBytes = currentBytes - deletedBytes + newBytes;
  if (projectedPeakBytes > targetBytes) {
    return {
      ...fail(`projected peak ${projectedPeakBytes} exceeds transition target ${targetBytes}`),
      currentBytes,
      projectedPeakBytes,
      targetBytes,
      retireFirstIds,
      staleCookIds: staleCooks.map((cache) => cache.id),
      staleRegistryIds: staleRegistries.map((cache) => cache.id),
    };
  }
  return {
    ok: true,
    currentBytes,
    projectedPeakBytes,
    targetBytes,
    deleteIds: [...new Set(deletedCaches.map((cache) => cache.id))].sort((a, b) => a - b),
    staleCookIds: staleCooks.map((cache) => cache.id),
    staleRegistryIds: staleRegistries.map((cache) => cache.id),
    selectedBuildCacheIds,
    retireFirstIds,
    estimatedNewBytes: newBytes,
  };
}

// #1822: bytes the still-running older main writers could add. Only per-SHA
// producer shapes (cacheShape != null) are keyed by commit, so each older push
// can add one new generation per shape; lock-keyed setup-soldr layers dedupe on
// an exact key and are already covered by the forecast's projected peak.
function inFlightWriterReserveBytes(caches) {
  const largestByShape = new Map();
  for (const cache of caches) {
    if (cache.ref !== "refs/heads/main" || typeof cache.key !== "string") continue;
    const shape = cacheShape(cache.key);
    if (shape === null) continue;
    const bytes = Number(cache.size_in_bytes) || 0;
    if (bytes > (largestByShape.get(shape) || 0)) largestByShape.set(shape, bytes);
  }
  let reserve = 0;
  for (const bytes of largestByShape.values()) reserve += bytes;
  return Math.max(reserve, IN_FLIGHT_WRITER_MIN_RESERVE_BYTES);
}

// #1822: decide whether the pre-prune may release producers without waiting
// for older writers. The wait only orders deletes after older saves, so it is
// unnecessary when the forecast plans no deletes and the projected peak plus
// the in-flight reserve still fits the target.
function planWriterBarrierSkip(caches, plan) {
  if (!plan || !plan.ok) return { skip: false, reason: "forecast did not pass" };
  if (plan.deleteIds.length > 0) {
    return { skip: false, reason: `${plan.deleteIds.length} deletes planned` };
  }
  const reserveBytes = inFlightWriterReserveBytes(caches);
  const totalBytes = plan.projectedPeakBytes + reserveBytes;
  if (totalBytes > plan.targetBytes) {
    return {
      skip: false,
      reason: `projected peak ${plan.projectedPeakBytes} + in-flight reserve ${reserveBytes} exceeds target ${plan.targetBytes}`,
      reserveBytes,
    };
  }
  return { skip: true, reason: "no deletes and peak plus in-flight reserve fit the target", reserveBytes };
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
  stale.push(...[...supersededBuildCacheIds(caches, currentRootLockHashes)]
    .map((id) => caches.find((cache) => cache.id === id))
    .filter(Boolean));
  for (const cache of caches) {
    if (!cache.key || !isEligible(cache.key)) continue;
    if (cache.ref === "refs/heads/main" && isRetiredMainKey(cache.key)) {
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
  const uniqueStale = [...new Map(stale.map((cache) => [cache.id, cache])).values()];
  uniqueStale.sort((a, b) => Date.parse(a.created_at) - Date.parse(b.created_at));
  return { keep, stale: uniqueStale };
}

function supersededBuildCacheIds(caches, currentRootLockHashes) {
  const currentHashes = currentLockHashesByOs(currentRootLockHashes);
  if (!currentHashes) return new Set();
  const rows = caches
    .filter((cache) => cache.ref === "refs/heads/main" && typeof cache.key === "string")
    .map((cache) => ({ cache, parts: setupSoldrBuildCacheKeyParts(cache.key) }))
    .filter((row) => row.parts);
  const currentShapes = new Set();
  for (const { parts } of rows) {
    if (currentHashes.get(parts.os).has(parts.lockHash)) currentShapes.add(buildCacheShape(parts));
  }
  const stale = new Set();
  for (const { cache, parts } of rows) {
    if (!currentHashes.get(parts.os).has(parts.lockHash) && currentShapes.has(buildCacheShape(parts))) {
      stale.add(cache.id);
    }
  }
  return stale;
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
  IN_FLIGHT_WRITER_MIN_RESERVE_BYTES,
  LOCK_TRANSITION_TARGET_BYTES,
  MAIN_CACHE_WRITER_WORKFLOW_NAMES,
  NATIVE_PYTHON_F9_RESERVE_BYTES,
  RETIRED_MAIN_PREFIXES,
  RETIRED_MAIN_PATTERNS,
  TRANSITION_BUILD_CACHE_FALLBACKS,
  TRANSITION_BUILD_CACHE_PROFILES,
  cacheShape,
  cargoLockHashes,
  effectiveCacheBytes,
  inFlightWriterReserveBytes,
  isRetiredMainKey,
  isEligible,
  planWriterBarrierSkip,
  planCountPrune,
  planHardCap,
  parseCookBaseKey,
  planCookBasePrune,
  planLockTransitionPrePrune,
  setupSoldrCargoRegistryKeyParts,
  setupSoldrBuildCacheKeyParts,
  supersededBuildCacheIds,
  supersededCookBaseIds,
  supersededGenerationIds,
};
