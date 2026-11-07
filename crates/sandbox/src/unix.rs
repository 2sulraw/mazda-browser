//! Linux sandbox: `no_new_privs`, `RLIMIT_AS`, and a seccomp filter that
//! refuses `execve`/`execveat`.
//!
//! Filesystem isolation (Landlock) and network namespaces are the next
//! increment; this stage already gives the acceptance shape: no child
//! processes, a hard memory cap, and no privilege gain.

use std::ffi::OsString;
use std::io;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::Limits;

pub const AVAILABLE: bool = true;

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
    limits: &Limits,
) -> io::Result<crate::Child> {
    let mut cmd = Command::new(exe);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let memory_mb = limits.memory_mb as u64;
    unsafe {
        cmd.pre_exec(move || harden(memory_mb));
    }

    let mut child = cmd.spawn()?;
    let stdin = unsafe { std::fs::File::from_raw_fd(child.stdin.take().unwrap().into_raw_fd()) };
    let stdout = unsafe { std::fs::File::from_raw_fd(child.stdout.take().unwrap().into_raw_fd()) };
    let stderr = unsafe { std::fs::File::from_raw_fd(child.stderr.take().unwrap().into_raw_fd()) };
    let pid = child.id();
    Ok(crate::Child {
        stdin,
        stdout,
        stderr,
        pid,
        inner: Process(child),
    })
}

#[cfg(target_os = "linux")]
unsafe fn harden(memory_mb: u64) -> io::Result<()> {
    const PR_SET_NO_NEW_PRIVS: i32 = 38;
    const PR_SET_SECCOMP: i32 = 22;
    const SECCOMP_MODE_FILTER: u64 = 2;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;

    #[repr(C)]
    struct SockFilter {
        code: u16,
        jt: u8,
        jf: u8,
        k: u32,
    }
    #[repr(C)]
    struct SockFprog {
        len: u16,
        filter: *const SockFilter,
    }

    if libc::prctl(PR_SET_NO_NEW_PRIVS, 1u64, 0u64, 0u64, 0u64) != 0 {
        return Err(io::Error::last_os_error());
    }

    let bytes = memory_mb * 1024 * 1024;
    let lim = libc::rlimit {
        rlim_cur: bytes as libc::rlim_t,
        rlim_max: bytes as libc::rlim_t,
    };
    if libc::setrlimit(libc::RLIMIT_AS, &lim) != 0 {
        return Err(io::Error::last_os_error());
    }

    // BPF: load seccomp_data.nr (offset 0), refuse execve/execveat, allow the
    // rest. Threads use clone, so they keep working.
    let filter = [
        SockFilter { code: 0x20, jt: 0, jf: 0, k: 0 },
        SockFilter {
            code: 0x15,
            jt: 0,
            jf: 1,
            k: libc::SYS_execve as u32,
        },
        SockFilter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | libc::EPERM as u32,
        },
        SockFilter {
            code: 0x15,
            jt: 0,
            jf: 1,
            k: libc::SYS_execveat as u32,
        },
        SockFilter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | libc::EPERM as u32,
        },
        SockFilter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ALLOW,
        },
    ];
    let prog = SockFprog {
        len: filter.len() as u16,
        filter: filter.as_ptr(),
    };
    if libc::prctl(
        PR_SET_SECCOMP,
        SECCOMP_MODE_FILTER,
        &prog as *const SockFprog,
    ) != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
unsafe fn harden(_memory_mb: u64) -> io::Result<()> {
    Ok(())
}

/// Escape hatch for debugging: spawn without OS restrictions.
pub fn spawn_plain(
    exe: &Path,
    args: &[OsString],
    env: &[(&str, OsString)],
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
    let stdin = unsafe { std::fs::File::from_raw_fd(child.stdin.take().unwrap().into_raw_fd()) };
    let stdout = unsafe { std::fs::File::from_raw_fd(child.stdout.take().unwrap().into_raw_fd()) };
    let stderr = unsafe { std::fs::File::from_raw_fd(child.stderr.take().unwrap().into_raw_fd()) };
    let pid = child.id();
    Ok(crate::Child {
        stdin,
        stdout,
        stderr,
        pid,
        inner: Process(child),
    })
}
