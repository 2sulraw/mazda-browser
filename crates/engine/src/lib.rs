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
pub(crate) use app::{build_submission, cache_dir, fetch, resolve, SubmitMethod};

pub use app::{run_headless, run_shell};
