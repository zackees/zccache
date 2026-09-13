//! Request-key byte encoding, independent of path policy and hashing effects.

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
    emit(b"zccache-request-v2\0")?;
    emit(compiler.as_bytes())?;
    emit(b"\0")?;
    for arg in normalized_args {
        emit(arg.as_ref().as_bytes())?;
        emit(b"\0")?;
    }
    if raw_args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-MD" | "-MMD"))
    {
        emit(b"user-depfile-raw-argv\0")?;
        let mut index = 0;
        while index < raw_args.len() {
            let arg = &raw_args[index];
            if arg == "-MF" {
                if raw_args.get(index + 1).is_some_and(|value| value == "-") {
                    emit(b"-MF-stdout\0")?;
                }
                index += 2;
                continue;
            }
            if arg == "-MF-" {
                emit(b"-MF-stdout\0")?;
            } else if !arg.starts_with("-MF") {
                emit(arg.as_bytes())?;
                emit(b"\0")?;
            }
            index += 1;
        }
    }
    emit(cwd.as_bytes())?;
    emit(b"\0")?;
    for (key, value) in selected_env {
        emit(key.as_bytes())?;
        emit(b"=")?;
        emit(value.as_bytes())?;
        emit(b"\0")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::emit_request_fingerprint;

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
