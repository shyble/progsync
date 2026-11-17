//! Operating-system differences (Unix and Windows), in one place.
//!
//! - File metadata: modification time in nanoseconds; inode numbers (0 on
//!   Windows, where change detection relies on size and mtime).
//! - Executable bit: real on Unix; on Windows there is none, so files keep the
//!   kind recorded before (like git's `core.filemode = false`).
//! - Symlinks: real on Unix; on Windows created when permitted (developer
//!   mode or admin), otherwise written as a plain file holding the target
//!   (like git's `core.symlinks = false`).
//! - Daemon connection: a Unix socket (mode 0600) on Unix, a loopback TCP
//!   port on Windows; both also require the daemon's token.
//! - Owner-only files: mode 0600 on Unix; on Windows the user profile's ACLs.
//! - Shell for checks: `sh -c` / `cmd /C`. Detached processes: new process
//!   group / `DETACHED_PROCESS`.
//! - Random bytes: `getentropy` on Unix, `BCryptGenRandom` on Windows.
//! - Durability: `syncfs` on Linux; `fsync` + one `F_BARRIERFSYNC` on macOS;
//!   `FlushFileBuffers` on Windows (see [`barrier`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether the file system records an executable bit.
pub const HAS_EXEC_BIT: bool = cfg!(unix);

#[cfg(unix)]
pub fn mtime_ns(md: &fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    md.mtime() * 1_000_000_000 + md.mtime_nsec()
}

#[cfg(not(unix))]
pub fn mtime_ns(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}

#[cfg(unix)]
pub fn inode(md: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.ino()
}

#[cfg(not(unix))]
pub fn inode(_md: &fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
pub fn is_executable(md: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub fn is_executable(_md: &fs::Metadata) -> bool {
    false
}

/// Make a regular file executable (0755) or not (0644). No-op on Windows.
#[cfg(unix)]
pub fn set_executable(path: &Path, exec: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))
}

#[cfg(not(unix))]
pub fn set_executable(_path: &Path, _exec: bool) -> io::Result<()> {
    Ok(())
}

/// Readable and writable by the owner only (0600). No-op on Windows.
#[cfg(unix)]
pub fn restrict_to_owner(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
pub fn restrict_to_owner(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// A symlink's target as stored in a blob.
#[cfg(unix)]
pub fn link_target_bytes(path: &Path) -> io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(fs::read_link(path)?.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
pub fn link_target_bytes(path: &Path) -> io::Result<Vec<u8>> {
    Ok(fs::read_link(path)?.to_string_lossy().replace('\\', "/").into_bytes())
}

/// Create a symlink at `path` pointing to `target` (blob bytes).
#[cfg(unix)]
pub fn symlink(target: &[u8], path: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), path)
}

#[cfg(not(unix))]
pub fn symlink(target: &[u8], path: &Path) -> io::Result<()> {
    let t = String::from_utf8_lossy(target).replace('/', "\\");
    let is_dir = path.parent().is_some_and(|p| p.join(&t).is_dir());
    let made = if is_dir { std::os::windows::fs::symlink_dir(&t, path) } else { std::os::windows::fs::symlink_file(&t, path) };
    // Without the privilege to create symlinks, store the target as text.
    made.or_else(|_| fs::write(path, target))
}

/// `canonicalize`, without Windows' `\\?\` prefix (so paths compare equal to
/// the ones the OS reports elsewhere, e.g. in file-system events).
pub fn canonicalize(p: &Path) -> io::Result<PathBuf> {
    let c = p.canonicalize()?;
    #[cfg(windows)]
    {
        let s = c.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            if !rest.starts_with("UNC\\") {
                return Ok(PathBuf::from(rest));
            }
        }
    }
    Ok(c)
}

/// A relative path as reported by the OS, in psync's `/`-separated form.
pub fn slash_path(rel: &str) -> String {
    if cfg!(windows) {
        rel.replace('\\', "/")
    } else {
        rel.to_string()
    }
}

