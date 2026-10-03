//! Ratatui workspace. Compiled only with the `tui` feature; nothing outside
//! this module may import ratatui or crossterm.

use std::path::Path;

pub fn run(_db: &Path) -> Result<(), Box<dyn std::error::Error>> {
    Err("the TUI is not built yet".into())
}
