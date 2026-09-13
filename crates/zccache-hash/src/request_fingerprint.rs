//! Request-key byte encoding, independent of path policy and hashing effects.

#[path = "request_fingerprint_cursor.rs"]
mod cursor;
pub use cursor::RequestFingerprint;

/// Emit the existing v2 request fingerprint without allocating a whole-key buffer.
///
/// The caller supplies normalized compiler/argv/cwd and selected, sorted environment
/// entries. Argument order is significant. `raw_args` must be the original expanded
/// argv: user depfile identity intentionally retains its unnormalized spelling.
/// Chunk boundaries are unspecified; their concatenation defines the key.
///
/// # Errors
/// Returns the first sink error immediately, without consuming further arguments.
pub fn emit_request_fingerprint<A, S, E>(
    compiler: &str,
    normalized_args: A,
    raw_args: &[String],
    cwd: &str,
    selected_env: &[(&str, &str)],
    mut emit: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<(), E>
where
    A: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut cursor = RequestFingerprint::new(
        compiler,
        normalized_args.into_iter(),
        raw_args,
        cwd,
        selected_env,
    );
    while let Some(fragment) = cursor.next_fragment() {
        emit(fragment)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::emit_request_fingerprint;

    #[test]
    fn resumable_cursor_preserves_bytes_and_lazy_arguments() {
        use std::cell::Cell;
        let consumed = Cell::new(0);
        let raw = ["-MMD", "-MF", "out.d", "-MF-", "source.c"].map(String::from);
        let args = ["first", "", "last"].into_iter().map(|arg| {
            consumed.set(consumed.get() + 1);
            arg.to_owned()
        });
        let env = [("A", "value")];
        let mut cursor = super::RequestFingerprint::new("cc", args, &raw, "cwd", &env);
        let mut bytes = Vec::new();
        for _ in 0..3 {
            bytes.extend_from_slice(cursor.next_fragment().unwrap());
            assert_eq!(consumed.get(), 0);
        }
        bytes.extend_from_slice(cursor.next_fragment().unwrap());
        assert_eq!(consumed.get(), 1);
        while let Some(fragment) = cursor.next_fragment() {
            bytes.extend_from_slice(fragment);
        }
        assert_eq!(consumed.get(), 3);
        assert!(cursor.next_fragment().is_none());
        assert_eq!(bytes, b"zccache-request-v2\0cc\0first\0\0last\0user-depfile-raw-argv\0-MMD\0-MF-stdout\0source.c\0cwd\0A=value\0");
    }

    #[test]
    fn emits_ordered_bytes_and_raw_depfile_salt() {
        let raw = ["-MD", "-MF-", "source.c"].map(String::from);
        let mut bytes = Vec::new();
        emit_request_fingerprint(
            "cc",
            ["-O2", "-O0", ""],
            &raw,
            "work",
            &[("A", ""), ("Z", "last")],
            |chunk| {
                bytes.extend_from_slice(chunk);
                Ok::<_, ()>(())
            },
        )
        .unwrap();
        assert_eq!(bytes, b"zccache-request-v2\0cc\0-O2\0-O0\0\0user-depfile-raw-argv\0-MD\0-MF-stdout\0source.c\0work\0A=\0Z=last\0");
    }

    #[test]
    fn emitter_failure_stops_immediately() {
        let raw = ["-MMD", "-MF", "-", "source.c"].map(String::from);
        let mut boundaries = Vec::new();
        let mut consumed = 0;
        emit_request_fingerprint(
            "cc",
            ["one", "two"].into_iter().inspect(|_| consumed += 1),
            &raw,
            "work",
            &[("A", "value")],
            |chunk| {
                boundaries.push(chunk.to_vec());
                Ok::<_, usize>(())
            },
        )
        .unwrap();
        assert_eq!(consumed, 2);
        for fail_at in 0..boundaries.len() {
            let mut calls = 0;
            let mut seen_args = 0;
            let result = emit_request_fingerprint(
                "cc",
                ["one", "two"].into_iter().inspect(|_| seen_args += 1),
                &raw,
                "work",
                &[("A", "value")],
                |chunk| {
                    assert_eq!(chunk, boundaries[calls]);
                    let index = calls;
                    calls += 1;
                    if index == fail_at {
                        Err(index)
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result, Err(fail_at));
            assert_eq!(calls, fail_at + 1);
            assert_eq!(
                seen_args,
                if fail_at < 3 {
                    0
                } else if fail_at < 5 {
                    1
                } else {
                    2
                }
            );
        }
    }
}
