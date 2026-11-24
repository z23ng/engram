//! Platform-independent POSIX VFS semantics backed by Engram's `MemVcs` & `MemSearchEngine`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use chrono::Utc;
use mem_core::{MemError, MemoryMetadata, MemoryNode, Revision, VirtualPath};
use mem_search::{parse_wiki_document, MemSearchEngine};
use mem_vcs::{MemVcs, VersionControlEngine};

pub const ROOT_INO: u64 = 1;
const TIER_ROOTS: [(&str, u64); 4] = [
    ("/core", 2),
    ("/episodic", 3),
    ("/semantic", 4),
    ("/procedural", 5),
];

/// Mount access mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountMode {
    ReadOnly,
    ReadWrite,
}

/// Configuration for mounting a memory revision via [`MemoryPosixFs`].
#[derive(Debug, Clone)]
pub struct MountConfig {
    /// Target VCS revision (`BranchWorkspace`, `BranchHead`, or `Commit`).
    pub revision: Revision,
    /// Requested read/write mode (automatically downgraded to `ReadOnly` for immutable revisions).
    pub mode: MountMode,
    /// Automatically re-index written Markdown blobs into FTS5 + Wiki graph on flush/save.
    pub auto_index: bool,
    /// Owner UID reported by `getattr`.
    pub uid: u32,
    /// Owner GID reported by `getattr`.
    pub gid: u32,
}

impl MountConfig {
    /// Creates a writable mount configuration for `BranchWorkspace(branch)`.
    pub fn branch_rw(branch: impl Into<String>) -> Self {
        Self {
            revision: Revision::BranchWorkspace(branch.into()),
            mode: MountMode::ReadWrite,
            auto_index: true,
            uid: current_uid(),
            gid: current_gid(),
        }
    }

    /// Creates a strictly read-only mount configuration for any [`Revision`].
    pub fn read_only(revision: Revision) -> Self {
        Self {
            revision,
            mode: MountMode::ReadOnly,
            auto_index: false,
            uid: current_uid(),
            gid: current_gid(),
        }
    }

    /// Effective mode after enforcing read-only on `BranchHead` and `Commit` revisions.
    pub fn effective_mode(&self) -> MountMode {
        match self.revision {
            Revision::BranchWorkspace(_) => self.mode,
            Revision::BranchHead(_) | Revision::Commit(_) => MountMode::ReadOnly,
        }
    }

    fn writable_branch(&self) -> Result<&str, PosixError> {
        if self.effective_mode() == MountMode::ReadOnly {
            return Err(PosixError::Erofs);
        }
        match &self.revision {
            Revision::BranchWorkspace(b) => Ok(b.as_str()),
            Revision::BranchHead(_) | Revision::Commit(_) => Err(PosixError::Erofs),
        }
    }
}

/// POSIX file kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PosixFileKind {
    Directory,
    RegularFile,
}

/// POSIX file/directory attributes mapped from VFS nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PosixAttr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub mtime_ms: i64,
    pub ctime_ms: i64,
    pub kind: PosixFileKind,
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
}

/// Single entry returned by `readdir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PosixDirEntry {
    pub ino: u64,
    pub kind: PosixFileKind,
    pub name: String,
}

/// POSIX errno abstraction testable across all platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PosixError {
    Enoent,
    Eexist,
    Enotdir,
    Eisdir,
    Enotempty,
    Einval,
    Erofs,
    Eacces,
    Ebadf,
    Eio,
}

impl std::fmt::Display for PosixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Enoent => "ENOENT (no such file or directory)",
            Self::Eexist => "EEXIST (file exists)",
            Self::Enotdir => "ENOTDIR (not a directory)",
            Self::Eisdir => "EISDIR (is a directory)",
            Self::Enotempty => "ENOTEMPTY (directory not empty)",
            Self::Einval => "EINVAL (invalid argument)",
            Self::Erofs => "EROFS (read-only filesystem)",
            Self::Eacces => "EACCES (permission denied)",
            Self::Ebadf => "EBADF (bad file handle)",
            Self::Eio => "EIO (storage error)",
        };
        f.write_str(label)
    }
}

