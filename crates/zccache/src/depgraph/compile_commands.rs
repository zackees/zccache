//! Parser for `compile_commands.json` (clang compilation database).
//!
//! Supports both the `"command"` (string) and `"arguments"` (array)
//! forms as defined by the clang compilation database specification.

use zccache::core::NormalizedPath;

use serde::Deserialize;

use super::args::{parse_compile_args, split_command, ParsedArgs};

/// A raw entry from `compile_commands.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct CompileCommand {
    /// The working directory of the compilation.
    pub directory: NormalizedPath,
    /// The source file (may be relative to `directory`).
    pub file: NormalizedPath,
    /// The compile command as a single string (shell-quoted).
    pub command: Option<String>,
    /// The compile command as an argument array.
    pub arguments: Option<Vec<String>>,
    /// The output file.
    pub output: Option<NormalizedPath>,
}

impl CompileCommand {
    /// Extract the argument list, preferring `arguments` over `command`.
    /// Returns args without the compiler executable (first element).
    pub fn args_without_compiler(&self) -> Vec<String> {
        if let Some(ref args) = self.arguments {
            if args.len() > 1 {
                args[1..].to_vec()
            } else {
                Vec::new()
            }
        } else if let Some(ref cmd) = self.command {
            let parts = split_command(cmd);
            if parts.len() > 1 {
                parts[1..].to_vec()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    }

    /// Extract the compiler executable from the command.
    pub fn compiler(&self) -> Option<NormalizedPath> {
        if let Some(ref args) = self.arguments {
            args.first().map(|s| s.as_str().into())
        } else if let Some(ref cmd) = self.command {
            split_command(cmd)
                .into_iter()
                .next()
                .map(|s| s.as_str().into())
        } else {
            None
        }
    }

    /// Parse this entry into structured `ParsedArgs`.
    pub fn parse(&self) -> ParsedArgs {
        let args = self.args_without_compiler();
        let mut parsed = parse_compile_args(&args, &self.directory);
        parsed.compiler = self.compiler();

        // If parse didn't find a source file from args, use the `file` field.
        if parsed.source_file.as_os_str().is_empty() {
            if self.file.is_absolute() {
                parsed.source_file = self.file.clone();
            } else {
                parsed.source_file = self.directory.join(&self.file);
            }
        }

        parsed
    }
}

/// Parse a `compile_commands.json` string into a list of entries.
///
/// # Errors
///
/// Returns an error if the JSON is malformed.
pub fn parse_compile_commands_json(json: &str) -> Result<Vec<CompileCommand>, serde_json::Error> {
    serde_json::from_str(json)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use zccache::core::NormalizedPath;

    use super::*;

    #[test]
    fn parse_with_command_string() {
        let json = r#"[
            {
                "directory": "/home/user/project/build",
                "command": "cc -I../src -DNDEBUG -std=c17 -c ../src/foo.c -o foo.o",
                "file": "../src/foo.c"
            }
        ]"#;

        let commands = parse_compile_commands_json(json).unwrap();
        assert_eq!(commands.len(), 1);

        let parsed = commands[0].parse();
        assert_eq!(
            parsed.source_file,
            Path::new("/home/user/project/build/../src/foo.c")
        );
        assert_eq!(
            parsed.output_file.as_deref(),
            Some(Path::new("/home/user/project/build/foo.o"))
        );
        assert_eq!(
            parsed.include_search.user,
            vec![Path::new("/home/user/project/build/../src")]
        );
        assert_eq!(parsed.defines, vec!["NDEBUG"]);
        assert!(parsed.flags.contains(&"-std=c17".to_string()));
        assert_eq!(parsed.compiler, Some("cc".into()));
    }

    #[test]
    fn parse_with_arguments_array() {
        let json = r#"[
            {
                "directory": "/build",
                "arguments": ["clang++", "-std=c++17", "-I", "/include", "-c", "main.cpp", "-o", "main.o"],
                "file": "main.cpp"
            }
        ]"#;

        let commands = parse_compile_commands_json(json).unwrap();
        let parsed = commands[0].parse();
        assert_eq!(parsed.source_file, Path::new("/build/main.cpp"));
        assert_eq!(parsed.include_search.user, vec![Path::new("/include")]);
        assert!(parsed.flags.contains(&"-std=c++17".to_string()));
        assert_eq!(parsed.compiler, Some("clang++".into()));
    }

    #[test]
    fn parse_multiple_entries() {
        let json = r#"[
            {
                "directory": "/build",
                "command": "cc -c a.c",
                "file": "a.c"
            },
            {
                "directory": "/build",
                "command": "cc -c b.c",
                "file": "b.c"
            }
        ]"#;

        let commands = parse_compile_commands_json(json).unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].parse().source_file, Path::new("/build/a.c"));
        assert_eq!(commands[1].parse().source_file, Path::new("/build/b.c"));
    }

    #[test]
    fn source_file_fallback_to_file_field() {
        let json = r#"[
            {
                "directory": "/build",
                "command": "cc -c",
                "file": "src/main.c"
            }
        ]"#;

        let commands = parse_compile_commands_json(json).unwrap();
        let parsed = commands[0].parse();
        // No source in args, should fall back to file field.
        assert_eq!(parsed.source_file, Path::new("/build/src/main.c"));
    }

    #[test]
    fn absolute_file_field() {
        let json = r#"[
            {
                "directory": "/build",
                "command": "cc -c",
                "file": "/src/main.c"
            }
        ]"#;

        let commands = parse_compile_commands_json(json).unwrap();
        let parsed = commands[0].parse();
        assert_eq!(parsed.source_file, Path::new("/src/main.c"));
    }

    #[test]
    fn empty_json() {
        let commands = parse_compile_commands_json("[]").unwrap();
        assert!(commands.is_empty());
    }

    #[test]
    fn malformed_json_returns_error() {
        let result = parse_compile_commands_json("not json");
        assert!(result.is_err());
    }

    #[test]
    fn with_output_field() {
        let json = r#"[
            {
                "directory": "/build",
                "command": "cc -c foo.c -o foo.o",
                "file": "foo.c",
                "output": "foo.o"
            }
        ]"#;

        let commands = parse_compile_commands_json(json).unwrap();
        assert_eq!(commands[0].output, Some("foo.o".into()));
    }

    #[test]
    fn complex_cmake_style() {
        let json = r#"[
            {
                "directory": "/home/user/project/build",
                "command": "/usr/bin/g++ -DPROJECT_VERSION=\"1.0\" -I/home/user/project/src -I/home/user/project/include -isystem /usr/local/include/boost -std=c++20 -O2 -Wall -Wextra -fPIC -pthread -o CMakeFiles/app.dir/src/main.cpp.o -c /home/user/project/src/main.cpp",
                "file": "/home/user/project/src/main.cpp"
            }
        ]"#;

        let commands = parse_compile_commands_json(json).unwrap();
        let parsed = commands[0].parse();
        assert_eq!(
            parsed.source_file,
            Path::new("/home/user/project/src/main.cpp")
        );
        assert_eq!(parsed.include_search.user.len(), 2);
        assert_eq!(parsed.include_search.system.len(), 1);
        assert!(parsed.defines.contains(&"PROJECT_VERSION=1.0".to_string()));
        assert!(parsed.flags.contains(&"-std=c++20".to_string()));
        assert!(parsed.flags.contains(&"-O2".to_string()));
        assert!(parsed.flags.contains(&"-fPIC".to_string()));
        assert!(parsed.flags.contains(&"-pthread".to_string()));
        assert_eq!(parsed.compiler, Some(NormalizedPath::from("/usr/bin/g++")));
    }
}
