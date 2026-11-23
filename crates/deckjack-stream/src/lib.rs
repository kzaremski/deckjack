//! `deckjack-stream`: Deckjack's own headless Wayland compositor, which
//! runs one native Linux app (with XWayland for X11 apps) and streams each
//! of its windows to the browser as H.264 (docs/lanes/8-stream.md).
//!
//! Linux only. On any other OS the binary exits with an error and the
//! library holds only the pure input rules.

#[cfg(target_os = "linux")]
pub mod compositor;
#[cfg(target_os = "linux")]
pub mod encode;
pub mod input;
#[cfg(target_os = "linux")]
pub mod link;
#[cfg(target_os = "linux")]
pub mod xauth;

/// Whether to log every encoded picture's size and encode time
/// (`DECKJACK_STREAM_TIMING=1`), which is how the brief's numbers were
/// taken.
pub fn trace_timing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("DECKJACK_STREAM_TIMING").is_some())
}
