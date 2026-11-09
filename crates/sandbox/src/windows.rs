//! Windows sandbox: AppContainer + Job Object.
//!
//! The renderer runs in an AppContainer with only the `internetClient`
//! capability. Its token therefore cannot open the user profile, cannot write
//! outside the paths granted to its package SID, and cannot touch the desktop;
//! the Job Object allows exactly one process and kills
//! the renderer when the shell exits.
//!
//! `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` is used
//! instead of `CreateProcessAsUser`: it needs no privileges (a standard user
//! can spawn an AppContainer process) and the token is built by the kernel.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, RawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, LocalFree, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    FreeSid, ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
    InitializeProcThreadAttributeList, ResumeThread, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    STARTUPINFOW,
};

use crate::Limits;

pub const AVAILABLE: bool = true;

/// AppContainer name: stable, so the package SID survives restarts.
const CONTAINER: &str = "MazdaBrowser.Renderer";
/// `S-1-15-3-1` is the well-known `internetClient` capability SID.
const INTERNET_CLIENT: &str = "S-1-15-3-1";

/// Owned process + job handles. Dropping it terminates the job, so the
/// renderer can never outlive the shell.
pub struct Process {
    job: HANDLE,
    process: HANDLE,
    pid: u32,
}

// SAFETY: the handles are owned by this struct and only used through &mut self.
unsafe impl Send for Process {}

impl Process {
    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn kill(&mut self) {
        if !self.job.is_null() {
            unsafe {
                TerminateJobObject(self.job, 1);
            }
        } else {
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(self.process, 1);
            }
        }
    }

    pub fn wait(&mut self) {
        unsafe {
            WaitForSingleObject(self.process, INFINITE);
        }
    }

    /// Wrap an already-running child (the unsandboxed escape hatch).
    fn from_child(child: &std::process::Child) -> io::Result<Self> {
        unsafe {
            let mut dup: HANDLE = null_mut();
            let ok = DuplicateHandle(
                GetCurrentProcess(),
                child.as_raw_handle() as HANDLE,
                GetCurrentProcess(),
                &mut dup,
                PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | 0x0010_0000, // SYNCHRONIZE
                0,
                0,
            );
            if ok == 0 {
                return Err(ctx("duplicate child handle"));
            }
            Ok(Self {
                job: null_mut(),
                process: dup,
                pid: child.id(),
            })
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            if !self.job.is_null() {
                TerminateJobObject(self.job, 1);
                WaitForSingleObject(self.process, INFINITE);
                CloseHandle(self.job);
            }
            CloseHandle(self.process);
        }
    }
}

/// Create the sandbox directory and grant the renderer access to it.
pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let sid = container_sid()?;
    let r = grant(dir, sid, true);
    if r.is_ok() {
        // the AppContainer's %TEMP% is redirected into its package profile;
        // create that path up front or the first frame write fails
        let local = dir.join("localappdata");
        let ac_temp = local
            .join("Packages")
            .join(CONTAINER.to_lowercase())
            .join("AC")
            .join("Temp");
        for sub in [dir.join("tmp"), local, ac_temp] {
            std::fs::create_dir_all(sub)?;
        }
    }
    unsafe { FreeSid(sid) };
    r
}

