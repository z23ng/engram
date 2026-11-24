use chrono::Utc;
use rusqlite::params;

use crate::{ApiKey, ControlPlane, ServerError};

/// One `audit_log` row: a successful write or branch operation.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub at_ms: i64,
    pub key_id: i64,
    pub key_name: String,
    pub role: String,
    pub project: String,
    pub operation: String,
    pub commit_id: Option<String>,
}

impl ControlPlane {
    pub(crate) fn record_audit(
        &self,
        key: &ApiKey,
        project: &str,
        operation: &str,
        commit_id: Option<&str>,
    ) -> Result<(), ServerError> {
        self.db()?.execute(
            "INSERT INTO audit_log (at, key_id, key_name, role, project, operation, commit_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                Utc::now().timestamp_millis(),
                key.id,
                key.name,
                key.role.as_str(),
                project,
                operation,
                commit_id
            ],
        )?;
        Ok(())
    }

    /// Returns up to `limit` audit rows of `project`, newest first.
    pub fn list_audit(&self, project: &str, limit: usize) -> Result<Vec<AuditEntry>, ServerError> {
        let db = self.db()?;
        let mut stmt = db.prepare(
            "SELECT at, key_id, key_name, role, project, operation, commit_id FROM audit_log
             WHERE project = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![project, limit as i64], |r| {
            Ok(AuditEntry {
                at_ms: r.get(0)?,
                key_id: r.get(1)?,
                key_name: r.get(2)?,
                role: r.get(3)?,
                project: r.get(4)?,
                operation: r.get(5)?,
                commit_id: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
