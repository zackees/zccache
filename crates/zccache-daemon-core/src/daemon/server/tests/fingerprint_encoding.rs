//! Literal byte contracts captured before extracting the request encoder.
use super::super::*;

#[test]
fn request_fingerprint_encoding_compatibility() {
    let cases: &[(&[&str], &[u8])] = &[
        (&["-O2", "-O0", ""], b"-O2\0-O0\0\0"),
        (
            &["--remap-path-prefix", "src=virtual", "--remap-path-prefix"],
            b"--remap-path-prefix\0src=virtual\0--remap-path-prefix\0",
        ),
        (
            &["-MD", "-MF", "dep.d", "source.c"],
            b"-MD\0-MF\0dep.d\0source.c\0user-depfile-raw-argv\0-MD\0source.c\0",
        ),
        (
            &["-MMD", "-MFdep.d", "source.c"],
            b"-MMD\0-MFdep.d\0source.c\0user-depfile-raw-argv\0-MMD\0source.c\0",
        ),
        (
            &["-MD", "-MF", "-", "source.c"],
            b"-MD\0-MF\0-\0source.c\0user-depfile-raw-argv\0-MD\0-MF-stdout\0source.c\0",
        ),
        (
            &["-MD", "-MF-", "source.c"],
            b"-MD\0-MF-\0source.c\0user-depfile-raw-argv\0-MD\0-MF-stdout\0source.c\0",
        ),
        (&["-MD", "-MF"], b"-MD\0-MF\0user-depfile-raw-argv\0-MD\0"),
        (&["-MF", "dep.d"], b"-MF\0dep.d\0"),
    ];
    let env = vec![
        ("CARGO_Z".to_owned(), String::new()),
        ("CARGO_A".to_owned(), "first".to_owned()),
        ("IGNORED".to_owned(), "not-keyed".to_owned()),
    ];
    for (arguments, encoded_arguments) in cases {
        let arguments: Vec<String> = arguments.iter().map(|value| (*value).to_owned()).collect();
        let mut expected = b"zccache-request-v2\0rustc\0".to_vec();
        expected.extend_from_slice(encoded_arguments);
        expected.extend_from_slice(b"work\0CARGO_A=first\0CARGO_Z=\0");
        assert_eq!(
            request_fingerprint(
                Path::new("rustc"),
                &arguments,
                Path::new("work"),
                None,
                Some(&env)
            ),
            crate::hash::hash_bytes(&expected),
            "{arguments:?}"
        );
    }
}
