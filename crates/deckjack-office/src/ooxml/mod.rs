//! Office Open XML, read and patched by this app itself.
//!
//! [`package`] is the hardened ZIP layer, [`scan`] the one pass over the
//! document body that both the reading view and the patcher rely on,
//! [`parts`] the small parts around it, [`patch`] the surgical comment
//! writer and [`text`] the agent-facing rendering.

pub mod docx;
pub mod package;
pub mod parts;
pub mod patch;
pub mod scan;
pub mod text;

#[cfg(test)]
mod tests;
