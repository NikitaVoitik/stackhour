//! config.json load / deep-merge / typed view.
//!
//! Rule enforced in review: READS may go through the typed view; every WRITE
//! goes through the raw `serde_json::Value` (preserve_order keeps unknown keys
//! in insertion order, exactly like the JS rewrite paths).
//!
//! Deep-merge semantics: plain objects merge recursively; arrays and scalars
//! REPLACE — so a user `pricing` table replaces the whole built-in table and
//! a user `projectRoots` replaces the default wholesale.

use crate::paths::{
    expand_home, resolve_storage_paths, resolve_storage_paths_from_process_env, StoragePaths,
};
use crate::pricing::{Price, PricingTable};
use crate::{Error, Result};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Typed view of `server` (reads only — writes go through `Config::raw`).
#[derive(Debug, Clone)]
pub struct ServerCfg {
    pub port: u16,
    pub host: String,
    /// `server.db` with `~` expanded.
    pub db: PathBuf,
    /// Legacy global ingest token ("" when unset).
    pub token: String,
    /// `server.publicUrl` as written by `init server`.
    pub public_url: Option<String>,
}

/// `agent.watch` toggles.
#[derive(Debug, Clone, Copy)]
pub struct WatchCfg {
    pub files: bool,
    pub claude: bool,
    pub codex: bool,
    pub mac_apps: bool,
    pub ssh: bool,
    pub zed: bool,
}

/// One `agent.apps` entry. The string shorthand `"Zed": "zed"` is normalized
/// at load time to `{ source, category: "coding" }`.
#[derive(Debug, Clone)]
pub struct AppCfg {
    pub source: String,
    pub category: String,
    /// Optional regex (JS syntax) extracting the project from a window title.
    pub project_from_title: Option<String>,
}

/// Typed view of `agent`.
#[derive(Debug, Clone)]
pub struct AgentCfg {
    pub server_url: String,
    pub token: String,
    pub machine: String,
    pub interval_seconds: f64,
    /// `~`-expanded, order preserved.
    pub project_roots: Vec<String>,
    pub project_aliases: IndexMap<String, String>,
    pub watch: WatchCfg,
    pub apps: IndexMap<String, AppCfg>,
    pub idle_seconds: f64,
    pub ignore_dirs: Vec<String>,
    pub max_scan_depth: f64,
}

/// Typed view of `summary` (credit model knobs).
#[derive(Debug, Clone, Copy)]
pub struct SummaryCfg {
    pub cap_seconds: f64,
    pub last_event_credit_seconds: f64,
    pub reattribute_window_seconds: f64,
    pub join_gap_seconds: f64,
}

/// Typed view of `wakatime`.
#[derive(Debug, Clone)]
pub struct WakatimeCfg {
    pub api_key: String,
}

/// Loaded configuration: the raw merged Value (authoritative for rewrites and
/// for maps the HTTP auth reads leniently, e.g. `server.tokens`) plus typed
/// read views and the resolved storage paths.
#[derive(Debug, Clone)]
pub struct Config {
    /// Merged defaults + user file, key order and unknown keys preserved.
    pub raw: Value,
    pub server: ServerCfg,
    pub agent: AgentCfg,
    pub summary: SummaryCfg,
    /// `Some` only when the user config declares a `pricing` section
    /// (its presence replaces the whole built-in table — quirk kept).
    pub pricing: Option<PricingTable>,
    /// Which modules the user's `modules` block leaves enabled. Absent block,
    /// absent sub-key, or a malformed block = everything enabled.
    pub modules: crate::modules::ModuleSet,
    pub wakatime: WakatimeCfg,
    /// Storage paths resolved alongside the config (config/data/db locations).
    pub paths: StoragePaths,
}

/// The DEFAULTS table (exact values from src/config.js, including `apps`,
/// `watch` and `ignoreDirs`; `pricing` is deliberately ABSENT from defaults).
pub fn defaults() -> Value {
    let storage = resolve_storage_paths_from_process_env();
    defaults_with(&storage.db_path.to_string_lossy(), &os_hostname())
}

