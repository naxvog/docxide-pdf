//! `Instant` for timing logs. wasm32-unknown-unknown has no clock (std's
//! `Instant::now` panics there), so the timings read zero on that target.
#[cfg(not(target_family = "wasm"))]
pub use std::time::Instant;

#[cfg(target_family = "wasm")]
#[derive(Clone, Copy)]
pub struct Instant;

#[cfg(target_family = "wasm")]
impl Instant {
    pub fn now() -> Self {
        Instant
    }
    pub fn elapsed(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}
