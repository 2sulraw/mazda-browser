//! test262 runner for the Mazda JS engine.
//!
//! This is a measurement tool, not a conformance product: it parses the
//! frontmatter the suite already carries, prepends the required harness files
//! and runs each test once in non-strict mode. Strict mode is not implemented
//! by the engine yet, so `onlyStrict` tests are reported as skipped rather than
//! silently run with different semantics.
//!
//! Usage:
//!   test262 --dir <test262/test/language> --harness <test262/harness>
//!           [--filter <substring>] [--limit <n>] [--failures <n>]
//!
//! Exit code is 1 when any executed test fails, 2 for bad arguments or I/O.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use mazda_engine::js::{JsEngine, JsError};

#[derive(Default, Debug)]
struct Meta {
    includes: Vec<String>,
    flags: Vec<String>,
    negative_phase: Option<String>,
    negative_type: Option<String>,
}

fn list_value(v: &str) -> Vec<String> {
    let v = v.trim();
    let inner = v
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(v);
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse the `/*--- ... ---*/` block: flat keys, inline or block lists, and the
/// two-level `negative` map. Anything else (description prose, info) is ignored.
fn metadata(src: &str) -> Meta {
    let mut m = Meta::default();
    let Some(start) = src.find("/*---") else {
        return m;
    };
    let rest = &src[start + 5..];
    let Some(end) = rest.find("---*/") else {
        return m;
    };
    #[derive(PartialEq)]
    enum Field {
        Other,
        Includes,
        Flags,
        Negative,
    }
    let mut field = Field::Other;
    for raw in rest[..end].lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let t = line.trim_start();
        if !indented {
            let Some((key, value)) = t.split_once(':') else {
                continue;
            };
            field = match key.trim() {
                "includes" => {
                    m.includes = list_value(value);
                    Field::Includes
                }
                "flags" => {
                    m.flags = list_value(value);
                    Field::Flags
                }
                "negative" => Field::Negative,
                _ => Field::Other,
            };
            continue;
        }
        match field {
            Field::Includes if t.starts_with('-') => {
                m.includes.extend(list_value(t.trim_start_matches('-').trim()));
            }
            Field::Flags if t.starts_with('-') => {
                m.flags.extend(list_value(t.trim_start_matches('-').trim()));
            }
            Field::Negative => {
                if let Some((key, value)) = t.split_once(':') {
                    match key.trim() {
                        "phase" => m.negative_phase = Some(value.trim().to_string()),
                        "type" => m.negative_type = Some(value.trim().to_string()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    m
}

fn error_name(e: &JsError) -> String {
    match e {
        JsError::Syntax { .. } => "SyntaxError".into(),
        JsError::Budget { .. } => "RangeError".into(),
        JsError::Throw(v) => {
            // Builtin failures are thrown as "TypeError: ..." strings; script
            // errors stringify through their Error prototype. Cover both
            // without a prototype walk.
            let text = v.to_string();
            for name in [
                "Test262Error",
                "TypeError",
                "RangeError",
                "SyntaxError",
                "ReferenceError",
                "EvalError",
                "URIError",
                "Error",
            ] {
                if text.starts_with(name) {
                    return name.into();
                }
            }
            v.type_of().into()
        }
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("js") {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !name.contains("_FIXTURE") {
                out.push(path);
            }
        }
    }
    Ok(())
}

struct Stats {
    pass: u32,
    fail: u32,
    skip: u32,
    by_dir: BTreeMap<String, (u32, u32, u32)>,
    failures: Vec<(PathBuf, String)>,
}

impl Stats {
    fn new() -> Self {
        Stats {
            pass: 0,
            fail: 0,
            skip: 0,
            by_dir: BTreeMap::new(),
            failures: Vec::new(),
        }
    }

    fn bump(&mut self, dir: &str, pass: bool) {
        let row = self.by_dir.entry(dir.to_string()).or_insert((0, 0, 0));
        if pass {
            self.pass += 1;
            row.0 += 1;
        } else {
            self.fail += 1;
            row.1 += 1;
        }
    }

    fn skip(&mut self, dir: &str) {
        self.skip += 1;
        self.by_dir.entry(dir.to_string()).or_insert((0, 0, 0)).2 += 1;
    }
}

/// `Ok(())` is a pass, `Err(reason)` a failure, `None` a skip.
fn run_one(path: &Path, harness: &Path) -> Result<Result<(), String>, String> {
    let src = fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    let src = src.strip_prefix('\u{feff}').unwrap_or(&src);
    let meta = metadata(src);
    let flag = |name: &str| meta.flags.iter().any(|f| f == name);
    if flag("module") {
        return Ok(Err("skip: module flag".into()));
    }
    if flag("async") {
        return Ok(Err("skip: async flag".into()));
    }
    if flag("onlyStrict") {
        return Ok(Err("skip: strict mode not implemented".into()));
    }
    if meta
        .negative_phase
        .as_deref()
        .is_some_and(|p| p == "resolution")
    {
        return Ok(Err("skip: module resolution phase".into()));
    }

    let mut code = String::new();
    if !flag("raw") {
        let mut seen = BTreeSet::new();
        for inc in ["sta.js", "assert.js"] {
            seen.insert(inc.to_string());
        }
        for inc in &meta.includes {
            seen.insert(inc.clone());
        }
        for inc in seen {
            let p = harness.join(&inc);
            match fs::read_to_string(&p) {
                Ok(text) => {
                    code.push_str(&text);
                    code.push_str("\n;\n");
                }
                Err(e) => return Err(format!("include {inc}: {e}")),
            }
        }
        // test262 supplies `print` from the host; the suite only needs it to
        // exist (async tests, which would call it, are skipped above).
        code.push_str("function print() {}\n;\n");
    }
    code.push_str(src);

    let mut engine = JsEngine::new();
    let result = engine.exec(&code);
    let expect = meta.negative_type.as_deref();
    match (result, expect) {
        (Err(e), Some(want)) => {
            let got = error_name(&e);
            if got == want {
                Ok(Ok(()))
            } else {
                Ok(Err(format!("expected {want}, got {got}")))
            }
        }
        (Err(e), None) => Ok(Err(match e {
            JsError::Throw(v) => format!("Uncaught {}", engine.describe_thrown(&v)),
            other => format!("{other}"),
        })),
        (Ok(_), Some(want)) => Ok(Err(format!("expected {want}, completed normally"))),
        (Ok(_), None) => Ok(Ok(())),
    }
}

fn main() -> ExitCode {
    // Some test262 cases nest expressions deeper than the default Windows
    // stack. The engine turns reasonable depth into a syntax error, but a
    // parser path can still recurse far; give the runner a large reserved
    // stack so one pathological file cannot kill the whole measurement.
    let handle = std::thread::Builder::new()
        .stack_size(128 * 1024 * 1024)
        .spawn(run)
        .expect("spawn test262 thread");
    handle.join().unwrap_or(ExitCode::FAILURE)
}

fn run() -> ExitCode {
    let mut dir = PathBuf::from("test262/test/language");
    let mut harness = PathBuf::from("test262/harness");
    let mut filter: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut max_failures = 40usize;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let take = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        match args[i].as_str() {
            "--dir" => dir = PathBuf::from(take(&mut i).unwrap_or_default()),
            "--harness" => harness = PathBuf::from(take(&mut i).unwrap_or_default()),
            "--filter" => filter = take(&mut i),
            "--limit" => limit = take(&mut i).and_then(|s| s.parse().ok()),
            "--failures" => max_failures = take(&mut i).and_then(|s| s.parse().ok()).unwrap_or(40),
            "-h" | "--help" => {
                println!("test262 --dir <language/> --harness <harness/> [--filter s] [--limit n] [--failures n]");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    if !dir.is_dir() || !harness.is_dir() {
        eprintln!("missing test262 checkout: {} or {}", dir.display(), harness.display());
        return ExitCode::from(2);
    }

    let mut files = Vec::new();
    if let Err(e) = collect(&dir, &mut files) {
        eprintln!("walk {}: {e}", dir.display());
        return ExitCode::from(2);
    }
    files.sort();
    if let Some(f) = &filter {
        files.retain(|p| p.to_string_lossy().contains(f.as_str()));
    }
    if let Some(n) = limit {
        files.truncate(n);
    }

    let started = Instant::now();
    let mut stats = Stats::new();
    let mut skipped_for_meta = 0u32;
    for path in &files {
        let rel = path.strip_prefix(&dir).unwrap_or(path);
        let top = rel
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .unwrap_or("?");
        if std::env::var_os("MAZDA_TEST262_TRACE").is_some() {
            eprintln!("run {}", rel.display());
        }
        // A panic is an engine bug, but it must not end the measurement: one
        // bad file is one failure, the rest still run.
        let t0 = Instant::now();
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_one(path, &harness)));
        if std::env::var_os("MAZDA_TEST262_SLOW").is_some() && t0.elapsed().as_millis() >= 50 {
            eprintln!("slow {:>6} ms {}", t0.elapsed().as_millis(), rel.display());
        }
        match outcome {
            Ok(Err(e)) => {
                stats.failures.push((rel.to_path_buf(), e));
                stats.bump(top, false);
            }
            Ok(Ok(Ok(()))) => stats.bump(top, true),
            Ok(Ok(Err(reason))) if reason.starts_with("skip: ") => {
                skipped_for_meta += 1;
                stats.skip(top);
            }
            Ok(Ok(Err(reason))) => {
                stats.failures.push((rel.to_path_buf(), reason));
                stats.bump(top, false);
            }
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".into());
                stats
                    .failures
                    .push((rel.to_path_buf(), format!("panic: {msg}")));
                stats.bump(top, false);
            }
        }
    }

    let run = stats.pass + stats.fail;
    println!("test262 language subset");
    println!(
        "files {} | run {} | pass {} | fail {} | skip {}",
        files.len(),
        run,
        stats.pass,
        stats.fail,
        stats.skip
    );
    if run > 0 {
        println!("pass rate {:.1}% of run", 100.0 * stats.pass as f64 / run as f64);
    }
    println!();
    println!("{:<24} {:>6} {:>6} {:>6}", "directory", "pass", "fail", "skip");
    for (name, (p, f, s)) in &stats.by_dir {
        println!("{name:<24} {p:>6} {f:>6} {s:>6}");
    }
    if !stats.failures.is_empty() {
        println!();
        println!("first {} failures:", stats.failures.len().min(max_failures));
        for (path, why) in stats.failures.iter().take(max_failures) {
            println!("  {}: {why}", path.display());
        }
    }
    println!();
    println!(
        "elapsed {:.1}s ({:.1} ms/test)",
        started.elapsed().as_secs_f64(),
        started.elapsed().as_secs_f64() * 1000.0 / files.len().max(1) as f64
    );
    if skipped_for_meta > 0 {
        println!("{skipped_for_meta} skipped by flags/phase");
    }
    if stats.fail > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
