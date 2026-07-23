# ban_normalized_path_deref_containment

Prevents `NormalizedPath` containment checks from silently resolving to
`std::path::Path` through `Deref`. Use the inherent normalized methods.

The pass compares the receiver's pre-adjustment ADT with the resolved method
`DefId`, so aliases and harmless-looking method syntax cannot bypass the
normalized equality/hash representation. Reviewed compatibility exemptions
belong in `src/allowlist.txt` with a rationale.
