use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use mem_vcs::VersionControlEngine;

use crate::{ApiKey, ControlPlane, Role, ServerError};

/// WebDAV changes on one branch that wait for the idle commit.
#[derive(Default)]
pub(crate) struct DavPending {
    generation: u64,
    changes: usize,
    /// Writers keyed by (name, id), so author names come out sorted.
    writers: BTreeMap<(String, i64), ApiKey>,
}

impl ControlPlane {
    /// Commits every branch whose workspace still holds changes, for example after a restart
    /// inside the WebDAV idle window. Returns the number of commits.
    pub fn commit_leftovers(&self) -> Result<usize, ServerError> {
        let system = ApiKey {
            id: 0,
            name: "system:startup".into(),
            role: Role::Admin,
            projects: None,
            expires_at_ms: None,
            revoked: false,
        };
        let mut commits = 0;
        for project in self.list_projects()? {
            let engine = self.engine(&project)?;
            for (branch, _) in engine.vcs().store().list_refs()? {
                let changes = engine.vcs().store().workspace_list(&branch)?.len();
                if changes == 0 {
                    continue;
                }
                let message = format!("startup: commit {changes} leftover change(s)");
                let id = engine
                    .vcs()
                    .commit(&branch, &system.name, &message)?
                    .to_hex();
                self.record_audit(&system, &project, "startup.commit", Some(&id))?;
                commits += 1;
            }
        }
        Ok(commits)
    }

    /// Records one WebDAV change and (re)starts the idle timer of `(project, branch)`.
    pub(crate) fn note_dav_change(self: &Arc<Self>, project: &str, branch: &str, key: &ApiKey) {
        let slot = (project.to_string(), branch.to_string());
        let generation = {
            let Ok(mut pending) = self.dav_pending.lock() else {
                return;
            };
            let p = pending.entry(slot.clone()).or_default();
            p.generation += 1;
            p.changes += 1;
            p.writers.insert((key.name.clone(), key.id), key.clone());
            p.generation
        };
        let cp = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(cp.dav_delay).await;
            let (project, branch) = slot;
            let result =
                tokio::task::spawn_blocking(move || cp.flush_dav(&project, &branch, generation))
                    .await;
            if let Ok(Err(err)) = result {
                eprintln!("[mem-server] webdav auto-commit failed: {err}");
            }
        });
    }

    /// Commits the branch if `generation` is still the latest change (no write since the timer).
    fn flush_dav(&self, project: &str, branch: &str, generation: u64) -> Result<(), ServerError> {
        let slot = (project.to_string(), branch.to_string());
        let pending = {
            let mut map = self
                .dav_pending
                .lock()
                .map_err(|_| ServerError::Internal("dav pending mutex poisoned".into()))?;
            match map.get(&slot) {
                Some(p) if p.generation == generation => map.remove(&slot),
                _ => None,
            }
        };
        let Some(pending) = pending else {
            return Ok(());
        };
        let engine = self.engine(project)?;
        if engine.vcs().store().workspace_list(branch)?.is_empty() {
            return Ok(());
        }
        let names: BTreeSet<&str> = pending.writers.keys().map(|(n, _)| n.as_str()).collect();
        let author = match names.len() {
            1 => names.into_iter().collect::<String>(),
            _ => format!("webdav:{}", names.into_iter().collect::<Vec<_>>().join(",")),
        };
        let message = format!("webdav sync: {} change(s)", pending.changes);
        let commit_id = engine.vcs().commit(branch, &author, &message)?.to_hex();
        for key in pending.writers.values() {
            self.record_audit(key, project, "dav.commit", Some(&commit_id))?;
        }
        Ok(())
    }
}
