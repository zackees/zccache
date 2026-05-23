//! Link arg normalization, LinkSearchAnalysis, -L/-l resolution.

use super::*;

pub(super) fn normalize_link_path_value_for_key(value: &str, key_root: Option<&Path>) -> String {
    let Some(root) = key_root else {
        return value.to_string();
    };

    normalize_request_path_value(value, Some(root)).unwrap_or_else(|| value.to_string())
}

pub(super) fn normalize_link_flag_atom_for_key(atom: &str, key_root: Option<&Path>) -> String {
    let Some(root) = key_root else {
        return atom.to_string();
    };

    if let Some(normalized) = normalize_cc_prefix_map_arg_for_key(atom, Some(root)) {
        return normalized;
    }

    if let Some(rest) = atom.strip_prefix("-L").filter(|rest| !rest.is_empty()) {
        if let Some(normalized) = normalize_request_path_value(rest, Some(root)) {
            return format!("-L{normalized}");
        }
    }

    for prefix in [
        "--library-path=",
        "--version-script=",
        "--script=",
        "--sysroot=",
    ] {
        if let Some(rest) = atom.strip_prefix(prefix) {
            if let Some(normalized) = normalize_request_path_value(rest, Some(root)) {
                return format!("{prefix}{normalized}");
            }
        }
    }

    for prefix in ["/LIBPATH:", "/DEF:"] {
        if atom
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            let rest = &atom[prefix.len()..];
            if let Some(normalized) = normalize_request_path_value(rest, Some(root)) {
                return format!("{}{normalized}", &atom[..prefix.len()]);
            }
        }
    }

    if let Some((left, right)) = atom.split_once('=') {
        if let Some(normalized_right) = normalize_request_path_value(right, Some(root)) {
            return format!("{left}={normalized_right}");
        }
    }

    atom.to_string()
}

pub(super) fn normalize_wl_flag_for_key(flag: &str, key_root: Option<&Path>) -> String {
    let mut parts: Vec<String> = flag.split(',').map(|part| part.to_string()).collect();

    let mut i = 1;
    while i < parts.len() {
        let normalize = matches!(
            parts[i].as_str(),
            "-L" | "-T" | "--script" | "--version-script" | "--library-path" | "--sysroot"
        );
        if normalize && i + 1 < parts.len() {
            parts[i + 1] = normalize_link_path_value_for_key(&parts[i + 1], key_root);
            i += 2;
            continue;
        }
        parts[i] = normalize_link_flag_atom_for_key(&parts[i], key_root);
        i += 1;
    }

    parts.join(",")
}

pub(super) fn normalize_link_cache_flag_for_key(flag: &str, key_root: Option<&Path>) -> String {
    if flag.starts_with("-Wl,") {
        normalize_wl_flag_for_key(flag, key_root)
    } else {
        normalize_link_flag_atom_for_key(flag, key_root)
    }
}

pub(super) fn normalize_link_cache_flags_for_key(
    flags: &[String],
    key_root: Option<&Path>,
) -> Vec<String> {
    let mut normalized = Vec::with_capacity(flags.len());
    let mut previous_path_flag = false;

    for flag in flags {
        if previous_path_flag {
            normalized.push(normalize_link_path_value_for_key(flag, key_root));
            previous_path_flag = false;
            continue;
        }

        normalized.push(normalize_link_cache_flag_for_key(flag, key_root));
        previous_path_flag = matches!(
            flag.as_str(),
            "-L" | "-T" | "--script" | "--version-script" | "--library-path" | "-isysroot"
        ) || flag.eq_ignore_ascii_case("/DEF");
    }

    normalized
}

#[derive(Debug, Default)]
pub(super) struct LinkSearchAnalysis {
    pub(super) search_dirs: Vec<NormalizedPath>,
    pub(super) lib_names: Vec<String>,
}

#[derive(Debug, Default)]
pub(super) struct LinkPathRemapKeyPlan {
    pub(super) flags: Vec<String>,
    pub(super) extra_input_files: Vec<NormalizedPath>,
    pub(super) root_specific: bool,
}

pub(super) fn link_path_to_absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

pub(super) fn path_is_under_root(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).is_ok()
}

pub(super) fn push_link_search_dir(analysis: &mut LinkSearchAnalysis, value: &str) {
    analysis.search_dirs.push(NormalizedPath::new(value));
}

pub(super) fn push_link_lib_name(analysis: &mut LinkSearchAnalysis, value: &str) {
    if !value.is_empty() {
        analysis.lib_names.push(value.to_string());
    }
}

