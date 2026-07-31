//! `files` watcher: mtime scan of agent.projectRoots.
//!
//! filesLastScan is set BEFORE walking (clock-backwards `since` recovery);
//! dot-skip with the `.env` whitelist BEFORE the ignoreDirs exact-basename
//! check; maxScanDepth; mtime accepted in (since, now+60] (future
//! tolerance); LANG_BY_EXT extension table; per-tick git-branch cache that
//! caches None too. Rows: source `editor-files`, category `coding`,
//! language, branch, is_write 1, project via
//! `stackhour_core::project::resolve_project`.

use crate::{Gate, Watcher};
use serde_json::{json, Value};
use stackhour_core::config::Config;
use stackhour_core::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Extension -> language label. Lower-cased extension INCLUDING the dot,
/// exactly as `path.extname(file).toLowerCase()` produces.
const LANG_BY_EXT: &[(&str, &str)] = &[
    (".ts", "TypeScript"),
    (".tsx", "TypeScript"),
    (".js", "JavaScript"),
    (".jsx", "JavaScript"),
    (".mjs", "JavaScript"),
    (".cjs", "JavaScript"),
    (".json", "JSON"),
    (".css", "CSS"),
    (".scss", "SCSS"),
    (".html", "HTML"),
    (".md", "Markdown"),
    (".py", "Python"),
    (".rs", "Rust"),
    (".go", "Go"),
    (".cs", "C#"),
    (".sh", "Shell"),
    (".yml", "YAML"),
    (".yaml", "YAML"),
    (".toml", "TOML"),
    (".sql", "SQL"),
    (".vue", "Vue"),
    (".svelte", "Svelte"),
];

/// `LANG_BY_EXT[extname(file).toLowerCase()] || null`.
fn language_for(file: &Path) -> Value {
    let Some(ext) = file.extension().and_then(|e| e.to_str()) else {
        return Value::Null;
    };
    let dotted = format!(".{}", ext.to_lowercase());
    LANG_BY_EXT
        .iter()
        .find(|(k, _)| *k == dotted)
        .map_or(Value::Null, |(_, lang)| Value::String((*lang).to_string()))
}

/// Recursive walk honouring maxScanDepth and the ignore list.
///
/// Order of the two skips is load-bearing: the dot-prefix test (with the
/// `.env` whitelist) runs BEFORE the ignoreDirs exact-basename test, so a
/// dotted directory is skipped even if it is not in ignoreDirs.
fn walk(dir: &Path, ignore_dirs: &[String], depth: u32, max_depth: u32, out: &mut Vec<PathBuf>) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        // An unreadable directory is skipped silently — one bad permission
        // must not abort the whole scan.
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && name != ".env" {
            continue;
        }
        let full = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if ignore_dirs.contains(&name) {
                continue;
            }
            walk(&full, ignore_dirs, depth + 1, max_depth, out);
        } else if file_type.is_file() {
            out.push(full);
        }
    }
}

/// Read the current git branch for `project_dir`, or None when it is not a
/// repository. Handles the `.git`-as-a-FILE worktree/submodule pointer form.
pub fn git_branch(project_dir: &Path) -> Option<String> {
    let dot_git = project_dir.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    let git_dir = if meta.is_file() {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let pointer = text.trim().strip_prefix("gitdir:")?.trim().to_string();
        let p = PathBuf::from(&pointer);
        if p.is_absolute() {
            p
        } else {
            project_dir.join(p)
        }
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: ") {
        Some(reference) => reference.replacen("refs/heads/", "", 1),
        // A detached HEAD: the first 12 chars of the raw sha.
        None => head.chars().take(12).collect(),
    })
}

/// The project-roots mtime scanner.
#[derive(Debug, Default)]
pub struct FilesWatcher;

