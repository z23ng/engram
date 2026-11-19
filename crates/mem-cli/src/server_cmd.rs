//! `mem serve` and `mem admin` for the multi-project Engram server.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use mem_search::MemSearchEngine;
use mem_server::{ControlPlane, EngineFactory, Role};
use mem_vcs::MemVcs;

use crate::{build_search_engine, GlobalOpts};

type CmdResult = Result<ExitCode, String>;

const ADMIN_USAGE: &str = "usage: mem admin --data <dir> (project create <slug> | project list | \
key create --name <n> --role admin|dev|pm|bot [--projects a,b] [--expires-days N] | key list | key revoke <id>)";

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
    let factory: EngineFactory = Box::new(move |vcs: MemVcs| build_search_engine(vcs, &emb_cfg).0);
    let (cp, _) = open_control(&mut args, factory)?;
    if let Some(extra) = args.first() {
        return Err(format!("unexpected serve argument '{extra}'"));
    }
    let now = Utc::now().timestamp_millis();
    let has_active_admin = cp.list_keys().map_err(|e| e.to_string())?.iter().any(|k| {
        k.role == Role::Admin && !k.revoked && k.expires_at_ms.is_none_or(|t| t > now)
    });
    if !has_active_admin {
        let (_, plain) = cp
            .create_key("admin", Role::Admin, None, None)
            .map_err(|e| e.to_string())?;
        println!("admin key (shown once): {plain}");
    }
    mem_server::run_server(Arc::new(cp), &listen).map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

pub fn cmd_admin(args: &[String]) -> CmdResult {
    let mut args = args.to_vec();
    let (cp, _) = open_control(&mut args, Box::new(MemSearchEngine::new_bm25_only))?;
    let name = take_flag(&mut args, "--name")?;
    let role = take_flag(&mut args, "--role")?;
    let projects = take_flag(&mut args, "--projects")?;
    let expires_days = take_flag(&mut args, "--expires-days")?;
    let err = |e: mem_server::ServerError| e.to_string();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["project", "create", slug] => {
            cp.create_project(slug).map_err(err)?;
            println!("created project {slug}");
        }
        ["project", "list"] => {
            cp.list_projects().map_err(err)?.iter().for_each(|p| println!("{p}"));
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
            println!("created key id={} name={} role={}", key.id, key.name, role.as_str());
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
                println!("{}\t{}\t{}\t{}\t{}", k.id, k.name, k.role.as_str(), scope, status);
            }
        }
        ["key", "revoke", id] => {
            let id: i64 = id.parse().map_err(|_| format!("invalid key id '{id}'"))?;
            if !cp.revoke_key(id).map_err(err)? {
                return Err(format!("key {id} does not exist or is already revoked"));
            }
            println!("revoked key {id}");
        }
        _ => return Err(ADMIN_USAGE.to_string()),
    }
    Ok(ExitCode::SUCCESS)
}
