//! `DavFileSystem` over one branch workspace: tier folders and `.md` files only.

use std::collections::BTreeMap;
use std::fmt;
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures_util::{stream, FutureExt};
use mem_core::{MemError, MemoryNode, Revision, VirtualPath};
use mem_search::MemSearchEngine;
use mem_vcs::VersionControlEngine;

use crate::{ApiKey, ControlPlane};

pub(crate) const TIERS: [&str; 4] = ["core", "semantic", "procedural", "episodic"];
pub(crate) const MAX_FILE: usize = 8 * 1024 * 1024;

/// Request-scoped filesystem: one project, one branch, one verified key.
#[derive(Clone)]
pub(crate) struct BranchFs {
    pub(crate) cp: Arc<ControlPlane>,
    pub(crate) engine: MemSearchEngine,
    pub(crate) project: String,
    pub(crate) branch: String,
    pub(crate) key: ApiKey,
}

#[derive(Debug, Clone)]
struct Meta {
    len: u64,
    modified: SystemTime,
    dir: bool,
    etag: Option<String>,
}

impl DavMetaData for Meta {
    fn len(&self) -> u64 {
        self.len
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }
    fn is_dir(&self) -> bool {
        self.dir
    }
    fn is_symlink(&self) -> bool {
        false
    }
    fn etag(&self) -> Option<String> {
        let ms = self
            .modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Some(self.etag.clone().unwrap_or_else(|| format!("{ms:x}")))
    }
}

struct Entry {
    name: String,
    meta: Meta,
}

impl DavDirEntry for Entry {
    fn name(&self) -> Vec<u8> {
        self.name.as_bytes().to_vec()
    }
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.meta.clone();
        async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) }.boxed()
    }
}

fn segments(path: &DavPath) -> Vec<String> {
    let rel = path.as_rel_ospath().to_string_lossy().to_string();
    rel.split('/')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn hidden(seg: &str) -> bool {
    seg.starts_with('.') || seg == "_templates"
}

fn in_tier(segs: &[String]) -> bool {
    !segs.is_empty() && TIERS.contains(&segs[0].as_str()) && !segs.iter().any(|s| hidden(s))
}

fn to_vpath(segs: &[String]) -> FsResult<VirtualPath> {
    VirtualPath::new(format!("/{}", segs.join("/"))).map_err(|_| FsError::Forbidden)
}

const IMAGES: [(&str, &str); 5] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

/// Content type of an allowed image name; SVG is excluded because it can carry scripts.
fn image_type(name: &str) -> Option<&'static str> {
    let (_, ext) = name.rsplit_once('.')?;
    IMAGES
        .iter()
        .find(|(e, _)| ext.eq_ignore_ascii_case(e))
        .map(|(_, t)| *t)
}

/// Writable target: a `.md` file below a tier folder, or an image below
/// `<tier>/_attachments/` for every tier except `core`. Everything else is forbidden.
fn writable(segs: &[String]) -> bool {
    let Some(name) = segs.last() else {
        return false;
    };
    let md = segs.len() >= 2 && name.ends_with(".md");
    let image = segs.len() >= 3
        && segs[0] != "core"
        && segs[1] == "_attachments"
        && image_type(name).is_some();
    in_tier(segs) && (md || image)
}

/// Content type of an image path that WebDAV and REST uploads accept; `None` otherwise.
pub(crate) fn attachment_type(vp: &VirtualPath) -> Option<&'static str> {
    let segs: Vec<String> = vp.as_str().split('/').skip(1).map(String::from).collect();
    image_type(vp.as_str()).filter(|_| writable(&segs))
}

fn file_target(path: &DavPath) -> FsResult<VirtualPath> {
    let segs = segments(path);
    if writable(&segs) {
        to_vpath(&segs)
    } else {
        Err(FsError::Forbidden)
    }
}

fn fs_err(err: MemError) -> FsError {
    match err {
        MemError::BranchNotFound(_)
        | MemError::CommitNotFound(_)
        | MemError::TreeNotFound(_)
        | MemError::BlobNotFound(_) => FsError::NotFound,
        MemError::InvalidPath(_) | MemError::DirectoryFileConflict { .. } => FsError::Forbidden,
        _ => FsError::GeneralFailure,
    }
}

fn ms_to_time(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(u64::try_from(ms).unwrap_or(0))
}

fn dir_meta(modified: SystemTime) -> Meta {
    Meta {
        len: 0,
        modified,
        dir: true,
        etag: None,
    }
}

