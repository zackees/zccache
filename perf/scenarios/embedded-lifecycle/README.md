# Embedded lifecycle scenario

This scenario measures one language through five soldr lifecycle states:
daemon startup, an already-running cold cache, a local artifact hit, a shared
cache hit from a sibling worktree, and a target-intact no-op. It retains strict
abort/fallback evidence, per-phase timing and cache reports, artifact hashes,
output replay, and process RSS.
