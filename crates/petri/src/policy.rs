//! Policy templates: a small named registry of reusable boot policies.
//!
//! A *template* is just a [policy-config](../../../docs/policy-config.md) TOML
//! document stored under a stable name. Templates come in two flavours:
//!
//! - **Built-in** templates ([`BUILTINS`]) are compiled into the binary, always
//!   available, and never editable or deletable in place. `edit` forks a copy
//!   into the user registry; `remove` only ever removes a user override.
//! - **User** templates live as `<name>.toml` files under [`policies_root`]
//!   (`~/.petri/policies` by default, overridable via `PETRI_POLICIES_DIR`). A
//!   user template whose name matches a built-in *shadows* it.
//!
//! Anywhere a `--policy` argument is accepted, [`resolve_reference`] lets the
//! caller pass a template name in place of a file path: an existing file always
//! wins, otherwise a bare name is resolved through the registry (user override
//! first, then built-in).

use std::path::{Path, PathBuf};

use petri_protocol::policy::Policy;

use crate::error::{PetriError, Result};

/// A built-in policy template baked into the binary.
pub struct Builtin {
    pub name: &'static str,
    pub description: &'static str,
    pub toml: &'static str,
}

/// The shipped default templates. Selected via the design discussion: a
/// maximally-locked-down posture, an everyday build/test posture, an
/// unrestricted posture, and a dependency-fetch posture.
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "locked-down",
        description: "No network, read-only inspection commands, no escalation room.",
        toml: include_str!("policy_templates/locked-down.toml"),
    },
    Builtin {
        name: "developer",
        description: "No network; boots read-only, escalates to edit with common build tools.",
        toml: include_str!("policy_templates/developer.toml"),
    },
    Builtin {
        name: "yolo",
        description: "Full network egress and unrestricted command execution. Trusted use only.",
        toml: include_str!("policy_templates/yolo.toml"),
    },
    Builtin {
        name: "fetch",
        description: "Network on with curated fetch commands (git, curl, wget) and tight caps.",
        toml: include_str!("policy_templates/fetch.toml"),
    },
];

/// Look up a built-in template by name.
pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

// --- on-disk layout ---------------------------------------------------------

/// Root directory holding user policy templates. Mirrors the other registries
/// under `~/.petri`. Overridable via `PETRI_POLICIES_DIR` (primarily for tests).
pub fn policies_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("PETRI_POLICIES_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".petri").join("policies"))
        .unwrap_or_else(|| std::env::temp_dir().join("petri").join("policies"))
}

/// Path of a user template file under the registry root.
fn user_path(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{name}.toml"))
}

/// Where a built-in is materialised on disk so a file-path consumer (the
/// backend) can read it. Hidden so it never shows up in `list`.
fn cache_path(root: &Path, name: &str) -> PathBuf {
    root.join(".cache").join(format!("{name}.toml"))
}

// --- name + content validation ----------------------------------------------

/// Template names are bare slugs: lowercase alphanumerics, `-`, and `_`,
/// starting with an alphanumeric. This keeps them unambiguous against file
/// paths in [`resolve_reference`] and safe as filename components.
fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !valid {
        return Err(PetriError::invalid_argument(format!(
            "invalid policy template name '{name}': use lowercase letters, digits, '-' and '_' (e.g. 'my-ci')"
        )));
    }
    Ok(())
}

/// Reject content that does not parse as a valid policy before we persist it,
/// so the registry never holds a template that would fail at boot.
fn validate_content(name: &str, toml: &str) -> Result<()> {
    Policy::from_toml_str(toml).map_err(|err| {
        PetriError::invalid_argument(format!("policy template '{name}' is invalid: {err}"))
    })?;
    Ok(())
}

#[cfg(unix)]
fn harden(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        PetriError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn harden(_path: &Path) -> Result<()> {
    Ok(())
}

fn write_template(path: &Path, toml: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| PetriError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, toml).map_err(|source| PetriError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    harden(path)
}

fn read_to_string(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|source| PetriError::Io {
        path: path.to_path_buf(),
        source,
    })
}

// --- registry queries -------------------------------------------------------

/// The materialised body of a named template: a user override if present,
/// otherwise the built-in. `None` if neither exists.
fn body(root: &Path, name: &str) -> Result<Option<(String, Source)>> {
    let user = user_path(root, name);
    if user.is_file() {
        return Ok(Some((read_to_string(&user)?, Source::user_for(name))));
    }
    if let Some(b) = builtin(name) {
        return Ok(Some((b.toml.to_string(), Source::Builtin)));
    }
    Ok(None)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Builtin,
    User,
    /// A user template that shadows a built-in of the same name.
    Override,
}