/// DEFAULTS with the two environment-dependent values injected (mirrors the
/// module-load-time `STORAGE.dbPath` and `os.hostname()` in src/config.js).
fn defaults_with(db_path: &str, machine: &str) -> Value {
    json!({
        "server": {
            "port": 4040,
            "host": "0.0.0.0",
            "db": db_path,
            "token": "",
            "tokens": {},
        },
        "agent": {
            "serverUrl": "http://127.0.0.1:4040",
            "token": "",
            "machine": machine,
            "intervalSeconds": 20,
            "projectRoots": [],
            // Map absolute repository paths, normalized remotes, or project
            // labels to one canonical display name shared by every machine.
            "projectAliases": {},
            "watch": { "files": true, "claude": true, "codex": true, "macApps": true, "ssh": true, "zed": true },
            // frontmost-app tracking (macOS only): process name -> source, or {source, category}
            "apps": {
                "Claude": { "source": "claude-desktop", "category": "ai coding" },
                "Codex": { "source": "codex-desktop", "category": "ai coding" },
                "ChatGPT": { "source": "codex-desktop", "category": "ai coding" },
                "WebStorm": { "source": "webstorm", "category": "coding" },
                "Zed": { "source": "zed", "category": "coding" },
            },
            "idleSeconds": 120,
            "ignoreDirs": ["node_modules", ".git", "dist", "build", "out", "target",
                ".next", ".venv", "venv", "vendor", "Library", ".cache", "Pods",
                "DerivedData", "Temp", "Logs", "obj"],
            "maxScanDepth": 8,
        },
        // credit model: each heartbeat earns time until the next one, capped.
        "summary": { "capSeconds": 120, "lastEventCreditSeconds": 60, "reattributeWindowSeconds": 120, "joinGapSeconds": 300 },
        // pricing deliberately ABSENT (JS `pricing: undefined`): a user table
        // replaces the whole built-in one because deepMerge sees no base object.
        //
        // `modules` is likewise deliberately ABSENT. Adding it here would insert
        // a new root key into every Config.raw (changing key order and making an
        // absent-key config distinguishable from today) and would make a user
        // block merge per-key instead of landing verbatim. DO NOT ADD IT.
        "wakatime": { "apiKey": "" },
    })
}

/// JS spread `{...v}` semantics: objects clone; arrays become objects with
/// stringified index keys; strings enumerate their chars; every other value
/// spreads to `{}`.
fn spread_into_map(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        Value::Array(a) => a
            .into_iter()
            .enumerate()
            .map(|(i, x)| (i.to_string(), x))
            .collect(),
        Value::String(s) => s
            .chars()
            .enumerate()
            .map(|(i, c)| (i.to_string(), Value::String(c.to_string())))
            .collect(),
        _ => Map::new(),
    }
}

/// `Object.entries(v)` semantics for the values `deepMerge` can receive:
/// objects yield entries, arrays yield index/value pairs, strings yield
/// index/char pairs, everything else yields nothing.
fn js_entries(v: Value) -> Vec<(String, Value)> {
    spread_into_map(v).into_iter().collect()
}

/// Deep merge: plain objects merge recursively; arrays/scalars replace.
///
/// Faithful port of src/config.js deepMerge, including the corner where a
/// base ARRAY under a user object is spread into an index-keyed object
/// (`typeof [] === 'object'` in JS, only `v` is Array-checked).
pub fn deep_merge(base: Value, user: Value) -> Value {
    let mut out = spread_into_map(base);
    for (k, v) in js_entries(user) {
        let merged = if v.is_object() {
            // v is a plain (non-array, non-null) object; recurse only when
            // base[k] is a truthy object (JS: object OR array; null is falsy).
            match out.get(&k) {
                Some(b @ (Value::Object(_) | Value::Array(_))) => deep_merge(b.clone(), v),
                _ => v,
            }
        } else {
            v
        };
        out.insert(k, merged);
    }
    Value::Object(out)
}

/// JS truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// Minimal `Number(v)` coercion used by the typed READ views. The full
/// golden-tested implementation lives in [`crate::jsnum`]; this local copy
/// only needs to cover values plausible in a JSON config file (numbers,
/// numeric strings, booleans, null) and deliberately avoids depending on the
/// sibling module while it is under construction.
fn js_number_of(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                u64::from_str_radix(hex, 16).map(|n| n as f64).unwrap_or(f64::NAN)
            } else if t == "Infinity" || t == "+Infinity" {
                f64::INFINITY
            } else if t == "-Infinity" {
                f64::NEG_INFINITY
            } else if t.eq_ignore_ascii_case("inf")
                || t.eq_ignore_ascii_case("infinity")
                || t.eq_ignore_ascii_case("nan")
            {
                // Rust's f64 parser accepts these; JS Number() does not.
                f64::NAN
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        _ => f64::NAN,
    }
}