impl Watcher for FilesWatcher {
    fn name(&self) -> &'static str {
        "files"
    }

    fn gate(&self, cfg: &Config) -> Gate {
        let enabled = cfg.agent.watch.files;
        let available = enabled && !cfg.agent.project_roots.is_empty();
        if enabled && available {
            Gate::Run
        } else {
            Gate::Skipped {
                enabled,
                available,
                reason: if enabled {
                    "no project roots configured".to_string()
                } else {
                    "disabled in config".to_string()
                },
            }
        }
    }

    fn input_marker(&self, _state: &Value) -> Option<String> {
        // The files watcher has no cheap input fingerprint: it discovers
        // change by scanning, so `rows.len() > 0` is the only signal.
        None
    }

    fn run(&mut self, cfg: &Config, state: &mut Value, now: f64) -> Result<Vec<Value>> {
        let interval = cfg.agent.interval_seconds;
        let previous = state
            .get("filesLastScan")
            .and_then(Value::as_f64)
            .unwrap_or(now - interval);
        // Recover if the wall clock moved backwards after the previous tick:
        // a future `filesLastScan` would otherwise suppress every file
        // forever.
        let since = if previous > now { now - interval } else { previous };
        // Set BEFORE walking, so files written during the scan are picked up
        // next tick rather than missed entirely.
        if let Some(obj) = state.as_object_mut() {
            obj.insert("filesLastScan".to_string(), json!(now));
        }

        let max_depth = cfg.agent.max_scan_depth.max(0.0) as u32;
        let mut rows = Vec::new();
        // Per-tick cache: a branch switch shows up on the next tick. None is
        // cached too, so non-repo dirs are probed once.
        let mut branch_cache: HashMap<PathBuf, Option<String>> = HashMap::new();

        for root in &cfg.agent.project_roots {
            let root_path = Path::new(root);
            let mut files = Vec::new();
            walk(root_path, &cfg.agent.ignore_dirs, 0, max_depth, &mut files);
            for file in files {
                let Ok(md) = std::fs::metadata(&file) else {
                    continue;
                };
                let Some(mtime) = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                else {
                    continue;
                };
                // Half-open window: strictly after `since` (so a file is not
                // re-sent every tick) and at most 60s into the future (so a
                // bad mtime cannot poison the timeline).
                if mtime <= since || mtime > now + 60.0 {
                    continue;
                }

                // project = the first directory level under the root, or the
                // root itself for a file sitting directly in it.
                let rel = file.strip_prefix(root_path).unwrap_or(&file);
                let mut components = rel.components();
                let top = components.next();
                let in_subdir = components.next().is_some();
                let project_dir = match (in_subdir, top) {
                    (true, Some(c)) => root_path.join(c.as_os_str()),
                    _ => root_path.to_path_buf(),
                };

                let branch = branch_cache
                    .entry(project_dir.clone())
                    .or_insert_with(|| git_branch(&project_dir))
                    .clone();

                rows.push(json!({
                    "time": mtime,
                    "source": "editor-files",
                    "project": stackhour_core::project::resolve_project_within(
                        &project_dir.to_string_lossy(),
                        root_path,
                        &cfg.agent.project_aliases,
                        None,
                    ),
                    "entity": file.to_string_lossy(),
                    "entity_type": "file",
                    "category": "coding",
                    "language": language_for(&file),
                    "branch": branch.map_or(Value::Null, Value::String),
                    "is_write": 1,
                }));
            }
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;

    fn now() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    fn config_with_roots(tmp: &TempDir, roots: &[&Path]) -> Config {
        let cfg_path = tmp.path().join("config.json");
        let roots: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        std::fs::write(
            &cfg_path,
            serde_json::to_string(&json!({
                "agent": { "projectRoots": roots, "intervalSeconds": 20 }
            }))
            .unwrap(),
        )
        .unwrap();
        stackhour_core::config::load_config(&cfg_path).unwrap()
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, "x").unwrap();
    }

    fn entities(rows: &[Value]) -> Vec<String> {
        let mut v: Vec<String> = rows
            .iter()
            .map(|r| r["entity"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn gate_requires_both_the_toggle_and_a_project_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir(&root).unwrap();
        assert_eq!(FilesWatcher.gate(&config_with_roots(&tmp, &[&root])), Gate::Run);

        // No roots -> skipped with the exact reason doctor surfaces.
        let cfg = config_with_roots(&tmp, &[]);
        match FilesWatcher.gate(&cfg) {
            Gate::Skipped {
                enabled,
                available,
                reason,
            } => {
                assert!(enabled);
                assert!(!available);
                assert_eq!(reason, "no project roots configured");
            }
            Gate::Run => panic!("must not run without roots"),
        }
    }

    /// The core scenario: a file touched after the last scan is reported.
    #[test]
    fn a_file_modified_since_the_last_scan_produces_a_row() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        let file = root.join("src").join("main.rs");
        touch(&file);
        let cfg = config_with_roots(&tmp, &[&root]);

        let mut state = json!({ "filesLastScan": now() - 60.0 });
        let rows = FilesWatcher.run(&cfg, &mut state, now()).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row["source"], "editor-files");
        assert_eq!(row["entity_type"], "file");
        assert_eq!(row["category"], "coding");
        assert_eq!(row["language"], "Rust");
        assert_eq!(row["is_write"], 1);
        assert_eq!(row["entity"], file.to_string_lossy().as_ref());
        assert!(row["time"].as_f64().unwrap() > 0.0);
    }

    /// The same file must NOT be re-reported on the next tick — otherwise
    /// every tick would double-count all recent work.
    #[test]
    fn a_second_tick_does_not_re_report_unchanged_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        touch(&root.join("a.rs"));
        let cfg = config_with_roots(&tmp, &[&root]);

        let mut state = json!({ "filesLastScan": now() - 60.0 });
        assert_eq!(FilesWatcher.run(&cfg, &mut state, now()).unwrap().len(), 1);
        let rows = FilesWatcher.run(&cfg, &mut state, now() + 1.0).unwrap();
        assert!(rows.is_empty(), "got {rows:?}");
    }

    /// `filesLastScan` is stamped before the walk, and a FUTURE value (from a
    /// clock that jumped backwards) must not suppress files forever.
    #[test]
    fn a_future_last_scan_is_recovered_from() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        touch(&root.join("a.rs"));
        let cfg = config_with_roots(&tmp, &[&root]);

        let t = now();
        // A last-scan an hour in the future would hide everything.
        let mut state = json!({ "filesLastScan": t + 3600.0 });
        let rows = FilesWatcher.run(&cfg, &mut state, t).unwrap();
        assert_eq!(rows.len(), 1, "clock-backwards recovery failed");
        assert_eq!(state["filesLastScan"].as_f64().unwrap(), t);
    }

    /// Dotted entries are skipped (except `.env`), and ignoreDirs are pruned.
    #[test]
    fn dotfiles_and_ignored_directories_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        touch(&root.join("keep.rs"));
        touch(&root.join(".env"));
        touch(&root.join(".hidden.rs"));
        touch(&root.join(".git").join("config"));
        touch(&root.join("node_modules").join("dep.js"));
        touch(&root.join("target").join("build.rs"));
        let cfg = config_with_roots(&tmp, &[&root]);

        let mut state = json!({ "filesLastScan": now() - 60.0 });
        let rows = FilesWatcher.run(&cfg, &mut state, now()).unwrap();
        assert_eq!(
            entities(&rows),
            vec![
                root.join(".env").to_string_lossy().to_string(),
                root.join("keep.rs").to_string_lossy().to_string(),
            ]
        );
    }

    /// maxScanDepth bounds the recursion.
    #[test]
    fn the_walk_respects_max_scan_depth() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        touch(&root.join("a.rs"));
        touch(&root.join("one").join("b.rs"));
        touch(&root.join("one").join("two").join("c.rs"));

        let cfg_path = tmp.path().join("config.json");
        std::fs::write(
            &cfg_path,
            serde_json::to_string(&json!({
                "agent": { "projectRoots": [root.display().to_string()], "maxScanDepth": 1 }
            }))
            .unwrap(),
        )
        .unwrap();
        let cfg = stackhour_core::config::load_config(&cfg_path).unwrap();

        let mut state = json!({ "filesLastScan": now() - 60.0 });
        let rows = FilesWatcher.run(&cfg, &mut state, now()).unwrap();
        // depth 0 = root, depth 1 = one/; two/ is past the limit.
        assert_eq!(
            entities(&rows),
            vec![
                root.join("a.rs").to_string_lossy().to_string(),
                root.join("one").join("b.rs").to_string_lossy().to_string(),
            ]
        );
    }

    /// A file with an mtime far in the future is rejected; 60s of skew is
    /// tolerated.
    #[test]
    fn files_too_far_in_the_future_are_rejected() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        touch(&root.join("a.rs"));
        let cfg = config_with_roots(&tmp, &[&root]);

        // Pretend "now" is two minutes BEFORE the file's real mtime.
        let mut state = json!({ "filesLastScan": now() - 3600.0 });
        let rows = FilesWatcher.run(&cfg, &mut state, now() - 120.0).unwrap();
        assert!(rows.is_empty(), "a +120s file should be rejected");

        // Within the 60s tolerance it is accepted.
        let mut state = json!({ "filesLastScan": now() - 3600.0 });
        let rows = FilesWatcher.run(&cfg, &mut state, now() - 30.0).unwrap();
        assert_eq!(rows.len(), 1);
    }

    /// The project is the first level under the root, not the root itself,
    /// for files nested in a subdirectory.
    #[test]
    fn project_is_the_first_directory_level_under_the_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("code");
        touch(&root.join("alpha").join("src").join("x.rs"));
        touch(&root.join("loose.rs"));
        let cfg = config_with_roots(&tmp, &[&root]);

        let mut state = json!({ "filesLastScan": now() - 60.0 });
        let rows = FilesWatcher.run(&cfg, &mut state, now()).unwrap();
        let by_entity: HashMap<String, String> = rows
            .iter()
            .map(|r| {
                (
                    r["entity"].as_str().unwrap().to_string(),
                    r["project"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            by_entity[root.join("alpha").join("src").join("x.rs").to_str().unwrap()],
            "alpha"
        );
        assert_eq!(by_entity[root.join("loose.rs").to_str().unwrap()], "code");
    }

    #[test]
    fn language_is_mapped_by_lowercased_extension_and_null_when_unknown() {
        assert_eq!(language_for(Path::new("/a/b.TS")), json!("TypeScript"));
        assert_eq!(language_for(Path::new("/a/b.py")), json!("Python"));
        assert_eq!(language_for(Path::new("/a/b.zzz")), Value::Null);
        assert_eq!(language_for(Path::new("/a/Makefile")), Value::Null);
    }

    #[test]
    fn git_branch_reads_a_symbolic_head() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(git_branch(&repo).as_deref(), Some("feature/x"));
    }

    /// A detached HEAD reports the short sha, not a branch name.
    #[test]
    fn git_branch_shortens_a_detached_head() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(
            repo.join(".git").join("HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(git_branch(&repo).as_deref(), Some("0123456789ab"));
    }

    /// Worktrees and submodules use a `.git` FILE pointing elsewhere.
    #[test]
    fn git_branch_follows_a_gitdir_pointer_file() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("worktree");
        let real = tmp.path().join("realgit");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(repo.join(".git"), format!("gitdir: {}\n", real.display())).unwrap();
        assert_eq!(git_branch(&repo).as_deref(), Some("wt"));
    }

    #[test]
    fn git_branch_is_none_outside_a_repository() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(git_branch(tmp.path()), None);
    }

    /// An unreadable directory must not abort the scan of its siblings.
    ///
    /// `chmod 000` does not make a directory unreadable for every user: root
    /// bypasses the permission check entirely, so under `sudo`, in most CI
    /// containers, and in any root shell the "locked" directory reads fine and
    /// this test would fail claiming a regression that is not there. The
    /// precondition is therefore verified rather than assumed, and the test
    /// skips when the environment cannot express it.
    #[test]
    fn an_unreadable_directory_is_skipped_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        touch(&root.join("visible.rs"));
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        touch(&locked.join("hidden.rs"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Does 0o000 actually deny US? If not, there is nothing to skip and
        // nothing to assert.
        let denied = std::fs::read_dir(&locked).is_err();
        if !denied {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!(
                "SKIPPED an_unreadable_directory_is_skipped_not_fatal: this user can read a \
                 0o000 directory (running as root?), so the unreadable case cannot be set up"
            );
            return;
        }

        let cfg = config_with_roots(&tmp, &[&root]);
        let mut state = json!({ "filesLastScan": now() - 60.0 });
        let rows = FilesWatcher.run(&cfg, &mut state, now()).unwrap();

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            entities(&rows),
            vec![root.join("visible.rs").to_string_lossy().to_string()]
        );
    }

    /// A configured root that does not exist is simply empty.
    #[test]
    fn a_missing_project_root_yields_no_rows() {
        let tmp = TempDir::new().unwrap();
        let cfg = config_with_roots(&tmp, &[&tmp.path().join("gone")]);
        let mut state = json!({ "filesLastScan": now() - 60.0 });
        assert!(FilesWatcher.run(&cfg, &mut state, now()).unwrap().is_empty());
    }
}
