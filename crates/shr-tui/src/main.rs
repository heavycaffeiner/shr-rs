//! Thin `shr-tui` binary entry point -- the actual event loop lives in
//! `shr_tui::run` (`src/runtime.rs`) so `shr-bin`'s TUI dispatch branch
//! can call the exact same implementation this binary does.

fn main() -> anyhow::Result<()> {
    shr_tui::run()
}
