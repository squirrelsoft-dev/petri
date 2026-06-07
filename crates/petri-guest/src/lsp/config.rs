//! Runtime LSP configuration.
//!
//! The guest is told which language servers exist in the image via a small TOML
//! document (mounted read-only alongside the boot policy). The same `[lsp]`
//! shape is authored in the image build config; the build tooling copies the
//! relevant fields into the image. Fields that only matter at build time (such
//! as `install`) are ignored here.

use std::io::Read;

use serde::Deserialize;

/// Parsed `[lsp]` configuration handed to the guest at boot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LspConfig {
    pub enabled: bool,
    pub servers: Vec<LspServerConfig>,
}

/// A single language server entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspServerConfig {
    /// Coarse language key, matched against [`crate::lsp::language`] detection
    /// (e.g. `rust`, `typescript`, `python`, `go`, `c`, `cpp`).
    pub language: String,
    /// Executable name of the language server binary.
    pub binary: String,
    /// Extra arguments passed to the server on launch.
    pub args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LspDocument {
    lsp: RawLsp,
}

#[derive(Debug, Deserialize)]
struct RawLsp {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    servers: Vec<RawServer>,
}

#[derive(Debug, Deserialize)]
struct RawServer {
    language: String,
    binary: String,
    #[serde(default)]
    args: Vec<String>,
    // `install` and any other build-only keys are intentionally ignored.
    #[serde(default)]
    #[allow(dead_code)]
    install: Option<String>,
}

impl LspConfig {
    /// A configuration with LSP support disabled. Every `lsp_*` request against
    /// this config degrades gracefully to "not available".
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            servers: Vec::new(),
        }
    }

    pub fn load(mut reader: impl Read) -> Result<Self, LspConfigError> {
        let mut input = String::new();
        reader.read_to_string(&mut input)?;
        Self::from_toml_str(&input)
    }

    pub fn from_toml_str(input: &str) -> Result<Self, LspConfigError> {
        let document: LspDocument = toml::from_str(input)?;
        Self::validate(document.lsp)
    }

    /// The configured server for a coarse language key, if any. Returns `None`
    /// when LSP is disabled or no server is configured for the language.
    pub fn server_for_language(&self, language: &str) -> Option<&LspServerConfig> {
        if !self.enabled {
            return None;
        }
        self.servers
            .iter()
            .find(|server| server.language == language)
    }

    fn validate(raw: RawLsp) -> Result<Self, LspConfigError> {
        let mut servers = Vec::with_capacity(raw.servers.len());
        for server in raw.servers {
            if server.language.trim().is_empty() {
                return Err(LspConfigError::Invalid(
                    "lsp server language must be non-empty".to_string(),
                ));
            }
            if !is_executable_name(&server.binary) {
                return Err(LspConfigError::Invalid(format!(
                    "lsp server binary '{}' must be an executable name",
                    server.binary
                )));
            }
            if servers
                .iter()
                .any(|existing: &LspServerConfig| existing.language == server.language)
            {
                return Err(LspConfigError::Invalid(format!(
                    "duplicate lsp server for language '{}'",
                    server.language
                )));
            }
            servers.push(LspServerConfig {
                language: server.language,
                binary: server.binary,
                args: server.args,
            });
        }

        Ok(Self {
            enabled: raw.enabled,
            servers,
        })
    }
}

fn is_executable_name(binary: &str) -> bool {
    !binary.is_empty()
        && !binary
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '/' | '\\' | '|' | '&' | ';' | '<' | '>'))
}

#[derive(Debug)]
pub enum LspConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Invalid(String),
}

impl std::fmt::Display for LspConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "failed to read lsp config: {err}"),
            Self::Parse(err) => write!(f, "failed to parse lsp config: {err}"),
            Self::Invalid(message) => write!(f, "invalid lsp config: {message}"),
        }
    }
}

impl std::error::Error for LspConfigError {}

impl From<std::io::Error> for LspConfigError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<toml::de::Error> for LspConfigError {
    fn from(err: toml::de::Error) -> Self {
        Self::Parse(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
[lsp]
enabled = true

[[lsp.servers]]
language = "rust"
binary = "rust-analyzer"
install = "rustup component add rust-analyzer"

[[lsp.servers]]
language = "typescript"
binary = "typescript-language-server"
args = ["--stdio"]
install = "npm install -g typescript-language-server typescript"
"#;

    #[test]
    fn parses_servers_and_ignores_install() {
        let config = LspConfig::from_toml_str(CONFIG).unwrap();

        assert!(config.enabled);
        assert_eq!(config.servers.len(), 2);
        let rust = config.server_for_language("rust").unwrap();
        assert_eq!(rust.binary, "rust-analyzer");
        assert!(rust.args.is_empty());
        let ts = config.server_for_language("typescript").unwrap();
        assert_eq!(ts.args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn disabled_config_has_no_servers() {
        let config = LspConfig::disabled();
        assert!(!config.enabled);
        assert!(config.server_for_language("rust").is_none());
    }

    #[test]
    fn enabled_false_hides_servers() {
        let input = CONFIG.replace("enabled = true", "enabled = false");
        let config = LspConfig::from_toml_str(&input).unwrap();
        assert!(config.server_for_language("rust").is_none());
    }

    #[test]
    fn rejects_shell_snippet_binary() {
        let input = CONFIG.replace("\"rust-analyzer\"", "\"rust-analyzer; rm -rf\"");
        let err = LspConfig::from_toml_str(&input).unwrap_err().to_string();
        assert!(err.contains("must be an executable name"));
    }

    #[test]
    fn parses_build_emitted_runtime_config() {
        // Mirrors the `lsp_py runtime` output baked into the image at
        // /etc/petri/lsp.toml (binary + args only, no install/apt keys).
        let runtime = r#"[lsp]
enabled = true

[[lsp.servers]]
language = "rust"
binary = "rust-analyzer"

[[lsp.servers]]
language = "typescript"
binary = "typescript-language-server"
args = ["--stdio"]

[[lsp.servers]]
language = "c"
binary = "clangd"

[[lsp.servers]]
language = "cpp"
binary = "clangd"
"#;
        let config = LspConfig::from_toml_str(runtime).unwrap();
        assert!(config.enabled);
        assert_eq!(
            config.server_for_language("rust").unwrap().binary,
            "rust-analyzer"
        );
        assert_eq!(
            config.server_for_language("typescript").unwrap().args,
            vec!["--stdio".to_string()]
        );
        // c and cpp are distinct languages sharing the clangd binary.
        assert_eq!(config.server_for_language("c").unwrap().binary, "clangd");
        assert_eq!(config.server_for_language("cpp").unwrap().binary, "clangd");
    }

    #[test]
    fn rejects_duplicate_language() {
        let input = CONFIG.replace("\"typescript\"", "\"rust\"");
        let err = LspConfig::from_toml_str(&input).unwrap_err().to_string();
        assert!(err.contains("duplicate lsp server"));
    }
}