/// `String(number)` formatting for the common cases: integral values print
/// without a fractional part, -0 prints "0".
fn fmt_js_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(f64::NAN);
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if f == 0.0 {
        return "0".to_string();
    }
    if f == f.trunc() && f.abs() < 1e21 {
        return format!("{f:.0}");
    }
    f.to_string()
}

/// `String(v)` semantics for the typed views (aliases may carry non-string
/// values that JS String()-coerces at use time).
fn js_display(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => fmt_js_number(n),
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|item| match item {
                // JS Array.prototype.toString maps null/undefined to ''.
                Value::Null => String::new(),
                other => js_display(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// String field with a default: strings pass through verbatim, non-null
/// non-string values are String()-coerced, null/missing yields the default.
fn string_field(obj: Option<&Value>, key: &str, default: &str) -> String {
    match obj.and_then(|o| o.get(key)) {
        None | Some(Value::Null) => default.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => js_display(other),
    }
}

/// Numeric field with a default for missing keys; present values go through
/// `Number()` coercion (NaN propagates, matching the JS raw read).
fn num_field(obj: Option<&Value>, key: &str, default: f64) -> f64 {
    match obj.and_then(|o| o.get(key)) {
        None => default,
        Some(v) => js_number_of(v),
    }
}

fn watch_flag(watch: Option<&Value>, key: &str) -> bool {
    watch.and_then(|w| w.get(key)).map(truthy).unwrap_or(false)
}

fn build_server(raw: &Value) -> ServerCfg {
    let s = raw.get("server");
    let port_num = num_field(s, "port", 4040.0);
    let port = if port_num.is_finite() && (1.0..=65535.0).contains(&port_num) {
        port_num as u16
    } else {
        4040
    };
    let public_url = match s.and_then(|o| o.get("publicUrl")) {
        None | Some(Value::Null) => None,
        Some(Value::String(u)) => Some(u.clone()),
        Some(other) => Some(js_display(other)),
    };
    ServerCfg {
        port,
        host: string_field(s, "host", "0.0.0.0"),
        db: PathBuf::from(string_field(s, "db", "")),
        token: string_field(s, "token", ""),
        public_url,
    }
}

fn build_agent(raw: &Value) -> AgentCfg {
    let a = raw.get("agent");
    let watch = a.and_then(|o| o.get("watch"));

    let project_roots = match a.and_then(|o| o.get("projectRoots")) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };

    let project_aliases = match a.and_then(|o| o.get("projectAliases")) {
        Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), js_display(v))).collect(),
        _ => IndexMap::new(),
    };

    let apps = match a.and_then(|o| o.get("apps")) {
        Some(Value::Object(m)) => m
            .iter()
            .filter_map(|(name, v)| {
                let entry = v.as_object()?;
                let category = match entry.get("category") {
                    Some(c) if truthy(c) => js_display(c),
                    // JS use site: `mapped.category || 'coding'`.
                    _ => "coding".to_string(),
                };
                let project_from_title = match entry.get("projectFromTitle") {
                    Some(p) if truthy(p) => Some(js_display(p)),
                    _ => None,
                };
                Some((
                    name.clone(),
                    AppCfg {
                        source: string_field(Some(v), "source", ""),
                        category,
                        project_from_title,
                    },
                ))
            })
            .collect(),
        _ => IndexMap::new(),
    };

    let ignore_dirs = match a.and_then(|o| o.get("ignoreDirs")) {
        // Only string entries can ever match a directory name under JS
        // strict-equality includes(); other types are dropped.
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };

    AgentCfg {
        server_url: string_field(a, "serverUrl", "http://127.0.0.1:4040"),
        token: string_field(a, "token", ""),
        machine: string_field(a, "machine", ""),
        interval_seconds: num_field(a, "intervalSeconds", 20.0),
        project_roots,
        project_aliases,
        watch: WatchCfg {
            files: watch_flag(watch, "files"),
            claude: watch_flag(watch, "claude"),
            codex: watch_flag(watch, "codex"),
            mac_apps: watch_flag(watch, "macApps"),
            ssh: watch_flag(watch, "ssh"),
            zed: watch_flag(watch, "zed"),
        },
        apps,
        idle_seconds: num_field(a, "idleSeconds", 120.0),
        ignore_dirs,
        max_scan_depth: num_field(a, "maxScanDepth", 8.0),
    }
}

fn build_summary(raw: &Value) -> SummaryCfg {
    let s = raw.get("summary");
    SummaryCfg {
        cap_seconds: num_field(s, "capSeconds", 120.0),
        last_event_credit_seconds: num_field(s, "lastEventCreditSeconds", 60.0),
        reattribute_window_seconds: num_field(s, "reattributeWindowSeconds", 120.0),
        join_gap_seconds: num_field(s, "joinGapSeconds", 300.0),
    }
}