async fn blocking<T, F>(f: F) -> FsResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> FsResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| FsError::GeneralFailure)?
}

impl BranchFs {
    fn rev(&self) -> Revision {
        Revision::BranchWorkspace(self.branch.clone())
    }

    fn head_time(&self) -> FsResult<SystemTime> {
        let vcs = self.engine.vcs();
        let head = vcs.head_commit(&self.branch).map_err(fs_err)?;
        let info = vcs
            .store()
            .get_commit(&head)
            .map_err(fs_err)?
            .ok_or(FsError::NotFound)?;
        Ok(ms_to_time(info.created_at_ms))
    }

    fn read_node(&self, vp: &VirtualPath) -> FsResult<Option<MemoryNode>> {
        self.engine.vcs().read(&self.rev(), vp).map_err(fs_err)
    }

    fn file_meta(&self, node: &MemoryNode, fallback: SystemTime) -> Meta {
        let ms = node.metadata.updated_at_ms;
        Meta {
            len: node.content.len() as u64,
            modified: if ms > 0 { ms_to_time(ms) } else { fallback },
            dir: false,
            etag: node.blob_id.as_ref().map(|b| b.to_hex()),
        }
    }

    fn mkcol_dirs(&self) -> Vec<String> {
        let dirs = self
            .cp
            .dav_dirs
            .lock()
            .map(|d| d.clone())
            .unwrap_or_default();
        let mine = dirs
            .into_iter()
            .filter(|(p, b, _)| *p == self.project && *b == self.branch);
        mine.map(|(_, _, path)| path).collect()
    }

    fn lookup(&self, segs: &[String]) -> FsResult<Meta> {
        let head = self.head_time()?;
        if segs.is_empty() || (segs.len() == 1 && in_tier(segs)) {
            return Ok(dir_meta(head));
        }
        if !in_tier(segs) {
            return Err(FsError::NotFound);
        }
        let vp = to_vpath(segs).map_err(|_| FsError::NotFound)?;
        if let Some(node) = self.read_node(&vp)? {
            return Ok(self.file_meta(&node, head));
        }
        let has_files = !self
            .engine
            .vcs()
            .list(&self.rev(), Some(&vp))
            .map_err(fs_err)?
            .is_empty();
        if has_files || self.mkcol_dirs().contains(&vp.as_str().to_string()) {
            return Ok(dir_meta(head));
        }
        Err(FsError::NotFound)
    }

