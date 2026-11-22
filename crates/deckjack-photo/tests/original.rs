//! The write paths, end to end through the engine: the original's bytes
//! never change, an export never replaces a file, a cancelled export leaves
//! nothing, and a stale save is refused.

mod common;

use deckjack_app::Cancellation;
use deckjack_photo::engine::Engine;
use deckjack_photo::export::Format;
use deckjack_photo::recipe::Recipe;
use deckjack_photo::sidecar::{Change, sha256_file};

use common::{Workdir, test_card, write_jpeg};

fn edit() -> Recipe {
    serde_json::from_value(serde_json::json!({
        "exposure_ev": 0.7,
        "geometry": { "straighten_deg": 3.0, "crop": { "x": 0.1, "y": 0.05, "w": 0.6, "h": 0.75 } },
        "curves": { "master": [[0, 0], [0.25, 0.18], [0.75, 0.85], [1, 1]] },
        "masks": [{ "mask": { "kind": "radial", "cx": 0.5, "cy": 0.4, "rx": 0.2, "ry": 0.25, "feather": 0.5 },
                    "exposure_ev": 0.7 }]
    }))
    .unwrap()
}

#[test]
fn every_write_path_leaves_the_original_byte_for_byte() {
    let w = Workdir::new();
    let photo = w.photos().join("IMG_1.jpg");
    write_jpeg(&photo, &test_card(320, 240));
    let before = sha256_file(&photo).unwrap();
    let e = Engine::new(w.path().to_path_buf());

    e.open("Photos/IMG_1.jpg").unwrap();
    let saved = e
        .edit(
            "Photos/IMG_1.jpg",
            0,
            Change::Save {
                recipe: edit(),
                label: "Edit".into(),
            },
        )
        .unwrap();
    assert_eq!(saved["rev"], 1);
    e.edit("Photos/IMG_1.jpg", 1, Change::Undo).unwrap();
    e.edit("Photos/IMG_1.jpg", 2, Change::Redo).unwrap();
    let none = Cancellation::new();
    let jpg = e
        .export(
            "Photos/IMG_1.jpg",
            None,
            Format::Jpeg,
            Some(90),
            None,
            None,
            &none,
            &|_| {},
        )
        .unwrap();
    let tif = e
        .export(
            "Photos/IMG_1.jpg",
            None,
            Format::Tiff16,
            None,
            Some(100),
            None,
            &none,
            &|_| {},
        )
        .unwrap();
    let cancelled = Cancellation::new();
    cancelled.cancel();
    assert!(
        e.export(
            "Photos/IMG_1.jpg",
            None,
            Format::Jpeg,
            None,
            None,
            None,
            &cancelled,
            &|_| {}
        )
        .is_err()
    );

    assert_eq!(sha256_file(&photo).unwrap(), before, "the original changed");
    assert_eq!(jpg["path"], "Photos/IMG_1-edit.jpg");
    assert_eq!(tif["path"], "Photos/IMG_1-edit.tif");
    assert_eq!(
        w.names(),
        [
            "IMG_1-edit.jpg",
            "IMG_1-edit.tif",
            "IMG_1.jpg",
            "IMG_1.jpg.deckjack.json"
        ]
    );
    // The export used the saved (redone) recipe: cropped to 60% x 75%.
    assert_eq!(
        (jpg["width"].as_u64(), jpg["height"].as_u64()),
        (Some(192), Some(180))
    );
    // The long-edge limit applied to the TIFF.
    assert_eq!(tif["width"], 100);
}

