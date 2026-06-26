# Vendored hotfix workflow for embedded zccache hosts

> **Issue:** zccache#909 — companion to the embedded-service contract
> ([`embedded-service.md`](embedded-service.md)) and the soldr / fbuild
> host-integration tracks (zccache#907, zccache#908).

When zccache runs embedded inside a host product (soldr, fbuild) and
the integration surfaces a bug — a race in the embedded service, an
audit-context wiring gap, a panic that only manifests under the host's
runtime — the host team needs a way to validate a candidate fix in a
real workload **before** the fix lands upstream. Without a documented
workflow this turns into long-lived untracked forks or hot-patched
release tarballs, which is exactly what the embedded-service contract's
"upstream as source of truth" principle is meant to prevent.

This document describes the supported workflow. It is the **only**
supported channel for in-flight zccache modifications consumed by an
embedded host; ad-hoc patches in the host's own checkout that never
make it back upstream violate the contract and will silently rot as
soon as the host bumps the zccache pin.

## The workflow at a glance

```text
1. branch  in zccache for the candidate fix
2. point   the host's zccache dep at that branch (git-rev pin)
3. validate against the host's real workload + audit traces
4. open    the zccache PR with the validation artifacts attached
5. merge   upstream; cut a zccache release
6. update  the host's zccache pin to the released version
```

Each step is described below. The whole pipeline is designed to finish
in **one host release cycle** — typically a few days, not weeks. If
step 5 stalls, the host's dep stays on a `rev = "<sha>"` git pin, which
is acceptable for a short window but is **not** an acceptable end state.

### 1. Branch in zccache for the candidate fix

The fix lives upstream from the very first commit. No host-local
patches against a vendored snapshot — those branches drift the moment
zccache lands an unrelated change, and the "validate before upstream"
loop only catches the original bug if the validation runs against the
exact code the upstream PR will land.

Recommended branch name: `fix/embedded-<host>-<short-description>` so
the upstream PR's intent is obvious from the branch list alone. Example:
`fix/embedded-soldr-cancel-during-flush`.

The fix branch's commit hygiene matches normal zccache work: one
logical change per commit, conventional-commit-style subject, the
commit body cites the host-side issue (`Refs soldr#NNN`) so the
cross-tree provenance is preserved even if the zccache and host repos
end up with different review queues.

### 2. Point the host's zccache dep at the branch

The host repo pins zccache by git revision (commit SHA), not by branch
name or tag, so the validation is reproducible:

```toml
# Cargo.toml in the host product
zccache = { git = "https://github.com/zackees/zccache.git",
            rev = "<sha-of-fix-branch-head>",
            optional = true }   # or default-on per the host's integration phase
```

Three rules for this pin:

1. **SHA, never branch.** A branch name `rev = "fix/..."` resolves to
   "whatever the branch points at right now", which silently changes
   the validation target every time someone pushes to the fix branch.
2. **One pin file, one place.** Soldr's pin lives in
   `crates/soldr-cli/Cargo.toml` next to `MANAGED_ZCCACHE_VERSION` and
   is called out in `CLAUDE.md`'s "Bumping managed_zccache_version"
   section as the lockstep fourth file. Fbuild has the equivalent
   convention in its own host-integration tracker (zccache#908).
3. **Tag the pin as a hotfix.** Add a TODO comment on the line —
   `# HOTFIX: <issue-link>, remove after upstream merges <PR link>` —
   so the next contributor to touch that line is reminded the pin is
   transient. The hotfix is **not** an excuse to leave the host on an
   un-released zccache rev once the upstream PR merges; step 6 closes
   that loop.

### 3. Validate against the host's real workload

This is the step the workflow exists for. The fix branch landed
upstream is only useful if a host workload proves the fix actually
resolves the original symptom and does not introduce a regression in
the host's hot path.

For each validation pass:

- Run the host's standard build / test suite with the new pin and
  confirm no regressions.
- Capture an audit trace ([JSONL writer landing per zccache#926](
  https://github.com/zackees/zccache/issues/926)) under a representative
  workload. The trace is the durable evidence of "the fix did what we
  expected" — if the bug was a cancellation-token race, the audit
  events show `mode: "cancelled"` landing in the right order. If the
  bug was a cache-key collision, the events show the namespacing fix
  routing each compile to the intended cache entry.
- Diff the trace against a baseline captured on the pre-fix commit.
  The diff itself is what you attach to the upstream PR. Two traces
  + one diff is enough; long-form prose explaining the diff is welcome
  but secondary.
- Note any non-trivial perf delta. The Linux Docker profile harness
  ([`bench/docker/`](../../bench)) is the canonical place to capture
  before/after numbers when the fix touches a hot path. If the fix is
  purely correctness, a one-line "no perceptible perf delta" line is
  acceptable evidence.

### 4. Open the zccache PR with validation artifacts

The upstream PR body must include:

- Link to the host-side issue that triggered the fix (`Refs soldr#NNN`).
- The validation artifacts from step 3: trace diff (or a short prose
  summary if the diff is large), the host workload that produced it,
  and any perf numbers from the Linux Docker harness.
- The full reproduction recipe — what host commit + what `Cargo.toml`
  edit + what command — so an upstream reviewer can replay the bug
  locally. A reviewer who cannot reproduce the bug without prior
  context cannot meaningfully sign off on the fix.

The PR uses the standard zccache PR template; the validation artifacts
go in the "Test plan" section verbatim. They are also re-attached to
the closing issue (zccache#NNN) so the issue's history captures the
provenance for future readers.

The point of putting the validation artifacts on the PR (not just the
host's internal tracker) is that **the upstream reviewer needs to see
them** to know the fix is real. Splitting the evidence across repos
forces the reviewer to chase context they shouldn't have to chase.

### 5. Merge upstream and cut a zccache release

The fix lands on `main`. The host's pin is still the fix-branch SHA at
this point — that's fine for a few days while the next zccache patch
release ships. The release cadence for these hotfix cycles is whatever
the active zccache release rhythm is (no special "patch release for
host fix" rule).

### 6. Update the host's pin to the released version

This is the step that prevents the workflow from rotting into
"vendored fork that nobody cleans up". As soon as the released
zccache version containing the fix is on `crates.io` (or available via
the host's normal version pin), the host:

- Bumps `MANAGED_ZCCACHE_VERSION` (or the host's equivalent) to the
  released version.
- Removes the `rev = "<sha>"` line and the HOTFIX TODO comment.
- Lands the bump in lockstep with all the other version-tracking files
  per the host's own bump-instructions doc (for soldr that's the four
  files listed under "Bumping managed_zccache_version" in
  `CLAUDE.md`).
- Closes the host-side issue if its only remaining gating condition
  was "fix this in zccache and pull it in".

If step 6 is skipped, the host is now running an off-release zccache
build with no version-tracking bump, no embed-manifest entry, and no
sha256-pinned download for the managed-binary path. This silently
breaks the published wheels and tarballs and is the failure mode this
workflow exists to prevent.

## Audit fixtures discovered in the host move back into zccache tests

The validation work from step 3 frequently turns up audit-trace
fragments that capture interesting boundary cases — a fresh cache
under contention, a cancellation observed mid-flush, a recovery after
a watcher overflow. These fragments belong in zccache's audit-fixture
directory (the parent for that lives behind zccache#906) so future
zccache changes are protected against regressing the same case.

The rule is: any non-trivial audit trace captured during a hotfix
validation gets minified (strip ephemeral fields: timestamps,
absolute paths, run IDs) and committed under
`crates/zccache/tests/audit-fixtures/embedded-host-<host>-<scenario>.jsonl`
as part of the upstream PR. The corresponding test asserts that
re-running the upstream code against the host's reproduction recipe
produces a trace shape-equivalent to the fixture.

This is the loop the embedded-service contract calls "audit
continuity": a bug surfaced once by a host integration becomes a
permanent regression test in zccache.

## What this workflow is NOT

- It is **not** a substitute for adding new public API to
  `zccache::embedded`. Additive contract changes go through the normal
  zccache RFC / issue process; this workflow is for bug fixes against
  the existing contract.
- It is **not** a path for soldr or fbuild to land host-specific
  behavior in zccache. Any change that only makes sense for one host
  belongs on the host side, behind whatever feature flag / dispatch
  surface the host already owns. If the same change would benefit
  another host, that is the signal to upstream — but the prior step
  is to confirm the change is host-agnostic, not host-specific.
- It is **not** a license to keep the host pinned to a git rev for
  longer than one release cycle. The HOTFIX TODO comment is a forcing
  function, not a decoration.

## Cross-references

- [`embedded-service.md`](embedded-service.md) — the embedded-service
  contract this workflow operates inside.
- zccache#907 — soldr integration tracker (the canonical example of a
  host product that vendor-hotfixes zccache).
- zccache#908 — fbuild integration tracker (the other one).
- zccache#929 — the embedded-service umbrella meta where the workflow
  is gated under "Operator surface".
- soldr `CLAUDE.md` "Bumping managed_zccache_version" — the soldr-side
  documented bump pipeline that step 6 plugs into.
