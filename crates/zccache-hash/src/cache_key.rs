//! Cache key computation.
//!
//! A cache key uniquely identifies a compilation invocation's inputs.

use super::ContentHash;
use std::collections::BTreeMap;

/// Builder for constructing a deterministic cache key.
///
/// Environment and dependency maps are sorted for determinism, while
/// compile arguments are hashed in their original argv order because
/// compilers can treat argument ordering as significant.
#[derive(Debug, Default)]
pub struct CacheKeyBuilder {
    compiler_id: Option<ContentHash>,
    arguments: Vec<String>,
    env_vars: BTreeMap<String, String>,
    source_hash: Option<ContentHash>,
    dependency_hashes: BTreeMap<String, ContentHash>,
}

impl CacheKeyBuilder {
    /// Create a new empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the compiler identity hash.
    #[must_use]
    pub fn compiler(mut self, hash: ContentHash) -> Self {
        self.compiler_id = Some(hash);
        self
    }

    /// Add a relevant command-line argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.arguments.push(arg.into());
        self
    }

    /// Add a relevant environment variable.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env_vars.insert(key.into(), value.into());
        self
    }

    /// Set the source content hash.
    #[must_use]
    pub fn source(mut self, hash: ContentHash) -> Self {
        self.source_hash = Some(hash);
        self
    }

    /// Add a dependency (e.g., header) content hash.
    #[must_use]
    pub fn dependency(mut self, name: impl Into<String>, hash: ContentHash) -> Self {
        self.dependency_hashes.insert(name.into(), hash);
        self
    }

    /// Build the final cache key by hashing all inputs deterministically.
    ///
    /// # Panics
    ///
    /// Panics if compiler or source hash is not set.
    #[must_use]
    pub fn build(self) -> ContentHash {
        let mut hasher = blake3::Hasher::new();

        // Domain separation tag
        hasher.update(b"zccache-cache-key-v1");

        // Compiler identity
        #[expect(
            clippy::expect_used,
            reason = "builder precondition: caller must call .compiler() before .build() — documented in `# Panics`"
        )]
        let compiler = self.compiler_id.expect("compiler hash is required");
        hasher.update(compiler.as_bytes());

        // Arguments — preserve original argv order because compile flag order
        // can affect semantics (for example, repeated flags where the last wins).
        for arg in &self.arguments {
            hasher.update(arg.as_bytes());
            hasher.update(b"\0");
        }

        // Environment variables (BTreeMap is already sorted)
        for (key, value) in &self.env_vars {
            hasher.update(key.as_bytes());
            hasher.update(b"=");
            hasher.update(value.as_bytes());
            hasher.update(b"\0");
        }

        // Source hash
        #[expect(
            clippy::expect_used,
            reason = "builder precondition: caller must call .source() before .build() — documented in `# Panics`"
        )]
        let source = self.source_hash.expect("source hash is required");
        hasher.update(source.as_bytes());

        // Dependency hashes (BTreeMap is already sorted)
        for (name, hash) in &self.dependency_hashes {
            hasher.update(name.as_bytes());
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
    fn cache_key_deterministic() {
        let compiler = hash_bytes(b"gcc-12");
        let source = hash_bytes(b"int main() {}");

        let k1 = CacheKeyBuilder::new()
            .compiler(compiler)
            .source(source)
            .arg("-O2")
            .build();

        let k2 = CacheKeyBuilder::new()
            .compiler(compiler)
            .source(source)
            .arg("-O2")
            .build();

        assert_eq!(k1, k2);
    }

    #[test]
    fn different_args_different_key() {
        let compiler = hash_bytes(b"gcc-12");
        let source = hash_bytes(b"int main() {}");

        let k1 = CacheKeyBuilder::new()
            .compiler(compiler)
            .source(source)
            .arg("-O2")
            .build();

        let k2 = CacheKeyBuilder::new()
            .compiler(compiler)
            .source(source)
            .arg("-O0")
            .build();

        assert_ne!(k1, k2);
    }

    #[test]
    fn argument_order_is_preserved_in_key() {
        let compiler = hash_bytes(b"gcc-12");
        let source = hash_bytes(b"int main() {}");

        let k1 = CacheKeyBuilder::new()
            .compiler(compiler)
            .source(source)
            .arg("-include")
            .arg("a.h")
            .build();

        let k2 = CacheKeyBuilder::new()
            .compiler(compiler)
            .source(source)
            .arg("-include")
            .arg("a.h")
            .build();

        assert_eq!(k1, k2);
    }

    #[test]
    fn different_argument_order_produces_different_key() {
        let compiler = hash_bytes(b"gcc-12");
        let source = hash_bytes(b"int main() {}");

        let k1 = CacheKeyBuilder::new()
            .compiler(compiler)
            .source(source)
            .arg("-DNAME=first")
            .arg("-DNAME=second")
            .build();

        let k2 = CacheKeyBuilder::new()
            .compiler(compiler)
            .source(source)
            .arg("-DNAME=second")
            .arg("-DNAME=first")
            .build();

        assert_ne!(k1, k2, "swapping compile arg order must change the key");
    }
}