    fn children(&self, segs: &[String]) -> FsResult<Vec<(String, Meta)>> {
        let head = self.head_time()?;
        if segs.is_empty() {
            return Ok(TIERS
                .iter()
                .map(|t| (t.to_string(), dir_meta(head)))
                .collect());
        }
        let vp = to_vpath(segs).map_err(|_| FsError::NotFound)?;
        let base = format!("{}/", vp.as_str().trim_end_matches('/'));
        let mut out: BTreeMap<String, Meta> = BTreeMap::new();
        for (path, _, _) in self
            .engine
            .vcs()
            .list(&self.rev(), Some(&vp))
            .map_err(fs_err)?
        {
            let Some(rest) = path.as_str().strip_prefix(&base) else {
                continue;
            };
            match rest.split_once('/') {
                Some((dir, _)) => {
                    out.entry(dir.to_string()).or_insert_with(|| dir_meta(head));
                }
                None => {
                    if let Some(node) = self.read_node(&path)? {
                        out.insert(rest.to_string(), self.file_meta(&node, head));
                    }
                }
            }
        }
        for dir in self.mkcol_dirs() {
            if let Some(name) = dir.strip_prefix(&base).filter(|n| !n.contains('/')) {
                out.entry(name.to_string())
                    .or_insert_with(|| dir_meta(head));
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Every DAV write (PUT, COPY, MOVE) passes this check; images are stored raw, not indexed.
    fn write_file(&self, vp: &VirtualPath, content: Vec<u8>) -> FsResult<()> {
        let segs: Vec<String> = vp.as_str().split('/').skip(1).map(String::from).collect();
        if !writable(&segs) {
            return Err(FsError::Forbidden);
        }
        let mut node = MemoryNode::new(vp.clone(), content);
        let image = image_type(vp.as_str());
        if let Some(content_type) = image {
            node.metadata.content_type = content_type.into();
        }
        let blob_id = self
            .engine
            .vcs()
            .write_node(&self.branch, node)
            .map_err(fs_err)?;
        if image.is_none() {
            self.engine
                .index_blob_if_needed(vp, &blob_id)
                .map_err(fs_err)?;
        }
        Ok(())
    }

    fn delete(&self, vp: &VirtualPath) -> FsResult<usize> {
        self.engine
            .vcs()
            .delete_path(&self.branch, vp)
            .map_err(fs_err)
    }

    /// Copies a file, or every file below a folder, from `from` to `to`.
    fn copy_tree(&self, from: &[String], to: &[String]) -> FsResult<usize> {
        let (src, dst) = (to_vpath(from)?, to_vpath(to)?);
        if let Some(node) = self.read_node(&src)? {
            self.write_file(&dst, node.content)?;
            return Ok(1);
        }
        let base = format!("{}/", src.as_str());
        let mut copies = Vec::new();
        for (path, _, _) in self
            .engine
            .vcs()
            .list(&self.rev(), Some(&src))
            .map_err(fs_err)?
        {
            let Some(rest) = path.as_str().strip_prefix(&base) else {
                continue;
            };
            if let Some(node) = self.read_node(&path)? {
                let target =
                    VirtualPath::new(format!("{}/{rest}", dst.as_str())).map_err(fs_err)?;
                copies.push((target, node.content));
            }
        }
        if copies.is_empty() {
            return Err(FsError::NotFound);
        }
        // Check every target first so a forbidden file does not leave a partial copy.
        let target_segs = |p: &VirtualPath| -> Vec<String> {
            p.as_str().split('/').skip(1).map(String::from).collect()
        };
        if !copies.iter().all(|(p, _)| writable(&target_segs(p))) {
            return Err(FsError::Forbidden);
        }
        let copied = copies.len();
        for (target, content) in copies {
            self.write_file(&target, content)?;
        }
        Ok(copied)
    }

    fn noted(&self) {
        self.cp
            .note_dav_change(&self.project, &self.branch, &self.key);
    }
}

impl DavFileSystem for BranchFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            if options.write || options.append || options.create || options.truncate {
                if options.append {
                    return Err(FsError::NotImplemented);
                }
                let vp = file_target(path)?;
                if options.size.is_some_and(|s| s > MAX_FILE as u64) {
                    return Err(FsError::TooLarge);
                }
                if options.create_new {
                    let (fs, v) = (self.clone(), vp.clone());
                    if blocking(move || fs.read_node(&v)).await?.is_some() {
                        return Err(FsError::Exists);
                    }
                }
                return Ok(Box::new(MemFile::writer(self.clone(), vp)) as Box<dyn DavFile>);
            }
            let segs = segments(path);
            if !in_tier(&segs) {
                return Err(FsError::NotFound);
            }
            let fs = self.clone();
            let (data, meta) = blocking(move || {
                let head = fs.head_time()?;
                let vp = to_vpath(&segs).map_err(|_| FsError::NotFound)?;
                let node = fs.read_node(&vp)?.ok_or(FsError::NotFound)?;
                let meta = fs.file_meta(&node, head);
                Ok((node.content, meta))
            })
            .await?;
            Ok(Box::new(MemFile::reader(self.clone(), data, meta)) as Box<dyn DavFile>)
        }
        .boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let (fs, segs) = (self.clone(), segments(path));
            let list = blocking(move || fs.children(&segs)).await?;
            let entries: Vec<FsResult<Box<dyn DavDirEntry>>> = list
                .into_iter()
                .map(|(name, meta)| Ok(Box::new(Entry { name, meta }) as Box<dyn DavDirEntry>))
                .collect();
            Ok(Box::pin(stream::iter(entries)) as FsStream<Box<dyn DavDirEntry>>)
        }
        .boxed()
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let (fs, segs) = (self.clone(), segments(path));
            let meta = blocking(move || fs.lookup(&segs)).await?;
            Ok(Box::new(meta) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    fn symlink_metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        self.metadata(path)
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let segs = segments(path);
            if segs.is_empty() || (segs.len() == 1 && in_tier(&segs)) {
                return Err(FsError::Exists);
            }
            if !in_tier(&segs) || segs.last().is_some_and(|s| s.ends_with(".md")) {
                return Err(FsError::Forbidden);
            }
            let dir = to_vpath(&segs)?.as_str().to_string();
            if let Ok(mut dirs) = self.cp.dav_dirs.lock() {
                dirs.insert((self.project.clone(), self.branch.clone(), dir));
            }
            Ok(())
        }
        .boxed()
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let vp = file_target(path).map_err(|_| FsError::NotFound)?;
            let fs = self.clone();
            if blocking(move || fs.delete(&vp)).await? == 0 {
                return Err(FsError::NotFound);
            }
            self.noted();
            Ok(())
        }
        .boxed()
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let segs = segments(path);
            if segs.len() < 2 || !in_tier(&segs) {
                return Err(FsError::Forbidden);
            }
            let vp = to_vpath(&segs)?;
            if let Ok(mut dirs) = self.cp.dav_dirs.lock() {
                let prefix = format!("{}/", vp.as_str());
                dirs.retain(|(p, b, d)| {
                    !(*p == self.project
                        && *b == self.branch
                        && (*d == vp.as_str() || d.starts_with(&prefix)))
                });
            }
            let fs = self.clone();
            if blocking(move || fs.delete(&vp)).await? > 0 {
                self.noted();
            }
            Ok(())
        }
        .boxed()
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let (src, dst) = (segments(from), segments(to));
            let is_file = file_target(from).is_ok();
            if (is_file && file_target(to).is_err())
                || dst.len() < 2
                || !in_tier(&dst)
                || src.len() < 2
            {
                return Err(FsError::Forbidden);
            }
            let fs = self.clone();
            blocking(move || {
                fs.copy_tree(&src, &dst)?;
                fs.delete(&to_vpath(&src)?)
            })
            .await?;
            self.noted();
            Ok(())
        }
        .boxed()
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            file_target(to)?;
            let (src, dst) = (segments(from), segments(to));
            let fs = self.clone();
            blocking(move || fs.copy_tree(&src, &dst)).await?;
            self.noted();
            Ok(())
        }
        .boxed()
    }
}

