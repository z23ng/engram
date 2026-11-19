use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use chrono::Utc;
use mem_search::{ConsolidateLlmConfig, MemSearchEngine};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};
use rusqlite::{params, Connection, OptionalExtension};

use crate::dav_commit::DavPending;
use crate::ServerError;

/// Builds the search engine for an opened project repository.
pub type EngineFactory = Box<dyn Fn(MemVcs) -> MemSearchEngine + Send + Sync>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (slug TEXT PRIMARY KEY, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS api_keys (
    id INTEGER PRIMARY KEY, name TEXT NOT NULL, role TEXT NOT NULL, projects TEXT NOT NULL,
    key_hash TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL, expires_at INTEGER,
    revoked_at INTEGER, last_used_at INTEGER);
CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY, at INTEGER NOT NULL, key_id INTEGER, key_name TEXT, role TEXT,
    project TEXT, operation TEXT NOT NULL, commit_id TEXT);
";

/// Control plane: `server.db` (projects, keys, audit) plus one engine file per project.
pub struct ControlPlane {
    data_dir: PathBuf,
    conn: Mutex<Connection>,
    engines: Mutex<HashMap<String, MemSearchEngine>>,
    make_engine: EngineFactory,
    llm_cfg: ConsolidateLlmConfig,
    pub(crate) dav_delay: Duration,
    /// Pending WebDAV changes per (project, branch), committed after `dav_delay` of idle time.
    pub(crate) dav_pending: Mutex<HashMap<(String, String), DavPending>>,
    /// Empty folders created by MKCOL (project, branch, path); process-local, not persisted.
    pub(crate) dav_dirs: Mutex<HashSet<(String, String, String)>>,
}

impl ControlPlane {
    /// Opens `<data_dir>/server.db`; the flag is `true` when the file was created now.
    pub fn open(data_dir: &Path, make_engine: EngineFactory) -> Result<(Self, bool), ServerError> {
        std::fs::create_dir_all(data_dir.join("projects"))
            .map_err(|e| ServerError::Internal(format!("cannot create data dir: {e}")))?;
        let db_path = data_dir.join("server.db");
        let created = !db_path.exists();
        let conn = Connection::open(&db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        let cp = Self {
            data_dir: data_dir.to_path_buf(),
            conn: Mutex::new(conn),
            engines: Mutex::new(HashMap::new()),
            make_engine,
            llm_cfg: ConsolidateLlmConfig::disabled(),
            dav_delay: Duration::from_secs(10),
            dav_pending: Mutex::new(HashMap::new()),
            dav_dirs: Mutex::new(HashSet::new()),
        };
        Ok((cp, created))
    }

    /// Sets the LLM used by `mem_ingest` when a caller passes `use_llm` (default: disabled).
    pub fn with_llm_config(mut self, cfg: ConsolidateLlmConfig) -> Self {
        self.llm_cfg = cfg;
        self
    }

    /// Sets the WebDAV idle time before an automatic commit (default: 10s).
    pub fn with_dav_commit_delay(mut self, delay: Duration) -> Self {
        self.dav_delay = delay;
        self
    }

    pub(crate) fn llm_config(&self) -> &ConsolidateLlmConfig {
        &self.llm_cfg
    }

    pub(crate) fn db(&self) -> Result<MutexGuard<'_, Connection>, ServerError> {
        self.conn
            .lock()
            .map_err(|_| ServerError::Internal("control plane mutex poisoned".into()))
    }

    /// Creates a project row and its initialized engine database.
    pub fn create_project(&self, slug: &str) -> Result<(), ServerError> {
        validate_slug(slug)?;
        if self.project_exists(slug)? {
            return Err(ServerError::BadRequest(format!(
                "project '{slug}' already exists"
            )));
        }
        MemVcs::new(SqliteStore::open(self.project_db(slug))?).init()?;
        self.db()?.execute(
            "INSERT INTO projects (slug, created_at) VALUES (?1, ?2)",
            params![slug, Utc::now().timestamp_millis()],
        )?;
        Ok(())
    }

    pub fn list_projects(&self) -> Result<Vec<String>, ServerError> {
        let db = self.db()?;
        let mut stmt = db.prepare("SELECT slug FROM projects ORDER BY slug")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Returns the cached engine of an existing project, opening it on first use.
    pub fn engine(&self, slug: &str) -> Result<MemSearchEngine, ServerError> {
        if !self.project_exists(slug)? {
            return Err(ServerError::NotFound(format!(
                "project '{slug}' does not exist"
            )));
        }
        let mut engines = self
            .engines
            .lock()
            .map_err(|_| ServerError::Internal("engine cache mutex poisoned".into()))?;
        if let Some(engine) = engines.get(slug) {
            return Ok(engine.clone());
        }
        let engine = (self.make_engine)(MemVcs::new(SqliteStore::open(self.project_db(slug))?));
        engines.insert(slug.to_string(), engine.clone());
        Ok(engine)
    }

    fn project_exists(&self, slug: &str) -> Result<bool, ServerError> {
        let found = self
            .db()?
            .query_row("SELECT 1 FROM projects WHERE slug = ?1", [slug], |_| Ok(()))
            .optional()?;
        Ok(found.is_some())
    }

    fn project_db(&self, slug: &str) -> PathBuf {
        self.data_dir.join("projects").join(format!("{slug}.db"))
    }
}

/// Accepts `[a-z0-9][a-z0-9-]{0,62}`, which also rules out path traversal.
pub(crate) fn validate_slug(slug: &str) -> Result<(), ServerError> {
    let ok = (1..=63).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !slug.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(ServerError::BadRequest(format!(
            "invalid project slug '{slug}'"
        )))
    }
}