impl std::error::Error for PosixError {}

#[derive(Debug, Clone)]
struct OpenHandle {
    path: String,
    data: Vec<u8>,
    dirty: bool,
    writable: bool,
}

#[derive(Debug)]
struct FsState {
    next_ino: u64,
    next_fh: u64,
    ino_to_path: HashMap<u64, String>,
    path_to_ino: HashMap<String, u64>,
    virtual_dirs: BTreeSet<String>,
    pending_uncommitted_files: HashMap<String, (Vec<u8>, i64)>,
    handles: HashMap<u64, OpenHandle>,
}

impl FsState {
    fn new() -> Self {
        let mut ino_to_path = HashMap::new();
        let mut path_to_ino = HashMap::new();
        let mut virtual_dirs = BTreeSet::new();

        ino_to_path.insert(ROOT_INO, "/".to_string());
        path_to_ino.insert("/".to_string(), ROOT_INO);
        for (tier_path, ino) in TIER_ROOTS {
            ino_to_path.insert(ino, tier_path.to_string());
            path_to_ino.insert(tier_path.to_string(), ino);
            virtual_dirs.insert(tier_path.to_string());
        }

        Self {
            next_ino: 6,
            next_fh: 1,
            ino_to_path,
            path_to_ino,
            virtual_dirs,
            pending_uncommitted_files: HashMap::new(),
            handles: HashMap::new(),
        }
    }

    fn ensure_ino(&mut self, path: &str) -> u64 {
        if let Some(&ino) = self.path_to_ino.get(path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.ino_to_path.insert(ino, path.to_string());
        self.path_to_ino.insert(path.to_string(), ino);
        ino
    }

    fn alloc_fh(&mut self, handle: OpenHandle) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.handles.insert(fh, handle);
        fh
    }
}

/// Thread-safe POSIX filesystem adapter over Engram's [`MemVcs`] and [`MemSearchEngine`].
pub struct MemoryPosixFs {
    search: MemSearchEngine,
    config: MountConfig,
    state: Mutex<FsState>,
}

impl std::fmt::Debug for MemoryPosixFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryPosixFs")
            .field("revision", &self.config.revision)
            .field("effective_mode", &self.config.effective_mode())
            .finish()
    }
}

impl MemoryPosixFs {
    /// Creates a new POSIX adapter exposing `config.revision` from `search`.
    pub fn new(search: MemSearchEngine, config: MountConfig) -> mem_core::Result<Self> {
        // Validate revision exists upfront so mount fails fast on nonexistent branch/commit.
        let _ = search.vcs().list(&config.revision, None)?;
        Ok(Self {
            search,
            config,
            state: Mutex::new(FsState::new()),
        })
    }

    /// Returns a reference to the underlying [`MemVcs`].
    pub fn vcs(&self) -> &MemVcs {
        self.search.vcs()
    }

    /// Returns the mount configuration.
    pub fn config(&self) -> &MountConfig {
        &self.config
    }

