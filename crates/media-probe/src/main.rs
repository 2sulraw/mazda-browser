//! Media backend self-test.
//!
//! `media-probe --file <clip.mp4> [--secs N]` runs the whole thing twice:
//! once directly (proves decode + audio), then again through the sandbox
//! (proves Media Foundation and WASAPI work under the AppContainer, which is
//! how the renderer will actually run).
//!
//! Inside each run it opens the file, prints the container metadata,
//! decodes for a few seconds and reports the frame count. Exit code 0 means
//! every check passed.

use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

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
        let file = get("--file").expect("--file is required");
        let secs = get("--secs")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(2.0);
        if let Err(e) = inside(&file, secs) {
            eprintln!("media probe (inside) failed: {e}");
            std::process::exit(1);
        }
        return;
    }

    let file = get("--file").expect("--file <clip.mp4> is required");
    if let Err(e) = outside(&file) {
        eprintln!("media probe failed: {e}");
        std::process::exit(1);
    }
}

fn outside(file: &str) -> std::io::Result<()> {
    // 1. plain run: decode + audio on the open desktop
    println!("--- plain");
    let mut plain = std::process::Command::new(std::env::current_exe()?);
    plain.args(["--inside", "--file", file, "--secs", "2"]);
    let out = plain.output()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    print!("{text}");
    if !err.is_empty() {
        eprint!("{err}");
    }
    let ok_plain =
        out.status.success() && text.contains("PASS decoded") && text.contains("PASS audio");
    if !ok_plain {
        eprintln!("FAIL: plain decode/audio did not pass");
        return Err(std::io::Error::other("plain run failed"));
    }

    // 2. sandboxed run: the renderer's real environment
    println!("--- sandboxed");
    let exe = std::env::current_exe()?;
    let base = std::env::temp_dir().join(format!("mazda-media-probe-{}", std::process::id()));
    let tmp = base.join("tmp");
    let local = base.join("localappdata");
    mazda_sandbox::prepare_dir(&base)?;
    std::fs::create_dir_all(&tmp)?;
    std::fs::create_dir_all(&local)?;
    // the AppContainer can read the prepared base dir; %TEMP% is redirected,
    // so the clip moves inside with it
    let inside_file = base.join("clip.mp4");
    std::fs::copy(file, &inside_file)?;

    let limits = Limits {
        memory_mb: 512,
        allow_network: false,
    };
    let env = [
        ("TMP", tmp.as_os_str().to_owned()),
        ("TEMP", tmp.as_os_str().to_owned()),
        ("LOCALAPPDATA", local.as_os_str().to_owned()),
    ];
    let inside_args: Vec<OsString> = vec![
        "--inside".into(),
        "--file".into(),
        inside_file.as_os_str().to_owned(),
        "--secs".into(),
        "2".into(),
    ];
    let child = spawn(&exe, &inside_args, &env, &limits)?;
    let (_stdin, stdout, stderr, mut proc) = child.into_parts();
    let mut out = String::new();
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        println!("{line}");
        out.push_str(&line);
        out.push('\n');
    }
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        eprintln!("{line}");
    }
    proc.wait();
    let _ = std::fs::remove_dir_all(&base);

    if !(out.contains("PASS decoded") && out.contains("PASS audio")) {
        eprintln!("FAIL: sandboxed decode/audio did not pass");
        return Err(std::io::Error::other("sandboxed run failed"));
    }
    println!("media probe: all checks passed");
    Ok(())
}

fn inside(file: &str, secs: f64) -> Result<(), String> {
    let path = std::path::Path::new(file);
    let t0 = Instant::now();
    let mut player = mazda_media::Player::open(path).map_err(|e| e.to_string())?;
    let info = player.info();
    println!(
        "open: {}x{} {:.2}s video={} audio={} in {:.0} ms",
        info.width,
        info.height,
        info.duration,
        info.has_video,
        info.has_audio,
        t0.elapsed().as_secs_f64() * 1000.0
    );

    let first = player.next_frame();
    match &first {
        Some(f) => println!("first frame {}x{} ({} px)", f.w, f.h, f.px.len()),
        None => println!("first frame unavailable"),
    }

    let audio = player.has_audio();
    println!("{} audio endpoint", if audio { "PASS" } else { "SKIP" });

    player.set_looping(true);
    player.play();
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    let mut frames = 0usize;
    let mut bytes = 0usize;
    while Instant::now() < deadline {
        if let Some(f) = player.next_frame() {
            frames += 1;
            bytes += f.px.len() * 4;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    println!(
        "{} decoded {frames} frames in {:.1} s ({:.1} MB)",
        if frames > 1 { "PASS" } else { "FAIL" },
        secs,
        bytes as f64 / (1024.0 * 1024.0)
    );
    if frames <= 1 {
        return Err(format!("only {frames} frames decoded"));
    }

    // DRM: an encrypted container must be refused before a decoder exists
    let mut fake = Vec::new();
    fake.extend_from_slice(&8u32.to_be_bytes());
    fake.extend_from_slice(b"ftyp");
    fake.extend_from_slice(&24u32.to_be_bytes());
    fake.extend_from_slice(b"moov");
    fake.extend_from_slice(&16u32.to_be_bytes());
    fake.extend_from_slice(b"pssh");
    fake.extend_from_slice(&[0u8; 8]);
    let fake_path = std::env::temp_dir().join("mazda-media-drm.mp4");
    std::fs::write(&fake_path, &fake).map_err(|e| e.to_string())?;
    let drm = matches!(
        mazda_media::Player::open(&fake_path).err(),
        Some(mazda_media::MediaError::Drm)
    );
    let _ = std::fs::remove_file(&fake_path);
    println!("{} DRM rejected", if drm { "PASS" } else { "FAIL" });
    if !drm {
        return Err("encrypted file was not rejected".into());
    }
    Ok(())
}
