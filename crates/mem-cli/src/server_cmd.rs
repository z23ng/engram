//! `mem serve` and `mem admin` for the multi-project Engram server.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use mem_server::{ControlPlane, EngineFactory, Role};
use mem_vcs::MemVcs;

use crate::{build_search_engine, GlobalOpts};

type CmdResult = Result<ExitCode, String>;

const ADMIN_USAGE: &str = "usage: mem admin --data <dir> (project create <slug> | project list | \
key create --name <n> --role admin|dev|pm|bot [--projects a,b] [--expires-days N] | key list | key revoke <id> | \
audit --project <slug> [--limit N] | reindex --project <slug> | consolidate --project <slug> [--full] | \
project schedule <slug> --every <hours|off>)";

/// Removes `flag` from `args` and returns whether it was there.
fn take_switch(args: &mut Vec<String>, flag: &str) -> bool {
    let found = args.iter().position(|a| a == flag);
    found.map(|i| args.remove(i)).is_some()
}

/// Runs `f` on the first Ctrl-C. On unix the handler is registered before this returns; on
/// other targets it is registered a moment later, on the first poll of `ctrl_c()`.
fn on_ctrl_c(f: impl FnOnce() + Send + 'static) -> Result<(), String> {
    let (ready_tx, ready) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => return drop(ready_tx.send(Err(e.to_string()))),
        };
        rt.block_on(async {
            #[cfg(unix)]
            let mut sig =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
                    Ok(sig) => sig,
                    Err(e) => return drop(ready_tx.send(Err(e.to_string()))),
                };
            let _ = ready_tx.send(Ok(()));
            #[cfg(unix)]
            sig.recv().await;
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
            f();
        });
    });
    ready.recv().map_err(|e| e.to_string())?
}

/// Removes `flag value` from `args` and returns the value.
fn take_flag(args: &mut Vec<String>, flag: &str) -> Result<Option<String>, String> {
    let Some(i) = args.iter().position(|a| a == flag) else {
        return Ok(None);
    };
    if i + 1 >= args.len() {
        return Err(format!("{flag} requires a value"));
    }
    let value = args.remove(i + 1);
    args.remove(i);
    Ok(Some(value))
}

fn open_control(
    args: &mut Vec<String>,
    factory: EngineFactory,
) -> Result<(ControlPlane, bool), String> {
    let data = take_flag(args, "--data")?.ok_or("--data <dir> is required")?;
    ControlPlane::open(Path::new(&data), factory).map_err(|e| e.to_string())
}

