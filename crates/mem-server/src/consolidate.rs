//! S6 background consolidation: run bookkeeping, REST trigger, and the per-project schedule.
//! The LLM work itself is `mem_search::consolidate`; it never runs on a request hot path.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::Utc;
use mem_core::CommitId;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::http::{guarded, Shared};
use crate::{Action, ApiKey, ControlPlane, Role, ServerError};

const HOUR_MS: i64 = 3_600_000;

/// Adds the S6 columns to control planes created before them (idempotent).
pub(crate) fn migrate(conn: &Connection) -> Result<(), ServerError> {
    add_column(conn, "projects", "consolidate_every_hours", "INTEGER")?;
    add_column(
        conn,
        "consolidation_runs",
        "batches_failed",
        "INTEGER NOT NULL DEFAULT 0",
    )
}

fn add_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<(), ServerError> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2",
            [table, column],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ConsolidationRun {
    pub id: i64,
    pub project: String,
    pub trigger: String,
    pub requested_by: String,
    pub status: String,
    pub source_commit: Option<String>,
    pub branch: Option<String>,
    pub files_in: i64,
    pub notes_written: i64,
    pub notes_dropped: i64,
    pub batches_failed: i64,
    pub error: Option<String>,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
}

const RUN_COLUMNS: &str = "id, project, trigger, requested_by, status, source_commit, branch, \
    files_in, notes_written, notes_dropped, error, started_at_ms, finished_at_ms, batches_failed";

fn run_row(r: &Row<'_>) -> rusqlite::Result<ConsolidationRun> {
    Ok(ConsolidationRun {
        id: r.get(0)?,
        project: r.get(1)?,
        trigger: r.get(2)?,
        requested_by: r.get(3)?,
        status: r.get(4)?,
        source_commit: r.get(5)?,
        branch: r.get(6)?,
        files_in: r.get(7)?,
        notes_written: r.get(8)?,
        notes_dropped: r.get(9)?,
        error: r.get(10)?,
        started_at_ms: r.get(11)?,
        finished_at_ms: r.get(12)?,
        batches_failed: r.get(13)?,
    })
}

/// Audit identity of runs that no API key started.
fn system_key(name: &str) -> ApiKey {
    ApiKey {
        id: 0,
        name: name.into(),
        role: Role::Admin,
        projects: None,
        expires_at_ms: None,
        revoked: false,
    }
}

impl ControlPlane {
    /// Reserves `project` for one run and records it as `running`; 409 when one is in progress.
    pub fn start_consolidation(
        &self,
        project: &str,
        trigger: &str,
        requested_by: &str,
    ) -> Result<i64, ServerError> {
        if !self.project_exists(project)? {
            return Err(ServerError::NotFound(format!(
                "project '{project}' does not exist"
            )));
        }
        let lock_err = || ServerError::Internal("consolidation mutex poisoned".into());
        if !self
            .consolidating
            .lock()
            .map_err(|_| lock_err())?
            .insert(project.to_string())
        {
            return Err(ServerError::Conflict(format!(
                "a consolidation run is already in progress for '{project}'"
            )));
        }
        let inserted = self.db().and_then(|db| {
            db.execute(
                "INSERT INTO consolidation_runs (project, trigger, requested_by, status, started_at_ms)
                 VALUES (?1, ?2, ?3, 'running', ?4)",
                params![project, trigger, requested_by, Utc::now().timestamp_millis()],
            )?;
            Ok(db.last_insert_rowid())
        });
        if inserted.is_err() {
            self.consolidating
                .lock()
                .map_err(|_| lock_err())?
                .remove(project);
        }
        inserted
    }

    /// Executes a reserved run to the end, records the result, and releases the project.
    pub fn finish_consolidation(
        &self,
        run_id: i64,
        project: &str,
        key: &ApiKey,
    ) -> Result<ConsolidationRun, ServerError> {
        let result = self.execute_run(run_id, project, key);
        if let Ok(mut running) = self.consolidating.lock() {
            running.remove(project);
        }
        if let Err(e) = result {
            self.db()?.execute(
                "UPDATE consolidation_runs SET status = 'failed', error = ?2, finished_at_ms = ?3
                 WHERE id = ?1",
                params![run_id, e.to_string(), Utc::now().timestamp_millis()],
            )?;
        }
        self.consolidation_run(run_id)
    }