    pub fn lookup(&self, parent_ino: u64, name: &str) -> Result<PosixAttr, PosixError> {
        validate_segment(name)?;
        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let parent_path = guard
            .ino_to_path
            .get(&parent_ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;

        let child_path = join_child(&parent_path, name);
        let snap = self.snapshot_tree(&guard)?;

        if child_path == "/" || snap.dirs.contains(&child_path) {
            let ino = guard.ensure_ino(&child_path);
            return Ok(self.dir_attr(ino));
        }

        if let Some((size, mtime_ms)) = snap.files.get(&child_path) {
            let ino = guard.ensure_ino(&child_path);
            let effective_size = active_dirty_size(&guard, &child_path).unwrap_or(*size);
            return Ok(self.file_attr(ino, effective_size, *mtime_ms));
        }

        Err(PosixError::Enoent)
    }

    pub fn getattr(&self, ino: u64) -> Result<PosixAttr, PosixError> {
        let guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let path = guard
            .ino_to_path
            .get(&ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;
        let snap = self.snapshot_tree(&guard)?;

        if path == "/" || snap.dirs.contains(&path) {
            return Ok(self.dir_attr(ino));
        }
        if let Some((size, mtime_ms)) = snap.files.get(&path) {
            let effective_size = active_dirty_size(&guard, &path).unwrap_or(*size);
            return Ok(self.file_attr(ino, effective_size, *mtime_ms));
        }
        Err(PosixError::Enoent)
    }

    pub fn readdir(&self, ino: u64) -> Result<Vec<PosixDirEntry>, PosixError> {
        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let dir_path = guard
            .ino_to_path
            .get(&ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;
        let snap = self.snapshot_tree(&guard)?;

        if dir_path != "/" && !snap.dirs.contains(&dir_path) {
            if snap.files.contains_key(&dir_path) {
                return Err(PosixError::Enotdir);
            }
            return Err(PosixError::Enoent);
        }

        let parent_ino = if dir_path == "/" {
            ROOT_INO
        } else {
            let parent_str = parent_of(&dir_path);
            guard.ensure_ino(&parent_str)
        };

        let mut entries = vec![
            PosixDirEntry {
                ino,
                kind: PosixFileKind::Directory,
                name: ".".to_string(),
            },
            PosixDirEntry {
                ino: parent_ino,
                kind: PosixFileKind::Directory,
                name: "..".to_string(),
            },
        ];

        let mut children: BTreeMap<String, PosixFileKind> = BTreeMap::new();
        for d in &snap.dirs {
            if let Some(child_name) = direct_child_name(&dir_path, d) {
                children.insert(child_name.to_string(), PosixFileKind::Directory);
            }
        }
        for f in snap.files.keys() {
            if let Some(child_name) = direct_child_name(&dir_path, f) {
                children.insert(child_name.to_string(), PosixFileKind::RegularFile);
            }
        }

        for (name, kind) in children {
            let child_full = join_child(&dir_path, &name);
            let child_ino = guard.ensure_ino(&child_full);
            entries.push(PosixDirEntry {
                ino: child_ino,
                kind,
                name,
            });
        }

        Ok(entries)
    }

    pub fn open(&self, ino: u64, write_access: bool, truncate: bool) -> Result<u64, PosixError> {
        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let path = guard
            .ino_to_path
            .get(&ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;

        if write_access || truncate {
            let _ = self.config.writable_branch()?;
        }

        if path == "/" || is_tier_root(&path) || guard.virtual_dirs.contains(&path) {
            return Err(PosixError::Eisdir);
        }
        let snap = self.snapshot_tree(&guard)?;
        if snap.dirs.contains(&path) {
            return Err(PosixError::Eisdir);
        }
        if !snap.files.contains_key(&path) {
            return Err(PosixError::Enoent);
        }

        let mut data = if truncate {
            Vec::new()
        } else {
            self.load_file_bytes(&guard, &path)?
        };

        if truncate {
            guard
                .pending_uncommitted_files
                .insert(path.clone(), (Vec::new(), Utc::now().timestamp_millis()));
        }

        let fh = guard.alloc_fh(OpenHandle {
            path,
            data: std::mem::take(&mut data),
            dirty: truncate,
            writable: write_access || truncate,
        });
        Ok(fh)
    }

    pub fn create(&self, parent_ino: u64, name: &str) -> Result<(PosixAttr, u64), PosixError> {
        let _ = self.config.writable_branch()?;
        validate_segment(name)?;
        if parent_ino == ROOT_INO {
            // Files cannot live directly under '/'; they must belong to /core, /episodic, /semantic, or /procedural.
            return Err(PosixError::Eacces);
        }

        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let parent_path = guard
            .ino_to_path
            .get(&parent_ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;
        let snap = self.snapshot_tree(&guard)?;
        if !snap.dirs.contains(&parent_path) {
            return Err(PosixError::Enotdir);
        }

        let child_path = join_child(&parent_path, name);
        let vpath = VirtualPath::new(&child_path).map_err(|_| PosixError::Einval)?;
        vpath.validate_file_path().map_err(|_| PosixError::Einval)?;

        if snap.dirs.contains(&child_path) || snap.files.contains_key(&child_path) {
            return Err(PosixError::Eexist);
        }

        let now = Utc::now().timestamp_millis();
        guard
            .pending_uncommitted_files
            .insert(child_path.clone(), (Vec::new(), now));
        let ino = guard.ensure_ino(&child_path);
        let fh = guard.alloc_fh(OpenHandle {
            path: child_path,
            data: Vec::new(),
            dirty: true,
            writable: true,
        });
        Ok((self.file_attr(ino, 0, now), fh))
    }

    pub fn read(
        &self,
        ino: u64,
        fh: Option<u64>,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, PosixError> {
        let guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let data = if let Some(fh_id) = fh {
            if let Some(handle) = guard.handles.get(&fh_id) {
                if handle.dirty {
                    handle.data.clone()
                } else {
                    self.load_file_bytes(&guard, &handle.path)?
                }
            } else {
                let path = guard
                    .ino_to_path
                    .get(&ino)
                    .cloned()
                    .ok_or(PosixError::Enoent)?;
                self.load_file_bytes(&guard, &path)?
            }
        } else {
            let path = guard
                .ino_to_path
                .get(&ino)
                .cloned()
                .ok_or(PosixError::Enoent)?;
            self.load_file_bytes(&guard, &path)?
        };

        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if start >= data.len() || size == 0 {
            return Ok(Vec::new());
        }
        let end = start.saturating_add(size).min(data.len());
        Ok(data[start..end].to_vec())
    }

    pub fn write(&self, ino: u64, fh: u64, offset: u64, data: &[u8]) -> Result<usize, PosixError> {
        let _ = self.config.writable_branch()?;
        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let path_for_ino = guard
            .ino_to_path
            .get(&ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;

        let handle = guard.handles.get_mut(&fh).ok_or(PosixError::Ebadf)?;
        if !handle.writable {
            return Err(PosixError::Eacces);
        }

        let off = usize::try_from(offset).map_err(|_| PosixError::Einval)?;
        let needed = off.saturating_add(data.len());
        if handle.data.len() < needed {
            handle.data.resize(needed, 0);
        }
        handle.data[off..needed].copy_from_slice(data);
        handle.dirty = true;
        let updated_buf = handle.data.clone();
        let target_path = handle.path.clone();

        guard
            .pending_uncommitted_files
            .insert(target_path, (updated_buf, Utc::now().timestamp_millis()));
        let _ = path_for_ino;
        Ok(data.len())
    }

    pub fn truncate(&self, ino: u64, new_size: u64) -> Result<PosixAttr, PosixError> {
        let branch = self.config.writable_branch()?.to_string();
        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let path = guard
            .ino_to_path
            .get(&ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;
        let snap = self.snapshot_tree(&guard)?;
        if snap.dirs.contains(&path) {
            return Err(PosixError::Eisdir);
        }
        if !snap.files.contains_key(&path) {
            return Err(PosixError::Enoent);
        }

        let mut bytes = self.load_file_bytes(&guard, &path)?;
        let target_len = usize::try_from(new_size).map_err(|_| PosixError::Einval)?;
        bytes.resize(target_len, 0);

        for handle in guard.handles.values_mut() {
            if handle.path == path {
                handle.data = bytes.clone();
                handle.dirty = true;
            }
        }

        let now = Utc::now().timestamp_millis();
        if guard.pending_uncommitted_files.contains_key(&path) {
            guard
                .pending_uncommitted_files
                .insert(path.clone(), (bytes.clone(), now));
        } else {
            self.persist_bytes_to_vcs(&branch, &path, &bytes)?;
        }

        Ok(self.file_attr(ino, bytes.len() as u64, now))
    }

    pub fn flush(&self, fh: u64) -> Result<(), PosixError> {
        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        self.flush_handle_locked(&mut guard, fh)
    }

    pub fn release(&self, fh: u64) -> Result<(), PosixError> {
        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let res = self.flush_handle_locked(&mut guard, fh);
        guard.handles.remove(&fh);
        res
    }

    pub fn unlink(&self, parent_ino: u64, name: &str) -> Result<(), PosixError> {
        let branch = self.config.writable_branch()?.to_string();
        validate_segment(name)?;
        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let parent_path = guard
            .ino_to_path
            .get(&parent_ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;
        let child_path = join_child(&parent_path, name);

        if is_tier_root(&child_path) {
            return Err(PosixError::Eacces);
        }
        let snap = self.snapshot_tree(&guard)?;
        if snap.dirs.contains(&child_path) {
            return Err(PosixError::Eisdir);
        }
        if !snap.files.contains_key(&child_path) {
            return Err(PosixError::Enoent);
        }

        guard.pending_uncommitted_files.remove(&child_path);
        for handle in guard.handles.values_mut() {
            if handle.path == child_path {
                handle.dirty = false;
            }
        }
        if let Some(old_ino) = guard.path_to_ino.remove(&child_path) {
            guard.ino_to_path.remove(&old_ino);
        }

        if let Ok(vpath) = VirtualPath::new(&child_path) {
            self.search
                .vcs()
                .delete_path(&branch, &vpath)
                .map_err(|_| PosixError::Eio)?;
        }
        Ok(())
    }

    pub fn mkdir(&self, parent_ino: u64, name: &str) -> Result<PosixAttr, PosixError> {
        let _ = self.config.writable_branch()?;
        validate_segment(name)?;
        if parent_ino == ROOT_INO {
            // Only the four cognitive tiers may exist at '/'.
            return Err(PosixError::Eacces);
        }

        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let parent_path = guard
            .ino_to_path
            .get(&parent_ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;
        let snap = self.snapshot_tree(&guard)?;
        if !snap.dirs.contains(&parent_path) {
            return Err(PosixError::Enotdir);
        }

        let child_path = join_child(&parent_path, name);
        let _ = VirtualPath::new(&child_path).map_err(|_| PosixError::Einval)?;
        if snap.dirs.contains(&child_path) || snap.files.contains_key(&child_path) {
            return Err(PosixError::Eexist);
        }

        guard.virtual_dirs.insert(child_path.clone());
        let ino = guard.ensure_ino(&child_path);
        Ok(self.dir_attr(ino))
    }

    pub fn rmdir(&self, parent_ino: u64, name: &str) -> Result<(), PosixError> {
        let _ = self.config.writable_branch()?;
        validate_segment(name)?;
        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let parent_path = guard
            .ino_to_path
            .get(&parent_ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;
        let child_path = join_child(&parent_path, name);

        if is_tier_root(&child_path) || child_path == "/" {
            return Err(PosixError::Eacces);
        }

        let snap = self.snapshot_tree(&guard)?;
        if snap.files.contains_key(&child_path) {
            return Err(PosixError::Enotdir);
        }
        if !snap.dirs.contains(&child_path) {
            return Err(PosixError::Enoent);
        }

        let prefix = format!("{child_path}/");
        if snap.files.keys().any(|f| f.starts_with(&prefix))
            || snap.dirs.iter().any(|d| d.starts_with(&prefix))
        {
            return Err(PosixError::Enotempty);
        }

        guard.virtual_dirs.remove(&child_path);
        if let Some(old_ino) = guard.path_to_ino.remove(&child_path) {
            guard.ino_to_path.remove(&old_ino);
        }
        Ok(())
    }

    pub fn rename(
        &self,
        parent_ino: u64,
        name: &str,
        newparent_ino: u64,
        newname: &str,
    ) -> Result<(), PosixError> {
        let branch = self.config.writable_branch()?.to_string();
        validate_segment(name)?;
        validate_segment(newname)?;
        if parent_ino == ROOT_INO || newparent_ino == ROOT_INO {
            return Err(PosixError::Eacces);
        }

        let mut guard = self.state.lock().map_err(|_| PosixError::Eio)?;
        let old_parent = guard
            .ino_to_path
            .get(&parent_ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;
        let new_parent = guard
            .ino_to_path
            .get(&newparent_ino)
            .cloned()
            .ok_or(PosixError::Enoent)?;

        let snap = self.snapshot_tree(&guard)?;
        if !snap.dirs.contains(&new_parent) {
            return Err(PosixError::Enotdir);
        }

        let old_path = join_child(&old_parent, name);
        let new_path = join_child(&new_parent, newname);

        if is_tier_root(&old_path) || is_tier_root(&new_path) {
            return Err(PosixError::Eacces);
        }
        if old_path == new_path {
            return Ok(());
        }

        let new_vp = VirtualPath::new(&new_path).map_err(|_| PosixError::Einval)?;
        if snap.files.contains_key(&old_path) {
            if snap.dirs.contains(&new_path) {
                return Err(PosixError::Eisdir);
            }
            let bytes = self.load_file_bytes(&guard, &old_path)?;
            self.persist_bytes_to_vcs(&branch, &new_path, &bytes)?;
            let old_vp = VirtualPath::new(&old_path).map_err(|_| PosixError::Einval)?;
            self.search
                .vcs()
                .delete_path(&branch, &old_vp)
                .map_err(|_| PosixError::Eio)?;

            guard.pending_uncommitted_files.remove(&old_path);
            guard.pending_uncommitted_files.remove(&new_path);

            for handle in guard.handles.values_mut() {
                if handle.path == old_path {
                    handle.path = new_path.clone();
                    handle.dirty = false;
                }
            }

            if let Some(old_ino) = guard.path_to_ino.remove(&old_path) {
                if let Some(replaced_ino) = guard.path_to_ino.remove(&new_path) {
                    guard.ino_to_path.remove(&replaced_ino);
                }
                guard.ino_to_path.insert(old_ino, new_path.clone());
                guard.path_to_ino.insert(new_path, old_ino);
            }
            return Ok(());
        }

        if snap.dirs.contains(&old_path) {
            if snap.files.contains_key(&new_path) {
                return Err(PosixError::Enotdir);
            }
            if new_path.starts_with(&format!("{old_path}/")) {
                return Err(PosixError::Einval);
            }

            let old_prefix = format!("{old_path}/");
            let mut moved_files = Vec::new();
            for f in snap.files.keys() {
                if let Some(suffix) = f.strip_prefix(&old_prefix) {
                    let target = format!("{new_path}/{suffix}");
                    let bytes = self.load_file_bytes(&guard, f)?;
                    moved_files.push((f.clone(), target, bytes));
                }
            }

            for (src, dst, bytes) in &moved_files {
                self.persist_bytes_to_vcs(&branch, dst, bytes)?;
                let src_vp = VirtualPath::new(src).map_err(|_| PosixError::Einval)?;
                self.search
                    .vcs()
                    .delete_path(&branch, &src_vp)
                    .map_err(|_| PosixError::Eio)?;
                guard.pending_uncommitted_files.remove(src);
                guard.pending_uncommitted_files.remove(dst);
                for handle in guard.handles.values_mut() {
                    if handle.path == *src {
                        handle.path = dst.clone();
                        handle.dirty = false;
                    }
                }
            }

            let old_vdirs: Vec<String> = guard
                .virtual_dirs
                .iter()
                .filter(|d| *d == &old_path || d.starts_with(&old_prefix))
                .cloned()
                .collect();
            for d in old_vdirs {
                guard.virtual_dirs.remove(&d);
                let replaced = if d == old_path {
                    new_path.clone()
                } else {
                    format!("{new_path}/{}", &d[old_prefix.len()..])
                };
                guard.virtual_dirs.insert(replaced);
            }
            guard.virtual_dirs.insert(new_vp.as_str().to_string());

            // Remap all inode <-> path entries under old_path so open directory/file inodes stay valid.
            let old_inodes: Vec<(String, u64)> = guard
                .path_to_ino
                .iter()
                .filter(|(p, _)| *p == &old_path || p.starts_with(&old_prefix))
                .map(|(p, &ino)| (p.clone(), ino))
                .collect();
            for (old_p, _) in &old_inodes {
                guard.path_to_ino.remove(old_p);
            }
            for (old_p, ino) in old_inodes {
                let mapped = if old_p == old_path {
                    new_path.clone()
                } else {
                    format!("{new_path}/{}", &old_p[old_prefix.len()..])
                };
                if let Some(replaced_ino) = guard.path_to_ino.remove(&mapped) {
                    if replaced_ino != ino {
                        guard.ino_to_path.remove(&replaced_ino);
                    }
                }
                guard.ino_to_path.insert(ino, mapped.clone());
                guard.path_to_ino.insert(mapped, ino);
            }
            return Ok(());
        }

        Err(PosixError::Enoent)
    }

    fn flush_handle_locked(&self, guard: &mut FsState, fh: u64) -> Result<(), PosixError> {
        let Some(handle) = guard.handles.get(&fh) else {
            return Ok(());
        };
        if !handle.dirty {
            return Ok(());
        }
        let branch = self.config.writable_branch()?.to_string();
        let path = handle.path.clone();
        let data = handle.data.clone();

        self.persist_bytes_to_vcs(&branch, &path, &data)?;
        guard.pending_uncommitted_files.remove(&path);
        if let Some(h) = guard.handles.get_mut(&fh) {
            h.dirty = false;
        }
        Ok(())
    }

    fn persist_bytes_to_vcs(
        &self,
        branch: &str,
        path: &str,
        bytes: &[u8],
    ) -> Result<(), PosixError> {
        let vpath = VirtualPath::new(path).map_err(|_| PosixError::Einval)?;
        vpath.validate_file_path().map_err(|_| PosixError::Einval)?;

        let mut meta = MemoryMetadata::default();
        if let Ok(text) = std::str::from_utf8(bytes) {
            let parsed = parse_wiki_document(&vpath, text, 800);
            if !parsed.tags.is_empty() {
                meta.tags = parsed.tags;
            }
            if let Some(imp) = parsed.importance {
                meta.importance = imp;
            }
        }

        let node = MemoryNode::new(vpath.clone(), bytes.to_vec()).with_metadata(meta);
        let blob_id = self
            .search
            .vcs()
            .write_node(branch, node)
            .map_err(|_| PosixError::Eio)?;
        if self.config.auto_index {
            self.search
                .index_blob_if_needed(&vpath, &blob_id)
                .map_err(|_| PosixError::Eio)?;
        }
        Ok(())
    }

    fn load_file_bytes(&self, guard: &FsState, path: &str) -> Result<Vec<u8>, PosixError> {
        if let Some((buf, _)) = guard.pending_uncommitted_files.get(path) {
            return Ok(buf.clone());
        }
        let vpath = VirtualPath::new(path).map_err(|_| PosixError::Enoent)?;
        let node = self
            .search
            .vcs()
            .read(&self.config.revision, &vpath)
            .map_err(|_| PosixError::Eio)?
            .ok_or(PosixError::Enoent)?;
        Ok(node.content)
    }

    fn snapshot_tree(&self, guard: &FsState) -> Result<TreeSnapshot, PosixError> {
        let entries = self
            .search
            .vcs()
            .list(&self.config.revision, None)
            .map_err(|_| PosixError::Eio)?;

        let mut dirs: BTreeSet<String> = guard.virtual_dirs.clone();
        let mut files: BTreeMap<String, (u64, i64)> = BTreeMap::new();

        for (vpath, blob_id, meta) in entries {
            let path_str = vpath.as_str().to_string();
            let raw_size = self
                .search
                .vcs()
                .store()
                .get_blob(&blob_id)
                .map_err(|_| PosixError::Eio)?
                .map(|(b, _)| b.len() as u64)
                .unwrap_or(0);
            files.insert(path_str.clone(), (raw_size, meta.updated_at_ms));
            register_ancestor_dirs(&path_str, &mut dirs);
        }

        for (path_str, (buf, ts)) in &guard.pending_uncommitted_files {
            files.insert(path_str.clone(), (buf.len() as u64, *ts));
            register_ancestor_dirs(path_str, &mut dirs);
        }

        Ok(TreeSnapshot { dirs, files })
    }

    fn dir_attr(&self, ino: u64) -> PosixAttr {
        let perm = if self.config.effective_mode() == MountMode::ReadOnly {
            0o555
        } else {
            0o755
        };
        PosixAttr {
            ino,
            size: 0,
            blocks: 0,
            mtime_ms: 0,
            ctime_ms: 0,
            kind: PosixFileKind::Directory,
            perm,
            nlink: 2,
            uid: self.config.uid,
            gid: self.config.gid,
        }
    }

    fn file_attr(&self, ino: u64, size: u64, mtime_ms: i64) -> PosixAttr {
        let perm = if self.config.effective_mode() == MountMode::ReadOnly {
            0o444
        } else {
            0o644
        };
        PosixAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            mtime_ms,
            ctime_ms: mtime_ms,
            kind: PosixFileKind::RegularFile,
            perm,
            nlink: 1,
            uid: self.config.uid,
            gid: self.config.gid,
        }
    }
}

struct TreeSnapshot {
    dirs: BTreeSet<String>,
    files: BTreeMap<String, (u64, i64)>,
}

fn active_dirty_size(guard: &FsState, path: &str) -> Option<u64> {
    guard
        .handles
        .values()
        .filter(|h| h.path == path && h.dirty)
        .map(|h| h.data.len() as u64)
        .max()
}

fn register_ancestor_dirs(file_path: &str, dirs: &mut BTreeSet<String>) {
    let mut cur = parent_of(file_path);
    while cur != "/" {
        dirs.insert(cur.clone());
        cur = parent_of(&cur);
    }
}

fn is_tier_root(path: &str) -> bool {
    TIER_ROOTS.iter().any(|(tp, _)| *tp == path)
}

fn validate_segment(name: &str) -> Result<(), PosixError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.chars().any(|c| c == '\0' || c.is_control())
    {
        return Err(PosixError::Einval);
    }
    Ok(())
}

fn join_child(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn parent_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/".to_string(),
        Some((prefix, _)) => prefix.to_string(),
    }
}

fn direct_child_name<'a>(dir: &str, candidate: &'a str) -> Option<&'a str> {
    if dir == "/" {
        let rest = candidate.strip_prefix('/')?;
        if !rest.is_empty() && !rest.contains('/') {
            return Some(rest);
        }
        return None;
    }
    let prefix = format!("{dir}/");
    let rest = candidate.strip_prefix(&prefix)?;
    if !rest.is_empty() && !rest.contains('/') {
        return Some(rest);
    }
    None
}

fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe_or_env_id("UID", 1000)
    }
    #[cfg(not(unix))]
    {
        1000
    }
}

fn current_gid() -> u32 {
    #[cfg(unix)]
    {
        unsafe_or_env_id("GID", 1000)
    }
    #[cfg(not(unix))]
    {
        1000
    }
}

fn unsafe_or_env_id(var: &str, fallback: u32) -> u32 {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(fallback)
}

#[allow(dead_code)]
fn _assert_error_mapping(_: MemError) -> PosixError {
    PosixError::Eio
}
