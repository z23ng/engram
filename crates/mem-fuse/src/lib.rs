//! Optional LayerFS-style POSIX FUSE mount adapter exposing an Engram branch or commit.

#[cfg(any(target_os = "linux", all(target_os = "macos", feature = "macos-fuse")))]
pub mod fuse_adapter;
pub mod vfs;

use std::io;
use std::path::Path;

pub use vfs::{
    MemoryPosixFs, MountConfig, MountMode, PosixAttr, PosixDirEntry, PosixError, PosixFileKind,
    ROOT_INO,
};

/// Mounts `vfs` at `mountpoint` and blocks until unmounted (Linux pure-Rust FUSE, or macOS with `macos-fuse` feature).
pub fn mount_blocking(vfs: MemoryPosixFs, mountpoint: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "linux", all(target_os = "macos", feature = "macos-fuse")))]
    {
        fuse_adapter::mount_blocking_impl(vfs, mountpoint)
    }
    #[cfg(not(any(target_os = "linux", all(target_os = "macos", feature = "macos-fuse"))))]
    {
        let _ = (vfs, mountpoint);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "FUSE mounting is supported on Linux (or macOS built with --features macos-fuse)",
        ))
    }
}

/// Spawns a background FUSE session mounted at `mountpoint` (unmounts on drop).
#[cfg(any(target_os = "linux", all(target_os = "macos", feature = "macos-fuse")))]
pub fn spawn_mount(vfs: MemoryPosixFs, mountpoint: &Path) -> io::Result<fuser::BackgroundSession> {
    fuse_adapter::spawn_mount_impl(vfs, mountpoint)
}
