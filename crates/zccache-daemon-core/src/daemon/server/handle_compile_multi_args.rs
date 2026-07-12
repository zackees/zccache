//! Source-token filtering for partial-hit multi-file invocations.

use std::collections::HashSet;

fn is_separated_msvc_source_flag(argument: &str) -> bool {
    matches!(argument, "/Tc" | "-Tc" | "/Tp" | "-Tp")
}

pub(super) fn filter_multi_source_args(
    original_args: &[String],
    source_indices: &[usize],
    retained_source_indices: &HashSet<usize>,
) -> Vec<String> {
    let mut removed: HashSet<usize> = source_indices
        .iter()
        .copied()
        .filter(|index| !retained_source_indices.contains(index))
        .collect();
    for source_index in source_indices {
        if removed.contains(source_index)
            && *source_index > 0
            && is_separated_msvc_source_flag(&original_args[*source_index - 1])
        {
            removed.insert(*source_index - 1);
        }
    }
    let filtered: Vec<String> = original_args
        .iter()
        .enumerate()
        .filter(|(index, _)| !removed.contains(index))
        .map(|(_, argument)| argument.clone())
        .collect();

    let mut normalized = Vec::with_capacity(filtered.len());
    let mut index = 0;
    while index < filtered.len() {
        let argument = &filtered[index];
        if is_separated_msvc_source_flag(argument) {
            if let Some(source) = filtered.get(index + 1) {
                normalized.push(format!("{argument}{source}"));
                index += 2;
                continue;
            }
        }
        normalized.push(argument.clone());
        index += 1;
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn filtering_separated_msvc_source_removes_language_token() {
        let original = args(&["/c", "/Tc", "first", "/Tp", "second", "/O2"]);
        let filtered = filter_multi_source_args(&original, &[2, 4], &HashSet::from([4]));
        assert_eq!(filtered, args(&["/c", "/Tpsecond", "/O2"]));
    }

    #[test]
    fn filtering_concatenated_msvc_source_removes_single_token() {
        let original = args(&["/c", "/Tcfirst", "/Tpsecond", "/O2"]);
        let filtered = filter_multi_source_args(&original, &[1, 2], &HashSet::from([2]));
        assert_eq!(filtered, args(&["/c", "/Tpsecond", "/O2"]));
    }
}
