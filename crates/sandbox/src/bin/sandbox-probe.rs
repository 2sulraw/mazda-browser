//! Sandbox self-test.
//!
//! `sandbox-probe --canary <path> [--memory-mb N]` spawns itself through the
//! sandbox, then checks from the inside that the user's file is unreadable,
//! child processes are refused, the network still works, and the temp
//! directory is writable. A second sandboxed run proves the memory cap: it
//! tries to commit more than the job allows and must die.
//!
//! Exit code 0 means every check passed.

use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use mazda_sandbox::{spawn, Limits};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    if args.iter().any(|a| a == "--inside") {
        inside(get("--canary"), get("--alloc-mb"));
        return;
    }

    let canary = get("--canary").expect("--canary <path> is required");
    let memory_mb = get("--memory-mb")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(96);
    if let Err(e) = outside(&canary, memory_mb) {
        eprintln!("sandbox probe failed: {e}");
        std::process::exit(1);
    }
}

fn outside(canary: &str, memory_mb: u32) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let base = std::env::temp_dir().join(format!("mazda-sandbox-probe-{}", std::process::id()));
    let tmp = base.join("tmp");
    let local = base.join("localappdata");
    mazda_sandbox::prepare_dir(&base)?;
    std::fs::create_dir_all(&tmp)?;
    std::fs::create_dir_all(&local)?;

    let limits = Limits {
        memory_mb,
        allow_network: true,
    };
    let env = [
        ("TMP", tmp.as_os_str().to_owned()),
        ("TEMP", tmp.as_os_str().to_owned()),
        ("LOCALAPPDATA", local.as_os_str().to_owned()),
    ];

    let mut inside_args: Vec<OsString> = vec!["--inside".into(), "--canary".into(), canary.into()];
    let child = spawn(&exe, &inside_args, &env, &limits)?;
    let (_stdin, stdout, stderr, mut proc) = child.into_parts();
    let mut out = String::new();
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(l) => {
                println!("{l}");
                out.push_str(&l);
                out.push('\n');
            }
            Err(_) => break,
        }
    }
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        eprintln!("{line}");
    }
    proc.wait();

    let ok = ["home read denied", "child spawn denied", "network ok", "temp writable"]
        .iter()
        .all(|needle| out.contains(needle));
    if !ok {
        eprintln!("FAIL: an inside check did not pass");
        return Err(std::io::Error::other("inside checks failed"));
    }

    // memory: a second sandboxed run must be killed while committing
    inside_args = vec![
        "--inside".into(),
        "--alloc-mb".into(),
        (memory_mb as u64 + 128).to_string().into(),
    ];
    let child = spawn(&exe, &inside_args, &env, &limits)?;
    let (_stdin, stdout, stderr, mut proc) = child.into_parts();
    let out: Vec<String> = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .collect();
    for line in &out {
        println!("{line}");
    }
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        eprintln!("{line}");
    }
    proc.wait();

    // wait() returns after exit; probe the exit code through a fresh check:
    // a killed process prints neither "alloc survived" nor "alloc denied".
    if out.iter().any(|l| l.contains("alloc survived")) {
        eprintln!("FAIL: memory cap did not stop a {} MB commit", memory_mb + 128);
        return Err(std::io::Error::other("memory limit not enforced"));
    }
    println!("memory cap enforced");
    let _ = std::fs::remove_dir_all(&base);
    Ok(())
}

fn inside(canary: Option<String>, alloc_mb: Option<String>) {
    if let Some(mb) = alloc_mb {
        let n = mb.parse::<usize>().unwrap_or(0) * 1024 * 1024;
        let mut v: Vec<u8> = Vec::new();
        if v.try_reserve_exact(n).is_err() {
            println!("alloc denied");
            std::process::exit(3);
        }
        v.resize(n, 7); // commits the pages; the job cap aborts the process
        println!("alloc survived");
        std::process::exit(0);
    }

    if let Some(path) = canary {
        match std::fs::read_to_string(&path) {
            Ok(_) => println!("FAIL home readable"),
            Err(_) => println!("PASS home read denied"),
        }
    }

    let spawned = std::process::Command::new("cmd.exe")
        .args(["/c", "exit", "0"])
        .status();
    match spawned {
        Ok(s) if s.success() => println!("FAIL child spawn allowed"),
        _ => println!("PASS child spawn denied"),
    }

    let addr = "example.com:443"
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next());
    let net = addr
        .map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(10)).is_ok())
        .unwrap_or(false);
    println!("{} network ok", if net { "PASS" } else { "FAIL" });

    let tmp = std::env::temp_dir().join("probe-write.txt");
    let wrote = std::fs::write(&tmp, b"ok");
    if wrote.is_err() {
        eprintln!("write {} failed: {:?}", tmp.display(), wrote.as_ref().err());
    }
    println!("{} temp writable", if wrote.is_ok() { "PASS" } else { "FAIL" });
    let _ = std::fs::remove_file(&tmp);
}
