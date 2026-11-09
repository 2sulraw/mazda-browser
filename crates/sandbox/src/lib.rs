//! Renderer sandbox: OS-level containment for the untrusted process.
//!
//! Windows: a restricted token (the user and Administrators SIDs are
//! deny-only, so the renderer cannot open the user's own files), low
//! integrity, and a Job Object that kills the renderer with the shell and
//! allows one process.
//!
//! Linux: `no_new_privs`, `RLIMIT_AS`, and a seccomp filter that refuses
//! `execve`/`execveat` (threads still work; child processes do not).
//!
//! The shell points `TMP`, `TEMP` and `LOCALAPPDATA` at a directory it created
//! and granted to the renderer, so the engine's frame file and cache never
//! need the real user profile.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(unix)]
#[path = "unix.rs"]
mod platform;

#[cfg(not(any(windows, unix)))]
#[path = "other.rs"]
mod platform;

pub use platform::Process;

/// Resource limits for one renderer process.
///
/// There is deliberately no memory cap: the renderer is expected to stay lean
/// by what it allocates, not by being killed at a threshold. The job object
/// still enforces one process and kills it with the shell.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Keep network access (the renderer fetches pages).
    pub allow_network: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self { allow_network: true }
    }
}

/// A spawned, sandboxed renderer.
///
/// The stdio fields are plain files so the caller can hand stdout/stderr to
/// reader threads; the process handle lives in `into_parts`.
pub struct Child {
    pub stdin: File,
    pub stdout: File,
    pub stderr: File,
    pub pid: u32,
    inner: platform::Process,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Split into the three stdio files and the process handle.
    pub fn into_parts(self) -> (File, File, File, platform::Process) {
        (self.stdin, self.stdout, self.stderr, self.inner)
    }
}

/// Create the sandbox directory layout and grant the renderer access to it.
///
/// Layout: `dir/tmp` (frame file), `dir/localappdata` (cache). On Windows the
/// AppContainer redirects `%TEMP%` to
/// `localappdata/Packages/<package>/AC/Temp`, so that path is created too.
pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    platform::prepare_dir(dir)
}

/// Spawn `exe` inside the sandbox. `env` entries replace inherited variables.
pub fn spawn(
    exe: &Path,
    args: &[OsString],
    env: &[(&str, OsString)],
    limits: &Limits,
) -> io::Result<Child> {
    platform::spawn(exe, args, env, limits)
}

/// Whether this platform has a real sandbox (false on the fallback build).
pub fn available() -> bool {
    platform::AVAILABLE
}

/// Escape hatch for debugging: spawn without OS restrictions.
pub fn spawn_plain(
    exe: &Path,
    args: &[OsString],
    env: &[(&str, OsString)],
) -> io::Result<Child> {
    platform::spawn_plain(exe, args, env)
}
