//! File-extension to language detection.
//!
//! Maps a workspace file path to the coarse language key used to select a
//! configured server (see [`crate::lsp::config`]) and the precise LSP
//! `languageId` sent in `textDocument/didOpen`.

use std::path::Path;

/// A detected language for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectedLanguage {
    /// Coarse key matched against configured servers (e.g. `typescript`).
    pub config_language: &'static str,
    /// Precise LSP `languageId` (e.g. `typescriptreact`).
    pub language_id: &'static str,
}

/// Detect the language for a file by extension, or `None` if unrecognized.
pub fn detect(path: &Path) -> Option<DetectedLanguage> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let (config_language, language_id) = match extension.as_str() {
        "rs" => ("rust", "rust"),
        "ts" => ("typescript", "typescript"),
        "tsx" => ("typescript", "typescriptreact"),
        "js" | "mjs" | "cjs" => ("typescript", "javascript"),
        "jsx" => ("typescript", "javascriptreact"),
        "py" | "pyi" => ("python", "python"),
        "go" => ("go", "go"),
        "c" | "h" => ("c", "c"),
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" => ("cpp", "cpp"),
        _ => return None,
    };
    Some(DetectedLanguage {
        config_language,
        language_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn detect_path(path: &str) -> Option<DetectedLanguage> {
        detect(&PathBuf::from(path))
    }

    #[test]
    fn detects_rust() {
        let lang = detect_path("/workspace/src/main.rs").unwrap();
        assert_eq!(lang.config_language, "rust");
        assert_eq!(lang.language_id, "rust");
    }

    #[test]
    fn maps_tsx_to_typescript_server_with_react_id() {
        let lang = detect_path("/workspace/app/page.tsx").unwrap();
        assert_eq!(lang.config_language, "typescript");
        assert_eq!(lang.language_id, "typescriptreact");
    }

    #[test]
    fn maps_cpp_family() {
        assert_eq!(detect_path("/w/a.cpp").unwrap().config_language, "cpp");
        assert_eq!(detect_path("/w/a.hpp").unwrap().config_language, "cpp");
        assert_eq!(detect_path("/w/a.c").unwrap().config_language, "c");
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(detect_path("/w/MAIN.RS").unwrap().config_language, "rust");
    }

    #[test]
    fn unknown_extension_is_none() {
        assert!(detect_path("/workspace/readme.md").is_none());
        assert!(detect_path("/workspace/Makefile").is_none());
    }
}
