//! Presentation layer. Only `terminal` exists today; `--json` is served by
//! `serde` directly from the domain model.

pub mod terminal;

use std::io::Write;

/// `"s"` unless `n == 1`, for "3 files" / "1 file".
pub fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Write bytes to stdout. A closed pipe (`trail | head`) is not an error
/// worth reporting, every other failure is.
pub fn write_stdout(data: &[u8]) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match lock.write_all(data) {
        Err(err) if err.kind() != std::io::ErrorKind::BrokenPipe => Err(err),
        _ => Ok(()),
    }
}
