//! The sidecar: `<photo>.deckjack.json`, beside the photo, holding the
//! recipe and its history.
//!
//! The photo itself is never opened for writing by anything in this module:
//! the only files written are the sidecar's temporary name and the sidecar,
//! and the sidecar's name is the photo's name plus a suffix, so the two can
//! never be the same file. `tests/original.rs` checks the photo's SHA-256 is
//! unchanged across every write path.
//!
//! Every write is a revision. A save names the revision it was based on
//! (`base_rev`) and is refused if the sidecar has moved on since, which is
//! how two windows on one photo (phone and desktop) find out instead of
//! overwriting each other. The format is `docs/formats/photo-edit-v1.md`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::recipe::Recipe;

pub const SCHEMA: &str = "deckjack.photo-edit/1";
pub const SUFFIX: &str = ".deckjack.json";
/// History entries kept, the original state included.
pub const MAX_HISTORY: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
    pub schema: String,
    pub original: Original,
    /// Which decoder read the photo when this was last saved. A RAW
    /// developed by another decoder version can look different.
    pub decoder: String,
    /// Increments on every write.
    pub rev: u64,
    /// The current recipe: `history[position].recipe`, repeated here so a
    /// reader does not have to know the history to see the edit.
    pub recipe: Recipe,
    /// Every state, oldest first. Entry 0 is the photo as it came.
    pub history: Vec<Entry>,
    /// Which history entry is current. Below the last after an undo.
    pub position: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Original {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub label: String,
    /// Seconds since the Unix epoch.
    pub at: u64,
    pub recipe: Recipe,
}

#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error(
        "edited elsewhere since you opened it: the sidecar is at revision {current}, this save \
         was based on {base}. Reopen the photo to see the other edit."
    )]
    Stale { base: u64, current: u64 },
    #[error("nothing to {0}")]
    NoStep(&'static str),
    #[error("{name} is not a sidecar this version reads: {why}")]
    Unreadable { name: String, why: String },
    #[error(transparent)]
    Invalid(#[from] crate::recipe::Invalid),
    #[error("cannot write the sidecar for {name}: {source}")]
    Io {
        name: String,
        #[source]
        source: std::io::Error,
    },
}

/// The sidecar's path for a photo.
pub fn path_for(photo: &Path) -> PathBuf {
    let mut name = photo.file_name().unwrap_or_default().to_os_string();
    name.push(SUFFIX);
    photo.with_file_name(name)
}

/// SHA-256 of a file, hex.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What `photo.open` reports about a photo's edit state.
#[derive(Debug, Clone, Serialize)]
pub struct State {
    pub rev: u64,
    pub recipe: Recipe,
    pub history: Vec<HistoryLine>,
    pub position: usize,
    /// False when the sidecar was written for different bytes: the photo
    /// was replaced, or the sidecar was copied from another photo. The
    /// recipe is still offered; the operator decides.
    pub matches_original: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryLine {
    pub label: String,
    pub at: u64,
}

/// Read a photo's sidecar. `None` when there is none.
pub fn load(photo: &Path) -> Result<Option<Sidecar>, SidecarError> {
    let path = path_for(photo);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SidecarError::Io {
                name: name_of(photo),
                source,
            });
        }
    };
    let sidecar: Sidecar = serde_json::from_str(&text).map_err(|e| SidecarError::Unreadable {
        name: name_of(&path),
        why: e.to_string(),
    })?;
    if sidecar.schema != SCHEMA {
        return Err(SidecarError::Unreadable {
            name: name_of(&path),
            why: format!("schema {:?}, this version reads {SCHEMA:?}", sidecar.schema),
        });
    }
    if sidecar.history.is_empty() || sidecar.position >= sidecar.history.len() {
        return Err(SidecarError::Unreadable {
            name: name_of(&path),
            why: "its history position is out of range".to_string(),
        });
    }
    Ok(Some(sidecar))
}

/// The edit state of a photo, sidecar or not.
pub fn state(photo: &Path) -> Result<State, SidecarError> {
    let Some(s) = load(photo)? else {
        return Ok(State {
            rev: 0,
            recipe: Recipe::default(),
            history: Vec::new(),
            position: 0,
            matches_original: true,
        });
    };
    let size = std::fs::metadata(photo).map(|m| m.len()).ok();
    let matches = size == Some(s.original.size)
        && sha256_file(photo).ok().as_deref() == Some(s.original.sha256.as_str());
    Ok(State {
        rev: s.rev,
        recipe: s.recipe.clone(),
        history: s
            .history
            .iter()
            .map(|e| HistoryLine {
                label: e.label.clone(),
                at: e.at,
            })
            .collect(),
        position: s.position,
        matches_original: matches,
    })
}

/// One sidecar write at a time in this process, so two saves cannot both
/// read revision 4 and both write revision 5.
static WRITING: Mutex<()> = Mutex::new(());

/// What a write changes about a sidecar.
///
/// Built once per save and matched at once, so the size gap between `Save`
/// and the other two costs nothing worth a box.
#[allow(clippy::large_enum_variant)]
pub enum Change {
    /// A new state, discarding any redo states past the current one.
    Save {
        recipe: Recipe,
        label: String,
    },
    Undo,
    Redo,
}

