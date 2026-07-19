//! Git-based project canonicalisation WITHOUT shelling out to git.
//!
//! Hand-parsed `.git` dir / `gitdir:` pointer files, `commondir` worktree
//! indirection, INI remote extraction (remote named `origin` preferred, else
//! the first remote), scp-form remote normalisation, and the 5-candidate
//! alias resolution order.
//!
//! Ports `src/project.js` entirely plus `gitBranch` from
//! `src/agent/watch-files.js`; pinned by `test/canonical-project.test.mjs`.

use indexmap::IndexMap;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use url::Url;

// ---------------------------------------------------------------------------
// Static regexes (patterns are compile-time constants; init cannot fail on
// runtime input).
// ---------------------------------------------------------------------------

/// JS: `/^gitdir:\s*(.+)$/i` applied to the trimmed `.git` file contents.
fn gitdir_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^gitdir:\s*(.+)$").expect("static regex"))
}

/// JS: `/^\[remote\s+"([^"]+)"\]$/i` on a trimmed config line.
fn remote_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)^\[remote\s+"([^"]+)"\]$"#).expect("static regex"))
}

/// JS: `/^url\s*=\s*(.+)$/i` on a trimmed config line.
fn url_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^url\s*=\s*(.+)$").expect("static regex"))
}

/// JS: `/^(?:[^@/\s]+@)?([^:/\s]+):(.+)$/` — the scp-form remote.
fn scp_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:[^@/\s]+@)?([^:/\s]+):(.+)$").expect("static regex"))
}

// ---------------------------------------------------------------------------
// Node `path` semantics (posix). We work on strings because Node's resolution
// is purely lexical: `..` and `.` are collapsed WITHOUT touching the
// filesystem, unlike `std::fs::canonicalize`.
// ---------------------------------------------------------------------------

fn cwd_string() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".to_string())
}

/// Lexically normalize an absolute posix path (Node `path.resolve` style:
/// collapse `.`/`..`/`//`, drop any trailing slash, `..` at root stays root).
fn normalize_abs(p: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", stack.join("/"))
    }
}

/// Node `path.resolve(p)`: absolute paths are normalized, relative paths are
/// resolved against the process cwd.
fn js_resolve1(p: &str) -> String {
    if p.starts_with('/') {
        normalize_abs(p)
    } else {
        normalize_abs(&format!("{}/{}", cwd_string(), p))
    }
}

/// Node `path.resolve(base, p)`.
fn js_resolve2(base: &str, p: &str) -> String {
    if p.starts_with('/') {
        normalize_abs(p)
    } else if base.starts_with('/') {
        normalize_abs(&format!("{base}/{p}"))
    } else {
        normalize_abs(&format!("{}/{}/{}", cwd_string(), base, p))
    }
}

/// Node `path.dirname` for the normalized absolute paths we produce
/// (`"/a/b"` -> `"/a"`, `"/a"` -> `"/"`, `"/"` -> `"/"`).
fn js_dirname(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
        None => ".".to_string(),
    }
}

