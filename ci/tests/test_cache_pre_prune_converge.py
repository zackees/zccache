"""Post-delete convergence of the cache pre-prune (#1854).

Run 36720817402: retire-first deleted 8 entries, then `convergeInventory`
waited 40 x 15 s (10 min) for the *usage* API to agree with the listing and
threw "did not converge". The listing was correct and static the whole time
(no entry created after the deletes, all 8 ids absent); only the eventually
consistent usage counter lagged. These tests run the real workflow script
against a mocked GitHub API that reproduces that shape.
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
_spec = importlib.util.spec_from_file_location(
    "live_9490", ROOT / "ci/tests/test_cache_pre_prune_live_9490.py"
)
live = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(live)

HARNESS = r"""
const fs = require("node:fs");
const s = JSON.parse(fs.readFileSync(0, "utf8"));
let caches = s.caches.map((c) => ({ ...c }));
const deleted = [];
let usageLagPolls = 0; // list reads since the first delete
let lagBytes = 0;
let listReads = 0;
let writerFired = false;
let reappearFired = false;
const listed = () => caches.reduce((a, c) => a + c.size_in_bytes, 0);
const github = {
  rest: {
    repos: { getBranch: async () => ({ data: { commit: { sha: "abc" } } }) },
    actions: {
      getActionsCacheUsage: async () => {
        const stale = deleted.length > 0 && usageLagPolls < s.usageLagPolls;
        return { data: { active_caches_size_in_bytes: stale ? listed() + lagBytes : listed() } };
      },
      getActionsCacheList: Symbol("list"),
      deleteActionsCacheById: async ({ cache_id }) => {
        const gone = caches.find((c) => c.id === cache_id);
        deleted.push(gone);
        lagBytes += gone.size_in_bytes;
        caches = caches.filter((c) => c.id !== cache_id);
      },
    },
  },
  paginate: async () => {
    if (deleted.length > 0) {
      usageLagPolls += 1;
      listReads += 1;
      if (s.concurrentWriterAt && listReads === s.concurrentWriterAt && !writerFired) {
        writerFired = true;
        caches.push({ id: 777001, key: "setup-uv-1-pr-writer", ref: "refs/pull/9/merge",
          size_in_bytes: 5_000_000, created_at: "2026-09-30T13:23:00Z" });
      }
      if (s.reappearAt && listReads === s.reappearAt && !reappearFired) {
        reappearFired = true;
        caches.push({ ...deleted[0] }); // eventual consistency: id shows up again once
      } else if (s.reappearAt && listReads === s.reappearAt + 1) {
        caches = caches.filter((c) => c.id !== deleted[0].id); // ...and is gone again
      }
      if (s.alwaysChanging) {
        caches.push({ id: 800000 + listReads, key: `setup-uv-1-churn-${listReads}`,
          ref: "refs/pull/9/merge", size_in_bytes: 1000, created_at: "2026-09-30T13:23:00Z" });
      }
    }
    return caches.map((c) => ({ ...c }));
  },
};
const infos = [];
const core = { info: (m) => infos.push(String(m)) };
global.setTimeout = (f) => f();
process.env.GITHUB_WORKSPACE = s.root;
process.env.ROOT_LOCK_HASH_LF = s.lock;
process.env.ROOT_LOCK_HASH_CRLF = s.lock;
const AF = Object.getPrototypeOf(async function () {}).constructor;
const run = new AF("github", "core", "context", "require", s.script);
(async () => {
  let error = null;
  try {
    await run(github, core, { repo: { owner: "o", repo: "r" }, sha: "abc" }, require);
  } catch (e) {
    error = e.message;
  }
  process.stdout.write(JSON.stringify({
    error, deleted: deleted.map((c) => c.id), remaining: caches.length, listReads,
    verified: infos.some((m) => m.startsWith("Pre-prune verified")),
  }));
})();
"""


def _script() -> str:
    workflow = yaml.safe_load(
        (ROOT / ".github/workflows/cache-pre-prune.yml").read_text(encoding="utf-8")
    )
    return next(
        step["with"]["script"]
        for step in workflow["jobs"]["pre-prune"]["steps"]
        if step.get("name") == "Forecast and retire old-lock cache generations"
    )


def _run(**scenario: object) -> dict:
    payload = {
        "caches": live._caches(),
        "script": _script(),
        "root": str(ROOT),
        "lock": live.NEW_LOCK,
        "usageLagPolls": 0,
        **scenario,
    }
    result = subprocess.run(
        ["node", "-e", HARNESS],
        input=json.dumps(payload),
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def test_pre_prune_completes_when_the_usage_api_lags_far_behind_our_own_deletes() -> None:
    """RED before #1854: usage never catches up within the 40-poll window."""
    out = _run(usageLagPolls=10_000)
    assert out["error"] is None, out
    assert out["verified"] is True
    assert len(out["deleted"]) > 8  # retire-first plus the transition set both ran


def test_pre_prune_tolerates_a_concurrent_writer_that_then_goes_quiet() -> None:
    out = _run(usageLagPolls=10_000, concurrentWriterAt=3)
    assert out["error"] is None, out
    assert out["verified"] is True


def test_a_deleted_id_that_reappears_once_is_waited_out() -> None:
    out = _run(usageLagPolls=10_000, reappearAt=1)
    assert out["error"] is None, out
    assert out["verified"] is True


def test_listing_that_never_settles_still_fails_closed_before_transition_deletes() -> None:
    """Mid-flight inventory must not drive the forecast-dependent deletes."""
    out = _run(usageLagPolls=10_000, alwaysChanging=True)
    assert out["error"] is not None and "did not converge" in out["error"]
    assert len(out["deleted"]) == 8  # the retire-first set only
