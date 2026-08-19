use std::path::Path;

use zccache_compiler::gnu_flag_takes_value;

use super::{resolve_path, UserDepFlags};

pub(super) fn parse_user_dep_flags(args: &[String], cwd: &Path) -> UserDepFlags {
    let mut result = UserDepFlags::default();
    let mut index = 0;
    while index < args.len() {
        if let Some(options) = args[index].strip_prefix("-Wp,") {
            parse_forwarded_stream(&options.split(',').collect::<Vec<_>>(), cwd, &mut result);
            index += 1;
            continue;
        }
        if xpreprocessor_arg(args, index).is_some() {
            let mut options = Vec::new();
            while let Some((option, consumed)) = xpreprocessor_arg(args, index) {
                options.push(option);
                index += consumed;
            }
            parse_forwarded_stream(&options, cwd, &mut result);
            continue;
        }
        index += parse_driver_option(
            &args[index],
            args.get(index + 1).map(String::as_str),
            cwd,
            &mut result,
        );
    }
    result
}

fn xpreprocessor_arg(args: &[String], index: usize) -> Option<(&str, usize)> {
    if args.get(index)? == "-Xpreprocessor" {
        args.get(index + 1).map(|arg| (arg.as_str(), 2))
    } else {
        args[index]
            .strip_prefix("-Xpreprocessor=")
            .map(|arg| (arg, 1))
    }
}

fn parse_forwarded_stream(options: &[&str], cwd: &Path, result: &mut UserDepFlags) {
    let mut index = 0;
    while index < options.len() {
        let option = options[index];
        let next = options.get(index + 1).copied();
        if matches!(option, "-MD" | "-MMD") {
            result.has_md = true;
            result.has_mmd = option == "-MMD";
            if let Some(path) = next {
                record_depfile_path(path, cwd, result);
                index += 2;
            } else {
                index += 1;
            }
        } else if option == "-MF" || option == "-dependency-file" {
            if let Some(path) = next {
                record_depfile_path(path, cwd, result);
                index += 2;
            } else {
                index += 1;
            }
        } else if let Some(path) = option.strip_prefix("-MF").filter(|path| !path.is_empty()) {
            record_depfile_path(path, cwd, result);
            index += 1;
        } else if forwarded_option_takes_value(option) && next.is_some() {
            index += 2;
        } else {
            index += 1;
        }
    }
}

fn parse_driver_option(
    option: &str,
    next: Option<&str>,
    cwd: &Path,
    result: &mut UserDepFlags,
) -> usize {
    if matches!(option, "-MD" | "-MMD") {
        result.has_md = true;
        result.has_mmd = option == "-MMD";
        return 1;
    }
    if option == "-MF" {
        if let Some(path) = next {
            record_depfile_path(path, cwd, result);
            return 2;
        }
        return 1;
    }
    if let Some(path) = option.strip_prefix("-MF").filter(|path| !path.is_empty()) {
        record_depfile_path(path, cwd, result);
        return 1;
    }
    if (gnu_flag_takes_value(option) || matches!(option, "-MT" | "-MQ")) && next.is_some() {
        2
    } else {
        1
    }
}

fn forwarded_option_takes_value(option: &str) -> bool {
    gnu_flag_takes_value(option)
        || matches!(
            option,
            "-D" | "-U"
                | "-I"
                | "-MT"
                | "-MQ"
                | "-include"
                | "-imacros"
                | "-isystem"
                | "-iquote"
                | "-idirafter"
                | "-iprefix"
                | "-iwithprefix"
                | "-iwithprefixbefore"
                | "-isysroot"
                | "--sysroot"
        )
}

fn record_depfile_path(path: &str, cwd: &Path, result: &mut UserDepFlags) {
    result.depfile_to_stdout = path == "-";
    result.mf_path = (path != "-").then(|| resolve_path(path, cwd));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn forwarded_md_consumes_its_output_path() {
        for forwarded in [
            args(&["-Wp,-MMD,forwarded.d", "-c", "foo.c"]),
            args(&[
                "-Xpreprocessor",
                "-MMD",
                "-Xpreprocessor",
                "forwarded.d",
                "-c",
                "foo.c",
            ]),
            args(&[
                "-Xpreprocessor=-MMD",
                "-Xpreprocessor=forwarded.d",
                "-c",
                "foo.c",
            ]),
        ] {
            let parsed = parse_user_dep_flags(&forwarded, Path::new("/p"));

            assert!(parsed.has_md);
            assert!(parsed.has_mmd);
            assert_eq!(parsed.mf_path.as_deref(), Some(Path::new("/p/forwarded.d")));
        }
    }

    #[test]
    fn driver_md_remains_valueless_and_target_operands_are_skipped() {
        let parsed = parse_user_dep_flags(
            &args(&["-MD", "-MT", "-MF", "-MF", "custom.d", "-c", "foo.c"]),
            Path::new("/p"),
        );

        assert!(parsed.has_md);
        assert!(!parsed.has_mmd);
        assert_eq!(parsed.mf_path.as_deref(), Some(Path::new("/p/custom.d")));
    }

    #[test]
    fn forwarded_target_operands_are_skipped() {
        let parsed = parse_user_dep_flags(
            &args(&["-Wp,-MMD,first.d,-MT,-MF,-MF,custom.d", "-c", "foo.c"]),
            Path::new("/p"),
        );

        assert!(parsed.has_mmd);
        assert_eq!(parsed.mf_path.as_deref(), Some(Path::new("/p/custom.d")));
    }
}
