//! Session-stage diagnostics. Never record the user's input text.

use std::time::Instant;

pub fn measure<T>(stage: &'static str, operation: impl FnOnce() -> T) -> T {
    tracing::info!(stage, "session stage started");
    let started = Instant::now();
    let result = operation();
    tracing::info!(
        stage,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "session stage finished"
    );
    result
}
