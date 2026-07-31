//! Storage path resolution and `~` expansion.
//!
//! Home is taken from the `$HOME` env var (matches Node `os.homedir()` under
//! the test suite, which sets HOME). `expand_home` preserves the JS quirk
//! where `~x` expands to `<home>/x`.
//!
//! Ports `resolveStoragePaths` / `expandHome` from `src/config.js`:
//!
//! ```js
//! const configPath = env.STACKHOUR_CONFIG
//!   || path.join(home, '.config', 'stackhour', 'config.json');
//! const dataDir = env.STACKHOUR_DATA
//!   || path.join(home, '.local', 'share', 'stackhour');
//! // dbPath = path.join(dataDir, 'stackhour.db')
//! ```
//!
//! plus the Rust-side addition `config_dir` = dirname(config_path) (root of
//! auxiliary configuration), overridable via `STACKHOUR_CONFIG_DIR`.

use std::path::{Path, PathBuf};

/// All resolved storage locations. `config_dir` = dirname(config_path), the
/// auxiliary configuration root, overridable via
/// `STACKHOUR_CONFIG_DIR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePaths {
    /// `~/.config/stackhour/config.json` (env: `STACKHOUR_CONFIG`).
    pub config_path: PathBuf,
    /// dirname(config_path) unless `STACKHOUR_CONFIG_DIR` overrides it.
    pub config_dir: PathBuf,
    /// `~/.local/share/stackhour` (env: `STACKHOUR_DATA`).
    pub data_dir: PathBuf,
    /// `<data_dir>/stackhour.db` default DB location.
    pub db_path: PathBuf,
}

/// JS truthiness for env vars: an unset OR empty-string variable falls back
/// to the default (`env.STACKHOUR_CONFIG || ...` treats `''` as falsy).
fn env_nonempty(env: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    env(key).filter(|v| !v.is_empty())
}

/// Node `path.dirname` semantics (posix), returned as a `PathBuf`:
/// `dirname('/a/b') == '/a'`, `dirname('a') == '.'`, `dirname('/') == '/'`.
fn dirname(p: &Path) -> PathBuf {
    match p.parent() {
        None => {
            // Root ("/") or empty path: Node returns "/" for root, "." for "".
            if p.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                p.to_path_buf()
            }
        }
        Some(parent) if parent.as_os_str().is_empty() => PathBuf::from("."),
        Some(parent) => parent.to_path_buf(),
    }
}

/// Resolve every storage path from an env-lookup closure and the home dir.
/// Injectable env lookup keeps this unit-testable without touching process env.
pub fn resolve_storage_paths(env: &impl Fn(&str) -> Option<String>, home: &Path) -> StoragePaths {
    let config_path = match env_nonempty(env, "STACKHOUR_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => home.join(".config").join("stackhour").join("config.json"),
    };
    let config_dir = match env_nonempty(env, "STACKHOUR_CONFIG_DIR") {
        Some(d) => PathBuf::from(d),
        None => dirname(&config_path),
    };
    let data_dir = match env_nonempty(env, "STACKHOUR_DATA") {
        Some(d) => PathBuf::from(d),
        None => home.join(".local").join("share").join("stackhour"),
    };
    let db_path = data_dir.join("stackhour.db");
    StoragePaths {
        config_path,
        config_dir,
        data_dir,
        db_path,
    }
}

/// Convenience: resolve from the real process environment and `$HOME`.
/// Home = the `HOME` env var (matches Node `os.homedir()` under the test
/// suite, which sets HOME); an unset HOME degrades to relative paths, the
/// same way `path.join('', ...)` would.
pub fn resolve_storage_paths_from_process_env() -> StoragePaths {
    let env = |key: &str| std::env::var(key).ok();
    let home = std::env::var("HOME").unwrap_or_default();
    resolve_storage_paths(&env, Path::new(&home))
}

/// `~` expansion with the JS quirk kept: `~` and `~/x` expand against `home`,
/// and `~x` (no slash) becomes `<home>/x` too. Non-`~` paths pass through.
///
/// Mirrors `path.join(homedir, p.slice(1))`, including Node's join-time
/// normalisation (`.`/`..` segments resolved, duplicate slashes collapsed,
/// trailing slash preserved). Empty input is returned as-is (JS falsy check).
pub fn expand_home(p: &str, home: &Path) -> String {
    if p.is_empty() || !p.starts_with('~') {
        return p.to_string();
    }
    posix_join(&home.to_string_lossy(), &p[1..])
}

/// Node `path.posix.join(a, b)`: concatenate non-empty segments with `/` and
/// normalize. Empty result becomes `"."`.
fn posix_join(a: &str, b: &str) -> String {
    let joined = match (a.is_empty(), b.is_empty()) {
        (true, true) => return ".".to_string(),
        (true, false) => b.to_string(),
        (false, true) => a.to_string(),
        (false, false) => format!("{a}/{b}"),
    };
    // Node's join normalizes the concatenation of non-empty segments.
    posix_normalize(&joined)
}

