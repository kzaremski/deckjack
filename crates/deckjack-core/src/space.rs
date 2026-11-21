//! How much room the working directory has: `files.space`.
//!
//! One `statvfs` of the working directory's filesystem, so Files can say
//! "12 GB free" before a copy rather than after it fails. It reports the
//! filesystem the working directory itself is on; a folder inside it that is
//! another mount has its own numbers, which this does not try to chase.

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::command::{Command, CommandError, Context, RiskLevel};
use crate::files::Namespace;
/// No parameters, and saying so: a client that sends a path expecting the
/// space of that folder is told it is not supported rather than answered
/// about a different one.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpaceParams {}

/// The working directory's filesystem, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    /// The filesystem's size.
    pub total: u64,
    /// Free, counting blocks only root may use.
    pub free: u64,
    /// What the server's own user can still write: what a copy can use.
    pub available: u64,
}

/// `statvfs` of `dir`.
pub fn of(dir: &Path) -> std::io::Result<Space> {
    let stat = nix::sys::statvfs::statvfs(dir).map_err(std::io::Error::from)?;
    // The fragment size is the unit the block counts are in; `f_bsize` is
    // only the preferred I/O size and on some filesystems differs.
    let unit = stat.fragment_size() as u64;
    Ok(Space {
        total: (stat.blocks() as u64).saturating_mul(unit),
        free: (stat.blocks_free() as u64).saturating_mul(unit),
        available: (stat.blocks_available() as u64).saturating_mul(unit),
    })
}

pub struct FreeSpace {
    pub(crate) files: Arc<Namespace>,
}

impl Command for FreeSpace {
    const ID: &'static str = "files.space";
    const TITLE: &'static str = "Free space";
    const RISK: RiskLevel = RiskLevel::ReadOnly;

    type Params = SpaceParams;
    type Output = Space;

    async fn run(&self, _params: SpaceParams, cx: Context) -> Result<Space, CommandError> {
        let files = Arc::clone(&self.files);
        let command = cx.command.clone();
        match tokio::task::spawn_blocking(move || of(files.root().real_path())).await {
            Ok(Ok(space)) => Ok(space),
            Ok(Err(e)) => Err(CommandError::Failed(anyhow::anyhow!(
                "cannot read the free space of the working directory: {e}"
            ))),
            Err(joined) => Err(CommandError::Failed(anyhow::anyhow!(
                "{command} did not finish: {joined}"
            ))),
        }
    }
}
