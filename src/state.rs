//! Local classifier cache and durable reduction checkpoints.
use crate::rewrite::Edit;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub fn fingerprint(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

pub fn classifier_key(kind: &str, text: &str, context: &str) -> String {
    fingerprint(&[
        "classifier-input-v1",
        crate::hook_model::MODEL_SHA256,
        include_str!("model/minilm-l12-classifier.json"),
        kind,
        text,
        context,
    ])
}

pub struct Database {
    connection: Connection,
    _lock: DatabaseLock,
    pub path: PathBuf,
}

struct DatabaseLock(File);

impl Drop for DatabaseLock {
    fn drop(&mut self) {
        // A concurrent subprocess fork can inherit the descriptor until exec or exit.
        // Explicit unlock releases ownership even while that inherited copy is open.
        let _ = self.0.unlock();
    }
}

pub(crate) struct Record {
    pub id: i64,
    pub ready: bool,
    pub result_type: Option<String>,
    pub edit: Option<Edit>,
}

pub(crate) struct ItemRecord<'a> {
    pub file: &'a str,
    pub source_hash: &'a str,
    pub start: usize,
    pub end: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: &'a str,
    pub classifier_key: &'a str,
    pub llm_key: &'a str,
    pub classification: &'a str,
}

impl Database {
    pub fn open(root: &Path, custom: Option<&Path>) -> Result<Self> {
        let path = if let Some(path) = custom {
            path.to_path_buf()
        } else {
            let dir = if root.is_file() {
                root.parent().context("input has no parent")?
            } else {
                root
            };
            let output = std::process::Command::new("git")
                .args([
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "commentreducr/state.sqlite",
                ])
                .current_dir(dir)
                .output()?;
            ensure!(
                output.status.success(),
                "cannot locate Git metadata for SQLite state"
            );
            PathBuf::from(String::from_utf8(output.stdout)?.trim_end_matches('\n'))
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("sqlite.lock"))?;
        lock.try_lock()
            .context("another commentreducr run is using this database")?;
        let lock = DatabaseLock(lock);
        let connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version <= 1,
            "unsupported commentreducr database version {version}"
        );
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS classifications (
                cache_key TEXT PRIMARY KEY, flagged INTEGER NOT NULL, probability REAL NOT NULL
            );
            CREATE TABLE IF NOT EXISTS items (
                id INTEGER PRIMARY KEY, file TEXT NOT NULL, source_hash TEXT NOT NULL,
                start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,
                start_line INTEGER NOT NULL, end_line INTEGER NOT NULL, kind TEXT NOT NULL,
                classifier_key TEXT NOT NULL, llm_key TEXT NOT NULL, classification TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'pending', result_type TEXT,
                edit_start INTEGER, edit_end INTEGER, replacement TEXT, error TEXT,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(file, source_hash, start_byte, end_byte, kind, classifier_key, llm_key)
            );
            CREATE TABLE IF NOT EXISTS file_writes (
                file TEXT NOT NULL, settings TEXT NOT NULL, before_hash TEXT NOT NULL,
                after_hash TEXT NOT NULL, after_source TEXT, item_ids TEXT NOT NULL,
                state TEXT NOT NULL, PRIMARY KEY(file, settings)
            );
            PRAGMA user_version=1;",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            connection,
            _lock: lock,
            path,
        })
    }

    pub fn classification(&self, key: &str) -> Result<Option<(bool, f64)>> {
        Ok(self
            .connection
            .query_row(
                "SELECT flagged, probability FROM classifications WHERE cache_key=?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn remember_classification(
        &self,
        key: &str,
        probability: f64,
        threshold: f64,
    ) -> Result<bool> {
        let flagged = probability >= threshold;
        self.connection.execute("INSERT OR REPLACE INTO classifications(cache_key, flagged, probability) VALUES (?1,?2,?3)", params![key, flagged, probability])?;
        Ok(flagged)
    }

    pub(crate) fn item(&self, item: &ItemRecord<'_>) -> Result<Record> {
        self.connection.execute("INSERT OR IGNORE INTO items
            (file, source_hash, start_byte, end_byte, start_line, end_line, kind, classifier_key, llm_key, classification)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![item.file, item.source_hash, item.start as i64, item.end as i64,
                item.start_line as i64, item.end_line as i64, item.kind, item.classifier_key, item.llm_key, item.classification])?;
        let mut record = self.connection.query_row("SELECT id, state, result_type, edit_start, edit_end, replacement FROM items
            WHERE file=?1 AND source_hash=?2 AND start_byte=?3 AND end_byte=?4 AND kind=?5 AND classifier_key=?6 AND llm_key=?7",
            params![item.file, item.source_hash, item.start as i64, item.end as i64, item.kind, item.classifier_key, item.llm_key],
            |r| {
                let state: String = r.get(1)?;
                let replacement: Option<String> = r.get(5)?;
                let edit = if let Some(replacement) = replacement {
                    Some(Edit {start:r.get::<_,i64>(3)? as usize, end:r.get::<_,i64>(4)? as usize, replacement})
                } else {None};
                Ok(Record {id:r.get(0)?, ready:matches!(state.as_str(), "ready" | "applied" | "kept"), result_type:r.get(2)?,
                    edit})
            })?;
        self.connection.execute(
            "UPDATE items SET classification=?2 WHERE id=?1",
            params![record.id, item.classification],
        )?;
        if item.classification == "PASS" && !record.ready {
            self.verdict(record.id, "keep", None)?;
            record.ready = true;
            record.result_type = Some("keep".into());
        }
        Ok(record)
    }

    pub(crate) fn running(&self, id: i64) -> Result<()> {
        self.connection.execute("UPDATE items SET state='in_progress', error=NULL, updated_at=CURRENT_TIMESTAMP WHERE id=?1", [id])?;
        Ok(())
    }

    pub(crate) fn verdict(&self, id: i64, result_type: &str, edit: Option<&Edit>) -> Result<()> {
        self.connection.execute("UPDATE items SET state='ready', result_type=?2, edit_start=?3, edit_end=?4, replacement=?5, error=NULL, updated_at=CURRENT_TIMESTAMP WHERE id=?1",
            params![id, result_type, edit.map(|e|e.start as i64), edit.map(|e|e.end as i64), edit.map(|e|e.replacement.as_str())])?;
        Ok(())
    }

    pub(crate) fn error(&self, id: i64, error: &str) -> Result<()> {
        self.connection.execute("UPDATE items SET state='error', result_type='error', error=?2, updated_at=CURRENT_TIMESTAMP WHERE id=?1", params![id,error])?;
        Ok(())
    }

    pub(crate) fn prepare(
        &self,
        file: &str,
        settings: &str,
        before: &str,
        after: &str,
        ids: &[i64],
    ) -> Result<()> {
        self.connection.execute("INSERT OR REPLACE INTO file_writes (file,settings,before_hash,after_hash,after_source,item_ids,state) VALUES (?1,?2,?3,?4,?5,?6,'prepared')",
            params![file, settings, fingerprint(&[before]), fingerprint(&[after]), after, serde_json::to_string(ids)?])?;
        Ok(())
    }

    pub(crate) fn complete(&mut self, file: &str, settings: &str) -> Result<()> {
        let tx = self.connection.transaction()?;
        let ids: String = tx.query_row(
            "SELECT item_ids FROM file_writes WHERE file=?1 AND settings=?2",
            params![file, settings],
            |r| r.get(0),
        )?;
        for id in serde_json::from_str::<Vec<i64>>(&ids)? {
            tx.execute("UPDATE items SET state=CASE WHEN result_type='keep' THEN 'kept' ELSE 'applied' END, updated_at=CURRENT_TIMESTAMP WHERE id=?1", [id])?;
        }
        tx.execute("UPDATE file_writes SET state='applied', after_source=NULL WHERE file=?1 AND settings=?2", params![file,settings])?;
        tx.commit()?;
        Ok(())
    }

    // true means a completed file was skipped or its prepared write was recovered.
    pub(crate) fn resume(
        &mut self,
        file: &str,
        settings: &str,
        source: &str,
    ) -> Result<Option<bool>> {
        let row = self.connection.query_row("SELECT state,before_hash,after_hash,after_source FROM file_writes WHERE file=?1 AND settings=?2", params![file, settings],
            |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?))).optional()?;
        let Some((state, before, after, prepared)) = row else {
            return Ok(None);
        };
        let current = fingerprint(&[source]);
        if current == after {
            if state == "prepared" {
                self.complete(file, settings)?;
            }
            return Ok(Some(false));
        }
        if state == "prepared" && current == before {
            let prepared = prepared.context("missing prepared rewrite")?;
            ensure!(
                fingerprint(&[&prepared]) == after,
                "prepared rewrite checksum mismatch"
            );
            crate::atomic_write(Path::new(file), source, &prepared)?;
            self.complete(file, settings)?;
            return Ok(Some(true));
        }
        self.connection.execute(
            "DELETE FROM file_writes WHERE file=?1 AND settings=?2",
            params![file, settings],
        )?;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durable_checkpoints_recover_both_write_boundaries_and_reject_changed_source() {
        let dir = std::env::temp_dir().join(format!(
            "commentreducr-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("item.py");
        let file_name = file.to_str().unwrap();
        let db_path = dir.join("state.sqlite");
        let before = "# narration\nvalue = 1\n";
        let after = "value = 1\n";
        std::fs::write(&file, before).unwrap();
        let mut db = Database::open(&dir, Some(&db_path)).unwrap();
        assert!(Database::open(&dir, Some(&db_path)).is_err());
        let key = classifier_key("comment", "# narration", "value = 1");
        assert_ne!(key, classifier_key("comment", "# narration", "value = 2"));
        db.remember_classification(&key, 0.99, 0.94).unwrap();
        let source_hash = fingerprint(&[before]);
        let record = db
            .item(&ItemRecord {
                file: file_name,
                source_hash: &source_hash,
                start: 0,
                end: 11,
                start_line: 1,
                end_line: 1,
                kind: "comment",
                classifier_key: &key,
                llm_key: "llm",
                classification: "FLAG",
            })
            .unwrap();
        db.running(record.id).unwrap();
        db.verdict(
            record.id,
            "delete",
            Some(&Edit {
                start: 0,
                end: 12,
                replacement: String::new(),
            }),
        )
        .unwrap();
        db.prepare(file_name, "settings", before, after, &[record.id])
            .unwrap();
        drop(db);
        db = Database::open(&dir, Some(&db_path)).unwrap();
        assert_eq!(db.classification(&key).unwrap(), Some((true, 0.99)));
        assert_eq!(
            db.resume(file_name, "settings", before).unwrap(),
            Some(true)
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), after);
        let state: String = db
            .connection
            .query_row("SELECT state FROM items WHERE id=?1", [record.id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "applied");
        db.prepare(file_name, "settings", before, after, &[record.id])
            .unwrap();
        drop(db);
        db = Database::open(&dir, Some(&db_path)).unwrap();
        assert_eq!(
            db.resume(file_name, "settings", after).unwrap(),
            Some(false)
        );
        db.prepare(file_name, "settings", before, after, &[record.id])
            .unwrap();
        std::fs::write(&file, "value = 2\n").unwrap();
        assert_eq!(
            db.resume(file_name, "settings", "value = 2\n").unwrap(),
            None
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "value = 2\n");
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
