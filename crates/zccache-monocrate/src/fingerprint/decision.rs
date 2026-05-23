use std::fmt;

/// The result of checking whether an operation should be skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheDecision {
    /// Inputs haven't changed since last success — safe to skip.
    Skip,
    /// The operation must run.
    Run(RunReason),
}

/// Why an operation must run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunReason {
    /// No cache file exists (first run).
    NoCacheFile,
    /// Cache file was corrupt or unreadable.
    CacheCorrupt,
    /// Previous run recorded a failure.
    PreviousFailure,
    /// File content or set changed.
    ContentChanged,
}

impl fmt::Display for CacheDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skip => write!(f, "skip (cache hit)"),
            Self::Run(reason) => write!(f, "run: {reason}"),
        }
    }
}

impl fmt::Display for RunReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCacheFile => write!(f, "no cache file"),
            Self::CacheCorrupt => write!(f, "cache corrupt"),
            Self::PreviousFailure => write!(f, "previous failure"),
            Self::ContentChanged => write!(f, "content changed"),
        }
    }
}

impl CacheDecision {
    /// Returns `true` if the operation can be skipped.
    #[must_use]
    pub fn should_skip(&self) -> bool {
        matches!(self, Self::Skip)
    }

    /// Returns `true` if the operation must run.
    #[must_use]
    pub fn should_run(&self) -> bool {
        matches!(self, Self::Run(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_skip() {
        assert_eq!(CacheDecision::Skip.to_string(), "skip (cache hit)");
    }

    #[test]
    fn display_run_reasons() {
        assert_eq!(
            CacheDecision::Run(RunReason::NoCacheFile).to_string(),
            "run: no cache file"
        );
        assert_eq!(
            CacheDecision::Run(RunReason::ContentChanged).to_string(),
            "run: content changed"
        );
    }

    #[test]
    fn should_skip_and_run() {
        assert!(CacheDecision::Skip.should_skip());
        assert!(!CacheDecision::Skip.should_run());
        assert!(CacheDecision::Run(RunReason::NoCacheFile).should_run());
        assert!(!CacheDecision::Run(RunReason::NoCacheFile).should_skip());
    }
}
