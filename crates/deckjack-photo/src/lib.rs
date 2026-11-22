//! The photo app's engine, apart from the bus seam in `main.rs`.
//!
//! Kept as a library so the spikes that measured the preview
//! (`examples/parity.rs`, `examples/banding.rs`) and the integration tests
//! drive the same code the app runs.

pub mod color;
pub mod decode;
pub mod engine;
pub mod export;
pub mod geometry;
pub mod lut;
pub mod paths;
pub mod recipe;
pub mod render;
pub mod sidecar;
