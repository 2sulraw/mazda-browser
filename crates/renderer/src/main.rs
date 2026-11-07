//! Phase S renderer process: headless, no window, no eframe init.
//!
//! The actual loop lives in `mazda_engine::run_headless` for now (it reuses
//! the shell's loader thread and paint pipeline unchanged). P1 moves it here
//! once paint is split out of `engine::app`.

fn main() {
    mazda_engine::run_headless();
}
