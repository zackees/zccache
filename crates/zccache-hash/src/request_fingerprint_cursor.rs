//! Lending cursor for the existing request-key byte protocol.

#[derive(Clone, Copy)]
enum Phase {
    Tag,
    Compiler,
    CompilerEnd,
    Argument,
    ArgumentEnd,
    RawTag,
    RawArgument,
    RawEnd,
    Cwd,
    CwdEnd,
    EnvKey,
    EnvEquals,
    EnvValue,
    EnvEnd,
    Done,
}

/// Resumable request-key encoding for synchronous or asynchronous sinks.
///
/// Holds only the current normalized argument, never a whole-key buffer.
/// A returned fragment borrows the cursor: consume it before requesting the next
/// fragment. Fragment boundaries are not part of the byte protocol.
pub struct RequestFingerprint<'a, I: Iterator> {
    compiler: &'a str,
    arguments: I,
    current_argument: Option<I::Item>,
    raw: &'a [String],
    raw_index: usize,
    cwd: &'a str,
    env: &'a [(&'a str, &'a str)],
    env_index: usize,
    phase: Phase,
}

impl<'a, I: Iterator> RequestFingerprint<'a, I>
where
    I::Item: AsRef<str>,
{
    /// Supply normalized identity fields and the original expanded argv.
    /// Selected environment entries must already be sorted by the caller.
    pub fn new(
        compiler: &'a str,
        arguments: I,
        raw: &'a [String],
        cwd: &'a str,
        env: &'a [(&'a str, &'a str)],
    ) -> Self {
        Self {
            compiler,
            arguments,
            current_argument: None,
            raw,
            raw_index: 0,
            cwd,
            env,
            env_index: 0,
            phase: Phase::Tag,
        }
    }

    /// Return the next byte fragment, or `None` permanently after exhaustion.
    /// Advancing is lazy: no subsequent normalized argument is consumed until
    /// its fragment is requested. Empty fragments are valid.
    pub fn next_fragment(&mut self) -> Option<&[u8]> {
        loop {
            match self.phase {
                Phase::Tag => {
                    self.phase = Phase::Compiler;
                    return Some(b"zccache-request-v2\0");
                }
                Phase::Compiler => {
                    self.phase = Phase::CompilerEnd;
                    return Some(self.compiler.as_bytes());
                }
                Phase::CompilerEnd => {
                    self.phase = Phase::Argument;
                    return Some(b"\0");
                }
                Phase::Argument => {
                    self.current_argument = self.arguments.next();
                    if self.current_argument.is_some() {
                        self.phase = Phase::ArgumentEnd;
                        return self
                            .current_argument
                            .as_ref()
                            .map(|arg| arg.as_ref().as_bytes());
                    }
                    self.phase = Phase::RawTag;
                }
                Phase::ArgumentEnd => {
                    self.phase = Phase::Argument;
                    return Some(b"\0");
                }
                Phase::RawTag => {
                    self.phase = Phase::Cwd;
                    if self
                        .raw
                        .iter()
                        .any(|arg| matches!(arg.as_str(), "-MD" | "-MMD"))
                    {
                        self.phase = Phase::RawArgument;
                        return Some(b"user-depfile-raw-argv\0");
                    }
                }
                Phase::RawArgument => {
                    let Some(arg) = self.raw.get(self.raw_index) else {
                        self.phase = Phase::Cwd;
                        continue;
                    };
                    self.raw_index += 1;
                    if arg == "-MF" {
                        let stdout = self
                            .raw
                            .get(self.raw_index)
                            .is_some_and(|value| value == "-");
                        self.raw_index += 1;
                        if stdout {
                            return Some(b"-MF-stdout\0");
                        }
                    } else if arg == "-MF-" {
                        return Some(b"-MF-stdout\0");
                    } else if !arg.starts_with("-MF") {
                        self.phase = Phase::RawEnd;
                        return Some(arg.as_bytes());
                    }
                }
                Phase::RawEnd => {
                    self.phase = Phase::RawArgument;
                    return Some(b"\0");
                }
                Phase::Cwd => {
                    self.phase = Phase::CwdEnd;
                    return Some(self.cwd.as_bytes());
                }
                Phase::CwdEnd => {
                    self.phase = Phase::EnvKey;
                    return Some(b"\0");
                }
                Phase::EnvKey => {
                    if let Some((key, _)) = self.env.get(self.env_index) {
                        self.phase = Phase::EnvEquals;
                        return Some(key.as_bytes());
                    }
                    self.phase = Phase::Done;
                }
                Phase::EnvEquals => {
                    self.phase = Phase::EnvValue;
                    return Some(b"=");
                }
                Phase::EnvValue => {
                    self.phase = Phase::EnvEnd;
                    return Some(self.env[self.env_index].1.as_bytes());
                }
                Phase::EnvEnd => {
                    self.env_index += 1;
                    self.phase = Phase::EnvKey;
                    return Some(b"\0");
                }
                Phase::Done => return None,
            }
        }
    }
}
