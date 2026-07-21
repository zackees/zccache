//! Context registration methods for [`DepGraph`].
//!
//! Carved out of `mod.rs` to keep each file under the 1k-LOC guard.

use std::path::Path;
use std::time::Instant;

use zccache_core::NormalizedPath;

use super::super::context::{compute_context_key_with, CompileContext, ContextKey};
use super::{rebase_project_path, ContextEntry, ContextRegistration, ContextState, DepGraph};

impl DepGraph {
    /// Register a compilation context. Returns the context key.
    /// If the context already exists, returns the existing key.
    pub fn register(&self, ctx: CompileContext) -> ContextKey {
        self.register_with_root(ctx, None)
    }

    /// Register a compilation context with an optional key root used to
    /// normalize project-local paths across workspace renames.
    /// Variant of [`Self::register_with_root`] that folds an optional
    /// `worktree_salt` into the context key (issue #474). Used by the
    /// multi-file compile path when `keys::requires_worktree_in_key` is
    /// true for the unit. Returns only the resulting [`ContextKey`].
    pub fn register_with_root_and_salt(
        &self,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
        worktree_salt: Option<&Path>,
    ) -> ContextKey {
        self.register_with_root_and_salt_result(ctx, key_root, worktree_salt)
            .key
    }

    pub fn register_with_root(
        &self,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
    ) -> ContextKey {
        self.register_with_root_result(ctx, key_root).key
    }

    pub fn register_with_root_result(
        &self,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
    ) -> ContextRegistration {
        self.register_with_root_and_salt_result(ctx, key_root, None)
    }

    /// Issue #474: variant of [`Self::register_with_root_result`] that folds an
    /// optional `worktree_salt` into the context key. Used by the C/C++
    /// compile pipeline when `keys::requires_worktree_in_key` returns true
    /// (PCH builds + MSVC), so the resulting cache entry is scoped to one
    /// worktree and can't be served to a sibling clone whose embedded
    /// paths would diverge from the artifact's.
    pub fn register_with_root_and_salt_result(
        &self,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
        worktree_salt: Option<&Path>,
    ) -> ContextRegistration {
        let key =
            compute_context_key_with(&ctx, key_root.as_deref(), worktree_salt, |path, root| {
                self.cached_normalize_key_path(path, root)
            });
        self.register_with_key_and_root_result(key, ctx, key_root)
    }

    /// Register a compilation context with a precomputed key.
    ///
    /// Used for Rustc compilations where the context key is computed from
    /// `RustcCompileContext` (different domain tag) but the dep_graph stores
    /// a `CompileContext` with the source file path for freshness checks.
    pub fn register_with_key(&self, key: ContextKey, ctx: CompileContext) -> ContextKey {
        self.register_with_key_and_root(key, ctx, None)
    }

    pub fn register_with_key_and_root(
        &self,
        key: ContextKey,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
    ) -> ContextKey {
        self.register_with_key_and_root_result(key, ctx, key_root)
            .key
    }

    pub fn register_with_key_and_root_result(
        &self,
        key: ContextKey,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
    ) -> ContextRegistration {
        let registration = self.register_context_entry(key, ctx, key_root);
        self.rustc_externs.remove(&registration.map_key);
        registration
    }

    /// Register a rustc context with its current `--extern` file inputs.
    ///
    /// Rustc context keys already reduce extern path prefixes to filename
    /// identity. The dependency graph keeps the actual extern paths here only
    /// for hashing/freshness; artifact keys incorporate them by crate name.
    pub fn register_rustc_with_key_and_root_result(
        &self,
        key: ContextKey,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
        externs: Vec<(String, NormalizedPath)>,
        check_metadata_compat_key: Option<ContextKey>,
    ) -> ContextRegistration {
        let source_file = ctx.source_file.clone();
        let registration = self.register_context_entry(key, ctx, key_root.clone());
        self.rustc_externs.insert(registration.map_key, externs);
        let metadata_compat_map_key = check_metadata_compat_key.map(|compat_key| {
            super::ContextInstanceKey::new(compat_key, &source_file, key_root.as_ref()).map_key()
        });
        if let Some(compat_map_key) = metadata_compat_map_key {
            self.rustc_check_metadata_compat
                .insert(compat_map_key, registration.map_key);
        }
        ContextRegistration {
            metadata_compat_map_key,
            ..registration
        }
    }