pub fn spawn(
    exe: &Path,
    args: &[OsString],
    env: &[(&str, OsString)],
    limits: &Limits,
) -> io::Result<crate::Child> {
    unsafe {
        let sid = container_sid()?;
        // the renderer may execute its own binary
        if let Err(e) = grant(exe, sid, false) {
            FreeSid(sid);
            return Err(e);
        }

        let mut capability: PSID = null_mut();
        let mut cap_name: Vec<u16> = INTERNET_CLIENT
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let cap_ok = if limits.allow_network {
            ConvertStringSidToSidW(cap_name.as_mut_ptr(), &mut capability) != 0
        } else {
            false
        };
        let mut cap_entry = SID_AND_ATTRIBUTES {
            Sid: capability,
            Attributes: SE_GROUP_ENABLED as u32,
        };
        let mut caps = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: if cap_ok { &mut cap_entry } else { null_mut() },
            CapabilityCount: if cap_ok { 1 } else { 0 },
            Reserved: 0,
        };

        let job = create_job()?;

        // child reads stdin_r / writes stdout_w + stderr_w; the parent keeps
        // the other ends and clears their inherit flag before the spawn
        let (stdin_r, stdin_w) = pipe()?;
        let (stdout_r, stdout_w) = pipe()?;
        let (stderr_r, stderr_w) = pipe()?;
        SetHandleInformation(stdin_w, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(stdout_r, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(stderr_r, HANDLE_FLAG_INHERIT, 0);

        let mut cmd = String::new();
        append_quoted(&mut cmd, exe.as_os_str());
        for a in args {
            cmd.push(' ');
            append_quoted(&mut cmd, a);
        }
        let mut cmd_w: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
        let exe_w = wide_path(exe);
        let env_w = env_block(env);

        let mut si: STARTUPINFOEXW = zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = stdin_r;
        si.StartupInfo.hStdOutput = stdout_w;
        si.StartupInfo.hStdError = stderr_w;

        let mut attr_size = 0usize;
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
        let mut attr_buf = vec![0u8; attr_size];
        let attrs = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        let mut fail = InitializeProcThreadAttributeList(attrs, 1, 0, &mut attr_size) == 0
            || UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                &caps as *const _ as _,
                size_of::<SECURITY_CAPABILITIES>(),
                null_mut(),
                null(),
            ) == 0;
        si.lpAttributeList = attrs;

        let mut pi: PROCESS_INFORMATION = zeroed();
        let ok = !fail
            && CreateProcessW(
                exe_w.as_ptr(),
                cmd_w.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_SUSPENDED
                    | CREATE_NO_WINDOW
                    | CREATE_UNICODE_ENVIRONMENT
                    | EXTENDED_STARTUPINFO_PRESENT,
                env_w.as_ptr() as _,
                null(),
                &si as *const _ as *const STARTUPINFOW,
                &mut pi,
            ) != 0;
        if !fail {
            DeleteProcThreadAttributeList(attrs);
        }
        CloseHandle(stdin_r);
        CloseHandle(stdout_w);
        CloseHandle(stderr_w);
        if cap_ok {
            LocalFree(capability as _);
        }
        FreeSid(sid);
        if !ok {
            let err = ctx("create process");
            CloseHandle(stdin_w);
            CloseHandle(stdout_r);
            CloseHandle(stderr_r);
            CloseHandle(job);
            return Err(err);
        }

        // assign while suspended so the renderer cannot allocate or spawn
        // before the limits are in force
        if AssignProcessToJobObject(job, pi.hProcess) == 0 {
            let err = ctx("assign to job object");
            TerminateJobObject(job, 1);
            WaitForSingleObject(pi.hProcess, INFINITE);
            CloseHandle(pi.hProcess);
            CloseHandle(pi.hThread);
            CloseHandle(stdin_w);
            CloseHandle(stdout_r);
            CloseHandle(stderr_r);
            CloseHandle(job);
            return Err(err);
        }
        ResumeThread(pi.hThread);
        CloseHandle(pi.hThread);

        let pid = pi.dwProcessId;
        Ok(crate::Child {
            stdin: File::from_raw_handle(stdin_w as RawHandle),
            stdout: File::from_raw_handle(stdout_r as RawHandle),
            stderr: File::from_raw_handle(stderr_r as RawHandle),
            pid,
            inner: Process {
                job,
                process: pi.hProcess,
                pid,
            },
        })
    }
}

/// Escape hatch for debugging (`MAZDA_SANDBOX=off`): normal process, same
/// stdio shape. Never used by the default path.
pub fn spawn_plain(
    exe: &Path,
    args: &[OsString],
    env: &[(&str, OsString)],
) -> io::Result<crate::Child> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    let inner = Process::from_child(&child)?;
    let stdin = unsafe { File::from_raw_handle(child.stdin.take().unwrap().into_raw_handle()) };
    let stdout = unsafe { File::from_raw_handle(child.stdout.take().unwrap().into_raw_handle()) };
    let stderr = unsafe { File::from_raw_handle(child.stderr.take().unwrap().into_raw_handle()) };
    let pid = child.id();
    Ok(crate::Child {
        stdin,
        stdout,
        stderr,
        pid,
        inner,
    })
}