/// In-memory file handle. Writers store the node on `flush`.
struct MemFile {
    fs: BranchFs,
    path: Option<VirtualPath>,
    data: Vec<u8>,
    pos: usize,
    meta: Meta,
    dirty: bool,
}

impl fmt::Debug for MemFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemFile")
            .field("path", &self.path)
            .field("len", &self.data.len())
            .finish()
    }
}

impl MemFile {
    fn writer(fs: BranchFs, path: VirtualPath) -> Self {
        let meta = Meta {
            len: 0,
            modified: SystemTime::now(),
            dir: false,
            etag: None,
        };
        // A PUT without body still creates an empty file, so start dirty.
        Self {
            fs,
            path: Some(path),
            data: Vec::new(),
            pos: 0,
            meta,
            dirty: true,
        }
    }

    fn reader(fs: BranchFs, data: Vec<u8>, meta: Meta) -> Self {
        Self {
            fs,
            path: None,
            data,
            pos: 0,
            meta,
            dirty: false,
        }
    }

    fn put(&mut self, chunk: &[u8]) -> FsResult<()> {
        if self.path.is_none() {
            return Err(FsError::Forbidden);
        }
        let end = self.pos + chunk.len();
        if end > MAX_FILE {
            return Err(FsError::TooLarge);
        }
        if end > self.data.len() {
            self.data.resize(end, 0);
        }
        self.data[self.pos..end].copy_from_slice(chunk);
        self.pos = end;
        self.dirty = true;
        Ok(())
    }
}

impl DavFile for MemFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let mut meta = self.meta.clone();
        meta.len = self.data.len() as u64;
        async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) }.boxed()
    }

    fn write_buf(&mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        let mut result = Ok(());
        while buf.has_remaining() && result.is_ok() {
            let n = buf.chunk().len();
            result = self.put(buf.chunk());
            buf.advance(n);
        }
        async move { result }.boxed()
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        let result = self.put(&buf);
        async move { result }.boxed()
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        let end = (self.pos + count).min(self.data.len());
        let out = Bytes::copy_from_slice(&self.data[self.pos..end]);
        self.pos = end;
        async move { Ok(out) }.boxed()
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        let len = self.data.len() as i64;
        let target = match pos {
            SeekFrom::Start(n) => i64::try_from(n).unwrap_or(i64::MAX),
            SeekFrom::End(d) => len + d,
            SeekFrom::Current(d) => self.pos as i64 + d,
        };
        let result = if target < 0 {
            Err(FsError::GeneralFailure)
        } else {
            self.pos = target as usize;
            Ok(target as u64)
        };
        async move { result }.boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move {
            let Some(vp) = self.path.clone().filter(|_| self.dirty) else {
                return Ok(());
            };
            let (fs, data) = (self.fs.clone(), self.data.clone());
            blocking(move || fs.write_file(&vp, data)).await?;
            self.dirty = false;
            self.fs.noted();
            Ok(())
        }
        .boxed()
    }
}
