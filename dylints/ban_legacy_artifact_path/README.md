# ban_legacy_artifact_path

Requires runtime artifact locations to be resolved by the persistence layout
owner rather than constructed with cache-key formatting at call sites.

The lint recognizes direct `format!` calls, names assembled before a later
path join, component joins, and equivalent string concatenation. Approved
layout owners, migration code, and fixtures are exempted only through
`src/allowlist.txt`; every entry must name a real file and carry a rationale.
