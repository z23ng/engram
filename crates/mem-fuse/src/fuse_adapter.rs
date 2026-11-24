//! `fuser 0.19` FUSE kernel adapter delegating POSIX operations to [`MemoryPosixFs`].

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BackgroundSession, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, LockOwner, MountOption, OpenAccMode, OpenFlags, Owner,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};

use crate::vfs::{MemoryPosixFs, MountMode, PosixAttr, PosixError, PosixFileKind};

const ZERO_TTL: Duration = Duration::from_secs(0);

/// `fuser::Filesystem` wrapper around [`MemoryPosixFs`].
pub struct EngramFuseFs {
    vfs: MemoryPosixFs,
}

impl EngramFuseFs {
    /// Wraps `vfs` for mounting via `fuser`.
    pub fn new(vfs: MemoryPosixFs) -> Self {
        Self { vfs }
    }
}

impl Filesystem for EngramFuseFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.vfs.lookup(parent.0, name_str) {
            Ok(attr) => reply.entry(&ZERO_TTL, &to_fuse_attr(attr), Generation(0)),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.vfs.getattr(ino.0) {
            Ok(attr) => reply.attr(&ZERO_TTL, &to_fuse_attr(attr)),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        _kill_suid_gid: bool,
        reply: ReplyAttr,
    ) {
        if let Some(new_size) = size {
            match self.vfs.truncate(ino.0, new_size) {
                Ok(attr) => reply.attr(&ZERO_TTL, &to_fuse_attr(attr)),
                Err(err) => reply.error(to_errno(err)),
            }
        } else {
            match self.vfs.getattr(ino.0) {
                Ok(attr) => reply.attr(&ZERO_TTL, &to_fuse_attr(attr)),
                Err(err) => reply.error(to_errno(err)),
            }
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _owner: Owner,
        reply: ReplyEntry,
    ) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.vfs.mkdir(parent.0, name_str) {
            Ok(attr) => reply.entry(&ZERO_TTL, &to_fuse_attr(attr), Generation(0)),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.vfs.unlink(parent.0, name_str) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.vfs.rmdir(parent.0, name_str) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        _owner: Option<Owner>,
        reply: ReplyEmpty,
    ) {
        let (Some(old_name), Some(new_name)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.vfs.rename(parent.0, old_name, newparent.0, new_name) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn open(
        &self,
        _req: &Request,
        ino: INodeNo,
        flags: OpenFlags,
        _kill_suid_gid: bool,
        reply: ReplyOpen,
    ) {
        let write_access = matches!(
            flags.acc_mode(),
            OpenAccMode::O_WRONLY | OpenAccMode::O_RDWR
        );
        let truncate = (flags.0 & libc::O_TRUNC) != 0;
        match self.vfs.open(ino.0, write_access, truncate) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.vfs.read(ino.0, Some(fh.0), offset, size as usize) {
            Ok(bytes) => reply.data(&bytes),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.vfs.write(ino.0, fh.0, offset, data) {
            Ok(written) => reply.written(written as u32),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.vfs.flush(fh.0) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.vfs.release(fh.0) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        _kill_suid_gid: bool,
        _owner: Owner,
        reply: ReplyCreate,
    ) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.vfs.create(parent.0, name_str) {
            Ok((attr, fh)) => reply.created(
                &ZERO_TTL,
                &to_fuse_attr(attr),
                Generation(0),
                FileHandle(fh),
                FopenFlags::FOPEN_DIRECT_IO,
            ),
            Err(err) => reply.error(to_errno(err)),
        }
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let entries = match self.vfs.readdir(ino.0) {
            Ok(e) => e,
            Err(err) => {
                reply.error(to_errno(err));
                return;
            }
        };

        for (idx, entry) in entries.iter().enumerate().skip(offset as usize) {
            let kind = match entry.kind {
                PosixFileKind::Directory => FileType::Directory,
                PosixFileKind::RegularFile => FileType::RegularFile,
            };
            let full = reply.add(INodeNo(entry.ino), (idx + 1) as u64, kind, &entry.name);
            if full {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        reply.statfs(
            1_048_576, 524_288, 524_288, 1_000_000, 999_000, 4096, 255, 4096,
        );
    }
}

fn build_mount_config(mode: MountMode) -> Config {
    let mut cfg = Config::default();
    cfg.mount_options = vec![
        MountOption::FSName("engram".to_string()),
        MountOption::Subtype("engram".to_string()),
        MountOption::NoDev,
        MountOption::NoSuid,
        match mode {
            MountMode::ReadOnly => MountOption::RO,
            MountMode::ReadWrite => MountOption::RW,
        },
    ];
    cfg
}

/// Mounts `vfs` synchronously at `mountpoint`, blocking until unmounted.
pub fn mount_blocking_impl(vfs: MemoryPosixFs, mountpoint: &Path) -> io::Result<()> {
    let cfg = build_mount_config(vfs.config().effective_mode());
    fuser::mount(EngramFuseFs::new(vfs), mountpoint, &cfg)
}

/// Spawns `vfs` in a background FUSE session thread, unmounting when dropped.
pub fn spawn_mount_impl(vfs: MemoryPosixFs, mountpoint: &Path) -> io::Result<BackgroundSession> {
    let cfg = build_mount_config(vfs.config().effective_mode());
    fuser::spawn_mount(EngramFuseFs::new(vfs), mountpoint, &cfg)
}

fn to_fuse_attr(attr: PosixAttr) -> FileAttr {
    let mtime = ms_to_system_time(attr.mtime_ms);
    let ctime = ms_to_system_time(attr.ctime_ms);
    FileAttr {
        ino: INodeNo(attr.ino),
        size: attr.size,
        blocks: attr.blocks,
        atime: mtime,
        mtime,
        ctime,
        crtime: ctime,
        kind: match attr.kind {
            PosixFileKind::Directory => FileType::Directory,
            PosixFileKind::RegularFile => FileType::RegularFile,
        },
        perm: attr.perm,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn ms_to_system_time(ms: i64) -> SystemTime {
    if ms <= 0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    }
}

fn to_errno(err: PosixError) -> Errno {
    match err {
        PosixError::Enoent => Errno::ENOENT,
        PosixError::Eexist => Errno::EEXIST,
        PosixError::Enotdir => Errno::ENOTDIR,
        PosixError::Eisdir => Errno::EISDIR,
        PosixError::Enotempty => Errno::ENOTEMPTY,
        PosixError::Einval => Errno::EINVAL,
        PosixError::Erofs => Errno::EROFS,
        PosixError::Eacces => Errno::EACCES,
        PosixError::Ebadf => Errno::EBADF,
        PosixError::Eio => Errno::EIO,
    }
}
