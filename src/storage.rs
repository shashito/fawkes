//! Storage snapshots and restores with SQLite's online backup API, so a live WAL-mode database is copied
//! consistently, uncheckpointed writes included.

use crate::manifest::is_inside;
use crate::state::{with_suffix, write_durably};
use rusqlite::backup::StepResult;
use rusqlite::{Connection, OpenFlags, backup::Backup};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long a copy waits for a database that another connection has locked, before it gives up. A copy takes the
/// source in one read transaction, so only a writer holding an exclusive lock (rollback-journal mode, or
/// `locking_mode=EXCLUSIVE`) can keep it out; no healthy app holds one for this long, and the step slot that
/// `test` holds stays taken while the copy waits.
const LOCKED_FOR: Duration = Duration::from_secs(10);

/// How long a copy sleeps before it asks a locked database again.
const LOCKED_RETRY: Duration = Duration::from_millis(10);

/// One line of `snapshot/index.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    path: String,
    file: String,
    present: bool,
}

/// Copy the database `src` to a fresh file `dst`, all pages in one step: one read transaction, so the copy is
/// consistent, takes no pauses while the app is stopped, and, under WAL, neither blocks the app's writers nor
/// restarts when they commit (a paced backup restarts on every write and may never end).
///
/// `dst` gets the mode of `src` before any page is written, so a snapshot keeps the live file's mode and a restore
/// gives it back; SQLite gives the `-wal`, `-shm` and `-journal` it creates later the mode of the database.
fn backup_file(src: &Path, dst: &Path) -> Result<(), String> {
    let fail =
        |e: rusqlite::Error| format!("cannot copy {} to {}: {e}", src.display(), dst.display());
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    remove_database(dst)?;
    let permissions = std::fs::metadata(src)
        .map_err(|e| format!("cannot read {}: {e}", src.display()))?
        .permissions();
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)
        .and_then(|file| file.set_permissions(permissions))
        .map_err(|e| format!("cannot create {}: {e}", dst.display()))?;
    let from = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(fail)?;
    let mut to = Connection::open(dst).map_err(fail)?;
    let backup = Backup::new(&from, &mut to).map_err(fail)?;
    let give_up = Instant::now() + LOCKED_FOR;
    loop {
        // `More` does not happen for -1 pages; stepping again is right if it ever does.
        match backup.step(-1).map_err(fail)? {
            StepResult::Done => return Ok(()),
            StepResult::Busy | StepResult::Locked if Instant::now() >= give_up => {
                return Err(format!(
                    "cannot copy {} to {}: the database stayed locked for {} s",
                    src.display(),
                    dst.display(),
                    LOCKED_FOR.as_secs()
                ));
            }
            StepResult::Busy | StepResult::Locked => std::thread::sleep(LOCKED_RETRY),
            StepResult::More => {}
            other => {
                return Err(format!(
                    "cannot copy {} to {}: unexpected backup step result {other:?}",
                    src.display(),
                    dst.display()
                ));
            }
        }
    }
}

/// Delete a database file together with its `-wal`, `-shm` and `-journal`.
fn remove_database(path: &Path) -> Result<(), String> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let name = with_suffix(path, suffix);
        if let Err(e) = std::fs::remove_file(&name)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(format!("cannot delete {}: {e}", name.display()));
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
    write_durably(&dir.join("index.json"), &json)
        .map_err(|e| format!("cannot write index.json: {e}"))
}

