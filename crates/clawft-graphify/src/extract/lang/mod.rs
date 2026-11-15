//! Language identification and extension dispatch.
//!
//! GRAPH-005: Maps file extensions to language IDs.

pub mod c;
pub(crate) mod common;
pub mod cpp;
pub mod csharp;
pub mod go;
pub mod java;
pub mod javascript;
pub mod python;
pub mod ruby;
pub mod rust_lang;

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Supported programming languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LanguageId {
    Python,
    JavaScript,
    TypeScript,
    Rust,
    Go,
    Java,
    C,
    Cpp,
    Ruby,
    CSharp,
    Kotlin,
    Scala,
    Php,
    Lua,
    Swift,
}

impl LanguageId {
    /// Human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Python => "Python",
            Self::JavaScript => "JavaScript",
            Self::TypeScript => "TypeScript",
            Self::Rust => "Rust",
            Self::Go => "Go",
            Self::Java => "Java",
            Self::C => "C",
            Self::Cpp => "C++",
            Self::Ruby => "Ruby",
            Self::CSharp => "C#",
            Self::Kotlin => "Kotlin",
            Self::Scala => "Scala",
            Self::Php => "PHP",
            Self::Lua => "Lua",
            Self::Swift => "Swift",
        }
    }
}

/// Map a file extension (without dot) to a LanguageId.
pub fn language_for_ext(ext: &str) -> Option<LanguageId> {
    match ext {
        "py" => Some(LanguageId::Python),
        "js" => Some(LanguageId::JavaScript),
        "ts" | "tsx" => Some(LanguageId::TypeScript),
        "rs" => Some(LanguageId::Rust),
        "go" => Some(LanguageId::Go),
        "java" => Some(LanguageId::Java),
        "c" | "h" => Some(LanguageId::C),
        "cpp" | "cc" | "cxx" | "hpp" => Some(LanguageId::Cpp),
        "rb" => Some(LanguageId::Ruby),
        "cs" => Some(LanguageId::CSharp),
        "kt" | "kts" => Some(LanguageId::Kotlin),
        "scala" => Some(LanguageId::Scala),
        "php" => Some(LanguageId::Php),
        "lua" | "toc" => Some(LanguageId::Lua),
        "swift" => Some(LanguageId::Swift),
        _ => None,
    }
}

/// Map a file path to a LanguageId based on its extension.
pub fn language_for_extension(path: &Path) -> Option<LanguageId> {
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(language_for_ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_mapping() {
        assert_eq!(language_for_ext("py"), Some(LanguageId::Python));
        assert_eq!(language_for_ext("tsx"), Some(LanguageId::TypeScript));
        assert_eq!(language_for_ext("cc"), Some(LanguageId::Cpp));
        assert_eq!(language_for_ext("kts"), Some(LanguageId::Kotlin));
        assert_eq!(language_for_ext("toc"), Some(LanguageId::Lua));
        assert_eq!(language_for_ext("xyz"), None);
    }

    #[test]
    fn path_mapping() {
        assert_eq!(
            language_for_extension(Path::new("foo/bar.py")),
            Some(LanguageId::Python)
        );
        assert_eq!(
            language_for_extension(Path::new("main.rs")),
            Some(LanguageId::Rust)
        );
        assert_eq!(language_for_extension(Path::new("Makefile")), None);
    }
}
