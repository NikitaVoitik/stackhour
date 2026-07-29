use stackhour_core::{Error, Result};
use std::path::{Path, PathBuf};

const HELP: &str = "\
usage: stackhour migrate tempo --from=FILE [--to=FILE]

Creates a consistent online SQLite snapshot, including committed WAL data,
migrates the copied database to the current Stackhour schema, verifies its
heartbeat count, and leaves the Tempo source untouched.
";

pub fn run(args: &[String], cfg: &stackhour_core::config::Config) -> Result<()> {
    if args.iter().any(|arg| matches!(arg.as_str(), "-h" | "--help")) {
        print!("{HELP}");
        return Ok(());
    }
    reject_unknown(args)?;
    let source = option(args, "from").ok_or_else(|| Error::msg("--from is required"))?;
    let source = PathBuf::from(source);
    let destination = option(args, "to")
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.server.db.clone());
    migrate(&source, &destination)
}

fn option(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("--{name}=");
    args.iter()
        .find_map(|arg| arg.strip_prefix(&prefix).map(str::to_string))
        .filter(|value| !value.trim().is_empty())
}

fn reject_unknown(args: &[String]) -> Result<()> {
    for arg in args {
        if matches!(arg.as_str(), "-h" | "--help") || arg.starts_with("--from=") || arg.starts_with("--to=") {
            continue;
        }
        return Err(Error::msg(format!("unknown tempo migration option: {arg}")));
    }
    Ok(())
}

fn migrate(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_file() {
        return Err(Error::msg(format!(
            "Tempo database not found: {}",
            source.display()
        )));
    }
    if destination.exists() {
        return Err(Error::msg(format!(
            "Stackhour database already exists: {}",
            destination.display()
        )));
    }
    let source_before = std::fs::metadata(source)
        .map_err(|error| Error::msg(format!("{}: {error}", source.display())))?
        .modified()
        .ok();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let snapshot = stackhour_store::backup::create_backup(source, Some(destination), false, now_ms)?;
    let db = stackhour_store::db::open_db(destination)?;
    let migrated_count: i64 = db
        .query_row("SELECT count(*) FROM heartbeats", [], |row| row.get(0))
        .map_err(|error| Error::msg(format!("cannot verify migrated heartbeats: {error}")))?;
    if migrated_count != snapshot.heartbeats {
        return Err(Error::msg(format!(
            "Tempo migration count mismatch: copied {}, verified {migrated_count}",
            snapshot.heartbeats
        )));
    }
    let source_after = std::fs::metadata(source)
        .map_err(|error| Error::msg(format!("{}: {error}", source.display())))?
        .modified()
        .ok();
    if source_before != source_after {
        return Err(Error::msg(
            "Tempo source changed during verification; the copy is retained for inspection",
        ));
    }
    println!(
        "Migrated {} Tempo heartbeats to {}. Source left untouched: {}",
        migrated_count,
        destination.display(),
        source.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_uses_online_backup_and_leaves_source_untouched() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("tempo.db");
        {
            let db = stackhour_store::db::open_db(&source).unwrap();
            let table_exists: i64 = db
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='heartbeats'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(table_exists, 1);
        }
        let original = std::fs::read(&source).unwrap();
        let destination = temp.path().join("stackhour.db");
        migrate(&source, &destination).unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(stackhour_store::db::open_db(&destination).is_ok());
        assert!(migrate(&source, &destination).is_err());
    }
}