impl Source {
    fn user_for(name: &str) -> Self {
        if builtin(name).is_some() {
            Source::Override
        } else {
            Source::User
        }
    }

    fn label(self) -> &'static str {
        match self {
            Source::Builtin => "builtin",
            Source::User => "user",
            Source::Override => "override",
        }
    }
}

/// Enumerate user template names (without `.toml`), skipping the hidden cache.
fn user_template_names(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(names),
        Err(source) => {
            return Err(PetriError::Io {
                path: root.to_path_buf(),
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| PetriError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(".toml").filter(|s| !s.is_empty()) {
            names.push(stem.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// Short summary of a policy's network and command posture, for `list`.
fn posture(toml: &str) -> (String, String) {
    let Ok(policy) = Policy::from_toml_str(toml) else {
        return ("?".to_string(), "?".to_string());
    };
    let network = if !policy.network_enabled {
        "off".to_string()
    } else {
        level_span(policy.network.default.as_str(), policy.network.max.as_str())
    };
    let command = level_span(policy.command.default.as_str(), policy.command.max.as_str());
    (network, command)
}

fn level_span(default: &str, max: &str) -> String {
    if default == max {
        default.to_string()
    } else {
        format!("{default}->{max}")
    }
}

// --- subcommands ------------------------------------------------------------

/// `petri policy list`: every available template, built-ins and user templates
/// merged, with the active source and a posture summary for each.
pub fn list(root: &Path) -> Result<String> {
    let user_names = user_template_names(root)?;

    // Union of built-in names and user names, built-ins first then extra user
    // templates, each name appearing once.
    let mut names: Vec<String> = BUILTINS.iter().map(|b| b.name.to_string()).collect();
    for name in &user_names {
        if !names.iter().any(|n| n == name) {
            names.push(name.clone());
        }
    }

    let mut lines = vec![format!(
        "{:<16}{:<10}{:<10}{}",
        "NAME", "SOURCE", "NETWORK", "COMMAND"
    )];
    for name in &names {
        let user = user_path(root, name);
        let (toml, source) = if user.is_file() {
            (read_to_string(&user)?, Source::user_for(name))
        } else if let Some(b) = builtin(name) {
            (b.toml.to_string(), Source::Builtin)
        } else {
            continue;
        };
        let (network, command) = posture(&toml);
        lines.push(format!(
            "{:<16}{:<10}{:<10}{}",
            name,
            source.label(),
            network,
            command
        ));
    }
    Ok(lines.join("\n"))
}

/// `petri policy show <name>`: print the resolved TOML verbatim (pipeable).
pub fn show(root: &Path, name: &str) -> Result<String> {
    match body(root, name)? {
        Some((toml, _)) => Ok(toml.trim_end().to_string()),
        None => Err(unknown_template(name)),
    }
}

/// `petri policy path <name>`: print the on-disk path of a resolved template,
/// materialising a built-in into the cache if needed. Handy for scripting:
/// `petri sandbox create … --policy "$(petri policy path developer)"`.
pub fn path(root: &Path, name: &str) -> Result<String> {
    let user = user_path(root, name);
    if user.is_file() {
        return Ok(user.display().to_string());
    }
    if let Some(b) = builtin(name) {
        let cached = cache_path(root, name);
        write_template(&cached, b.toml)?;
        return Ok(cached.display().to_string());
    }
    Err(unknown_template(name))
}

/// `petri policy create <name> [--from <template>] [--force]`: write a new user
/// template, seeded from an existing template (default `locked-down`).
pub fn create(root: &Path, name: &str, from: Option<&str>, force: bool) -> Result<String> {
    validate_name(name)?;

    let dest = user_path(root, name);
    if dest.is_file() && !force {
        return Err(PetriError::invalid_argument(format!(
            "policy template '{name}' already exists; pass --force to overwrite"
        )));
    }

    let from = from.unwrap_or("locked-down");
    let (toml, _) = body(root, from)?.ok_or_else(|| {
        PetriError::invalid_argument(format!(
            "cannot seed from '{from}': no such policy template (run 'petri policy list')"
        ))
    })?;
    validate_content(name, &toml)?;

    write_template(&dest, &toml)?;
    let note = if builtin(name).is_some() {
        format!(" (shadows the built-in '{name}')")
    } else {
        String::new()
    };
    Ok(format!(
        "created policy template '{name}' from '{from}'{note}\n  {}",
        dest.display()
    ))
}

/// `petri policy edit <name>`: open the user template in `$EDITOR`. A built-in
/// with no override is forked to a user file first (copy-on-write). After the
/// editor exits the result is re-validated; an invalid edit is reported but the
/// file is left in place for the user to fix.
pub fn edit(root: &Path, name: &str) -> Result<String> {
    let dest = user_path(root, name);
    // Decide whether this edit needs a copy-on-write fork, but don't mutate
    // anything until we know we can actually open an editor.
    let needs_fork = if dest.is_file() {
        false
    } else if builtin(name).is_some() {
        true
    } else {
        return Err(unknown_template(name));
    };

    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .filter(|e| !e.trim().is_empty());
    let Some(editor) = editor else {
        return Err(PetriError::invalid_argument(if needs_fork {
            format!(
                "no $EDITOR (or $VISUAL) set; fork the built-in first with 'petri policy create {name} --from {name}', then edit the file"
            )
        } else {
            format!(
                "no $EDITOR (or $VISUAL) set; edit the file directly: {}",
                dest.display()
            )
        }));
    };

    // Editor is available: now perform the fork if needed.
    let forked = needs_fork;
    if needs_fork {
        let b = builtin(name).expect("builtin presence checked above");
        write_template(&dest, b.toml)?;
    }

    let status = std::process::Command::new(&editor)
        .arg(&dest)
        .status()
        .map_err(|source| PetriError::Io {
            path: PathBuf::from(&editor),
            source,
        })?;
    if !status.success() {
        return Err(PetriError::invalid_argument(format!(
            "editor '{editor}' exited with failure; left {} in place",
            dest.display()
        )));
    }

    let opened = if forked {
        format!("forked built-in '{name}' and edited")
    } else {
        format!("edited policy template '{name}'")
    };
    match validate_content(name, &read_to_string(&dest)?) {
        Ok(()) => Ok(format!("{opened}\n  {}", dest.display())),
        Err(err) => Ok(format!(
            "{opened}, but it no longer parses:\n  {err}\n  fix or remove: {}",
            dest.display()
        )),
    }
}

/// `petri policy remove <name>`: delete a user template. Built-ins cannot be
/// removed; removing the override of a built-in restores the built-in.
pub fn remove(root: &Path, name: &str) -> Result<String> {
    let dest = user_path(root, name);
    if dest.is_file() {
        std::fs::remove_file(&dest).map_err(|source| PetriError::Io {
            path: dest.clone(),
            source,
        })?;
        if builtin(name).is_some() {
            return Ok(format!(
                "removed user override '{name}'; the built-in '{name}' is active again"
            ));
        }
        return Ok(format!("removed policy template '{name}'"));
    }
    if builtin(name).is_some() {
        return Err(PetriError::invalid_argument(format!(
            "'{name}' is a built-in template and has no user override to remove"
        )));
    }
    Err(unknown_template(name))
}

// --- name-or-path resolution ------------------------------------------------

/// Resolve a `--policy` argument that may be either a file path or a template
/// name to a concrete, readable file path.
///
/// An existing file always wins. A value that looks like a path (contains a
/// separator, ends in `.toml`, or is absolute) is returned untouched so the
/// usual "file not found" error surfaces. Otherwise the bare name is resolved
/// through the registry: a user override first, then a built-in materialised
/// into the cache.
pub fn resolve_reference(root: &Path, value: &Path) -> Result<PathBuf> {
    if value.is_file() {
        return Ok(value.to_path_buf());
    }

    let text = value.to_string_lossy();
    let looks_like_path = value.is_absolute()
        || value.components().count() > 1
        || text.contains('/')
        || text.contains('\\')
        || text.ends_with(".toml");
    if looks_like_path {
        // Let the downstream open/canonicalize produce a clear path error.
        return Ok(value.to_path_buf());
    }

    let name = text.as_ref();
    validate_name(name)?;

    let user = user_path(root, name);
    if user.is_file() {
        return Ok(user);
    }
    if let Some(b) = builtin(name) {
        let cached = cache_path(root, name);
        write_template(&cached, b.toml)?;
        return Ok(cached);
    }

    Err(PetriError::invalid_argument(format!(
        "--policy '{name}' is neither an existing file nor a known policy template (run 'petri policy list')"
    )))
}

fn unknown_template(name: &str) -> PetriError {
    PetriError::invalid_argument(format!(
        "no such policy template '{name}' (run 'petri policy list')"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("petri-policy-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_builtin_parses() {
        for b in BUILTINS {
            Policy::from_toml_str(b.toml)
                .unwrap_or_else(|err| panic!("built-in '{}' invalid: {err}", b.name));
        }
    }

    #[test]
    fn list_includes_builtins_when_root_empty() {
        let root = temp_root("list-empty");
        let out = list(&root).unwrap();
        for b in BUILTINS {
            assert!(out.contains(b.name), "missing '{}' in:\n{out}", b.name);
        }
        assert!(out.contains("builtin"));
    }

    #[test]
    fn create_seeds_from_locked_down_by_default() {
        let root = temp_root("create-default");
        create(&root, "my-ci", None, false).unwrap();
        assert!(user_path(&root, "my-ci").is_file());
        // Seeded content matches the locked-down built-in.
        assert_eq!(
            show(&root, "my-ci").unwrap(),
            show(&root, "locked-down").unwrap()
        );
    }

    #[test]
    fn create_rejects_existing_without_force() {
        let root = temp_root("create-exists");
        create(&root, "dup", None, false).unwrap();
        let err = create(&root, "dup", None, false).unwrap_err().to_string();
        assert!(err.contains("already exists"), "got: {err}");
        create(&root, "dup", Some("yolo"), true).unwrap();
    }

    #[test]
    fn create_rejects_bad_name() {
        let root = temp_root("create-badname");
        let err = create(&root, "Bad Name", None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid policy template name"), "got: {err}");
    }

    #[test]
    fn override_shadows_builtin_in_list() {
        let root = temp_root("override");
        create(&root, "yolo", Some("locked-down"), true).unwrap();
        let out = list(&root).unwrap();
        // The yolo row is now sourced from the user override.
        let yolo_line = out
            .lines()
            .find(|l| l.starts_with("yolo"))
            .unwrap_or_default();
        assert!(yolo_line.contains("override"), "got: {yolo_line}");
    }

    #[test]
    fn remove_user_override_restores_builtin() {
        let root = temp_root("remove-override");
        create(&root, "developer", Some("yolo"), true).unwrap();
        let msg = remove(&root, "developer").unwrap();
        assert!(msg.contains("active again"), "got: {msg}");
        assert!(!user_path(&root, "developer").is_file());
    }

    #[test]
    fn remove_builtin_without_override_errors() {
        let root = temp_root("remove-builtin");
        let err = remove(&root, "yolo").unwrap_err().to_string();
        assert!(err.contains("built-in"), "got: {err}");
    }

    #[test]
    fn remove_unknown_errors() {
        let root = temp_root("remove-unknown");
        let err = remove(&root, "ghost").unwrap_err().to_string();
        assert!(err.contains("no such policy template"), "got: {err}");
    }

    #[test]
    fn resolve_prefers_existing_file_over_name() {
        let root = temp_root("resolve-file");
        let file = root.join("yolo"); // same stem as a built-in, but a real file
        std::fs::write(&file, "irrelevant").unwrap();
        assert_eq!(resolve_reference(&root, &file).unwrap(), file);
    }

    #[test]
    fn resolve_builtin_name_materialises_cache() {
        let root = temp_root("resolve-builtin");
        let resolved = resolve_reference(&root, Path::new("developer")).unwrap();
        assert_eq!(resolved, cache_path(&root, "developer"));
        assert!(resolved.is_file());
        Policy::load(std::fs::File::open(&resolved).unwrap()).unwrap();
    }

    #[test]
    fn resolve_user_override_wins_over_builtin() {
        let root = temp_root("resolve-override");
        create(&root, "developer", Some("yolo"), true).unwrap();
        let resolved = resolve_reference(&root, Path::new("developer")).unwrap();
        assert_eq!(resolved, user_path(&root, "developer"));
    }

    #[test]
    fn resolve_pathlike_passes_through() {
        let root = temp_root("resolve-path");
        let p = Path::new("./does/not/exist.toml");
        assert_eq!(resolve_reference(&root, p).unwrap(), p);
    }

    #[test]
    fn resolve_unknown_bare_name_errors() {
        let root = temp_root("resolve-unknown");
        let err = resolve_reference(&root, Path::new("ghost"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("known policy template"), "got: {err}");
    }
}