/// Apply a change against `base_rev` and write the sidecar atomically.
/// Returns the new state.
pub fn write(
    photo: &Path,
    base_rev: u64,
    change: Change,
    decoder: &str,
) -> Result<Sidecar, SidecarError> {
    let _guard = WRITING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let current = load(photo)?;
    let current_rev = current.as_ref().map_or(0, |s| s.rev);
    if base_rev != current_rev {
        return Err(SidecarError::Stale {
            base: base_rev,
            current: current_rev,
        });
    }
    let io = |source| SidecarError::Io {
        name: name_of(photo),
        source,
    };
    let size = std::fs::metadata(photo).map_err(io)?.len();
    let sha = sha256_file(photo).map_err(io)?;
    let mut s = current.unwrap_or_else(|| Sidecar {
        schema: SCHEMA.to_string(),
        original: Original {
            name: name_of(photo),
            size,
            sha256: sha.clone(),
        },
        decoder: decoder.to_string(),
        rev: 0,
        recipe: Recipe::default(),
        history: vec![Entry {
            label: "Original".to_string(),
            at: now(),
            recipe: Recipe::default(),
        }],
        position: 0,
    });
    match change {
        Change::Save { recipe, label } => {
            recipe.validate()?;
            s.history.truncate(s.position + 1);
            s.history.push(Entry {
                label,
                at: now(),
                recipe,
            });
            if s.history.len() > MAX_HISTORY {
                let drop = s.history.len() - MAX_HISTORY;
                s.history.drain(..drop);
            }
            s.position = s.history.len() - 1;
        }
        Change::Undo => {
            if s.position == 0 {
                return Err(SidecarError::NoStep("undo"));
            }
            s.position -= 1;
        }
        Change::Redo => {
            if s.position + 1 >= s.history.len() {
                return Err(SidecarError::NoStep("redo"));
            }
            s.position += 1;
        }
    }
    s.recipe = s.history[s.position].recipe.clone();
    s.rev = current_rev + 1;
    s.original = Original {
        name: name_of(photo),
        size,
        sha256: sha,
    };
    s.decoder = decoder.to_string();
    let mut text = serde_json::to_string_pretty(&s).map_err(|e| SidecarError::Unreadable {
        name: name_of(photo),
        why: e.to_string(),
    })?;
    text.push('\n');
    let target = path_for(photo);
    atomic_write(&target, text.as_bytes()).map_err(io)?;
    Ok(s)
}

