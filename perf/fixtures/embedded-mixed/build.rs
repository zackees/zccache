use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn compiler(variable: &str, fallback: &str) -> Vec<OsString> {
    env::var_os(variable)
        .map(|value| {
            value
                .to_string_lossy()
                .split_whitespace()
                .map(OsString::from)
                .collect()
        })
        .unwrap_or_else(|| vec![OsString::from(fallback)])
}

fn run(command: &[OsString], args: &[OsString]) {
    let mut process = Command::new(&command[0]);
    process.args(&command[1..]).args(args);
    let status = process.status().expect("failed to launch fixture compiler");
    assert!(status.success(), "fixture compiler failed: {process:?}");
}

fn main() {
    let language = env::var("EMBEDDED_LANGUAGE").unwrap_or_else(|_| "rust".into());
    println!("cargo:rustc-env=EMBEDDED_FIXTURE_LANGUAGE={language}");
    println!("cargo:rerun-if-env-changed=EMBEDDED_LANGUAGE");

    if language == "rust" {
        return;
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let target = PathBuf::from(env::var_os("CARGO_TARGET_DIR").unwrap());
    let output_dir = target.join("embedded-artifacts").join(&language);
    fs::create_dir_all(&output_dir).unwrap();

    let (variable, fallback, extension, output_name) = match language.as_str() {
        "c" => ("CC", "clang", "c", "app"),
        "cpp" => ("CXX", "clang++", "cpp", "app"),
        "emscripten" => ("CXX", "em++", "cpp", "app.js"),
        other => panic!("unsupported EMBEDDED_LANGUAGE={other}"),
    };
    let command = compiler(variable, fallback);
    let source_root = manifest.join("native").join(&language);
    for unit in ["unit_a", "unit_b", "main"] {
        println!(
            "cargo:rerun-if-changed={}",
            source_root.join(format!("{unit}.{extension}")).display()
        );
    }
    fs::write(
        output_dir.join("compiler-command.txt"),
        command
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
            + "\n",
    )
    .unwrap();
    let mut objects = Vec::new();
    for unit in ["unit_a", "unit_b", "main"] {
        let object = output_dir.join(format!("{unit}.o"));
        run(
            &command,
            &[
                OsString::from("-O2"),
                OsString::from("-g0"),
                OsString::from("-c"),
                source_root
                    .join(format!("{unit}.{extension}"))
                    .into_os_string(),
                OsString::from("-o"),
                object.as_os_str().to_owned(),
            ],
        );
        objects.push(object.into_os_string());
    }
    let mut link_args = objects;
    link_args.push(OsString::from("-o"));
    link_args.push(output_dir.join(output_name).into_os_string());
    run(&command, &link_args);

    if language != "emscripten" {
        let output = Command::new(output_dir.join(output_name))
            .output()
            .expect("failed to run native fixture");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
    }
}