fn build_pricing(raw: &Value) -> Option<PricingTable> {
    let v = raw.get("pricing")?;
    if !truthy(v) {
        // JS `pricing || DEFAULT_PRICING`: a falsy table falls back wholesale.
        return None;
    }
    match v {
        Value::Object(m) => Some(
            m.iter()
                .filter_map(|(k, entry)| {
                    serde_json::from_value::<Price>(entry.clone())
                        .ok()
                        .map(|p| (k.clone(), p))
                })
                .collect(),
        ),
        // Truthy non-object: JS iterates zero model keys and falls back to
        // the built-in default entry — an empty table reproduces that.
        _ => Some(PricingTable::new()),
    }
}

/// Load and merge the config at `path`. Missing file -> pure defaults; a
/// parse error propagates untouched (this is how a corrupt config crashes
/// even `stackhour help`). Applies apps normalisation and `~` expansion of
/// `server.db` and `agent.projectRoots`.
pub fn load_config(path: &Path) -> Result<Config> {
    let env = |key: &str| std::env::var(key).ok();
    let home = std::env::var("HOME").unwrap_or_default();
    load_config_with(path, &env, Path::new(&home), &os_hostname())
}

/// Injectable-environment variant (unit tests; parity: the JS module resolves
/// storage and hostname from the real process env at import time).
fn load_config_with(
    path: &Path,
    env: &dyn Fn(&str) -> Option<String>,
    home: &Path,
    machine: &str,
) -> Result<Config> {
    // Mirror initServer's `resolveStoragePaths({...env, STACKHOUR_CONFIG: configPath})`:
    // the config path is the one we were given; data/db still follow the env.
    let forced = |key: &str| {
        if key == "STACKHOUR_CONFIG" {
            Some(path.to_string_lossy().into_owned())
        } else {
            env(key)
        }
    };
    let storage = resolve_storage_paths(&forced, home);

    let user: Value = if path.exists() {
        let text = std::fs::read_to_string(path)?;
        // Parse errors propagate untouched (JS throws the raw SyntaxError).
        serde_json::from_str(&text)?
    } else {
        json!({})
    };

    let mut raw = deep_merge(defaults_with(&storage.db_path.to_string_lossy(), machine), user);

    // cfg.server.db = expandHome(cfg.server.db)
    if let Some(server) = raw.get_mut("server").and_then(Value::as_object_mut) {
        if let Some(db) = server.get_mut("db") {
            if let Value::String(s) = db {
                *db = Value::String(expand_home(s, home));
            }
        }
    }

    if let Some(agent) = raw.get_mut("agent").and_then(Value::as_object_mut) {
        // cfg.agent.projectRoots = (cfg.agent.projectRoots || []).map(expandHome)
        match agent.get("projectRoots") {
            Some(Value::Array(_)) => {
                if let Some(Value::Array(items)) = agent.get_mut("projectRoots") {
                    for item in items.iter_mut() {
                        if let Value::String(s) = item {
                            *item = Value::String(expand_home(s, home));
                        }
                    }
                }
            }
            Some(v) if truthy(v) => {
                // Truthy non-array: JS would crash on .map; leave it in the
                // raw view and let the typed view degrade to [].
            }
            _ => {
                agent.insert("projectRoots".to_string(), json!([]));
            }
        }
        // normalize app entries: allow plain-string shorthand
        if let Some(Value::Object(apps)) = agent.get_mut("apps") {
            for (_name, v) in apps.iter_mut() {
                if let Value::String(s) = v {
                    *v = json!({ "source": s.clone(), "category": "coding" });
                }
            }
        }
    }

    let server = build_server(&raw);
    let agent = build_agent(&raw);
    let summary = build_summary(&raw);
    let pricing = build_pricing(&raw);
    // Read post-merge like `pricing`; NEVER mutate `raw` and NEVER fail —
    // `load_config` has exactly two failure modes (IO, parse) and a third
    // would change `stackhour help`/`serve`/`status`/`doctor` for malformed input.
    let modules = crate::modules::from_raw(&raw);
    let wakatime = WakatimeCfg {
        api_key: string_field(raw.get("wakatime"), "apiKey", ""),
    };

    Ok(Config {
        raw,
        server,
        agent,
        summary,
        pricing,
        modules,
        wakatime,
        paths: storage,
    })
}