/// A command that runs `cmd` with the platform shell.
pub fn shell(cmd: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    }
}

/// Detach a background process from the terminal that started it.
#[cfg(unix)]
pub fn detach(c: &mut Command) {
    use std::os::unix::process::CommandExt;
    c.process_group(0);
}

#[cfg(windows)]
pub fn detach(c: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    c.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// This machine's name, without its domain (`laptop`, not `laptop.local`).
pub fn hostname() -> String {
    #[cfg(unix)]
    let name = {
        extern "C" {
            fn gethostname(name: *mut std::ffi::c_char, len: usize) -> std::ffi::c_int;
        }
        let mut buf = [0u8; 256];
        // SAFETY: the pointer and length describe a valid, writable buffer.
        let r = unsafe { gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) };
        let n = buf.iter().position(|&b| b == 0).unwrap_or(0);
        if r == 0 { String::from_utf8_lossy(&buf[..n]).into_owned() } else { String::new() }
    };
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").unwrap_or_default();
    match name.split('.').next() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => "localhost".into(),
    }
}

/// Fill `buf` with cryptographically secure random bytes from the OS.
#[cfg(unix)]
pub fn random_bytes(buf: &mut [u8]) {
    extern "C" {
        fn getentropy(buf: *mut std::ffi::c_void, len: usize) -> std::ffi::c_int;
    }
    // getentropy fills at most 256 bytes per call.
    for chunk in buf.chunks_mut(256) {
        // SAFETY: the pointer and length describe a valid, writable buffer.
        let r = unsafe { getentropy(chunk.as_mut_ptr().cast(), chunk.len()) };
        assert_eq!(r, 0, "getentropy failed: {}", io::Error::last_os_error());
    }
}

#[cfg(windows)]
pub fn random_bytes(buf: &mut [u8]) {
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(alg: *mut std::ffi::c_void, buf: *mut u8, len: u32, flags: u32) -> i32;
    }
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;
    for chunk in buf.chunks_mut(u32::MAX as usize) {
        // SAFETY: the pointer and length describe a valid, writable buffer;
        // no algorithm handle is needed with the system-preferred flag.
        let r = unsafe { BCryptGenRandom(std::ptr::null_mut(), chunk.as_mut_ptr(), chunk.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
        assert_eq!(r, 0, "BCryptGenRandom failed: status {r:#x}");
    }
}

// ---- durability ------------------------------------------------------
//
// psync writes many new object files, then moves `op_head` to an operation
// that refers to them. The objects must reach the disk first, or a power cut
// could leave `op_head` pointing at missing data. Per platform:
// - Linux: no per-file work; one `syncfs` before `op_head` moves flushes
//   everything written (file data and directory entries) at once.
// - macOS: `fsync` each new file and directory (to the drive), then one
//   `F_BARRIERFSYNC` before `op_head` moves: the drive persists everything
//   before the barrier before anything after it (the rename). The newest
//   operation may be lost in a power cut, but never half-written.
//   (`F_FULLFSYNC`, which empties the drive's cache, costs ~10 ms per
//   command; it is the fallback where barriers are unsupported.)
// - Windows: `FlushFileBuffers` on each new file; NTFS journals directory
//   entries itself.

/// Flush a newly written file of a batch (see [`barrier`]).
pub fn sync_new_file(f: &fs::File) -> io::Result<()> {
    if cfg!(target_os = "linux") {
        return Ok(());
    }
    fsync(f)
}

/// Flush a directory that got new entries in a batch (see [`barrier`]).
pub fn sync_new_dir(dir: &Path) -> io::Result<()> {
    if cfg!(any(target_os = "linux", windows)) {
        return Ok(());
    }
    sync_dir(dir)
}

/// Make `f` and everything written before it durable: call before the
/// rename that publishes a batch.
pub fn barrier(f: &fs::File) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn syncfs(fd: std::ffi::c_int) -> std::ffi::c_int;
        }
        // SAFETY: the descriptor is open for the duration of the call.
        if unsafe { syncfs(f.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn fcntl(fd: std::ffi::c_int, cmd: std::ffi::c_int, ...) -> std::ffi::c_int;
        }
        const F_BARRIERFSYNC: std::ffi::c_int = 85;
        // SAFETY: the descriptor is open for the duration of the call.
        if unsafe { fcntl(f.as_raw_fd(), F_BARRIERFSYNC) } == 0 {
            return Ok(());
        }
        f.sync_all() // F_FULLFSYNC
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // FlushFileBuffers on Windows, fsync on other Unix systems.
        f.sync_all()
    }
}

