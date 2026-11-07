//! Fallback for platforms without a sandbox implementation: the process is
//! spawned normally. `available()` reports false so the shell can say so.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::Limits;

pub const AVAILABLE: bool = false;

pub struct Process(std::process::Child);

impl Process {
    pub fn id(&self) -> u32 {
        self.0.id()
    }

    pub fn kill(&mut self) {
        let _ = self.0.kill();
    }

    pub fn wait(&mut self) {
        let _ = self.0.wait();
    }
}

pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir.join("tmp"))?;
    std::fs::create_dir_all(dir.join("localappdata"))?;
    Ok(())
}

pub fn spawn(
    exe: &Path,
    args: &[OsString],
    env: &[(&str, OsString)],
    _limits: &Limits,
) -> io::Result<crate::Child> {
    let mut cmd = Command::new(exe);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdin = std::fs::File::from(child.stdin.take().unwrap());
    let stdout = std::fs::File::from(child.stdout.take().unwrap());
    let stderr = std::fs::File::from(child.stderr.take().unwrap());
    let pid = child.id();
    Ok(crate::Child {
        stdin,
        stdout,
        stderr,
        pid,
        inner: Process(child),
    })
}

/// Escape hatch for debugging: the fallback platform has no restrictions.
pub fn spawn_plain(
    exe: &Path,
    args: &[OsString],
    env: &[(&str, OsString)],
) -> io::Result<crate::Child> {
    spawn(exe, args, env, &Limits::default())
}