    pub(super) fn register_context_entry(
        &self,
        key: ContextKey,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
    ) -> ContextRegistration {
        const MAX_EQUIVALENT_CONTEXTS: usize = 4;

        let instance = super::ContextInstanceKey::new(key, &ctx.source_file, key_root.as_ref());
        let instance_key = instance.map_key();
        if let Some(mut existing) = self.contexts.get_mut(&instance_key) {
            existing.last_accessed = Instant::now();
            return ContextRegistration {
                key,
                map_key: instance_key,
                instance,
                metadata_compat_map_key: None,
                rebased_from_equivalent_root: false,
            };
        }

        let candidate = self
            .indexes
            .equivalent_contexts
            .get(&key)
            .and_then(|instances| instances.iter().find_map(|candidate| {
                self.contexts.get(candidate).map(|entry| entry.clone())
            }));
        let rebased_from_equivalent_root = candidate
            .as_ref()
            .is_some_and(|entry| entry.key_root.is_some() && key_root.is_some() && entry.key_root != key_root);
        let entry = candidate.map_or_else(
            || ContextEntry {
                logical_key: key,
                context: ctx.clone(),
                key_root: key_root.clone(),
                resolved_includes: Vec::new(),
                unresolved_includes: Vec::new(),
                has_computed_includes: false,
                artifact_key: None,
                last_file_hashes: Vec::new(),
                last_accessed: Instant::now(),
                state: ContextState::Cold,
            },
            |mut candidate| {
                let old_root = candidate.key_root.clone();
                candidate.resolved_includes = candidate
                    .resolved_includes
                    .iter()
                    .map(|path| rebase_project_path(path, old_root.as_ref(), key_root.as_ref()))
                    .collect();
                candidate.last_file_hashes = candidate
                    .last_file_hashes
                    .iter()
                    .map(|(path, hash)| {
                        (rebase_project_path(path, old_root.as_ref(), key_root.as_ref()), *hash)
                    })
                    .collect();
                candidate.context = ctx.clone();
                candidate.key_root = key_root.clone();
                candidate.last_accessed = Instant::now();
                candidate
            },
        );
        self.contexts.entry(instance_key).or_insert(entry);

        let mut evicted = self.indexes.equivalent_contexts.entry(key).and_modify(|instances| {
            if !instances.contains(&instance_key) {
                instances.push(instance_key);
            }
        }).or_insert_with(|| vec![instance_key]);
        let evicted = if evicted.len() > MAX_EQUIVALENT_CONTEXTS {
            Some(evicted.remove(0))
        } else {
            None
        };
        if let Some(evicted) = evicted {
            self.contexts.remove(&evicted);
            self.rustc_externs.remove(&evicted);
            self.rustc_env_deps.remove(&evicted);
            let stale_compat: Vec<ContextKey> = self
                .rustc_check_metadata_compat
                .iter()
                .filter_map(|entry| (*entry.value() == evicted).then_some(*entry.key()))
                .collect();
            for compat_key in stale_compat {
                self.rustc_check_metadata_compat.remove(&compat_key);
            }
        }

        ContextRegistration {
            key,
            map_key: instance_key,
            instance,
            metadata_compat_map_key: None,
            rebased_from_equivalent_root,
        }
    }

    /// Returns `true` if the context has never been updated (no artifact key).
    /// Used by the server to skip pre-compile hashing on cold contexts where
    /// `check_diagnostic` would return `Cold` without examining any hashes.
    #[must_use]
    pub fn is_cold(&self, key: &ContextKey) -> bool {
        match self.contexts.get(key) {
            Some(entry) => entry.state == ContextState::Cold,
            None => true,
        }
    }
}
