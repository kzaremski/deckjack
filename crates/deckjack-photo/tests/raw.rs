//! Camera RAW through rawler, on a DNG written by the test itself.

mod common;

use deckjack_photo::decode::{self, Format};
use deckjack_photo::engine::Engine;

use common::Workdir;
use common::dng::dng;

#[test]
fn a_flat_dng_decodes_to_the_same_linear_gray() {
    let w = Workdir::new();
    let path = w.photos().join("flat.dng");
    // 18% gray everywhere.
    std::fs::write(&path, dng(64, 48, (0.18 * 65535.0) as u16, 1, 32803)).unwrap();
    let src = decode::decode(&path).unwrap();
    assert_eq!(src.format, Format::Raw);
    assert_eq!((src.width(), src.height()), (64, 48));
    // Away from the demosaic's borders, every channel reads 0.18.
    let mid = ((24 * 64 + 32) * 3) as usize;
    for c in 0..3 {
        let v = src.pixels[mid + c];
        assert!((v - 0.18).abs() < 0.01, "channel {c}: {v}");
    }
    assert_eq!(src.info.camera.as_deref(), Some("Deckjack Test DNG"));
}

#[test]
fn a_raws_exif_orientation_is_applied() {
    // Orientation 6: turned 90 degrees clockwise to display. A 64 x 48
    // sensor shows as 48 x 64. (rawler's own orientation field missed this
    // on a real CR2; spike 3.)
    let w = Workdir::new();
    let path = w.photos().join("turned.dng");
    std::fs::write(&path, dng(64, 48, 10000, 6, 32803)).unwrap();
    let src = decode::decode(&path).unwrap();
    assert_eq!(
        (src.width(), src.height(), src.info.orientation),
        (48, 64, 6)
    );
}

#[test]
fn a_malformed_raw_is_a_decode_error_not_a_crash() {
    // Photometric 2 in a DNG's raw IFD reaches a `todo!()` in rawler 0.8.0.
    // The app must answer "could not decode", not lose its process and
    // every command in flight with it.
    let w = Workdir::new();
    std::fs::write(w.photos().join("bad.dng"), dng(16, 16, 1000, 1, 2)).unwrap();
    let err = decode::decode(&w.photos().join("bad.dng")).err().unwrap();
    assert!(err.to_string().contains("could not decode"), "{err}");
    std::fs::write(w.photos().join("junk.cr2"), b"not a raw file at all").unwrap();
    let e = Engine::new(w.path().to_path_buf());
    assert!(e.open("Photos/junk.cr2").is_err());
}

#[test]
fn a_raw_proxy_is_log_encoded_and_the_lut_bake_follows() {
    let w = Workdir::new();
    std::fs::write(
        w.photos().join("flat.dng"),
        dng(64, 48, (0.18 * 65535.0) as u16, 1, 32803),
    )
    .unwrap();
    let e = Engine::new(w.path().to_path_buf());
    let open = e.open("Photos/flat.dng").unwrap();
    assert_eq!(open["proxy_encoding"], "log");
    assert!(open["note"].as_str().unwrap().contains("neutral"));
    let p = e.proxy("Photos/flat.dng", Some(64)).unwrap();
    assert_eq!(p["encoding"], "log");
    let l = e.lut("Photos/flat.dng", &Default::default(), 17).unwrap();
    assert_eq!(l["encoding"], "log");
}

#[test]
fn the_embedded_preview_of_a_display_photo_is_the_photo_scaled() {
    let w = Workdir::new();
    common::write_jpeg(&w.photos().join("a.jpg"), &common::test_card(400, 300));
    let e = Engine::new(w.path().to_path_buf());
    let p = e.embedded("Photos/a.jpg", Some(100)).unwrap();
    assert_eq!(
        (p["width"].as_u64(), p["height"].as_u64()),
        (Some(100), Some(75))
    );
    // A DNG with no preview IFD says so rather than inventing one.
    std::fs::write(w.photos().join("flat.dng"), dng(64, 48, 9000, 1, 32803)).unwrap();
    assert!(
        e.embedded("Photos/flat.dng", None)
            .unwrap_err()
            .contains("preview")
    );
}