/// Node `path.basename` (posix): trailing slashes stripped, last segment
/// returned; `""` and `"/"` yield `""` (falsy in the JS fallback chain).
fn js_basename(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    match trimmed.rfind('/') {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Read a file as text the way Node's `'utf8'` mode does: invalid UTF-8 is
/// replaced, never an error.
fn read_text(path: &str) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------------------
// Git plumbing (string-path internals).
// ---------------------------------------------------------------------------

/// `gitDirFor(root)`: `root/.git` as a directory -> that path; as a file ->
/// the first `gitdir: <path>` pointer resolved relative to root; anything
/// else (including fs errors) -> None.
fn git_dir_for_s(root: &str) -> Option<String> {
    let dot_git = format!("{}/.git", root.trim_end_matches('/'));
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let content = read_text(&dot_git)?;
    let caps = gitdir_re().captures(content.trim())?;
    Some(js_resolve2(root, caps.get(1).map(|m| m.as_str())?))
}

/// `commonGitDir(gitDir)`: follow the `commondir` worktree indirection;
/// on any failure return `gitDir` unchanged.
fn common_git_dir_s(git_dir: &str) -> String {
    match read_text(&format!("{git_dir}/commondir")) {
        Some(content) => js_resolve2(git_dir, content.trim()),
        None => git_dir.to_string(),
    }
}

fn repository_root_s(location: &str) -> Option<String> {
    let mut current = match std::fs::metadata(location) {
        Ok(meta) if meta.is_dir() => js_resolve1(location),
        Ok(_) => js_dirname(&js_resolve1(location)),
        Err(_) => js_resolve1(location),
    };
    loop {
        if git_dir_for_s(&current).is_some() {
            return Some(current);
        }
        let parent = js_dirname(&current);
        if parent == current {
            return None;
        }
        current = parent;
    }
}

/// Line-based INI scan of `.git/config`: `[remote "name"]` headers open a
/// remote section, any other `[...` line closes it; `url = value` lines
/// inside a section collect. Returns the `origin` remote's url if present,
/// else the first remote's url.
fn remote_from_config(config: &str) -> Option<String> {
    let mut section = String::new();
    let mut remotes: Vec<(String, String)> = Vec::new();
    for raw in config.split('\n') {
        let line = raw.trim();
        if let Some(caps) = remote_header_re().captures(line) {
            section = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            continue;
        }
        if line.starts_with('[') {
            section = String::new();
            continue;
        }
        if !section.is_empty() {
            if let Some(caps) = url_line_re().captures(line) {
                if let Some(url) = caps.get(1) {
                    remotes.push((section.clone(), url.as_str().trim().to_string()));
                }
            }
        }
    }
    remotes
        .iter()
        .find(|(name, _)| name == "origin")
        .or_else(|| remotes.first())
        .map(|(_, url)| url.clone())
}

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

/// Resolve the actual git dir for a repo root: `.git` directory, or the
/// `gitdir:` pointer file (worktrees / submodules), with `commondir`
/// indirection applied by callers that need shared files.
pub fn git_dir_for(root: &Path) -> Option<PathBuf> {
    git_dir_for_s(&path_str(root)).map(PathBuf::from)
}

/// Walk up from `location` to the enclosing repository root (the directory
/// containing `.git`), if any. A file location starts the walk from its
/// parent directory; a missing location starts from its resolved path.
pub fn repository_root(location: &str) -> Option<PathBuf> {
    repository_root_s(location).map(PathBuf::from)
}

/// Normalize a git remote to `host/owner/repo`: scp-form unless the remote
/// contains `://` anywhere; host lowercased, path case preserved; leading and
/// trailing slashes stripped; exactly one trailing `.git` removed
/// (case-insensitively). Unusable remotes (no host, no path, unparsable URL)
/// -> None.
pub fn normalize_git_remote(remote: &str) -> Option<String> {
    if remote.is_empty() {
        return None;
    }
    let trimmed = remote.trim();
    let (host, pathname): (String, String) = match scp_re().captures(trimmed) {
        Some(caps) if !remote.contains("://") => (
            caps.get(1).map(|m| m.as_str().to_string())?,
            caps.get(2).map(|m| m.as_str().to_string())?,
        ),
        _ => {
            let parsed = Url::parse(trimmed).ok()?;
            (
                parsed.host_str().unwrap_or("").to_string(),
                parsed.path().to_string(),
            )
        }
    };
    let mut clean_path = pathname.trim_matches('/').to_string();
    if clean_path.len() >= 4 {
        let tail_start = clean_path.len() - 4;
        if clean_path
            .get(tail_start..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(".git"))
        {
            clean_path.truncate(tail_start);
        }
    }
    if host.is_empty() || clean_path.is_empty() {
        return None;
    }
    Some(format!("{}/{}", host.to_lowercase(), clean_path))
}

/// Normalized remote of the repository containing `location` (via
/// `commondir`, so worktrees read the main repo's config). None when there is
/// no repository, no readable config, or no usable remote.
pub fn git_remote(location: &str) -> Option<String> {
    let root = repository_root_s(location)?;
    let git_dir = git_dir_for_s(&root)?;
    let config = read_text(&format!("{}/config", common_git_dir_s(&git_dir)))?;
    normalize_git_remote(&remote_from_config(&config)?)
}

/// The JS `aliasFor`: for each non-empty candidate, exact own-key match
/// first, then the first alias key equal case-insensitively (in map order).
fn alias_for(aliases: &IndexMap<String, String>, candidates: &[Option<String>]) -> Option<String> {
    for candidate in candidates.iter().flatten() {
        if candidate.is_empty() {
            continue;
        }
        if let Some(value) = aliases.get(candidate) {
            return Some(value.clone());
        }
        let lowered = candidate.to_lowercase();
        if let Some((_, value)) = aliases.iter().find(|(key, _)| key.to_lowercase() == lowered) {
            return Some(value.clone());
        }
    }
    None
}

/// Resolve the display project name for a location. Candidate order (exact
/// key first, then a case-insensitive scan, per candidate): cwd-resolved
/// value, repository root, full normalized remote, `owner/repo`, name.
/// Final fallback chain: alias || `owner/repo` || fallback || basename ||
/// `"unknown"`. The filesystem is only walked for ABSOLUTE locations.
pub fn resolve_project(location: &str, aliases: &IndexMap<String, String>, fallback: Option<&str>) -> String {
    let value = location.trim();
    let root = if !value.is_empty() && value.starts_with('/') {
        repository_root_s(value)
    } else {
        None
    };
    let remote = root.as_deref().and_then(git_remote);
    // JS: remote?.split('/').slice(-2).join('/') — the last two segments.
    let remote_project = remote.as_deref().map(|r| {
        let segments: Vec<&str> = r.split('/').collect();
        segments[segments.len().saturating_sub(2)..].join("/")
    });
    let path_fallback = match (&root, value) {
        (Some(r), _) => Some(js_basename(r)),
        (None, v) if !v.is_empty() => Some(js_basename(v)),
        _ => None,
    };
    let name = fallback
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .or_else(|| path_fallback.filter(|p| !p.is_empty()))
        .unwrap_or_else(|| "unknown".to_string());

    let candidates = [
        if value.is_empty() {
            None
        } else {
            Some(js_resolve1(value))
        },
        root,
        remote,
        remote_project.clone(),
        Some(name.clone()),
    ];
    alias_for(aliases, &candidates)
        .filter(|a| !a.is_empty())
        .or(remote_project)
        .unwrap_or(name)
}

/// Current branch of the repository whose root is `project_dir` (`.git`
/// directly inside it — no upward walk, matching the JS agent): strip the
/// FIRST `refs/heads/` occurrence only; detached HEAD -> first 12 chars of
/// the hash. Reads the worktree-local HEAD (no `commondir` indirection).
pub fn git_branch(project_dir: &Path) -> Option<String> {
    let dir = path_str(project_dir);
    let dot_git = format!("{}/.git", dir.trim_end_matches('/'));
    let meta = std::fs::metadata(&dot_git).ok()?;
    let git_dir = if meta.is_file() {
        let content = read_text(&dot_git)?;
        let caps = gitdir_re().captures(content.trim())?;
        js_resolve2(&dir, caps.get(1).map(|m| m.as_str())?)
    } else {
        dot_git
    };
    let head = read_text(&format!("{git_dir}/HEAD"))?.trim().to_string();
    Some(match head.strip_prefix("ref: ") {
        Some(reference) => reference.replacen("refs/heads/", "", 1),
        None => head.chars().take(12).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_repository(root: &Path, remotes: &[(&str, &str)]) -> PathBuf {
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let config = remotes
            .iter()
            .map(|(name, url)| {
                format!("[remote \"{name}\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*")
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(git_dir.join("config"), config).unwrap();
        git_dir
    }

    fn aliases(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn normalizes_https_ssh_and_scp_remotes_to_one_identity() {
        let expected = Some("github.com/Acme/widget".to_string());
        assert_eq!(
            normalize_git_remote("https://github.com/Acme/widget.git"),
            expected
        );
        assert_eq!(
            normalize_git_remote("ssh://git@github.com/Acme/widget.git"),
            expected
        );
        assert_eq!(normalize_git_remote("git@github.com:Acme/widget.git"), expected);
        assert_eq!(normalize_git_remote("github.com:Acme/widget/"), expected);
    }

    #[test]
    fn normalization_strips_only_transport_syntax_and_rejects_unusable() {
        assert_eq!(
            normalize_git_remote("  HTTPS://GitHub.COM/Owner/Repo.GIT/  "),
            Some("github.com/Owner/Repo".to_string())
        );
        assert_eq!(
            normalize_git_remote("https://git.example.test/groups/team/repo.git"),
            Some("git.example.test/groups/team/repo".to_string())
        );
        assert_eq!(normalize_git_remote("file:///srv/git/repo.git"), None);
        assert_eq!(normalize_git_remote("/srv/git/repo.git"), None);
        assert_eq!(normalize_git_remote(""), None);
    }

    #[test]
    fn strips_exactly_one_dot_git_and_keeps_path_case() {
        assert_eq!(
            normalize_git_remote("git@github.com:Acme/app.git.git"),
            Some("github.com/Acme/app.git".to_string())
        );
        assert_eq!(
            normalize_git_remote("GIT@GitHub.com:MixedCase/Path"),
            Some("github.com/MixedCase/Path".to_string())
        );
    }

    #[test]
    fn reads_origin_in_preference_regardless_of_config_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        write_repository(
            &root,
            &[
                ("upstream", "https://github.com/canonical/upstream.git"),
                ("origin", "git@github.com:personal/fork.git"),
            ],
        );
        let root_s = root.to_string_lossy();
        assert_eq!(git_remote(&root_s), Some("github.com/personal/fork".to_string()));
        assert_eq!(resolve_project(&root_s, &IndexMap::new(), None), "personal/fork");
    }

    #[test]
    fn uses_first_remote_when_origin_absent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        write_repository(
            &root,
            &[
                ("company", "ssh://git@gitlab.example.test/platform/service.git"),
                ("backup", "https://backup.example.test/archive/service.git"),
            ],
        );
        let nested = root.join("src/deep");
        let nested_s = nested.to_string_lossy();
        assert_eq!(
            git_remote(&nested_s),
            Some("gitlab.example.test/platform/service".to_string())
        );
        assert_eq!(
            resolve_project(&nested_s, &IndexMap::new(), None),
            "platform/service"
        );
    }

    #[test]
    fn finds_repository_roots_from_nested_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        let nested = root.join("src/feature");
        let file = nested.join("index.js");
        write_repository(&root, &[("origin", "https://github.com/acme/widget.git")]);
        fs::create_dir_all(&nested).unwrap();
        fs::write(&file, "export default 1;\n").unwrap();

        assert_eq!(repository_root(&root.to_string_lossy()), Some(root.clone()));
        assert_eq!(repository_root(&nested.to_string_lossy()), Some(root.clone()));
        assert_eq!(repository_root(&file.to_string_lossy()), Some(root.clone()));
        assert_eq!(
            resolve_project(&file.to_string_lossy(), &IndexMap::new(), None),
            "acme/widget"
        );
    }

    #[test]
    fn worktrees_read_the_remote_from_the_common_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("topic-checkout");
        let common = dir.path().join("main.git");
        let worktree_git_dir = common.join("worktrees/topic");
        fs::create_dir_all(&checkout).unwrap();
        fs::create_dir_all(&worktree_git_dir).unwrap();
        // Relative gitdir pointer, exactly as git writes for local worktrees.
        fs::write(checkout.join(".git"), "gitdir: ../main.git/worktrees/topic\n").unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
        fs::write(worktree_git_dir.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
        fs::write(
            common.join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:Acme/worktree-app.git",
        )
        .unwrap();

        let checkout_s = checkout.to_string_lossy();
        assert_eq!(repository_root(&checkout_s), Some(checkout.clone()));
        assert_eq!(
            git_remote(&checkout_s),
            Some("github.com/Acme/worktree-app".to_string())
        );
        assert_eq!(
            resolve_project(
                &checkout.join("not-created-yet.js").to_string_lossy(),
                &IndexMap::new(),
                None
            ),
            "Acme/worktree-app"
        );
        // Branch comes from the worktree-local HEAD, not the common dir.
        assert_eq!(git_branch(&checkout), Some("topic".to_string()));
    }

    #[test]
    fn aliases_resolve_by_path_nested_path_remote_short_remote_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("renamed-checkout");
        let nested = root.join("src");
        write_repository(&root, &[("origin", "git@github.com:Acme/widget.git")]);
        fs::create_dir_all(&nested).unwrap();
        let root_s = root.to_string_lossy().into_owned();
        let nested_s = nested.to_string_lossy().into_owned();

        assert_eq!(
            resolve_project(&nested_s, &aliases(&[(&root_s, "path-canonical")]), None),
            "path-canonical"
        );
        assert_eq!(
            resolve_project(&nested_s, &aliases(&[(&nested_s, "nested-canonical")]), None),
            "nested-canonical"
        );
        assert_eq!(
            resolve_project(
                &root_s,
                &aliases(&[("github.com/Acme/widget", "remote-canonical")]),
                None
            ),
            "remote-canonical"
        );
        assert_eq!(
            resolve_project(
                &root_s,
                &aliases(&[("Acme/widget", "short-remote-canonical")]),
                None
            ),
            "short-remote-canonical"
        );
        assert_eq!(
            resolve_project(
                "/workspace/local-label",
                &aliases(&[("local-label", "name-canonical")]),
                None
            ),
            "name-canonical"
        );
    }

    #[test]
    fn alias_keys_match_case_insensitively_and_path_wins_over_remote() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Checkout");
        write_repository(&root, &[("origin", "https://GitHub.com/Acme/Widget.git")]);
        let root_s = root.to_string_lossy().into_owned();
        let upper = root_s.to_uppercase();
        assert_eq!(
            resolve_project(
                &root_s,
                &aliases(&[
                    (&upper, "by-path"),
                    ("GITHUB.COM/ACME/WIDGET", "by-remote"),
                    ("acme/widget", "by-short-remote"),
                ]),
                None
            ),
            "by-path"
        );
        assert_eq!(
            resolve_project(
                &root_s,
                &aliases(&[("GITHUB.COM/ACME/WIDGET", "by-remote")]),
                None
            ),
            "by-remote"
        );
    }

    #[test]
    fn non_git_locations_fall_back_predictably() {
        let missing = format!("/.stackhour-no-repo-{}-test/plain-project", std::process::id());
        assert_eq!(repository_root(&missing), None);
        assert_eq!(git_remote(&missing), None);
        assert_eq!(resolve_project(&missing, &IndexMap::new(), None), "plain-project");
        assert_eq!(
            resolve_project("display label", &IndexMap::new(), None),
            "display label"
        );
        assert_eq!(
            resolve_project("", &IndexMap::new(), Some("explicit-fallback")),
            "explicit-fallback"
        );
        assert_eq!(resolve_project("", &IndexMap::new(), None), "unknown");
    }

    #[test]
    fn remote_owner_keeps_same_basename_repositories_from_colliding() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one/app");
        let two = dir.path().join("two/app");
        write_repository(&one, &[("origin", "https://github.com/alpha/app.git")]);
        write_repository(&two, &[("origin", "https://github.com/beta/app.git")]);
        let empty = IndexMap::new();
        assert_eq!(resolve_project(&one.to_string_lossy(), &empty, None), "alpha/app");
        assert_eq!(resolve_project(&two.to_string_lossy(), &empty, None), "beta/app");
    }

    #[test]
    fn repo_without_config_falls_back_to_basename() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("bare-ish");
        fs::create_dir_all(root.join(".git")).unwrap();
        // No config file at all: remote is None, basename wins.
        assert_eq!(
            resolve_project(&root.to_string_lossy(), &IndexMap::new(), None),
            "bare-ish"
        );
        assert_eq!(git_remote(&root.to_string_lossy()), None);
    }

    #[test]
    fn git_branch_reads_head_ref_and_detached_hash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write_repository(&root, &[("origin", "https://github.com/a/b.git")]);
        assert_eq!(git_branch(&root), Some("main".to_string()));

        fs::write(
            root.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(git_branch(&root), Some("0123456789ab".to_string()));

        // Only the FIRST 'refs/heads/' occurrence is stripped.
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/refs/heads/x\n").unwrap();
        assert_eq!(git_branch(&root), Some("refs/heads/x".to_string()));

        // Non-branch refs pass through untouched.
        fs::write(root.join(".git/HEAD"), "ref: refs/tags/v1\n").unwrap();
        assert_eq!(git_branch(&root), Some("refs/tags/v1".to_string()));

        // No .git at all -> None; no upward walk from subdirectories.
        assert_eq!(git_branch(&dir.path().join("nope")), None);
        let sub = root.join("src");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(git_branch(&sub), None);
    }

    #[test]
    fn git_dir_for_handles_dir_pointer_and_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(git_dir_for(&root), Some(root.join(".git")));

        let wt = dir.path().join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: /abs/git/dir\n").unwrap();
        assert_eq!(git_dir_for(&wt), Some(PathBuf::from("/abs/git/dir")));

        let bad = dir.path().join("bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join(".git"), "this is not a pointer\n").unwrap();
        assert_eq!(git_dir_for(&bad), None);

        assert_eq!(git_dir_for(&dir.path().join("missing")), None);
    }

    #[test]
    fn remote_from_config_ini_parsing_details() {
        // Case-insensitive headers and url keys; sections closed by other headers.
        let config = "[Remote \"other\"]\n\tURL = https://example.test/one/two.git\n[core]\n\turl = https://example.test/ignored.git\n";
        assert_eq!(
            remote_from_config(config),
            Some("https://example.test/one/two.git".to_string())
        );
        assert_eq!(remote_from_config("[core]\n\tbare = false\n"), None);
        assert_eq!(remote_from_config(""), None);
    }

    #[test]
    fn empty_alias_value_falls_through_but_ends_the_alias_search() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        write_repository(&root, &[("origin", "https://github.com/alpha/app.git")]);
        let root_s = root.to_string_lossy().into_owned();
        // JS: aliasFor returns '' (falsy) -> `|| remoteProject` kicks in, even
        // though a later candidate ('alpha/app') has a non-empty alias.
        assert_eq!(
            resolve_project(
                &root_s,
                &aliases(&[(&root_s, ""), ("alpha/app", "never-reached")]),
                None
            ),
            "alpha/app"
        );
    }

    #[test]
    fn resolve_candidate_uses_lexical_normalisation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write_repository(&root, &[("origin", "https://github.com/a/b.git")]);
        // Trailing slash and dot segments are collapsed before alias lookup.
        let messy = format!("{}/./sub/../", root.to_string_lossy());
        let root_s = root.to_string_lossy().into_owned();
        assert_eq!(
            resolve_project(&messy, &aliases(&[(&root_s, "clean")]), None),
            "clean"
        );
    }

    #[test]
    fn js_path_helpers_match_node() {
        assert_eq!(normalize_abs("/a/b/../c//d/."), "/a/c/d");
        assert_eq!(normalize_abs("/../.."), "/");
        assert_eq!(js_dirname("/a/b"), "/a");
        assert_eq!(js_dirname("/a"), "/");
        assert_eq!(js_dirname("/"), "/");
        assert_eq!(js_basename("/a/b/"), "b");
        assert_eq!(js_basename("display label"), "display label");
        assert_eq!(js_basename("/"), "");
        assert_eq!(js_resolve2("/base", "../x"), "/x");
        assert_eq!(js_resolve2("/base", "/abs"), "/abs");
    }
}
