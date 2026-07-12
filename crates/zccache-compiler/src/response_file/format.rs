//! Compiler-specific response-file argument formatting.

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn gnu(args: &[String]) -> String {
    let mut content = String::with_capacity(args.iter().map(|arg| arg.len() + 3).sum());
    for arg in args {
        #[expect(clippy::expect_used, reason = "test helper input is representable")]
        let formatted = format_gnu_argument(arg).expect("argument should be representable");
        content.push_str(&formatted);
        content.push('\n');
    }
    content
}

pub(super) fn gnu_if_safe(args: &[String]) -> Option<String> {
    let mut content = String::with_capacity(args.iter().map(|arg| arg.len() + 3).sum());
    for arg in args {
        content.push_str(&format_gnu_argument(arg)?);
        content.push('\n');
    }
    Some(content)
}

pub(super) fn msvc_if_safe(args: &[String]) -> Option<String> {
    let mut content = String::with_capacity(args.iter().map(|arg| arg.len() + 3).sum());
    for arg in args {
        content.push_str(&format_msvc_argument(arg)?);
        content.push('\n');
    }
    Some(content)
}

fn format_msvc_argument(arg: &str) -> Option<String> {
    if arg.contains('\n') || arg.contains('\r') {
        return None;
    }
    if arg.is_empty() {
        return Some("\"\"".to_string());
    }
    if !arg.contains(char::is_whitespace) && !arg.contains('"') && !arg.starts_with('@') {
        return Some(arg.to_string());
    }

    let mut result = String::with_capacity(arg.len() + 2);
    result.push('"');
    let mut backslashes = 0;
    for character in arg.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                result.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                result.push('"');
                backslashes = 0;
            }
            _ => {
                result.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                result.push(character);
            }
        }
    }
    result.extend(std::iter::repeat_n('\\', backslashes * 2));
    result.push('"');
    Some(result)
}

fn format_gnu_argument(arg: &str) -> Option<String> {
    if arg.is_empty() {
        return Some("''".to_string());
    }
    if arg.contains('\n') || arg.contains('\r') {
        return None;
    }
    if arg.contains('\\') {
        let escaped = arg.replace('\\', "\\\\").replace('"', "\\\"");
        return Some(format!("\"{escaped}\""));
    }
    let needs_quoting = arg.contains(char::is_whitespace)
        || arg.contains('"')
        || arg.contains('\'')
        || arg.starts_with('@');
    if !needs_quoting {
        return Some(arg.to_string());
    }
    if !arg.contains('\'') {
        return Some(format!("'{arg}'"));
    }
    Some(format!("\"{}\"", arg.replace('"', "\\\"")))
}

pub(super) fn rustc_if_safe(args: &[String]) -> Option<String> {
    let mut content = String::with_capacity(args.iter().map(|arg| arg.len() + 1).sum());
    for arg in args {
        if arg.contains('\n') || arg.contains('\r') {
            return None;
        }
        content.push_str(arg);
        content.push('\n');
    }
    Some(content)
}