pub fn cmd_serve(opts: &GlobalOpts, args: &[String]) -> CmdResult {
    let mut args = args.to_vec();
    let listen = take_flag(&mut args, "--listen")?.unwrap_or_else(|| "127.0.0.1:7878".into());
    let emb_cfg = opts.embedding_config();
    let factory: EngineFactory = Box::new(move |vcs: MemVcs| {
        let (engine, notice) = build_search_engine(vcs, &emb_cfg);
        if let Some(notice) = notice {
            eprintln!("{notice}");
        }
        engine
    });
    let (cp, _) = open_control(&mut args, factory)?;
    let llm = opts.consolidate_config();
    let backend = format!("{:?}", llm.backend).to_lowercase();
    eprintln!("consolidation LLM: {backend} {}", llm.model);
    let cp = cp.with_llm_config(llm);
    if let Some(extra) = args.first() {
        return Err(format!("unexpected serve argument '{extra}'"));
    }
    let now = Utc::now().timestamp_millis();
    let has_active_admin =
        cp.list_keys().map_err(|e| e.to_string())?.iter().any(|k| {
            k.role == Role::Admin && !k.revoked && k.expires_at_ms.is_none_or(|t| t > now)
        });
    if !has_active_admin {
        let (_, plain) = cp
            .create_key("admin", Role::Admin, None, None)
            .map_err(|e| e.to_string())?;
        println!("admin key (shown once): {plain}");
    }
    let leftovers = cp.commit_leftovers().map_err(|e| e.to_string())?;
    if leftovers > 0 {
        println!("committed leftover workspace changes on {leftovers} branch(es)");
    }
    mem_server::run_server(Arc::new(cp), &listen).map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

pub fn cmd_admin(opts: &GlobalOpts, args: &[String]) -> CmdResult {
    let mut args = args.to_vec();
    // Same embedder as `mem serve`, so `reindex` produces the vectors the server searches with.
    let emb_cfg = opts.embedding_config();
    let factory: EngineFactory = Box::new(move |vcs: MemVcs| build_search_engine(vcs, &emb_cfg).0);
    let (cp, _) = open_control(&mut args, factory)?;
    let cp = cp.with_llm_config(opts.consolidate_config());
    let name = take_flag(&mut args, "--name")?;
    let role = take_flag(&mut args, "--role")?;
    let projects = take_flag(&mut args, "--projects")?;
    let expires_days = take_flag(&mut args, "--expires-days")?;
    let project = take_flag(&mut args, "--project")?;
    let limit = take_flag(&mut args, "--limit")?;
    let every = take_flag(&mut args, "--every")?;
    let full = take_switch(&mut args, "--full");
    let err = |e: mem_server::ServerError| e.to_string();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    if full && words != ["consolidate"] {
        return Err(format!("--full applies only to consolidate\n{ADMIN_USAGE}"));
    }
    match words.as_slice() {
        ["project", "create", slug] => {
            cp.create_project(slug).map_err(err)?;
            println!("created project {slug}");
        }
        ["project", "list"] => {
            cp.list_projects()
                .map_err(err)?
                .iter()
                .for_each(|p| println!("{p}"));
        }
        ["key", "create"] => {
            let name = name.ok_or("--name is required")?;
            let role = role
                .as_deref()
                .and_then(Role::parse)
                .ok_or("--role must be admin|dev|pm|bot")?;
            let scope = projects.map(|p| p.split(',').map(|s| s.trim().to_string()).collect());
            let expires = match expires_days {
                Some(d) => {
                    let days: i64 = d.parse().map_err(|_| "--expires-days must be an integer")?;
                    Some(Utc::now().timestamp_millis() + days * 86_400_000)
                }
                None => None,
            };
            let (key, plain) = cp.create_key(&name, role, scope, expires).map_err(err)?;
            println!(
                "created key id={} name={} role={}",
                key.id,
                key.name,
                role.as_str()
            );
            println!("key (shown once): {plain}");
        }
        ["key", "list"] => {
            let now = Utc::now().timestamp_millis();
            for k in cp.list_keys().map_err(err)? {
                let status = match (k.revoked, k.expires_at_ms) {
                    (true, _) => "revoked",
                    (false, Some(t)) if t <= now => "expired",
                    _ => "active",
                };
                let scope = k.projects.map_or_else(|| "*".to_string(), |p| p.join(","));
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    k.id,
                    k.name,
                    k.role.as_str(),
                    scope,
                    status
                );
            }
        }
        ["key", "revoke", id] => {
            let id: i64 = id.parse().map_err(|_| format!("invalid key id '{id}'"))?;
            if !cp.revoke_key(id).map_err(err)? {
                return Err(format!("key {id} does not exist or is already revoked"));
            }
            println!("revoked key {id}");
        }
        ["project", "schedule", slug] => {
            let every_hours = match every.as_deref().ok_or("--every <hours|off> is required")? {
                "off" => None,
                n => Some(
                    n.parse::<u32>()
                        .map_err(|_| "--every must be hours or off")?,
                ),
            };
            cp.set_consolidate_schedule(slug, every_hours)
                .map_err(err)?;
            let shown = every_hours.map_or_else(|| "off".to_string(), |h| format!("every {h} h"));
            println!("consolidation schedule {slug}: {shown}");
        }
        ["consolidate"] => {
            let project = project.ok_or("--project is required")?;
            // `None` = Ctrl-C: the run thread may still wait for the LLM, so mark the row here.
            // The handler is registered before the run row exists, so no Ctrl-C leaves it running.
            let (tx, rx) = std::sync::mpsc::channel();
            let interrupt = tx.clone();
            on_ctrl_c(move || {
                let _ = interrupt.send(None);
            })?;
            let run_id = cp
                .start_consolidation(&project, "manual", "cli:admin")
                .map_err(err)?;
            let cp = Arc::new(cp);
            let (worker, name) = (cp.clone(), project.clone());
            std::thread::spawn(move || {
                let key = mem_server::system_key("cli:admin");
                // A panic must still send a result: the Ctrl-C thread keeps the channel open.
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker.finish_consolidation(run_id, &name, &key, full)
                }))
                .unwrap_or_else(|_| {
                    Err(mem_server::ServerError::Internal(
                        "consolidation run panicked".into(),
                    ))
                });
                let _ = tx.send(Some(run));
            });
            let Some(run) = rx.recv().map_err(|e| e.to_string())? else {
                return Err(match cp.abort_run(run_id, "interrupted").map_err(err)? {
                    0 => format!("consolidate {project}: interrupted after run {run_id} finished"),
                    // After the LLM phase a `consolidate/*` branch may already hold notes.
                    _ => format!(
                        "consolidate {project}: interrupted; run {run_id} marked failed \
                         (check for a consolidate/* branch without a run)"
                    ),
                });
            };
            let run = run.map_err(err)?;
            println!(
                "consolidate {project}: {} branch {} files {} notes {} dropped {}",
                run.status,
                run.branch.as_deref().unwrap_or("-"),
                run.files_in,
                run.notes_written,
                run.notes_dropped
            );
            match run.error {
                Some(e) if run.status == "failed" => {
                    return Err(format!("consolidate {project}: failed: {e}"))
                }
                // Partial run: notes were committed; the skipped files run again next time.
                Some(e) => eprintln!("warning: {} batch(es) skipped: {e}", run.batches_failed),
                None => {}
            }
        }
        ["reindex"] => {
            let project = project.ok_or("--project is required")?;
            let engine = cp.engine(&project).map_err(err)?;
            let stats = engine.reindex_all().map_err(|e| e.to_string())?;
            println!(
                "reindexed {project}: {} file(s), {} chunk(s), model {}",
                stats.blobs_newly_indexed,
                stats.chunks_indexed,
                engine.embedding_model()
            );
        }
        ["audit"] => {
            let project = project.ok_or("--project is required")?;
            let limit = match limit {
                Some(raw) => raw
                    .parse()
                    .map_err(|_| "--limit must be a positive integer")?,
                None => 50,
            };
            for a in cp.list_audit(&project, limit).map_err(err)? {
                let commit = a.commit_id.unwrap_or_else(|| "-".into());
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    a.at_ms, a.key_name, a.role, a.project, a.operation, commit
                );
            }
        }
        _ => return Err(ADMIN_USAGE.to_string()),
    }
    Ok(ExitCode::SUCCESS)
}