/// Put the storage of `home` back as `snapshot` found it. The app must be stopped.
pub fn restore(home: &Path, dir: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(dir.join("index.json"))
        .map_err(|e| format!("no usable snapshot in {}: {e}", dir.display()))?;
    let index: Vec<Entry> =
        serde_json::from_str(&text).map_err(|e| format!("corrupt snapshot index: {e}"))?;
    // Checked again, as the manifest's `storage` was: whatever wrote this file, a restore deletes and writes only
    // inside `home`, and reads only the snapshot's own files.
    let snapshot_file = |file: &str| {
        file.strip_suffix(".sqlite")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    if let Some(entry) = index
        .iter()
        .find(|entry| !is_inside(&entry.path) || !snapshot_file(&entry.file))
    {
        return Err(format!(
            "corrupt snapshot index in {}: {:?} as {:?} is not a storage file of home; the storage was left as it is",
            dir.display(),
            entry.path,
            entry.file
        ));
    }
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

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
        let root = tempfile::tempdir().unwrap();
        let (home, snap) = (root.path().join("home"), root.path().join("snap"));
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
    }

    #[test]
    fn copy_of_a_database_written_all_the_while_finishes_and_is_consistent() {
        let root = tempfile::tempdir().unwrap();
        let (home, dest) = (root.path().join("home"), root.path().join("dest"));
        std::fs::create_dir_all(&home).unwrap();
        let live = home.join("a.sqlite");
        // A few MB, so that a copy in steps of 100 pages needs dozens of steps.
        let db = wal_database(&live, 0);
        db.execute_batch(
            "WITH RECURSIVE s(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM s WHERE n < 3000)
             INSERT INTO t SELECT zeroblob(2000) FROM s",
        )
        .unwrap();
        let before: i64 = db
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (stop, live) = (stop.clone(), live.clone());
            std::thread::spawn(move || {
                let db = Connection::open(live).unwrap();
                db.busy_timeout(Duration::from_secs(5)).unwrap();
                while !stop.load(Ordering::Relaxed) {
                    db.execute("INSERT INTO t VALUES (zeroblob(2000))", [])
                        .unwrap();
                    std::thread::sleep(Duration::from_millis(3));
                }
            })
        };
        // Bounded, so that a copy that never ends fails the test instead of hanging it.
        let (done, copied) = std::sync::mpsc::channel();
        let (copy_home, copy_dest) = (home.clone(), dest.clone());
        std::thread::spawn(move || {
            let _ = done.send(copy(&copy_home, &["a.sqlite".to_string()], &copy_dest));
        });
        let outcome = copied.recv_timeout(Duration::from_secs(5));
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        outcome
            .expect("the copy did not finish while the app kept writing")
            .unwrap();
        let copy =
            Connection::open_with_flags(dest.join("a.sqlite"), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let check: String = copy
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        assert!(count(&dest.join("a.sqlite")) >= before);
    }

    #[test]
    fn a_restored_database_keeps_the_mode_it_had_at_the_snapshot() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let root = tempfile::tempdir().unwrap();
        let (home, snap) = (root.path().join("home"), root.path().join("snap"));
        std::fs::create_dir_all(&home).unwrap();
        let live = home.join("a.sqlite");
        drop(wal_database(&live, 1));
        std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o600)).unwrap();
        snapshot(&home, &["a.sqlite".to_string()], &snap).unwrap();
        assert_eq!(mode(&snap.join("0.sqlite")), 0o600);

        restore(&home, &snap).unwrap();
        assert_eq!(mode(&live), 0o600);
        assert_eq!(count(&live), 1);
        // The app's next write creates the WAL with the database's mode.
        let db = Connection::open(&live).unwrap();
        db.execute("INSERT INTO t VALUES (2)", []).unwrap();
        assert_eq!(mode(&with_suffix(&live, "-wal")), 0o600);
    }

    #[test]
    fn a_snapshot_index_that_points_outside_home_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let (home, snap) = (root.path().join("home"), root.path().join("snap"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&snap).unwrap();
        let outside = root.path().join("outside.sqlite");
        std::fs::write(&outside, "keep me").unwrap();
        for (path, file) in [
            ("../outside.sqlite", "0.sqlite"),
            ("a.sqlite", "../outside.sqlite"),
        ] {
            let index = format!(r#"[{{"path": "{path}", "file": "{file}", "present": false}}]"#);
            std::fs::write(snap.join("index.json"), index).unwrap();
            let refused = restore(&home, &snap).unwrap_err();
            assert!(refused.contains("corrupt snapshot index"), "{refused}");
            assert!(outside.exists(), "{path} was deleted");
        }
    }
}