fn container_sid() -> io::Result<PSID> {
    let name = wide_str(CONTAINER);
    let mut sid: PSID = null_mut();
    let hr = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            name.as_ptr(),
            name.as_ptr(),
            null(),
            0,
            &mut sid,
        )
    };
    if hr >= 0 {
        return Ok(sid);
    }
    // HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)
    if hr as u32 != 0x8007_00B7 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("create app container: 0x{:08X}", hr as u32),
        ));
    }
    let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
    if hr < 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("derive app container sid: 0x{:08X}", hr as u32),
        ));
    }
    Ok(sid)
}

/// Add a package-SID ACE to `path`, keeping the existing DACL. `dir` also
/// grants inheritance so the renderer's frame file and cache inherit access.
fn grant(path: &Path, sid: PSID, dir: bool) -> io::Result<()> {
    unsafe {
        let wide = wide_path(path);
        let mut old_acl: *mut ACL = null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let rc = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old_acl,
            null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }

        let mut ea: EXPLICIT_ACCESS_W = zeroed();
        ea.grfAccessPermissions = if dir { 0x001F_01FF } else { 0x0012_00A9 }; // all / read+exec
        ea.grfAccessMode = GRANT_ACCESS;
        ea.grfInheritance = if dir {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            0
        };
        ea.Trustee = TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: 0, // TRUSTEE_IS_UNKNOWN: fine for a raw SID
            ptstrName: sid as _,
        };

        let mut new_acl: *mut ACL = null_mut();
        let rc = SetEntriesInAclW(1, &ea, old_acl, &mut new_acl);
        LocalFree(sd as _);
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let rc = SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_acl,
            null(),
        );
        LocalFree(new_acl as _);
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
    }
    Ok(())
}

unsafe fn create_job() -> io::Result<HANDLE> {
    let job = CreateJobObjectW(null(), null());
    if job.is_null() {
        return Err(ctx("create job"));
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    info.BasicLimitInformation.ActiveProcessLimit = 1;
    if SetInformationJobObject(
        job,
        JobObjectExtendedLimitInformation,
        &info as *const _ as _,
        size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    ) == 0
    {
        let err = ctx("job limits");
        CloseHandle(job);
        return Err(err);
    }

    let mut ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = zeroed();
    ui.UIRestrictionsClass = JOB_OBJECT_UILIMIT_HANDLES
        | JOB_OBJECT_UILIMIT_READCLIPBOARD
        | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
        | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
        | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
        | JOB_OBJECT_UILIMIT_GLOBALATOMS
        | JOB_OBJECT_UILIMIT_DESKTOP
        | JOB_OBJECT_UILIMIT_EXITWINDOWS;
    SetInformationJobObject(
        job,
        JobObjectBasicUIRestrictions,
        &ui as *const _ as _,
        size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
    );

    Ok(job)
}

unsafe fn pipe() -> io::Result<(HANDLE, HANDLE)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let mut read: HANDLE = null_mut();
    let mut write: HANDLE = null_mut();
    if CreatePipe(&mut read, &mut write, &sa, 0) == 0 {
        return Err(ctx("pipe"));
    }
    Ok((read, write))
}

/// Environment block: inherited variables plus the sandbox overrides, sorted
/// the way CreateProcess expects.
fn env_block(env: &[(&str, OsString)]) -> Vec<u16> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    vars.retain(|(k, _)| {
        let k = k.to_string_lossy();
        !env.iter().any(|(name, _)| name.eq_ignore_ascii_case(&k))
    });
    for (name, value) in env {
        vars.push((OsString::from(name), value.clone()));
    }
    vars.sort_by(|a, b| {
        a.0.to_string_lossy()
            .to_uppercase()
            .cmp(&b.0.to_string_lossy().to_uppercase())
    });

    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_wide());
        block.push(b'=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Quote one argument the way CommandLineToArgvW parses it back.
fn append_quoted(out: &mut String, arg: &OsStr) {
    let s = arg.to_string_lossy();
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        out.push_str(&s);
        return;
    }
    out.push('"');
    let mut slashes = 0;
    for c in s.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat('\\').take(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat('\\').take(slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat('\\').take(slashes * 2));
    out.push('"');
}

/// Last OS error with the failing call named, so a spawn failure is not just
/// "access denied" with no idea which step refused.
fn ctx(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}