    /// Start plus finish on the calling thread (`mem admin consolidate`).
    pub fn consolidate_now(
        &self,
        project: &str,
        key_name: &str,
    ) -> Result<ConsolidationRun, ServerError> {
        let run_id = self.start_consolidation(project, "manual", key_name)?;
        self.finish_consolidation(run_id, project, &system_key(key_name))
    }

    fn execute_run(&self, run_id: i64, project: &str, key: &ApiKey) -> Result<(), ServerError> {
        let engine = self.engine(project)?;
        // Succeeded, no_changes, and skipped runs all consumed the episodic files up to their source.
        let since: Option<String> = self
            .db()?
            .query_row(
                "SELECT source_commit FROM consolidation_runs WHERE project = ?1
                 AND status IN ('succeeded', 'no_changes', 'skipped') ORDER BY id DESC LIMIT 1",
                [project],
                |r| r.get(0),
            )
            .optional()?;
        let since = since.map(|hex| CommitId::from_hex(&hex)).transpose()?;
        let mut branch = format!("consolidate/{}", Utc::now().format("%Y%m%d-%H%M"));
        if engine.vcs().head_commit(&branch).is_ok() {
            branch = format!("{branch}-{run_id}");
        }
        let out = mem_search::consolidate(&engine, self.llm_config(), since.as_ref(), &branch)?;
        let commit = out.commit_id.map(|c| c.to_hex());
        self.db()?.execute(
            "UPDATE consolidation_runs SET status = ?2, source_commit = ?3, branch = ?4,
             files_in = ?5, notes_written = ?6, notes_dropped = ?7, finished_at_ms = ?8,
             batches_failed = ?9, error = ?10 WHERE id = ?1",
            params![
                run_id,
                out.status.as_str(),
                out.source_commit.to_hex(),
                out.branch,
                out.files_in as i64,
                out.notes_written as i64,
                out.notes_dropped as i64,
                Utc::now().timestamp_millis(),
                out.batches_failed as i64,
                // Skipped batches of a run that still committed notes.
                (!out.batch_errors.is_empty()).then(|| out.batch_errors.join("; ")),
            ],
        )?;
        self.record_audit(key, project, "consolidate.run", commit.as_deref())
    }
}

impl ControlPlane {
    pub fn consolidation_run(&self, run_id: i64) -> Result<ConsolidationRun, ServerError> {
        let sql = format!("SELECT {RUN_COLUMNS} FROM consolidation_runs WHERE id = ?1");
        Ok(self.db()?.query_row(&sql, [run_id], run_row)?)
    }