#[test]
fn an_export_named_like_the_original_still_never_replaces_it() {
    // `x-edit.jpg` exported is `x-edit-edit.jpg`; and with that taken too,
    // the next number. Nothing existing is replaced.
    let w = Workdir::new();
    let photo = w.photos().join("x-edit.jpg");
    write_jpeg(&photo, &test_card(64, 48));
    std::fs::write(w.photos().join("x-edit-edit.jpg"), b"keep me").unwrap();
    let before = sha256_file(&photo).unwrap();
    let e = Engine::new(w.path().to_path_buf());
    let out = e
        .export(
            "Photos/x-edit.jpg",
            Some(edit()),
            Format::Jpeg,
            None,
            None,
            None,
            &Cancellation::new(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(out["path"], "Photos/x-edit-edit (2).jpg");
    assert_eq!(
        std::fs::read(w.photos().join("x-edit-edit.jpg")).unwrap(),
        b"keep me"
    );
    assert_eq!(sha256_file(&photo).unwrap(), before);
}

#[test]
fn an_export_cancelled_midway_leaves_no_file_and_reports_cancelled() {
    let w = Workdir::new();
    write_jpeg(&w.photos().join("big.jpg"), &test_card(200, 1000));
    let e = Engine::new(w.path().to_path_buf());
    let cancel = Cancellation::new();
    let seen = std::sync::Mutex::new(Vec::new());
    // Cancelled from the progress callback once the render is underway,
    // as a cancel arriving mid-export would be.
    let err = e
        .export(
            "Photos/big.jpg",
            Some(edit()),
            Format::Tiff16,
            None,
            None,
            None,
            &cancel,
            &|f| {
                seen.lock().unwrap().push(f);
                if f > 0.2 {
                    cancel.cancel();
                }
            },
        )
        .unwrap_err();
    assert!(err.contains("cancelled"), "{err}");
    let seen = seen.into_inner().unwrap();
    assert!(seen.iter().any(|&f| f > 0.2 && f < 0.85), "{seen:?}");
    assert_eq!(w.names(), ["big.jpg"]);
}

#[test]
fn a_named_export_job_is_cancelled_by_its_name() {
    let e = Engine::new(std::env::temp_dir());
    let c = Cancellation::new();
    assert!(!e.cancel_job("export-1"));
    e.start_job(Some("export-1"), &c);
    assert!(e.cancel_job("export-1"));
    assert!(c.is_cancelled());
    e.end_job(Some("export-1"));
    assert!(!e.cancel_job("export-1"));
}

#[test]
fn a_stale_save_is_refused_with_the_operators_sentence() {
    let w = Workdir::new();
    write_jpeg(&w.photos().join("a.jpg"), &test_card(32, 32));
    let e = Engine::new(w.path().to_path_buf());
    // Two windows open revision 0; the phone saves first.
    e.edit(
        "Photos/a.jpg",
        0,
        Change::Save {
            recipe: edit(),
            label: "Phone".into(),
        },
    )
    .unwrap();
    let err = e
        .edit(
            "Photos/a.jpg",
            0,
            Change::Save {
                recipe: Recipe::default(),
                label: "Desktop".into(),
            },
        )
        .unwrap_err();
    assert!(
        err.contains("edited elsewhere since you opened it"),
        "{err}"
    );
    let state = e.open("Photos/a.jpg").unwrap();
    assert_eq!(state["edit"]["rev"], 1);
    // Read as written (0.7), not as the f32 widened (0.699999988...).
    assert_eq!(state["edit"]["recipe"]["exposure_ev"].as_f64(), Some(0.7));
}

#[test]
fn paths_outside_photos_and_sidecars_are_refused() {
    let w = Workdir::new();
    write_jpeg(&w.photos().join("a.jpg"), &test_card(16, 16));
    std::fs::write(w.path().join("outside.jpg"), b"x").unwrap();
    let e = Engine::new(w.path().to_path_buf());
    assert!(e.open("outside.jpg").unwrap_err().contains("Photos/"));
    assert!(e.open("Photos/../outside.jpg").is_err());
    e.edit(
        "Photos/a.jpg",
        0,
        Change::Save {
            recipe: edit(),
            label: "x".into(),
        },
    )
    .unwrap();
    assert!(
        e.open("Photos/a.jpg.deckjack.json")
            .unwrap_err()
            .contains("sidecar")
    );
    assert!(e.open("Photos/notes.txt").is_err());
}

#[test]
fn describe_names_the_edit_and_its_clipping() {
    let w = Workdir::new();
    write_jpeg(&w.photos().join("a.jpg"), &test_card(200, 100));
    let e = Engine::new(w.path().to_path_buf());
    let r = Recipe {
        exposure_ev: 3.0,
        ..Recipe::default()
    };
    e.edit(
        "Photos/a.jpg",
        0,
        Change::Save {
            recipe: r,
            label: "Bright".into(),
        },
    )
    .unwrap();
    let d = e.describe("Photos/a.jpg").unwrap();
    assert_eq!(d["changes"][0], "exposure +3.00 EV");
    // Three stops up blows most of the card out. (Below, one stop up
    // clips the sky's blue but rarely all three channels: a pixel counts as
    // clipped when any channel is.)
    assert!(
        d["histogram"]["clipped"]["highlights"].as_f64().unwrap() > 0.3,
        "{d}"
    );
    assert_eq!(d["output"]["width"], 200);
    assert_eq!(d["rev"], 1);
    let one = Recipe {
        exposure_ev: 1.0,
        ..Recipe::default()
    };
    e.edit(
        "Photos/a.jpg",
        1,
        Change::Save {
            recipe: one,
            label: "One stop".into(),
        },
    )
    .unwrap();
    let d = e.describe("Photos/a.jpg").unwrap();
    let clipped = d["histogram"]["clipped"]["highlights"].as_f64().unwrap();
    assert!(clipped > 0.4, "one stop up clips the sky's blue: {clipped}");
    let listing = e.list("Photos").unwrap();
    assert_eq!(listing["entries"].as_array().unwrap().len(), 1);
    assert_eq!(listing["entries"][0]["edited"], true);
}
