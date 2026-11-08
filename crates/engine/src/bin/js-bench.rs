//! Small JavaScript benchmark set for the Mazda interpreter.
//!
//! Every sample builds a fresh `JsEngine`, so the number includes parse and
//! execution (what a page pays once per script). Warmup runs absorb thread and
//! allocator warm-up; the table reports min, median and mean of the timed runs.
//!
//! Usage: js-bench [--iterations N] [--warmup N] [--filter <substring>]

use std::process::ExitCode;
use std::time::Instant;

use mazda_engine::js::JsEngine;

struct Bench {
    name: &'static str,
    src: &'static str,
}

const BENCHES: &[Bench] = &[
    Bench {
        name: "loop x300k",
        src: include_str!("../../benches-js/loop.js"),
    },
    Bench {
        name: "fib(22)",
        src: include_str!("../../benches-js/fib.js"),
    },
    Bench {
        name: "property x300k",
        src: include_str!("../../benches-js/property.js"),
    },
    Bench {
        name: "array x100k",
        src: include_str!("../../benches-js/array.js"),
    },
    Bench {
        name: "string x2k",
        src: include_str!("../../benches-js/string.js"),
    },
    Bench {
        name: "closure x300k",
        src: include_str!("../../benches-js/closure.js"),
    },
];

fn run(src: &str) -> Result<(), String> {
    let mut engine = JsEngine::new();
    engine.exec(src).map(|_| ()).map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut iterations = 10usize;
    let mut warmup = 2usize;
    let mut filter: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--iterations" => {
                i += 1;
                iterations = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(10);
            }
            "--warmup" => {
                i += 1;
                warmup = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(2);
            }
            "--filter" => {
                i += 1;
                filter = args.get(i).cloned();
            }
            "-h" | "--help" => {
                println!("js-bench [--iterations N] [--warmup N] [--filter <substring>]");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    if iterations == 0 {
        eprintln!("--iterations must be at least 1");
        return ExitCode::from(2);
    }

    println!(
        "{:<18} {:>8} {:>8} {:>8}   sample ms",
        "benchmark", "min", "median", "mean"
    );
    for bench in BENCHES {
        if let Some(f) = &filter {
            if !bench.name.contains(f.as_str()) {
                continue;
            }
        }
        for _ in 0..warmup {
            if let Err(e) = run(bench.src) {
                eprintln!("{}: {e}", bench.name);
                return ExitCode::FAILURE;
            }
        }
        let mut times = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let t = Instant::now();
            if let Err(e) = run(bench.src) {
                eprintln!("{}: {e}", bench.name);
                return ExitCode::FAILURE;
            }
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let min = times[0];
        let median = times[times.len() / 2];
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        println!("{:<18} {min:>8.2} {median:>8.2} {mean:>8.2}", bench.name);
    }
    ExitCode::SUCCESS
}
