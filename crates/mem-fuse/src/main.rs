use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use mem_core::{CommitId, Revision};
use mem_fuse::{mount_blocking, MemoryPosixFs, MountConfig, MountMode};
use mem_search::MemSearchEngine;
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    let mut db_path = default_db_path();
    let mut branch = "main".to_string();
    let mut commit_hex: Option<String> = None;
    let mut read_only = false;
    let mut mountpoint: Option<PathBuf> = None;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                db_path = PathBuf::from(args.get(i).ok_or("--db requires a file path")?);
            }
            "--branch" | "-b" => {
                i += 1;
                branch = args
                    .get(i)
                    .ok_or("--branch requires a branch name")?
                    .clone();
            }
            "--commit" | "-c" => {
                i += 1;
                commit_hex = Some(
                    args.get(i)
                        .ok_or("--commit requires a commit hash")?
                        .clone(),
                );
            }
            "--read-only" | "--ro" => {
                read_only = true;
            }
            other => {
                if mountpoint.is_none() {
                    mountpoint = Some(PathBuf::from(other));
                } else {
                    return Err(format!("unexpected argument '{other}'"));
                }
            }
        }
        i += 1;
    }

    let target_dir = mountpoint.ok_or(
        "usage: mem-fuse <MOUNTPOINT> [--db <PATH>] [--branch <b> | --commit <cid>] [--read-only]",
    )?;
    std::fs::create_dir_all(&target_dir).map_err(|e| {
        format!(
            "failed to prepare mountpoint '{}': {e}",
            target_dir.display()
        )
    })?;

    let store = SqliteStore::open(&db_path).map_err(|e| e.to_string())?;
    let vcs = MemVcs::new(store);
    vcs.init().map_err(|e| e.to_string())?;
    let search = MemSearchEngine::new_bm25_only(vcs);

    let cfg = if let Some(cid_raw) = commit_hex {
        let cid = CommitId::from_hex(&cid_raw).map_err(|e| e.to_string())?;
        MountConfig::read_only(Revision::Commit(cid))
    } else if read_only {
        MountConfig::read_only(Revision::BranchWorkspace(branch))
    } else {
        let mut c = MountConfig::branch_rw(branch);
        c.mode = MountMode::ReadWrite;
        c
    };

    let fs = MemoryPosixFs::new(search, cfg).map_err(|e| e.to_string())?;
    eprintln!(
        "Mounting Engram VFS at {} (press Ctrl-C or run fusermount -u to unmount)...",
        target_dir.display()
    );
    mount_blocking(fs, &target_dir).map_err(|e| format!("FUSE mount failed: {e}"))
}

fn default_db_path() -> PathBuf {
    if let Some(p) = env::var_os("MEM_DB_PATH").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(home) = env::var_os("MEM_HOME").or_else(|| env::var_os("HOME")) {
        let mut p = PathBuf::from(home);
        if env::var_os("MEM_HOME").is_none() {
            p.push(".ai-memory");
        }
        p.push("memory.db");
        p
    } else {
        PathBuf::from("memory.db")
    }
}

fn print_help() {
    println!(
        r#"mem-fuse — POSIX FUSE Mount Adapter for Engram

USAGE:
  mem-fuse <MOUNTPOINT> [OPTIONS]

OPTIONS:
  --db <PATH>          SQLite database path (default: $MEM_DB_PATH or ~/.ai-memory/memory.db)
  --branch <NAME>      Branch workspace to mount (default: main)
  --commit <HASH>      Mount an immutable historical commit snapshot (read-only)
  --read-only          Mount branch workspace read-only
  -h, --help           Print help
"#
    );
}
