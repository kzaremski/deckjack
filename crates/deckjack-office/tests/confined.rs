//! LibreOffice under the real Deckjack sandbox.
//!
//! The brief's spike reproduced Landlock with `setpriv`, which mirrors the
//! filesystem rules but not seccomp or the network rules. These run the
//! office binary under the launcher itself: the real manifest, the real
//! `LaunchPlan::build` with the real `OsEnv`, spawned through the same
//! `Env::spawn` the core uses. Nothing is faked.
//!
//! They skip, saying why, on a host that cannot confine or has no
//! LibreOffice. On such a host they are evidence of nothing.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{Rig, have_libreoffice};
use deckjack_core::apps::Env as _;

/// Convert one fixture with the worker, launched by the core's own launcher
/// as `deckjack-office --worker <dir>`.
fn convert_confined(rig: &Rig, fixture: &str) -> (serde_json::Value, Vec<u8>) {
    let ext = Path::new(fixture).extension().unwrap().to_str().unwrap();
    let job = rig.tmp.join(format!("spike-{ext}"));
    std::fs::create_dir_all(&job).unwrap();
    let input = job.join(format!("input.{ext}"));
    std::fs::copy(common::fixture(fixture), &input).unwrap();
    let profile = rig.home.join("lok").join(format!("spike-{ext}"));
    std::fs::create_dir_all(profile.join("user")).unwrap();
    let spec = serde_json::json!({
        "install": "/usr/lib/libreoffice/program",
        "profile": profile,
        "input": input,
        "output": job.join("output.pdf"),
        "format": "pdf",
        "address_space": 16u64 << 30,
        "data": 2u64 << 30,
        "cpu_seconds": 120,
    });
    std::fs::write(job.join("job.json"), spec.to_string()).unwrap();

    let mut plan = rig.plan();
    plan.args = vec!["--worker".into(), job.to_string_lossy().into_owned()];
    // What the app sets for its worker; the plan's own env (HOME, TMPDIR)
    // is kept.
    for (key, value) in deckjack_office::worker::WORKER_ENV {
        plan.env.push((key.to_string(), value.to_string()));
    }
    let pid = rig.env.spawn(&plan).expect("the confined worker starts");
    let deadline = Instant::now() + Duration::from_secs(120);
    while rig.env.is_running(pid) {
        assert!(Instant::now() < deadline, "the confined worker hung");
        std::thread::sleep(Duration::from_millis(50));
    }
    let result: serde_json::Value = serde_json::from_slice(
        &std::fs::read(job.join("result.json")).expect("the worker wrote no result"),
    )
    .unwrap();
    let pdf = std::fs::read(job.join("output.pdf")).unwrap_or_default();
    (result, pdf)
}

/// S1: docx, xlsx and pptx each become a PDF with the engine confined to
/// exactly what the office manifest is granted.
///
/// This drives the worker alone, on a copy in its own job directory, so it
/// says nothing about the operator's folders. That nothing lands there (no
/// `.~lock` file, nothing but what the commands wrote) is asserted in
/// `tests/app.rs`, through the app, which is what copies a document into a
/// job directory before the engine sees it.
#[test]
fn libreoffice_converts_docx_xlsx_and_pptx_under_the_real_launcher() {
    let Some(rig) = Rig::new() else { return };
    if !have_libreoffice() {
        return;
    }
    for fixture in ["report.docx", "sheet.xlsx", "deck.pptx"] {
        let (result, pdf) = convert_confined(&rig, fixture);
        assert_eq!(result["ok"], true, "{fixture}: {result}");
        assert!(pdf.starts_with(b"%PDF-"), "{fixture}: no PDF was written");
        // The layout check: a function table read at the wrong offsets does
        // not return LibreOffice's version JSON.
        let version = result["version"].as_str().unwrap_or_default();
        assert!(version.contains("\"ProductName\""), "{fixture}: {version}");
        eprintln!(
            "S1 {fixture}: {} ms, peak RSS {} MiB",
            result["millis"],
            result["peak_rss_kib"].as_u64().unwrap_or(0) / 1024
        );
    }
}

/// The control for the test above: the same worker with the data limit far
/// below what LibreOffice needs to boot fails, and says so, rather than
/// succeeding because the limit was never applied.
#[test]
fn a_worker_held_under_its_memory_limit_fails_instead_of_converting() {
    let Some(rig) = Rig::new() else { return };
    if !have_libreoffice() {
        return;
    }
    let job = rig.tmp.join("starved");
    std::fs::create_dir_all(&job).unwrap();
    std::fs::copy(common::fixture("report.docx"), job.join("input.docx")).unwrap();
    let spec = serde_json::json!({
        "install": "/usr/lib/libreoffice/program",
        "profile": rig.home.join("lok/starved"),
        "input": job.join("input.docx"),
        "output": job.join("output.pdf"),
        "format": "pdf",
        "address_space": 16u64 << 30,
        "data": 64u64 << 20,
        "cpu_seconds": 120,
    });
    std::fs::write(job.join("job.json"), spec.to_string()).unwrap();
    let mut plan = rig.plan();
    plan.args = vec!["--worker".into(), job.to_string_lossy().into_owned()];
    let pid = rig.env.spawn(&plan).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while rig.env.is_running(pid) {
        assert!(Instant::now() < deadline, "the starved worker hung");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !job.join("output.pdf").exists(),
        "a worker limited to 64 MiB of data converted anyway: the limit is not applied"
    );
}