    /// Newest first.
    pub fn list_consolidations(
        &self,
        project: &str,
        limit: usize,
    ) -> Result<Vec<ConsolidationRun>, ServerError> {
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM consolidation_runs WHERE project = ?1 ORDER BY id DESC LIMIT ?2"
        );
        let db = self.db()?;
        let mut stmt = db.prepare(&sql)?;
        let rows = stmt.query_map(params![project, limit as i64], run_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn consolidate_schedule(&self, project: &str) -> Result<Option<u32>, ServerError> {
        let every: Option<Option<u32>> = self
            .db()?
            .query_row(
                "SELECT consolidate_every_hours FROM projects WHERE slug = ?1",
                [project],
                |r| r.get(0),
            )
            .optional()?;
        every.ok_or_else(|| ServerError::NotFound(format!("project '{project}' does not exist")))
    }

    /// `None` turns the schedule off.
    pub fn set_consolidate_schedule(
        &self,
        project: &str,
        every_hours: Option<u32>,
    ) -> Result<(), ServerError> {
        if every_hours == Some(0) {
            return Err(ServerError::BadRequest(
                "every_hours must be at least 1".into(),
            ));
        }
        let changed = self.db()?.execute(
            "UPDATE projects SET consolidate_every_hours = ?2 WHERE slug = ?1",
            params![project, every_hours],
        )?;
        if changed == 0 {
            return Err(ServerError::NotFound(format!(
                "project '{project}' does not exist"
            )));
        }
        Ok(())
    }

    /// Scheduled projects whose last finished run (any status) is at least `every_hours` old
    /// at `now_ms`, or that never ran, and that have no run in progress.
    pub fn due_consolidations(&self, now_ms: i64) -> Result<Vec<String>, ServerError> {
        let due: Vec<String> = {
            let db = self.db()?;
            let mut stmt = db.prepare(
                "SELECT p.slug FROM projects p WHERE p.consolidate_every_hours IS NOT NULL AND
                 coalesce((SELECT max(r.finished_at_ms) FROM consolidation_runs r WHERE r.project = p.slug), 0)
                   <= ?1 - p.consolidate_every_hours * ?2
                 ORDER BY p.slug",
            )?;
            let rows = stmt.query_map(params![now_ms, HOUR_MS], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let running = self
            .consolidating
            .lock()
            .map_err(|_| ServerError::Internal("consolidation mutex poisoned".into()))?;
        Ok(due.into_iter().filter(|p| !running.contains(p)).collect())
    }
}

/// Wakes every 60 s and starts one `schedule` run per due project.
pub(crate) async fn scheduler(cp: Arc<ControlPlane>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        let due = match cp.due_consolidations(Utc::now().timestamp_millis()) {
            Ok(due) => due,
            Err(e) => {
                eprintln!("consolidation schedule: {e}");
                continue;
            }
        };
        for project in due {
            let run_id = match cp.start_consolidation(&project, "schedule", "system:schedule") {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("consolidation schedule {project}: {e}");
                    continue;
                }
            };
            let cp = cp.clone();
            tokio::task::spawn_blocking(move || {
                let key = system_key("system:schedule");
                if let Err(e) = cp.finish_consolidation(run_id, &project, &key) {
                    eprintln!("consolidation run {run_id} ({project}): {e}");
                }
            });
        }
    }
}

pub(crate) fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/v1/projects/{project}/consolidate", post(trigger))
        .route("/api/v1/projects/{project}/consolidate/runs", get(runs))
        .route(
            "/api/v1/projects/{project}/consolidate/schedule",
            put(schedule),
        )
}

/// 202 with the run id; the run continues on a blocking thread.
async fn trigger(
    State(cp): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<Value>), ServerError> {
    let (bg, scope, name) = (cp.clone(), project.clone(), project.clone());
    let (run_id, key) = guarded(cp, headers, scope, Action::Manage, move |cp, key, _| {
        Ok((
            cp.start_consolidation(&project, "manual", &key.name)?,
            key.clone(),
        ))
    })
    .await?;
    let project = name;
    tokio::task::spawn_blocking(move || {
        if let Err(e) = bg.finish_consolidation(run_id, &project, &key) {
            eprintln!("consolidation run {run_id} ({project}): {e}");
        }
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "run_id": run_id }))))
}

#[derive(Deserialize)]
struct RunsQuery {
    limit: Option<usize>,
}

async fn runs(
    State(cp): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Query(q): Query<RunsQuery>,
) -> Result<Json<Value>, ServerError> {
    let scope = project.clone();
    guarded(cp, headers, scope, Action::Manage, move |cp, _, _| {
        let runs = cp.list_consolidations(&project, q.limit.unwrap_or(10).min(100))?;
        Ok(Json(
            json!({ "runs": runs, "every_hours": cp.consolidate_schedule(&project)? }),
        ))
    })
    .await
}

#[derive(Deserialize)]
struct ScheduleBody {
    every_hours: Option<u32>,
}

async fn schedule(
    State(cp): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ScheduleBody>,
) -> Result<Json<Value>, ServerError> {
    let scope = project.clone();
    guarded(cp, headers, scope, Action::Manage, move |cp, key, _| {
        cp.set_consolidate_schedule(&project, body.every_hours)?;
        cp.record_audit(key, &project, "consolidate.schedule", None)?;
        Ok(Json(json!({ "every_hours": body.every_hours })))
    })
    .await
}