/// Node `path.posix.normalize`: resolve `.` and `..`, collapse duplicate
/// slashes, keep a trailing slash, map empty to `.`.
fn posix_normalize(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let absolute = p.starts_with('/');
    let trailing_slash = p.ends_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if let Some(last) = stack.last() {
                    if *last != ".." {
                        stack.pop();
                        continue;
                    }
                }
                if !absolute {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }
    let mut out = String::new();
    if absolute {
        out.push('/');
    }
    out.push_str(&stack.join("/"));
    if out.is_empty() {
        return ".".to_string();
    }
    if trailing_slash && !out.ends_with('/') {
        out.push('/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn defaults_from_home() {
        let env = env_of(&[]);
        let p = resolve_storage_paths(&env, Path::new("/home/nikita"));
        assert_eq!(
            p.config_path,
            PathBuf::from("/home/nikita/.config/stackhour/config.json")
        );
        assert_eq!(p.config_dir, PathBuf::from("/home/nikita/.config/stackhour"));
        assert_eq!(p.data_dir, PathBuf::from("/home/nikita/.local/share/stackhour"));
        assert_eq!(
            p.db_path,
            PathBuf::from("/home/nikita/.local/share/stackhour/stackhour.db")
        );
    }

    #[test]
    fn env_overrides_config_and_data() {
        let env = env_of(&[
            ("STACKHOUR_CONFIG", "/tmp/custom/cfg.json"),
            ("STACKHOUR_DATA", "/var/lib/sh"),
        ]);
        let p = resolve_storage_paths(&env, Path::new("/home/u"));
        assert_eq!(p.config_path, PathBuf::from("/tmp/custom/cfg.json"));
        assert_eq!(p.config_dir, PathBuf::from("/tmp/custom"));
        assert_eq!(p.data_dir, PathBuf::from("/var/lib/sh"));
        assert_eq!(p.db_path, PathBuf::from("/var/lib/sh/stackhour.db"));
    }

    #[test]
    fn empty_env_values_are_falsy() {
        // JS: env.STACKHOUR_CONFIG || default — '' falls through.
        let env = env_of(&[
            ("STACKHOUR_CONFIG", ""),
            ("STACKHOUR_DATA", ""),
            ("STACKHOUR_CONFIG_DIR", ""),
        ]);
        let p = resolve_storage_paths(&env, Path::new("/h"));
        assert_eq!(p.config_path, PathBuf::from("/h/.config/stackhour/config.json"));
        assert_eq!(p.config_dir, PathBuf::from("/h/.config/stackhour"));
        assert_eq!(p.data_dir, PathBuf::from("/h/.local/share/stackhour"));
    }

    #[test]
    fn config_dir_env_override() {
        let env = env_of(&[("STACKHOUR_CONFIG_DIR", "/etc/stackhour.d")]);
        let p = resolve_storage_paths(&env, Path::new("/home/u"));
        // config_path unaffected; only the auxiliary root moves.
        assert_eq!(
            p.config_path,
            PathBuf::from("/home/u/.config/stackhour/config.json")
        );
        assert_eq!(p.config_dir, PathBuf::from("/etc/stackhour.d"));
    }

    #[test]
    fn config_dir_from_relative_config_path() {
        // dirname('cfg.json') == '.' in Node.
        let env = env_of(&[("STACKHOUR_CONFIG", "cfg.json")]);
        let p = resolve_storage_paths(&env, Path::new("/h"));
        assert_eq!(p.config_path, PathBuf::from("cfg.json"));
        assert_eq!(p.config_dir, PathBuf::from("."));
    }

    #[test]
    fn config_dir_from_root_config_path() {
        // dirname('/cfg.json') == '/'.
        let env = env_of(&[("STACKHOUR_CONFIG", "/cfg.json")]);
        let p = resolve_storage_paths(&env, Path::new("/h"));
        assert_eq!(p.config_dir, PathBuf::from("/"));
    }

    #[test]
    fn expand_home_basic() {
        let home = Path::new("/home/nikita");
        assert_eq!(expand_home("~", home), "/home/nikita");
        assert_eq!(expand_home("~/x", home), "/home/nikita/x");
        assert_eq!(expand_home("~/a/b", home), "/home/nikita/a/b");
    }

    #[test]
    fn expand_home_tilde_x_quirk() {
        // '~alice/x' -> $HOME/alice/x, NOT user alice's home.
        let home = Path::new("/home/nikita");
        assert_eq!(expand_home("~x", home), "/home/nikita/x");
        assert_eq!(expand_home("~alice/x", home), "/home/nikita/alice/x");
    }

    #[test]
    fn expand_home_passthrough() {
        let home = Path::new("/home/nikita");
        assert_eq!(expand_home("/abs/path", home), "/abs/path");
        assert_eq!(expand_home("rel/path", home), "rel/path");
        assert_eq!(expand_home("", home), "");
        // Only a LEADING '~' triggers expansion.
        assert_eq!(expand_home("a~b", home), "a~b");
    }

    #[test]
    fn expand_home_join_normalisation() {
        // path.join normalizes '.', '..' and duplicate slashes.
        let home = Path::new("/home/nikita");
        assert_eq!(expand_home("~/./x", home), "/home/nikita/x");
        assert_eq!(expand_home("~/../x", home), "/home/x");
        assert_eq!(expand_home("~//x", home), "/home/nikita/x");
        // Trailing slash is preserved (Node join keeps it).
        assert_eq!(expand_home("~/x/", home), "/home/nikita/x/");
    }

    #[test]
    fn posix_normalize_matches_node() {
        assert_eq!(posix_normalize("/a/b/../c"), "/a/c");
        assert_eq!(posix_normalize("a/.."), ".");
        assert_eq!(posix_normalize("/.."), "/");
        assert_eq!(posix_normalize("../a"), "../a");
        assert_eq!(posix_normalize("a//b//"), "a/b/");
        assert_eq!(posix_normalize(""), ".");
    }

    #[test]
    fn posix_join_matches_node() {
        assert_eq!(posix_join("/home/u", ""), "/home/u");
        assert_eq!(posix_join("", "x"), "x");
        assert_eq!(posix_join("", ""), ".");
        assert_eq!(posix_join("/home/u", "/x"), "/home/u/x");
    }
}
