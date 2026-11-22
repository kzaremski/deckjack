//! The Office app's engine, as a library so its tests can drive the same
//! code the binary runs. See `app.toml` and docs/lanes/6-office.md.

pub mod app;
pub mod lok;
pub mod ooxml;
pub mod pages;
pub mod paths;
pub mod worker;