pub(super) fn analyze_link_search_flags(flags: &[String]) -> LinkSearchAnalysis {
    let mut analysis = LinkSearchAnalysis::default();
    let mut previous_search_dir_flag = false;

    for flag in flags {
        if previous_search_dir_flag {
            push_link_search_dir(&mut analysis, flag);
            previous_search_dir_flag = false;
            continue;
        }

        match flag.as_str() {
            "-L" | "--library-path" => {
                previous_search_dir_flag = true;
                continue;
            }
            _ => {}
        }

        if let Some(rest) = flag.strip_prefix("-L").filter(|rest| !rest.is_empty()) {
            push_link_search_dir(&mut analysis, rest);
            continue;
        }
        if let Some(rest) = flag.strip_prefix("--library-path=") {
            push_link_search_dir(&mut analysis, rest);
            continue;
        }
        if let Some(rest) = flag.strip_prefix("-l").filter(|rest| !rest.is_empty()) {
            push_link_lib_name(&mut analysis, rest);
            continue;
        }

        if let Some(rest) = flag.strip_prefix("-Wl,") {
            let parts: Vec<&str> = rest.split(',').collect();
            let mut i = 0;
            while i < parts.len() {
                match parts[i] {
                    "-L" | "--library-path" => {
                        if i + 1 < parts.len() {
                            push_link_search_dir(&mut analysis, parts[i + 1]);
                        }
                        i += 2;
                        continue;
                    }
                    "-l" => {
                        if i + 1 < parts.len() {
                            push_link_lib_name(&mut analysis, parts[i + 1]);
                        }
                        i += 2;
                        continue;
                    }
                    part => {
                        if let Some(rest) = part.strip_prefix("-L").filter(|s| !s.is_empty()) {
                            push_link_search_dir(&mut analysis, rest);
                        } else if let Some(rest) = part
                            .strip_prefix("--library-path=")
                            .filter(|s| !s.is_empty())
                        {
                            push_link_search_dir(&mut analysis, rest);
                        } else if let Some(rest) = part.strip_prefix("-l").filter(|s| !s.is_empty())
                        {
                            push_link_lib_name(&mut analysis, rest);
                        }
                    }
                }
                i += 1;
            }
        }
    }

    analysis
}

pub(super) fn link_library_candidate_names(lib: &str) -> Vec<String> {
    if let Some(exact) = lib.strip_prefix(':') {
        return vec![exact.to_string()];
    }

    vec![
        format!("lib{lib}.a"),
        format!("lib{lib}.so"),
        format!("lib{lib}.dylib"),
        format!("{lib}.lib"),
        format!("lib{lib}.dll.a"),
        format!("{lib}.dll.a"),
    ]
}

pub(super) fn resolve_link_library(
    lib: &str,
    search_dirs: &[NormalizedPath],
    cwd: &Path,
) -> Option<NormalizedPath> {
    let candidate_names = link_library_candidate_names(lib);
    for dir in search_dirs {
        let abs_dir = link_path_to_absolute(dir.as_path(), cwd);
        for name in &candidate_names {
            let candidate = abs_dir.join(name);
            if candidate.is_file() {
                return Some(candidate.into());
            }
        }
    }
    None
}

pub(super) fn build_link_path_remap_key_plan(
    flags: &[String],
    cwd: &Path,
    key_root: Option<&Path>,
) -> LinkPathRemapKeyPlan {
    let analysis = analyze_link_search_flags(flags);
    let normalized_flags = normalize_link_cache_flags_for_key(flags, key_root);
    let Some(root) = key_root else {
        return LinkPathRemapKeyPlan {
            flags: normalized_flags,
            ..Default::default()
        };
    };

    let root_local_search = analysis.search_dirs.iter().any(|dir| {
        let abs_dir = link_path_to_absolute(dir.as_path(), cwd);
        path_is_under_root(&abs_dir, root)
    });
    let mut extra_input_files = Vec::new();
    let mut root_specific = false;

    if root_local_search && analysis.lib_names.is_empty() {
        root_specific = true;
    }

    for lib in &analysis.lib_names {
        match resolve_link_library(lib, &analysis.search_dirs, cwd) {
            Some(path) => {
                let abs_path = link_path_to_absolute(path.as_path(), cwd);
                if path_is_under_root(&abs_path, root) {
                    extra_input_files.push(abs_path.into());
                } else if root_local_search {
                    root_specific = true;
                }
            }
            None if root_local_search => {
                root_specific = true;
            }
            None => {}
        }
    }

    extra_input_files.sort();
    extra_input_files.dedup();

    LinkPathRemapKeyPlan {
        flags: normalized_flags,
        extra_input_files,
        root_specific,
    }
}
