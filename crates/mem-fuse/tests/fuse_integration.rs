use mem_core::{MemoryNode, Revision, VirtualPath};
use mem_fuse::{MemoryPosixFs, MountConfig, PosixError, PosixFileKind, ROOT_INO};
use mem_search::{MemSearchEngine, SearchRequest};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};

fn vp(s: &str) -> VirtualPath {
    VirtualPath::new(s).expect("valid virtual path")
}

fn setup_search() -> MemSearchEngine {
    let store = SqliteStore::open_in_memory().unwrap();
    let vcs = MemVcs::new(store);
    vcs.init().unwrap();
    MemSearchEngine::new_bm25_only(vcs)
}

#[test]
fn posix_vfs_supports_create_write_read_truncate_mkdir_rename_unlink_and_search() {
    let search = setup_search();
    let vcs = search.vcs();

    vcs.write(
        "main",
        MemoryNode::new(vp("/core/rules.md"), b"Always pin rules.".to_vec()),
    )
    .unwrap();
    vcs.commit("main", "init", "seed core").unwrap();

    // Branch isolation: create a sibling branch whose edits stay invisible.
    vcs.create_branch("other", &Revision::BranchHead("main".into()))
        .unwrap();
    vcs.write(
        "other",
        MemoryNode::new(vp("/semantic/secret.md"), b"other branch secret".to_vec()),
    )
    .unwrap();

    let fs = MemoryPosixFs::new(search.clone(), MountConfig::branch_rw("main")).unwrap();

    // Root directory lists the 4 canonical cognitive tiers.
    let root_entries = fs.readdir(ROOT_INO).unwrap();
    let root_names: Vec<&str> = root_entries.iter().map(|e| e.name.as_str()).collect();
    assert!(root_names.contains(&"core"));
    assert!(root_names.contains(&"semantic"));
    assert!(root_names.contains(&"procedural"));
    assert!(root_names.contains(&"episodic"));

    // Lookup /core/rules.md and read its bytes.
    let core_dir = fs.lookup(ROOT_INO, "core").unwrap();
    assert_eq!(core_dir.kind, PosixFileKind::Directory);
    let rules_attr = fs.lookup(core_dir.ino, "rules.md").unwrap();
    let fh = fs.open(rules_attr.ino, false, false).unwrap();
    let bytes = fs.read(rules_attr.ino, Some(fh), 0, 1024).unwrap();
    assert_eq!(bytes, b"Always pin rules.");
    fs.release(fh).unwrap();

    // Must not see sibling branch's /semantic/secret.md.
    let sem_dir = fs.lookup(ROOT_INO, "semantic").unwrap();
    assert_eq!(fs.lookup(sem_dir.ino, "secret.md"), Err(PosixError::Enoent));

    // Create subdirectory /semantic/notes, write file, flush, and verify search hits it.
    let notes_dir = fs.mkdir(sem_dir.ino, "notes").unwrap();
    let (tmp_attr, wfh) = fs.create(notes_dir.ino, "draft.md.tmp").unwrap();
    let md = "---\ntitle: \"POSIX Mount Note\"\naliases: [\"FUSE_MOUNT_OK\"]\n---\n\n# POSIX\nWritten through FUSE adapter.\n";
    let written = fs.write(tmp_attr.ino, wfh, 0, md.as_bytes()).unwrap();
    assert_eq!(written, md.len());
    fs.flush(wfh).unwrap();
    fs.release(wfh).unwrap();

    // Atomic editor rename: draft.md.tmp -> posix-note.md.
    fs.rename(
        notes_dir.ino,
        "draft.md.tmp",
        notes_dir.ino,
        "posix-note.md",
    )
    .unwrap();
    assert_eq!(
        fs.lookup(notes_dir.ino, "draft.md.tmp"),
        Err(PosixError::Enoent)
    );
    let renamed_attr = fs.lookup(notes_dir.ino, "posix-note.md").unwrap();
    assert_eq!(renamed_attr.size, md.len() as u64);

    // Written node is immediately readable from MemVcs and searchable via MemSearchEngine!
    let node = vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/notes/posix-note.md"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(node.content_str().unwrap(), md);

    let hits = search
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "FUSE_MOUNT_OK",
        ))
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, vp("/semantic/notes/posix-note.md"));

    // Truncate, unlink, and rmdir.
    let trunc_attr = fs.truncate(renamed_attr.ino, 10).unwrap();
    assert_eq!(trunc_attr.size, 10);

    // Rename directory /semantic/notes -> /semantic/archived_notes and verify inode & child file move cleanly.
    fs.rename(sem_dir.ino, "notes", sem_dir.ino, "archived_notes")
        .unwrap();
    assert_eq!(fs.lookup(sem_dir.ino, "notes"), Err(PosixError::Enoent));
    let moved_dir = fs.lookup(sem_dir.ino, "archived_notes").unwrap();
    assert_eq!(moved_dir.ino, notes_dir.ino);
    assert_eq!(
        fs.getattr(notes_dir.ino).unwrap().kind,
        PosixFileKind::Directory
    );
    let moved_file = fs.lookup(moved_dir.ino, "posix-note.md").unwrap();
    assert_eq!(moved_file.size, 10);
    assert!(vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/archived_notes/posix-note.md"),
        )
        .unwrap()
        .is_some());

    fs.unlink(moved_dir.ino, "posix-note.md").unwrap();
    fs.rmdir(sem_dir.ino, "archived_notes").unwrap();
    assert_eq!(
        fs.lookup(sem_dir.ino, "archived_notes"),
        Err(PosixError::Enoent)
    );

    // Cannot create files directly under '/', nor delete tier roots.
    assert_eq!(fs.create(ROOT_INO, "invalid.md"), Err(PosixError::Eacces));
    assert_eq!(fs.rmdir(ROOT_INO, "semantic"), Err(PosixError::Eacces));
}