/// Read the existing raw user config for a rewrite path: ENOENT -> `{}`,
/// any other failure -> error `cannot read existing config: <detail>`.
pub fn read_existing_raw(path: &Path) -> Result<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(e) => {
            return Err(Error::msg(format!("cannot read existing config: {e}")));
        }
    };
    serde_json::from_str(&text).map_err(|e| Error::msg(format!("cannot read existing config: {e}")))
}

fn os_hostname() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/test";
    const MACHINE: &str = "testhost";

    fn no_env(_key: &str) -> Option<String> {
        None
    }

    fn load_str(dir: &tempfile::TempDir, contents: Option<&str>) -> Result<Config> {
        let path = dir.path().join("config.json");
        if let Some(text) = contents {
            std::fs::write(&path, text).expect("write config");
        }
        load_config_with(&path, &no_env, Path::new(HOME), MACHINE)
    }

    // ---- defaults ------------------------------------------------------

    #[test]
    fn defaults_exact_values() {
        let d = defaults_with("/data/stackhour.db", "mach");
        assert_eq!(d["server"]["port"], json!(4040));
        assert_eq!(d["server"]["host"], json!("0.0.0.0"));
        assert_eq!(d["server"]["db"], json!("/data/stackhour.db"));
        assert_eq!(d["server"]["token"], json!(""));
        assert_eq!(d["server"]["tokens"], json!({}));
        assert_eq!(d["agent"]["serverUrl"], json!("http://127.0.0.1:4040"));
        assert_eq!(d["agent"]["machine"], json!("mach"));
        assert_eq!(d["agent"]["intervalSeconds"], json!(20));
        assert_eq!(d["agent"]["idleSeconds"], json!(120));
        assert_eq!(d["agent"]["maxScanDepth"], json!(8));
        assert_eq!(
            d["agent"]["watch"],
            json!({ "files": true, "claude": true, "codex": true, "macApps": true, "ssh": true, "zed": true })
        );
        assert_eq!(
            d["agent"]["apps"]["Claude"],
            json!({ "source": "claude-desktop", "category": "ai coding" })
        );
        assert_eq!(
            d["agent"]["apps"]["ChatGPT"],
            json!({ "source": "codex-desktop", "category": "ai coding" })
        );
        assert_eq!(
            d["agent"]["apps"]["Zed"],
            json!({ "source": "zed", "category": "coding" })
        );
        let app_names: Vec<&String> = d["agent"]["apps"].as_object().unwrap().keys().collect();
        assert_eq!(app_names, ["Claude", "Codex", "ChatGPT", "WebStorm", "Zed"]);
        assert_eq!(
            d["agent"]["ignoreDirs"],
            json!([
                "node_modules",
                ".git",
                "dist",
                "build",
                "out",
                "target",
                ".next",
                ".venv",
                "venv",
                "vendor",
                "Library",
                ".cache",
                "Pods",
                "DerivedData",
                "Temp",
                "Logs",
                "obj"
            ])
        );
        assert_eq!(
            d["summary"],
            json!({ "capSeconds": 120, "lastEventCreditSeconds": 60, "reattributeWindowSeconds": 120, "joinGapSeconds": 300 })
        );
        assert_eq!(d["wakatime"], json!({ "apiKey": "" }));
        // pricing is ABSENT, not null.
        assert!(d.get("pricing").is_none());
    }

    // ---- deep_merge ----------------------------------------------------

    #[test]
    fn deep_merge_objects_recursively() {
        let base = json!({ "a": { "x": 1, "y": 2 }, "b": 3 });
        let user = json!({ "a": { "y": 9, "z": 8 } });
        assert_eq!(
            deep_merge(base, user),
            json!({ "a": { "x": 1, "y": 9, "z": 8 }, "b": 3 })
        );
    }

    #[test]
    fn deep_merge_replaces_arrays_and_scalars() {
        let base = json!({ "arr": [1, 2, 3], "n": 5, "o": { "k": 1 } });
        let user = json!({ "arr": [9], "n": 7, "o": null });
        // Arrays and scalars replace; null replaces an object wholesale.
        assert_eq!(deep_merge(base, user), json!({ "arr": [9], "n": 7, "o": null }));
    }

    #[test]
    fn deep_merge_object_over_scalar_replaces() {
        let base = json!({ "a": 5 });
        let user = json!({ "a": { "x": 1 } });
        assert_eq!(deep_merge(base, user), json!({ "a": { "x": 1 } }));
    }

    #[test]
    fn deep_merge_base_array_under_user_object_spreads() {
        // JS: typeof [] === 'object', only v is Array-checked, so the base
        // array is spread into an index-keyed object and merged.
        let base = json!({ "a": [10, 20] });
        let user = json!({ "a": { "x": 1 } });
        assert_eq!(
            deep_merge(base, user),
            json!({ "a": { "0": 10, "1": 20, "x": 1 } })
        );
    }

    #[test]
    fn deep_merge_preserves_unknown_keys_and_order() {
        let base = json!({ "server": { "port": 4040 }, "agent": {} });
        let user = json!({ "custom": { "hello": true } });
        let merged = deep_merge(base, user);
        let keys: Vec<&String> = merged.as_object().unwrap().keys().collect();
        // Base keys keep their positions; new keys append.
        assert_eq!(keys, ["server", "agent", "custom"]);
        assert_eq!(merged["custom"]["hello"], json!(true));
    }

    #[test]
    fn deep_merge_non_object_user_yields_base() {
        let base = json!({ "a": 1 });
        assert_eq!(deep_merge(base.clone(), json!(5)), base);
        assert_eq!(deep_merge(base.clone(), json!(null)), base);
    }

    // ---- load_config ---------------------------------------------------

    #[test]
    fn missing_file_yields_pure_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(&dir, None).unwrap();
        assert_eq!(cfg.server.port, 4040);
        assert_eq!(cfg.server.host, "0.0.0.0");
        assert_eq!(cfg.server.token, "");
        assert!(cfg.server.public_url.is_none());
        assert_eq!(cfg.agent.server_url, "http://127.0.0.1:4040");
        assert_eq!(cfg.agent.machine, MACHINE);
        assert_eq!(cfg.agent.interval_seconds, 20.0);
        assert!(cfg.agent.watch.files && cfg.agent.watch.zed && cfg.agent.watch.ssh);
        assert_eq!(cfg.agent.apps.len(), 5);
        assert_eq!(cfg.agent.apps["WebStorm"].source, "webstorm");
        assert_eq!(cfg.agent.apps["WebStorm"].category, "coding");
        assert_eq!(cfg.agent.ignore_dirs.len(), 17);
        assert_eq!(cfg.summary.cap_seconds, 120.0);
        assert_eq!(cfg.summary.join_gap_seconds, 300.0);
        assert!(cfg.pricing.is_none());
        assert_eq!(cfg.wakatime.api_key, "");
        // Default db lives under the resolved data dir.
        assert_eq!(
            cfg.server.db,
            PathBuf::from(format!("{HOME}/.local/share/stackhour/stackhour.db"))
        );
        // pricing key absent from the merged raw too.
        assert!(cfg.raw.get("pricing").is_none());
    }

    #[test]
    fn parse_error_propagates_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_str(&dir, Some("{ not json")).unwrap_err();
        // The raw serde message — NOT wrapped in 'cannot read existing config'.
        assert!(!err.message().starts_with("cannot read existing config"));
        assert!(!err.message().is_empty());
    }

    #[test]
    fn user_values_merge_over_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(
            &dir,
            Some(
                r#"{
                  "server": { "port": 8080, "token": "sekrit", "publicUrl": "http://ex.example" },
                  "agent": { "machine": "laptop", "watch": { "files": false } },
                  "summary": { "capSeconds": 90 },
                  "extraTopLevel": { "kept": true }
                }"#,
            ),
        )
        .unwrap();
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.server.token, "sekrit");
        assert_eq!(cfg.server.public_url.as_deref(), Some("http://ex.example"));
        assert_eq!(cfg.agent.machine, "laptop");
        // watch merges key-by-key: files off, the rest still on.
        assert!(!cfg.agent.watch.files);
        assert!(cfg.agent.watch.claude && cfg.agent.watch.mac_apps);
        assert_eq!(cfg.summary.cap_seconds, 90.0);
        assert_eq!(cfg.summary.last_event_credit_seconds, 60.0);
        // Unknown top-level keys survive in raw (append order).
        assert_eq!(cfg.raw["extraTopLevel"]["kept"], json!(true));
        let keys: Vec<&String> = cfg.raw.as_object().unwrap().keys().collect();
        assert_eq!(keys.last().unwrap().as_str(), "extraTopLevel");
    }

    #[test]
    fn tilde_expansion_of_db_and_project_roots() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(
            &dir,
            Some(
                r#"{
                  "server": { "db": "~/data/sh.db" },
                  "agent": { "projectRoots": ["~/code", "/abs/path", "~x"] }
                }"#,
            ),
        )
        .unwrap();
        assert_eq!(cfg.server.db, PathBuf::from(format!("{HOME}/data/sh.db")));
        assert_eq!(
            cfg.agent.project_roots,
            vec![
                format!("{HOME}/code"),
                "/abs/path".to_string(),
                format!("{HOME}/x"),
            ]
        );
        // Raw view carries the expanded values too (single shared object in JS).
        assert_eq!(cfg.raw["server"]["db"], json!(format!("{HOME}/data/sh.db")));
        assert_eq!(cfg.raw["agent"]["projectRoots"][0], json!(format!("{HOME}/code")));
    }

    #[test]
    fn apps_string_shorthand_normalizes() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(
            &dir,
            Some(r#"{ "agent": { "apps": { "Ghostty": "terminal" } } }"#),
        )
        .unwrap();
        // Normalized in the raw view...
        assert_eq!(
            cfg.raw["agent"]["apps"]["Ghostty"],
            json!({ "source": "terminal", "category": "coding" })
        );
        // ...and in the typed view; defaults still merged in per-key.
        assert_eq!(cfg.agent.apps["Ghostty"].source, "terminal");
        assert_eq!(cfg.agent.apps["Ghostty"].category, "coding");
        assert!(cfg.agent.apps["Ghostty"].project_from_title.is_none());
        assert_eq!(cfg.agent.apps["Claude"].source, "claude-desktop");
        assert_eq!(cfg.agent.apps.len(), 6);
    }

    #[test]
    fn app_project_from_title_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(
            &dir,
            Some(r#"{ "agent": { "apps": { "Foo": { "source": "foo", "projectFromTitle": "^(\\w+)" } } } }"#),
        )
        .unwrap();
        let foo = &cfg.agent.apps["Foo"];
        assert_eq!(foo.source, "foo");
        assert_eq!(foo.category, "coding"); // `|| 'coding'` fallback
        assert_eq!(foo.project_from_title.as_deref(), Some("^(\\w+)"));
    }

    #[test]
    fn user_pricing_replaces_whole_table() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(
            &dir,
            Some(r#"{ "pricing": { "my-model": { "in": 1, "out": 2, "cacheRead": 0.1, "cacheWrite": 0.2 } } }"#),
        )
        .unwrap();
        let table = cfg.pricing.expect("pricing table set");
        // ONLY the user's keys — no per-model merge with built-in defaults.
        assert_eq!(table.len(), 1);
        let p = &table["my-model"];
        assert_eq!(p.in_, 1.0);
        assert_eq!(p.out, 2.0);
        assert_eq!(p.cache_read, 0.1);
        assert_eq!(p.cache_write, 0.2);
        // Raw view holds it verbatim.
        assert_eq!(cfg.raw["pricing"]["my-model"]["in"], json!(1));
    }

    #[test]
    fn null_pricing_behaves_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(&dir, Some(r#"{ "pricing": null }"#)).unwrap();
        // Falsy table -> built-in fallback (priceFor's `pricing || DEFAULT`).
        assert!(cfg.pricing.is_none());
        // But the raw view keeps the user's null (rewrite fidelity).
        assert_eq!(cfg.raw["pricing"], json!(null));
    }

    #[test]
    fn ignore_dirs_replace_wholesale() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(&dir, Some(r#"{ "agent": { "ignoreDirs": ["only-this"] } }"#)).unwrap();
        assert_eq!(cfg.agent.ignore_dirs, vec!["only-this"]);
    }

    #[test]
    fn tokens_map_lives_in_raw() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(
            &dir,
            Some(r#"{ "server": { "tokens": { "mac": "t1", "gcp": "t2" } } }"#),
        )
        .unwrap();
        // Reads of the tokens map go through raw (auth is deliberately lenient).
        assert_eq!(cfg.raw["server"]["tokens"]["mac"], json!("t1"));
        assert_eq!(cfg.raw["server"]["tokens"]["gcp"], json!("t2"));
    }

    #[test]
    fn storage_paths_follow_env_and_given_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        let env = |key: &str| {
            if key == "STACKHOUR_DATA" {
                Some("/var/lib/sh".to_string())
            } else {
                None
            }
        };
        let cfg = load_config_with(&path, &env, Path::new(HOME), MACHINE).unwrap();
        assert_eq!(cfg.paths.config_path, path);
        assert_eq!(cfg.paths.data_dir, PathBuf::from("/var/lib/sh"));
        // Default db comes from the env-resolved data dir.
        assert_eq!(cfg.server.db, PathBuf::from("/var/lib/sh/stackhour.db"));
    }

    // ---- read_existing_raw --------------------------------------------

    #[test]
    fn read_existing_raw_enoent_yields_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let v = read_existing_raw(&dir.path().join("nope.json")).unwrap();
        assert_eq!(v, json!({}));
    }

    #[test]
    fn read_existing_raw_parse_error_is_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ broken").unwrap();
        let err = read_existing_raw(&path).unwrap_err();
        assert!(err.message().starts_with("cannot read existing config: "));
    }

    #[test]
    fn read_existing_raw_returns_user_value_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{ "agent": { "token": "x" }, "zzz": 1 }"#).unwrap();
        let v = read_existing_raw(&path).unwrap();
        assert_eq!(v, json!({ "agent": { "token": "x" }, "zzz": 1 }));
    }

    // ---- helpers -------------------------------------------------------

    #[test]
    fn truthiness_matches_js() {
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!(false)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!("x")));
        assert!(truthy(&json!(1)));
        assert!(truthy(&json!({})));
        assert!(truthy(&json!([])));
    }

    #[test]
    fn js_display_matches_string_coercion() {
        assert_eq!(js_display(&json!(4040)), "4040");
        assert_eq!(js_display(&json!(4040.0)), "4040");
        assert_eq!(js_display(&json!(1.5)), "1.5");
        assert_eq!(js_display(&json!(true)), "true");
        assert_eq!(js_display(&json!(null)), "null");
        assert_eq!(js_display(&json!([1, null, "a"])), "1,,a");
        assert_eq!(js_display(&json!({"k": 1})), "[object Object]");
    }

    #[test]
    fn js_number_of_covers_config_cases() {
        assert_eq!(js_number_of(&json!(20)), 20.0);
        assert_eq!(js_number_of(&json!("20")), 20.0);
        assert_eq!(js_number_of(&json!(" 1.5 ")), 1.5);
        assert_eq!(js_number_of(&json!("")), 0.0);
        assert_eq!(js_number_of(&json!(null)), 0.0);
        assert_eq!(js_number_of(&json!(true)), 1.0);
        assert_eq!(js_number_of(&json!("0x10")), 16.0);
        assert!(js_number_of(&json!("abc")).is_nan());
        assert!(js_number_of(&json!({})).is_nan());
    }

    #[test]
    fn project_aliases_are_string_coerced() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(
            &dir,
            Some(r#"{ "agent": { "projectAliases": { "github.com/a/b": "myproj", "n": 7 } } }"#),
        )
        .unwrap();
        assert_eq!(cfg.agent.project_aliases["github.com/a/b"], "myproj");
        assert_eq!(cfg.agent.project_aliases["n"], "7");
    }

    // ---- modules -------------------------------------------------------

    #[test]
    fn a_config_without_modules_enables_every_module() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(&dir, None).unwrap();
        assert_eq!(cfg.modules, crate::modules::ModuleSet::ALL);
        let cfg = load_str(&dir, Some(r#"{ "server": { "port": 1 } }"#)).unwrap();
        assert_eq!(cfg.modules, crate::modules::ModuleSet::ALL);
        assert!(cfg.raw.get("modules").is_none());
    }

    /// `modules` must stay out of DEFAULTS for the same reason `pricing` does:
    /// a config with no `modules` key must be indistinguishable from today.
    #[test]
    fn modules_is_absent_from_the_defaults_table() {
        assert!(defaults().get("modules").is_none());
        assert!(defaults_with("/data/stackhour.db", "mach")
            .get("modules")
            .is_none());
    }

    #[test]
    fn a_user_modules_block_lands_in_raw_verbatim_and_last() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(&dir, Some(r#"{ "modules": { "control": false } }"#)).unwrap();
        let keys: Vec<&String> = cfg.raw.as_object().unwrap().keys().collect();
        assert_eq!(keys.last().unwrap().as_str(), "modules");
        assert_eq!(cfg.raw["modules"], json!({ "control": false }));
    }

    #[test]
    fn a_disabled_module_reaches_the_typed_view() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_str(&dir, Some(r#"{ "modules": { "control": false } }"#)).unwrap();
        assert_eq!(cfg.modules, crate::modules::ModuleSet::new(true, true, false));
        assert!(!cfg.modules.contains(crate::modules::Module::Control));
        // A malformed block still fails open.
        let cfg = load_str(&dir, Some(r#"{ "modules": 3 }"#)).unwrap();
        assert_eq!(cfg.modules, crate::modules::ModuleSet::ALL);
    }
}
