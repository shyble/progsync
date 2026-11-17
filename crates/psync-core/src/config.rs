//! Configuration: flat `key = value` settings in JSON files.
//!
//! Lookup order for a key: environment variable, repository config
//! (`.psync/config.json`), global config (`$XDG_CONFIG_HOME/psync/config.json`,
//! default `~/.config/psync/config.json`).

use crate::object::Identity;
use crate::util::atomic_write;
use crate::bail;
use crate::error::Result;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const KNOWN_KEYS: &[&str] =
    &["user.name", "user.email", "land.check", "land.queue", "commit.check", "commit.max_file_mb", "watch.auto", "text.eol"];

pub type Settings = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Repo,
    Global,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::Repo => "repo",
            Source::Global => "global",
        }
    }
}

pub fn env_var(key: &str) -> String {
    format!("PSYNC_{}", key.replace('.', "_").to_uppercase())
}

pub fn global_path() -> Option<PathBuf> {
    let base = crate::platform::config_home()?;
    Some(base.join("psync").join("config.json"))
}

pub fn repo_path(dot: &Path) -> PathBuf {
    dot.join("config.json")
}

pub fn load(path: &Path) -> Settings {
    fs::read(path).ok().and_then(|b| crate::json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn set(path: &Path, key: &str, value: Option<&str>) -> Result<()> {
    if !KNOWN_KEYS.contains(&key) {
        bail!("unknown config key '{key}' (known: {})", KNOWN_KEYS.join(", "));
    }
    match (value, key) {
        (Some(v), "text.eol") => drop(crate::eol::Ending::parse(v)?),
        (Some(v), "land.queue" | "watch.auto") if v != "true" && v != "false" => bail!("{key} is true or false"),
        (Some(v), "commit.max_file_mb") if !v.trim().parse::<f64>().is_ok_and(|n| n >= 0.0) => {
            bail!("commit.max_file_mb is a size in MB, e.g. 50 (0 turns the guard off)")
        }
        _ => {}
    }
    let mut s = load(path);
    match value {
        Some(v) => s.insert(key.to_string(), v.to_string()),
        None => s.remove(key),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(path, &crate::json::to_vec_pretty(&s)?)
}

/// Resolve one key; `dot` is the repository's `.psync` directory, if any.
pub fn get(dot: Option<&Path>, key: &str) -> Option<(String, Source)> {
    if let Ok(v) = std::env::var(env_var(key)) {
        if !v.is_empty() {
            return Some((v, Source::Env));
        }
    }
    if let Some(dot) = dot {
        if let Some(v) = load(&repo_path(dot)).get(key) {
            return Some((v.clone(), Source::Repo));
        }
    }
    let g = global_path()?;
    load(&g).get(key).map(|v| (v.clone(), Source::Global))
}

/// All effective settings with their sources.
pub fn list(dot: Option<&Path>) -> Vec<(String, String, Source)> {
    KNOWN_KEYS
        .iter()
        .filter_map(|k| get(dot, k).map(|(v, s)| (k.to_string(), v, s)))
        .collect()
}

/// The identity to record for actions, from config plus the acting agent.
pub fn identity(dot: Option<&Path>, agent: Option<String>) -> Identity {
    let name = get(dot, "user.name")
        .map(|(v, _)| v)
        .unwrap_or_else(crate::platform::user_name);
    let email = get(dot, "user.email").map(|(v, _)| v).unwrap_or_default();
    Identity { name, email, agent: agent.filter(|a| !a.is_empty()) }
}

/// If no owner identity is configured, copy it from git's global config into
/// psync's global config. Returns a message describing what was done.
pub fn ensure_owner_identity() -> Option<String> {
    if get(None, "user.name").is_some() || get(None, "user.email").is_some() {
        return None;
    }
    let git = |key: &str| -> Option<String> {
        let out = Command::new("git").args(["config", "--global", "--get", key]).output().ok()?;
        let v = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (out.status.success() && !v.is_empty()).then_some(v)
    };
    let (name, email) = (git("user.name"), git("user.email"));
    if name.is_none() && email.is_none() {
        return Some("No owner identity configured. Set one with: psync config --global user.name \"Your Name\" && psync config --global user.email you@example.com".into());
    }
    let path = global_path()?;
    if let Some(n) = &name {
        set(&path, "user.name", Some(n)).ok()?;
    }
    if let Some(e) = &email {
        set(&path, "user.email", Some(e)).ok()?;
    }
    Some(format!(
        "Owner identity {} imported from git config into {}",
        Identity { name: name.unwrap_or_default(), email: email.unwrap_or_default(), agent: None }.owner(),
        path.display()
    ))
}
