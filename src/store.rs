use crate::{
    model::{Job, JobState, Settings},
    secure,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;

/// Schema generation written to `PRAGMA user_version`. A database from a newer
/// generation is refused instead of being rewritten by an older build.
const SCHEMA_VERSION: i64 = 1;

/// SQLite is owned by the engine actor. Keeping the connection on that one thread
/// makes every state transition and its durable representation ordered.
pub(crate) struct Store {
    connection: Connection,
    pub warnings: Vec<String>,
}

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Veritabanı klasörü oluşturulamadı: {}", parent.display())
            })?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("İndirme veritabanı açılamadı: {}", path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS jobs (
                 id TEXT PRIMARY KEY NOT NULL,
                 payload TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS jobs_created_at ON jobs(created_at, id);
             CREATE TABLE IF NOT EXISTS settings (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                 payload TEXT NOT NULL
             );",
        )?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS quarantine (id TEXT, payload TEXT, reason TEXT, quarantined_at INTEGER);")?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "{}",
                crate::i18n::ui(
                    "İndirme veritabanı bu sürümden daha yeni bir SSDownload ile oluşturulmuş; güncel sürümü kurun.",
                    "The download database was created by a newer SSDownload; install the current version.",
                )
            );
        }
        // Generation 0 is the same table layout without a recorded version.
        if version < SCHEMA_VERSION {
            connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self {
            connection,
            warnings: Vec::new(),
        })
    }

    /// Moves quarantined job records back once they decode again - for example after
    /// the profile returned to the Windows account whose DPAPI key sealed them.
    fn restore_quarantined(&mut self) -> Result<usize> {
        let rows = self
            .connection
            .prepare("SELECT rowid, id, payload FROM quarantine WHERE id IS NOT NULL AND id != 'settings'")?
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut restored = 0;
        for (rowid, id, payload) in rows {
            let Ok(job) = decode_job(&payload) else {
                continue;
            };
            if job.id != id || uuid::Uuid::parse_str(&id).is_err() {
                continue;
            }
            let tx = self.connection.transaction()?;
            let present: bool = tx
                .query_row("SELECT 1 FROM jobs WHERE id=?1", params![id], |_| Ok(true))
                .optional()?
                .unwrap_or(false);
            if !present {
                tx.execute(
                    "INSERT INTO jobs(id, payload, created_at) VALUES(?1, ?2, ?3)",
                    params![id, payload, job.created_at],
                )?;
                restored += 1;
            }
            tx.execute("DELETE FROM quarantine WHERE rowid=?1", params![rowid])?;
            tx.commit()?;
        }
        Ok(restored)
    }

    pub(crate) fn load_jobs(&mut self) -> Result<Vec<Job>> {
        let restored = self.restore_quarantined()?;
        if restored > 0 {
            self.warnings.push(crate::i18n::ui_owned!(
                format!(
                    "{restored} karantinadaki iş kaydı yeniden okunabildi ve listeye geri alındı."
                ),
                format!(
                    "{restored} quarantined job record(s) could be read again and were restored."
                )
            ));
        }
        let rows = self
            .connection
            .prepare("SELECT id, payload FROM jobs ORDER BY created_at, id")?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut jobs = Vec::new();
        for (id, payload) in rows {
            match decode_job(&payload).and_then(|job| {
                anyhow::ensure!(
                    job.id == id && uuid::Uuid::parse_str(&id).is_ok(),
                    "İş kimliği geçersiz"
                );
                Ok(job)
            }) {
                Ok(job) => jobs.push(job),
                Err(_) => {
                    let tx = self.connection.transaction()?;
                    tx.execute(
                        "INSERT INTO quarantine VALUES(?1,?2,'İş kaydı okunamadı',unixepoch())",
                        params![id, payload],
                    )?;
                    tx.execute("DELETE FROM jobs WHERE id=?1", params![id])?;
                    tx.commit()?;
                    self.warnings.push(crate::i18n::ui_owned!(
                        format!("Okunamayan bir iş kaydı karantinaya alındı: {id}. Diğer indirmeler korundu."),
                        format!("An unreadable job record was quarantined: {id}. Other downloads were kept.")
                    ));
                }
            }
        }
        Ok(jobs)
    }

    pub(crate) fn load_settings(&mut self) -> Result<Option<Settings>> {
        let payload = self
            .connection
            .query_row(
                "SELECT payload FROM settings WHERE singleton = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match payload {
            None => Ok(None),
            Some(value) => {
                match serde_json::from_str(&value) {
                    Ok(settings) => Ok(Some(settings)),
                    Err(_) => {
                        // Do not leave the unreadable singleton in place: otherwise every
                        // start would create another quarantine record and repeat the warning.
                        let tx = self.connection.transaction()?;
                        tx.execute(
                            "INSERT INTO quarantine VALUES('settings',?1,'Ayar kaydı okunamadı',unixepoch())",
                            params![value],
                        )?;
                        tx.execute("DELETE FROM settings WHERE singleton = 1", [])?;
                        tx.commit()?;
                        self.warnings.push(
                            crate::i18n::ui(
                                "Kayıtlı ayarlar okunamadı; yedeği karantinada, varsayılan ayarlar açıldı.",
                                "The saved settings could not be read; a copy is in quarantine and the defaults are in use.",
                            )
                            .into(),
                        );
                        Ok(None)
                    }
                }
            }
        }
    }

    pub(crate) fn save_job(&mut self, job: &Job) -> Result<()> {
        let payload = encode_job(job)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO jobs(id, payload, created_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET payload=excluded.payload, created_at=excluded.created_at",
            params![job.id, payload, job.created_at],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn save_jobs(&mut self, jobs: &[Job]) -> Result<()> {
        let encoded = jobs
            .iter()
            .map(|job| Ok((job, encode_job(job)?)))
            .collect::<Result<Vec<_>>>()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut statement = tx.prepare(
                "INSERT INTO jobs(id, payload, created_at) VALUES(?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET payload=excluded.payload, created_at=excluded.created_at",
            )?;
            for (job, payload) in encoded {
                statement.execute(params![job.id, payload, job.created_at])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn delete_job(&mut self, id: &str) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM jobs WHERE id=?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn delete_jobs(&mut self, ids: &[String]) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut statement = tx.prepare("DELETE FROM jobs WHERE id=?1")?;
            for id in ids {
                statement.execute(params![id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn save_settings(&mut self, settings: &Settings) -> Result<()> {
        self.save_jobs_and_settings(&[], settings)
    }

    pub(crate) fn save_job_and_settings(&mut self, job: &Job, settings: &Settings) -> Result<()> {
        self.save_jobs_and_settings(std::slice::from_ref(job), settings)
    }

    /// Commits queue membership/job progress and its settings-owned policy counters
    /// together so a crash cannot grant quota or split a queue move across owners.
    pub(crate) fn save_jobs_and_settings(
        &mut self,
        jobs: &[Job],
        settings: &Settings,
    ) -> Result<()> {
        let settings_payload = serde_json::to_string(settings).context("Ayarlar kodlanamadı")?;
        let encoded = jobs
            .iter()
            .map(|job| Ok((job, encode_job(job)?)))
            .collect::<Result<Vec<_>>>()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut statement = tx.prepare(
                "INSERT INTO jobs(id, payload, created_at) VALUES(?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET payload=excluded.payload, created_at=excluded.created_at",
            )?;
            for (job, payload) in encoded {
                statement.execute(params![job.id, payload, job.created_at])?;
            }
        }
        tx.execute(
            "INSERT INTO settings(singleton, payload) VALUES(1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET payload=excluded.payload",
            params![settings_payload],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A process can disappear after persisting an active state. No transfer is
    /// alive after open, so such jobs must be made schedulable before exposure.
    pub(crate) fn recover_interrupted(&mut self) -> Result<Vec<Job>> {
        let mut jobs = self.load_jobs()?;
        let now = chrono::Utc::now().timestamp();
        let mut changed = Vec::new();
        for job in &mut jobs {
            if job.state.is_active() {
                job.state = if job.browser_transfer_authorized {
                    JobState::Paused
                } else {
                    JobState::Queued
                };
                job.speed = 0;
                job.eta = None;
                job.phase = if job.browser_transfer_authorized {
                    crate::i18n::ui(
                        "Tarayıcı bağlantısı koptu; açıkça sürdürmeniz gerekiyor",
                        "The browser connection was lost; resume it explicitly",
                    )
                } else {
                    crate::i18n::ui(
                        "Önceki oturumdan devam edecek",
                        "Continues from the previous session",
                    )
                }
                .into();
                job.updated_at = now;
                changed.push(job.clone());
            }
        }
        if !changed.is_empty() {
            self.save_jobs(&changed)?;
        }
        Ok(jobs)
    }

    pub(crate) fn flush(&self) -> Result<()> {
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(FULL);")?;
        Ok(())
    }
}

fn encode_job(job: &Job) -> Result<String> {
    let json = serde_json::to_string(job).context("İş kaydı kodlanamadı")?;
    secure::seal(json).context("İş kaydı şifrelenemedi")
}

fn decode_job(payload: &str) -> Result<Job> {
    let json = secure::unseal(payload).context("İş kaydı çözülemedi")?;
    serde_json::from_str(&json).context("İş kaydı okunamadı")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AddRequest, JobState};
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    fn scratch() -> PathBuf {
        let value = std::env::temp_dir().join(format!("ssdownload-store-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&value).unwrap();
        value
    }

    fn job(id: String, state: JobState) -> Job {
        Job {
            id,
            request: AddRequest {
                url: "https://example.test/file.bin".into(),
                ..Default::default()
            },
            name: "file.bin".into(),
            state,
            path: PathBuf::from(r"C:\\isolated\\file.bin"),
            downloaded: 17,
            total: Some(20),
            speed: 4,
            eta: Some(1),
            error: None,
            phase: "İndiriliyor".into(),
            created_at: 1,
            updated_at: 1,
            attempts: 1,
            work_dir: None,
            remove_requested: None,
            claim: None,
            legacy_completed: false,
            browser_transfer_authorized: false,
            priority: 0,
            force_start: false,
            open_when_done: false,
        }
    }

    #[test]
    fn corrupt_job_is_quarantined_without_losing_other_jobs() {
        let root = scratch();
        let mut store = Store::open(&root.join("queue.sqlite3")).unwrap();
        let valid = job(Uuid::new_v4().to_string(), JobState::Queued);
        let corrupt_id = Uuid::new_v4().to_string();
        store.save_job(&valid).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO jobs(id, payload, created_at) VALUES(?1, 'not-a-dpapi-record', 2)",
                params![corrupt_id],
            )
            .unwrap();

        let loaded = store.load_jobs().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, valid.id);
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM quarantine WHERE id = ?1",
                    params![corrupt_id],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_settings_are_quarantined_once_then_cleared() {
        let root = scratch();
        let mut store = Store::open(&root.join("queue.sqlite3")).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO settings(singleton, payload) VALUES(1, 'not-json')",
                [],
            )
            .unwrap();

        assert!(store.load_settings().unwrap().is_none());
        assert!(store.load_settings().unwrap().is_none());
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM settings", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM quarantine WHERE id = 'settings'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_active_job_is_durably_requeued() {
        let root = scratch();
        let mut store = Store::open(&root.join("queue.sqlite3")).unwrap();
        let active = job(Uuid::new_v4().to_string(), JobState::Downloading);
        store.save_job(&active).unwrap();

        let recovered = store.recover_interrupted().unwrap();
        assert_eq!(recovered[0].state, JobState::Queued);
        assert_eq!(recovered[0].speed, 0);
        assert_eq!(recovered[0].eta, None);
        assert_eq!(store.load_jobs().unwrap()[0].state, JobState::Queued);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saving_same_id_updates_one_record_instead_of_creating_duplicates() {
        let root = scratch();
        let mut store = Store::open(&root.join("queue.sqlite3")).unwrap();
        let id = Uuid::new_v4().to_string();
        store.save_job(&job(id.clone(), JobState::Queued)).unwrap();
        store.save_job(&job(id.clone(), JobState::Paused)).unwrap();

        let loaded = store.load_jobs().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, id);
        assert_eq!(loaded[0].state, JobState::Paused);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn database_write_failure_is_returned_to_the_engine() {
        let root = scratch();
        let mut store = Store::open(&root.join("queue.sqlite3")).unwrap();
        store.connection.execute("DROP TABLE jobs", []).unwrap();
        assert!(store
            .save_job(&job(Uuid::new_v4().to_string(), JobState::Queued))
            .is_err());
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}