/// Plain `fsync` (on Apple systems std's `sync_all` would be the much
/// slower `F_FULLFSYNC`; [`barrier`] orders the batch instead).
fn fsync(f: &fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn fsync(fd: std::ffi::c_int) -> std::ffi::c_int;
        }
        // SAFETY: the descriptor is open for the duration of the call.
        if unsafe { fsync(f.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        f.sync_all()
    }
}

/// Flush a directory's entries (after a rename into it). No-op on Windows,
/// where directories cannot be opened as files and NTFS journals renames.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fsync(&fs::File::open(dir)?)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Read exactly `buf.len()` bytes at `offset` (without moving a shared
/// file position, so threads can read one file concurrently).
pub fn read_at(f: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        f.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            match f.seek_read(&mut buf[done..], offset + done as u64)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => done += n,
            }
        }
        Ok(())
    }
}

/// Set a file's modification time to now (marks an object as in use, so a
/// concurrent `gc` keeps it).
pub fn touch(path: &Path) -> io::Result<()> {
    fs::OpenOptions::new().write(true).open(path)?.set_modified(std::time::SystemTime::now())
}

/// The current user's login name (default owner name).
pub fn user_name() -> String {
    std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default()
}

/// Base directory for per-user configuration.
pub fn config_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => return Some(PathBuf::from(x)),
        _ => {}
    }
    if let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h).join(".config"));
    }
    std::env::var_os("APPDATA").filter(|a| !a.is_empty()).map(PathBuf::from)
}

// ---- daemon connection ------------------------------------------------------

#[cfg(unix)]
pub type IpcStream = std::os::unix::net::UnixStream;
#[cfg(not(unix))]
pub type IpcStream = std::net::TcpStream;

#[cfg(unix)]
pub struct IpcListener(std::os::unix::net::UnixListener, PathBuf);
#[cfg(not(unix))]
pub struct IpcListener(std::net::TcpListener);

impl IpcListener {
    /// Listen for this workspace's clients; returns the listener and the
    /// address clients connect to. `socket_hint` is the Unix socket path.
    #[cfg(unix)]
    pub fn bind(socket_hint: &Path) -> io::Result<(IpcListener, String)> {
        let _ = fs::remove_file(socket_hint);
        let l = std::os::unix::net::UnixListener::bind(socket_hint)?;
        restrict_to_owner(socket_hint)?;
        Ok((IpcListener(l, socket_hint.to_path_buf()), socket_hint.display().to_string()))
    }

    #[cfg(not(unix))]
    pub fn bind(_socket_hint: &Path) -> io::Result<(IpcListener, String)> {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = l.local_addr()?.to_string();
        Ok((IpcListener(l), addr))
    }

    pub fn accept(&self) -> io::Result<IpcStream> {
        self.0.accept().map(|(s, _)| s)
    }

    /// Remove what `bind` created (the socket file on Unix).
    pub fn cleanup(&self) {
        #[cfg(unix)]
        let _ = fs::remove_file(&self.1);
    }
}

pub fn ipc_connect(address: &str) -> io::Result<IpcStream> {
    IpcStream::connect(address)
}
