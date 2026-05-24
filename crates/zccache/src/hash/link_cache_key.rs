//! Cache key computation for link/archive operations.
//!
//! Unlike compilation cache keys, link cache keys preserve input ordering
//! because linker behavior is order-sensitive (symbol resolution, archive
//! member ordering).

use super::ContentHash;
use std::collections::BTreeMap;

/// Builder for constructing a deterministic link/archive cache key.
///
/// Input file hashes are stored in order (not sorted) because linker and
/// archiver behavior can depend on the order of input files.
#[derive(Debug, Default)]
pub struct LinkCacheKeyBuilder {
    tool_id: Option<ContentHash>,
    flags: Vec<String>,
    env_vars: BTreeMap<String, String>,
    /// Input file hashes in order (NOT sorted — order matters for linking).
    input_hashes: Vec<ContentHash>,
}

impl LinkCacheKeyBuilder {
    /// Create a new empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the tool identity hash (ar/ld/lib.exe binary hash).
    #[must_use]
    pub fn tool(mut self, hash: ContentHash) -> Self {
        self.tool_id = Some(hash);
        self
    }

    /// Add a cache-relevant flag.
    #[must_use]
    pub fn flag(mut self, flag: impl Into<String>) -> Self {
        self.flags.push(flag.into());
        self
    }

    /// Add a relevant environment variable.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env_vars.insert(key.into(), value.into());
        self
    }

    /// Add an input file's content hash (order is preserved).
    #[must_use]
    pub fn input(mut self, hash: ContentHash) -> Self {
        self.input_hashes.push(hash);
        self
    }

    /// Build the final cache key by hashing all inputs deterministically.
    ///
    /// # Panics
    ///
    /// Panics if tool hash is not set or no input hashes are provided.
    #[must_use]
    pub fn build(self) -> ContentHash {
        let mut hasher = blake3::Hasher::new();

        // Domain separation tag — distinct from compilation keys
        hasher.update(b"zccache-link-key-v1");

        // Tool identity
        let tool = self.tool_id.expect("tool hash is required");
        hasher.update(tool.as_bytes());

        // Flags — NOT sorted, order can matter for linker flags
        // (but we keep them in the order they appeared on the command line)
        for flag in &self.flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        // Environment variables (BTreeMap is already sorted)
        for (key, value) in &self.env_vars {
            hasher.update(key.as_bytes());
            hasher.update(b"=");
            hasher.update(value.as_bytes());
            hasher.update(b"\0");
        }

        // Input file hashes — IN ORDER (not sorted!)
        assert!(
            !self.input_hashes.is_empty(),
            "at least one input hash is required"
        );
        for hash in &self.input_hashes {
            hasher.update(hash.as_bytes());
        }

        ContentHash::from_bytes(*hasher.finalize().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::super::hash_bytes;
    use super::*;

    #[test]
    fn link_key_deterministic() {
        let tool = hash_bytes(b"ar");
        let input1 = hash_bytes(b"a.o contents");
        let input2 = hash_bytes(b"b.o contents");

        let k1 = LinkCacheKeyBuilder::new()
            .tool(tool)
            .flag("rcs")
            .input(input1)
            .input(input2)
            .build();

        let k2 = LinkCacheKeyBuilder::new()
            .tool(tool)
            .flag("rcs")
            .input(input1)
            .input(input2)
            .build();

        assert_eq!(k1, k2);
    }

    #[test]
    fn different_flags_different_key() {
        let tool = hash_bytes(b"ar");
        let input = hash_bytes(b"a.o");

        let k1 = LinkCacheKeyBuilder::new()
            .tool(tool)
            .flag("rcs")
            .input(input)
            .build();

        let k2 = LinkCacheKeyBuilder::new()
            .tool(tool)
            .flag("rcsD")
            .input(input)
            .build();

        assert_ne!(k1, k2);
    }

    #[test]
    fn different_inputs_different_key() {
        let tool = hash_bytes(b"ar");
        let input1 = hash_bytes(b"a.o contents");
        let input2 = hash_bytes(b"different a.o");

        let k1 = LinkCacheKeyBuilder::new()
            .tool(tool)
            .flag("rcs")
            .input(input1)
            .build();

        let k2 = LinkCacheKeyBuilder::new()
            .tool(tool)
            .flag("rcs")
            .input(input2)
            .build();

        assert_ne!(k1, k2);
    }

    #[test]
    fn input_order_matters() {
        let tool = hash_bytes(b"ar");
        let a = hash_bytes(b"a.o");
        let b = hash_bytes(b"b.o");

        let k1 = LinkCacheKeyBuilder::new()
            .tool(tool)
            .flag("rcs")
            .input(a)
            .input(b)
            .build();

        let k2 = LinkCacheKeyBuilder::new()
            .tool(tool)
            .flag("rcs")
            .input(b)
            .input(a)
            .build();

        assert_ne!(k1, k2, "swapping input order must produce different key");
    }

    #[test]
    fn different_tool_different_key() {
        let ar = hash_bytes(b"ar");
        let llvm_ar = hash_bytes(b"llvm-ar");
        let input = hash_bytes(b"a.o");

        let k1 = LinkCacheKeyBuilder::new()
            .tool(ar)
            .flag("rcs")
            .input(input)
            .build();

        let k2 = LinkCacheKeyBuilder::new()
            .tool(llvm_ar)
            .flag("rcs")
            .input(input)
            .build();

        assert_ne!(k1, k2);
    }

    #[test]
    fn env_vars_affect_key() {
        let tool = hash_bytes(b"ar");
        let input = hash_bytes(b"a.o");

        let k1 = LinkCacheKeyBuilder::new()
            .tool(tool)
            .flag("rcs")
            .input(input)
            .build();

        let k2 = LinkCacheKeyBuilder::new()
            .tool(tool)
            .flag("rcs")
            .env("LIB", "/some/path")
            .input(input)
            .build();

        assert_ne!(k1, k2);
    }

    #[test]
    fn link_key_differs_from_compile_key() {
        // Ensure domain separation works — same inputs through compile vs link
        // key builders produce different keys.
        let tool = hash_bytes(b"tool-binary");
        let source = hash_bytes(b"source-content");

        let compile_key = super::super::cache_key::CacheKeyBuilder::new()
            .compiler(tool)
            .source(source)
            .arg("rcs")
            .build();

        let link_key = LinkCacheKeyBuilder::new()
            .tool(tool)
            .flag("rcs")
            .input(source)
            .build();

        assert_ne!(
            compile_key, link_key,
            "compile and link keys must differ (domain separation)"
        );
    }
}
