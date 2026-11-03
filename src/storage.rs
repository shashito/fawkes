//! Storage snapshots and restores with SQLite's online backup API, so a live WAL-mode database is copied
//! consistently, uncheckpointed writes included.

use rusqlite::{Connection, OpenFlags, backup::Backup};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// One line of `snapshot/index.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    path: String,
    file: String,
    present: bool,
}

/// Copy the database `src` to a fresh file `dst`.
fn backup_file(src: &Path, dst: &Path) -> Result<(), String> {
    let fail =
        |e: rusqlite::Error| format!("cannot copy {} to {}: {e}", src.display(), dst.display());
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    remove_database(dst)?;
    let from = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(fail)?;
    let mut to = Connection::open(dst).map_err(fail)?;
    Backup::new(&from, &mut to)
        .and_then(|b| b.run_to_completion(100, Duration::from_millis(10), None))
        .map_err(fail)
}

/// Delete a database file together with its `-wal`, `-shm` and `-journal`.
fn remove_database(path: &Path) -> Result<(), String> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        match std::fs::remove_file(&name) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot delete {}: {e}", Path::new(&name).display())),
        }
    }
    Ok(())
}

/// Snapshot every storage file of `home` into `dir`. A file that does not exist is recorded as absent.
pub fn snapshot(home: &Path, storage: &[String], dir: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut index = Vec::new();
    for (n, path) in storage.iter().enumerate() {
        let source = home.join(path);
        let entry = Entry {
            path: path.clone(),
            file: format!("{n}.sqlite"),
            present: source.exists(),
        };
        if entry.present {
            backup_file(&source, &dir.join(&entry.file))?;
        }
        index.push(entry);
    }
    // Written last and durably: a rollback trusts a snapshot only through its index.
    let json = serde_json::to_vec_pretty(&index).map_err(|e| e.to_string())?;
    crate::state::write_durably(&dir.join("index.json"), &json)
        .map_err(|e| format!("cannot write index.json: {e}"))
}

/// Put the storage of `home` back as `snapshot` found it. The app must be stopped.
pub fn restore(home: &Path, dir: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(dir.join("index.json"))
        .map_err(|e| format!("no usable snapshot in {}: {e}", dir.display()))?;
    let index: Vec<Entry> =
        serde_json::from_str(&text).map_err(|e| format!("corrupt snapshot index: {e}"))?;
    for entry in index {
        let live = home.join(&entry.path);
        if entry.present {
            backup_file(&dir.join(&entry.file), &live)?;
        } else {
            remove_database(&live)?;
        }
    }
    Ok(())
}

/// Copy every storage file of `home` to the same relative path under `dest`, for a test run.
pub fn copy(home: &Path, storage: &[String], dest: &Path) -> Result<(), String> {
    for path in storage {
        if home.join(path).exists() {
            backup_file(&home.join(path), &dest.join(path))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(path: &Path) -> i64 {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap()
    }

    /// A WAL-mode database whose writes stay in the `-wal` file because the connection never checkpoints.
    fn wal_database(path: &Path, rows: i64) -> Connection {
        let db = Connection::open(path).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t (n INTEGER)",
        )
        .unwrap();
        for n in 0..rows {
            db.execute("INSERT INTO t VALUES (?)", [n]).unwrap();
        }
        db
    }

    #[test]
    fn snapshot_and_restore_wal_database() {
        let root = std::env::temp_dir().join(format!("fk-storage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, snap) = (root.join("home"), root.join("snap"));
        std::fs::create_dir_all(&home).unwrap();
        let storage = vec!["a.sqlite".to_string(), "later.sqlite".to_string()];

        // Three rows exist only in the WAL while the snapshot is taken from the live, open database.
        let live = wal_database(&home.join("a.sqlite"), 3);
        assert!(std::fs::metadata(home.join("a.sqlite-wal")).unwrap().len() > 0);
        snapshot(&home, &storage, &snap).unwrap();

        // Afterwards the app writes more and creates a file that did not exist at snapshot time.
        live.execute("INSERT INTO t VALUES (99)", []).unwrap();
        let later = wal_database(&home.join("later.sqlite"), 1);
        assert!(home.join("later.sqlite-wal").exists());

        restore(&home, &snap).unwrap();
        assert_eq!(count(&home.join("a.sqlite")), 3);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            assert!(
                !home.join(format!("later.sqlite{suffix}")).exists(),
                "later.sqlite{suffix} survived"
            );
        }

        drop((live, later));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