/// Write `bytes` to a hidden temporary name in the same directory, flush it
/// to disk, then rename it over `target`. A crash leaves either the old
/// sidecar or the new one, never half of one.
///
/// The temporary file is created new (`O_EXCL`): the name is predictable
/// and sits in the photo's own folder, so a symlink planted there must make
/// the save pick another name, not truncate whatever the link points at,
/// which could be the original photo. Landlock cannot keep this app from
/// writing an original; this code is what does.
fn atomic_write(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = target.parent().unwrap_or(Path::new("."));
    let mut n = 0u32;
    let (tmp, mut f) = loop {
        let tmp = dir.join(format!(
            ".{}.tmp-{}-{n}",
            name_of(target),
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 16 => n += 1,
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, target)
    })();
    if result.is_err() {
        // Only ever a file this call created.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("IMG_1.CR2");
        std::fs::write(&p, b"pretend raw bytes").unwrap();
        (dir, p)
    }

    fn bright(ev: f32) -> Recipe {
        Recipe {
            exposure_ev: ev,
            ..Recipe::default()
        }
    }

    fn save(p: &Path, base: u64, ev: f32) -> Result<Sidecar, SidecarError> {
        write(
            p,
            base,
            Change::Save {
                recipe: bright(ev),
                label: format!("Exposure {ev}"),
            },
            "test",
        )
    }

    #[test]
    fn the_sidecar_sits_beside_the_photo_under_its_full_name() {
        // `IMG_1.CR2.deckjack.json`, not `IMG_1.xmp`: Lightroom and
        // darktable own the .xmp names.
        assert_eq!(
            path_for(Path::new("/p/IMG_1.CR2")),
            Path::new("/p/IMG_1.CR2.deckjack.json")
        );
    }

    #[test]
    fn a_first_save_records_the_original_and_starts_at_revision_one() {
        let (_d, p) = photo();
        assert_eq!(state(&p).unwrap().rev, 0);
        let s = save(&p, 0, 0.7).unwrap();
        assert_eq!(s.rev, 1);
        assert_eq!(s.history.len(), 2);
        assert_eq!(s.history[0].recipe, Recipe::default());
        assert_eq!(s.recipe.exposure_ev, 0.7);
        assert_eq!(s.original.size, 17);
        assert_eq!(s.original.sha256, sha256_file(&p).unwrap());
        let text = std::fs::read_to_string(path_for(&p)).unwrap();
        // Two-space indented, schema first.
        assert!(
            text.starts_with("{\n  \"schema\": \"deckjack.photo-edit/1\""),
            "{text}"
        );
        assert!(state(&p).unwrap().matches_original);
    }

    #[test]
    fn a_save_based_on_an_old_revision_is_refused_and_writes_nothing() {
        let (_d, p) = photo();
        save(&p, 0, 0.5).unwrap();
        save(&p, 1, 1.0).unwrap();
        let before = std::fs::read(path_for(&p)).unwrap();
        let e = save(&p, 1, 2.0).unwrap_err();
        assert!(
            matches!(
                e,
                SidecarError::Stale {
                    base: 1,
                    current: 2
                }
            ),
            "{e}"
        );
        assert!(e.to_string().contains("edited elsewhere"));
        assert_eq!(std::fs::read(path_for(&p)).unwrap(), before);
        // A first save racing another first save is stale too.
        let (_d2, q) = photo();
        save(&q, 0, 0.5).unwrap();
        assert!(matches!(save(&q, 0, 0.6), Err(SidecarError::Stale { .. })));
    }

    #[test]
    fn undo_and_redo_move_through_history_and_a_save_drops_redo() {
        let (_d, p) = photo();
        save(&p, 0, 0.5).unwrap();
        save(&p, 1, 1.0).unwrap();
        let s = write(&p, 2, Change::Undo, "test").unwrap();
        assert_eq!((s.rev, s.position, s.recipe.exposure_ev), (3, 1, 0.5));
        let s = write(&p, 3, Change::Undo, "test").unwrap();
        assert_eq!(s.recipe, Recipe::default());
        assert!(matches!(
            write(&p, 4, Change::Undo, "test"),
            Err(SidecarError::NoStep("undo"))
        ));
        let s = write(&p, 4, Change::Redo, "test").unwrap();
        assert_eq!(s.recipe.exposure_ev, 0.5);
        // A new edit after an undo replaces the states ahead of it.
        let s = save(&p, 5, 2.0).unwrap();
        assert_eq!(s.history.len(), 3);
        assert_eq!(s.history[2].recipe.exposure_ev, 2.0);
        assert!(matches!(
            write(&p, 6, Change::Redo, "test"),
            Err(SidecarError::NoStep("redo"))
        ));
    }

    #[test]
    fn history_is_bounded_and_keeps_the_newest() {
        let (_d, p) = photo();
        for i in 0..(MAX_HISTORY as u64 + 5) {
            save(&p, i, (i % 50) as f32 / 20.0).unwrap();
        }
        let s = load(&p).unwrap().unwrap();
        assert_eq!(s.history.len(), MAX_HISTORY);
        assert_eq!(s.position, MAX_HISTORY - 1);
        assert_eq!(s.recipe, s.history[MAX_HISTORY - 1].recipe);
    }

    #[test]
    fn an_invalid_recipe_is_refused_before_anything_is_written() {
        let (_d, p) = photo();
        assert!(matches!(save(&p, 0, 9.0), Err(SidecarError::Invalid(_))));
        assert!(!path_for(&p).exists());
    }

    #[test]
    fn a_sidecar_for_other_bytes_is_flagged_not_hidden() {
        let (_d, p) = photo();
        save(&p, 0, 0.5).unwrap();
        std::fs::write(&p, b"a different photo now").unwrap();
        let st = state(&p).unwrap();
        assert!(!st.matches_original);
        assert_eq!(st.recipe.exposure_ev, 0.5);
    }

    #[test]
    fn a_sidecar_from_another_schema_or_with_a_bad_position_is_refused() {
        let (_d, p) = photo();
        let mut s = save(&p, 0, 0.5).unwrap();
        s.schema = "deckjack.photo-edit/2".to_string();
        std::fs::write(path_for(&p), serde_json::to_string(&s).unwrap()).unwrap();
        assert!(matches!(load(&p), Err(SidecarError::Unreadable { .. })));
        s.schema = SCHEMA.to_string();
        s.position = 7;
        std::fs::write(path_for(&p), serde_json::to_string(&s).unwrap()).unwrap();
        assert!(matches!(load(&p), Err(SidecarError::Unreadable { .. })));
    }

    #[test]
    fn writing_leaves_no_temporary_file_and_never_touches_the_photo() {
        let (d, p) = photo();
        let before = sha256_file(&p).unwrap();
        save(&p, 0, 0.5).unwrap();
        write(&p, 1, Change::Undo, "test").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), before);
        let names: Vec<String> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }

    #[test]
    fn a_symlink_at_the_temporary_name_is_passed_over_not_written_through() {
        let (d, p) = photo();
        let before = std::fs::read(&p).unwrap();
        let planted = d.path().join(format!(
            ".IMG_1.CR2.deckjack.json.tmp-{}-0",
            std::process::id()
        ));
        std::os::unix::fs::symlink(&p, &planted).unwrap();
        save(&p, 0, 0.5).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), before, "the photo was written");
        assert_eq!(load(&p).unwrap().unwrap().rev, 1);
        // Left as found: it is not this app's file.
        assert!(planted.symlink_metadata().unwrap().file_type().is_symlink());
    }
}
