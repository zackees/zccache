//! `exec_test_tool` — deterministic test fixture for `zccache exec` (issue #272).
//!
//! Driven entirely by argv so its behavior — and therefore the resulting cache
//! key — varies predictably from one invocation to the next. Used by the
//! `daemon_generic_exec_test` and `daemon_generic_exec_advanced_test`
//! integration suites.
//!
//! Argv contract:
//!   exec_test_tool `<exit_code>` [`<input_file>`|-] [`<output_file>`|-]
//!                  [`<bytes>`|-] [`<depfile_path>`|-] [`<tick_file>`|-]
//!                  [`<extra_dep_paths_or_->`]
//!
//! - `exit_code`  : i32 returned by `exit()`
//! - `input_file` : if not `-`, read up to 4 KiB and echo to stdout after the
//!   `ETT-OUT\n` marker
//! - `output_file`: if not `-`, write `bytes` (or `OUT:<exit_code>` when
//!   `bytes` is `-`) to this path
//! - `bytes`      : payload for `output_file`
//! - `depfile_path`: if not `-`, emit a make-style depfile listing
//!   `<output_file>`-or-`depfile-target` as the target and `input_file` plus
//!   any `extra_dep_paths` (semicolon-separated) as deps. Path B tests use
//!   this to exercise depfile harvesting end-to-end.
//! - `tick_file`  : if not `-`, append `"tick\n"` to this path BEFORE the
//!   exit. Lets the concurrent-coalescing test count actual tool spawns.
//! - `extra_dep_paths`: if not `-`, `;`-separated list of additional paths
//!   to write into the depfile.
//!
//! Always prints:
//!   - "ETT-OUT\n" to stdout (plus the echoed input content)
//!   - "ETT-ERR <exit_code>\n" to stderr

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let exit_code: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

    let input_file = args.get(2).map(String::as_str).unwrap_or("-");
    let output_file = args.get(3).map(String::as_str).unwrap_or("-");
    let output_bytes = args.get(4).map(String::as_str).unwrap_or("-");
    let depfile_path = args.get(5).map(String::as_str).unwrap_or("-");
    let tick_file = args.get(6).map(String::as_str).unwrap_or("-");
    let extra_deps = args.get(7).map(String::as_str).unwrap_or("-");

    if tick_file != "-" {
        let _ = append(tick_file, b"tick\n");
    }

    let mut stdout_buf = Vec::<u8>::from(&b"ETT-OUT\n"[..]);

    if input_file != "-" {
        match std::fs::read(input_file) {
            Ok(mut bytes) => {
                bytes.truncate(4096);
                stdout_buf.extend_from_slice(&bytes);
            }
            Err(e) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "ETT-ERR: cannot read input {input_file}: {e}"
                );
                std::process::exit(2);
            }
        }
    }

    if output_file != "-" {
        let payload: Vec<u8> = if output_bytes != "-" {
            output_bytes.as_bytes().to_vec()
        } else {
            format!("OUT:{exit_code}").into_bytes()
        };
        if let Err(e) = std::fs::write(output_file, &payload) {
            let _ = writeln!(
                std::io::stderr(),
                "ETT-ERR: cannot write output {output_file}: {e}"
            );
            std::process::exit(2);
        }
    }

    if depfile_path != "-" && exit_code == 0 {
        // make-style depfile:
        //   <target>: <dep1> <dep2> \
        //     <dep3> ...
        let target = if output_file != "-" {
            output_file.to_string()
        } else {
            "depfile-target".to_string()
        };
        let mut deps: Vec<String> = Vec::new();
        if input_file != "-" {
            deps.push(input_file.to_string());
        }
        if extra_deps != "-" {
            for p in extra_deps.split(';') {
                let trimmed = p.trim();
                if !trimmed.is_empty() {
                    deps.push(trimmed.to_string());
                }
            }
        }
        // Escape spaces in paths per make conventions (backslash + space).
        let escape = |s: &str| s.replace(' ', "\\ ");
        let dep_str = deps.iter().map(|d| escape(d)).collect::<Vec<_>>().join(" ");
        let content = format!("{target}: {dep_str}\n", target = escape(&target));
        if let Err(e) = std::fs::write(depfile_path, content) {
            let _ = writeln!(
                std::io::stderr(),
                "ETT-ERR: cannot write depfile {depfile_path}: {e}"
            );
            std::process::exit(2);
        }
    }

    let _ = std::io::stdout().write_all(&stdout_buf);
    let _ = std::io::stdout().flush();
    let _ = writeln!(std::io::stderr(), "ETT-ERR {exit_code}");

    std::process::exit(exit_code);
}

fn append(path: &str, data: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(data)
}
