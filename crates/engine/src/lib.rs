//! Mazda engine — HTML/CSS/JS/DOM/layout/paint, all in Rust.
//!
//! Phase S keeps the engine in the shape it had in `mazda-browser-rust`: one
//! crate, modules at the root. `app` still contains the shell + the paint
//! pipeline; the headless renderer reuses it through `run_headless`. The
//! paint/shell split is deliberately deferred until the spike numbers exist.

pub mod cookie;
pub mod css;
pub mod html;
pub mod image;
pub mod js;
pub mod layout;
pub mod net;
pub mod runtime;
pub mod script;
pub mod srcset;
pub mod svg;
pub mod ui_chrome;
pub mod woff;

pub mod app;

// Modules (cookie, runtime, script, js::module) call these through `crate::`
// because they lived next to them in the single-file shell. Keep the paths
// working until the engine split moves them to their real home.
pub(crate) use app::{build_submission, cache_dir, fetch, fetch_nanos_take, resolve, SubmitMethod};

pub use app::{run_headless, run_shell};

// ---------------------------------------------------------------------------
// Diagnostic heap accounting (`--features count-alloc`)
// ---------------------------------------------------------------------------
// The leak gate compares process private bytes, which cannot tell "Rust heap
// retained" from "allocator reserved, contents freed". This wrapper counts
// live allocated bytes at the Rust-allocator boundary, so a reload slope can
// be attributed to code that actually holds memory. Off in normal builds.

#[cfg(feature = "count-alloc")]
mod live_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub static LIVE: AtomicUsize = AtomicUsize::new(0);
    pub static PEAK: AtomicUsize = AtomicUsize::new(0);
    /// live bytes per size class: <=64B, <=256, <=1K, <=4K, <=16K, <=64K,
    /// <=256K, <=1M, larger
    pub static BUCKETS: [AtomicUsize; 9] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    pub const BUCKET_NAMES: [&str; 9] = [
        "64B", "256B", "1K", "4K", "16K", "64K", "256K", "1M", ">1M",
    ];

    pub fn bucket_of(n: usize) -> usize {
        match n {
            0..=64 => 0,
            65..=256 => 1,
            257..=1024 => 2,
            1025..=4096 => 3,
            4097..=16384 => 4,
            16385..=65536 => 5,
            65537..=262144 => 6,
            262145..=1048576 => 7,
            _ => 8,
        }
    }

    pub struct Counting;

    impl Counting {
        fn add(n: usize) {
            let i = bucket_of(n);
            // saturating: the bucket is only diagnostics, never a decision
            let _ = BUCKETS[i].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(n))
            });
            let v = LIVE.fetch_add(n, Ordering::Relaxed) + n;
            PEAK.fetch_max(v, Ordering::Relaxed);
        }

        fn sub(n: usize) {
            let i = bucket_of(n);
            let _ = BUCKETS[i].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(n))
            });
            let _ = LIVE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(n))
            });
        }
    }

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let p = System.alloc(l);
            if !p.is_null() {
                Self::add(l.size());
            }
            p
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            let p = System.alloc_zeroed(l);
            if !p.is_null() {
                Self::add(l.size());
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            Self::sub(l.size());
            System.dealloc(p, l)
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            let q = System.realloc(p, l, new);
            if !q.is_null() {
                Self::sub(l.size());
                Self::add(new);
            }
            q
        }
    }
}

#[cfg(feature = "count-alloc")]
#[global_allocator]
static ALLOC: live_alloc::Counting = live_alloc::Counting;

/// Bytes currently allocated and not freed (0 without `count-alloc`).
pub fn live_alloc_bytes() -> usize {
    #[cfg(feature = "count-alloc")]
    {
        live_alloc::LIVE.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(feature = "count-alloc"))]
    {
        0
    }
}

/// High-water mark of [`live_alloc_bytes`] (0 without `count-alloc`).
pub fn peak_alloc_bytes() -> usize {
    #[cfg(feature = "count-alloc")]
    {
        live_alloc::PEAK.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(feature = "count-alloc"))]
    {
        0
    }
}

/// Live bytes per size class, largest classes listed once (diagnostics).
pub fn alloc_buckets() -> Vec<(&'static str, usize)> {
    #[cfg(feature = "count-alloc")]
    {
        use std::sync::atomic::Ordering::Relaxed;
        live_alloc::BUCKET_NAMES
            .iter()
            .zip(live_alloc::BUCKETS.iter())
            .map(|(n, b)| (*n, b.load(Relaxed)))
            .collect()
    }
    #[cfg(not(feature = "count-alloc"))]
    {
        Vec::new()
    }
}