#[test]
fn posix_vfs_enforces_read_only_for_commits_and_ro_mounts() {
    let search = setup_search();
    let vcs = search.vcs();
    vcs.write(
        "main",
        MemoryNode::new(vp("/core/immutable.md"), b"immutable".to_vec()),
    )
    .unwrap();
    let c1 = vcs.commit("main", "init", "snapshot").unwrap();

    // Even if someone passes BranchHead or Commit, mutations return EROFS.
    let ro_fs = MemoryPosixFs::new(search, MountConfig::read_only(Revision::Commit(c1))).unwrap();
    let core = ro_fs.lookup(ROOT_INO, "core").unwrap();
    assert_eq!(ro_fs.create(core.ino, "new.md"), Err(PosixError::Erofs));
    assert_eq!(ro_fs.mkdir(core.ino, "sub"), Err(PosixError::Erofs));
    assert_eq!(
        ro_fs.unlink(core.ino, "immutable.md"),
        Err(PosixError::Erofs)
    );
    let file_attr = ro_fs.lookup(core.ino, "immutable.md").unwrap();
    assert_eq!(
        ro_fs.open(file_attr.ino, true, false),
        Err(PosixError::Erofs)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_real_fuse_mount_round_trips_std_fs_into_vcs_and_search() {
    use std::fs;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let db_path = dir.path().join("fuse_mount.db");
    let mount_dir = dir.path().join("mnt");
    fs::create_dir_all(&mount_dir).unwrap();

    let store = SqliteStore::open(&db_path).unwrap();
    let vcs = MemVcs::new(store);
    vcs.init().unwrap();
    vcs.write(
        "main",
        MemoryNode::new(vp("/core/hello.md"), b"hello from fuse".to_vec()),
    )
    .unwrap();
    vcs.commit("main", "init", "seed fuse").unwrap();

    let search = MemSearchEngine::new_bm25_only(vcs.clone());
    let posix_fs = MemoryPosixFs::new(search.clone(), MountConfig::branch_rw("main")).unwrap();

    let session = match mem_fuse::spawn_mount(posix_fs, &mount_dir) {
        Ok(sess) => sess,
        Err(err) => {
            // Restricted container / sandbox environments without FUSE mount permissions
            // may return PermissionDenied or ENODEV/EPERM; skip kernel mount gracefully there.
            eprintln!("skipping real kernel FUSE mount in sandboxed environment: {err}");
            return;
        }
    };

    // Perform real POSIX filesystem operations through the mounted directory.
    assert_eq!(
        fs::read_to_string(mount_dir.join("core/hello.md")).unwrap(),
        "hello from fuse"
    );

    fs::create_dir(mount_dir.join("semantic/guides")).unwrap();
    let note_body = "---\ntitle: \"Live FUSE Guide\"\naliases: [\"POSIX_LIVE_KEY\"]\n---\n\n# Guide\nEdited via std::fs.\n";
    fs::write(mount_dir.join("semantic/guides/live.md"), note_body).unwrap();
    assert_eq!(
        fs::read_to_string(mount_dir.join("semantic/guides/live.md")).unwrap(),
        note_body
    );

    fs::rename(
        mount_dir.join("semantic/guides/live.md"),
        mount_dir.join("semantic/guides/renamed.md"),
    )
    .unwrap();
    assert!(!mount_dir.join("semantic/guides/live.md").exists());

    drop(session);

    // Verify changes persisted in MemVcs and are searchable via MemSearchEngine.
    let persisted = vcs
        .read(
            &Revision::BranchWorkspace("main".into()),
            &vp("/semantic/guides/renamed.md"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(persisted.content_str().unwrap(), note_body);

    let hits = search
        .search(&SearchRequest::new(
            Revision::BranchWorkspace("main".into()),
            "POSIX_LIVE_KEY",
        ))
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, vp("/semantic/guides/renamed.md"));
}
