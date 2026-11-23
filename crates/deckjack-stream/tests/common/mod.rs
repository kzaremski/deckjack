//! The compositor installed the way the core lays a native app out, with
//! the launch plan the core would build for it, and the core's end of the
//! link.

#![allow(dead_code)]

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use deckjack_core::apps::{Confinement, Env as _, LaunchPlan, OsEnv};
use deckjack_core::sandbox;
use deckjack_core::stream::{NativeApp, NativeDecl, native_plan};
use deckjack_protocol::Manifest;
use deckjack_protocol::stream::{
    KIND_CONTROL, KIND_VIDEO, LaunchSpec, StreamCommand, StreamEvent, VideoHeader, link_frame,
};

pub struct Rig {
    pub env: OsEnv,
    pub install: PathBuf,
    pub work: PathBuf,
    manifest: Manifest,
    _dir: tempfile::TempDir,
}

/// Put the compositor at `dest`.
///
/// Copied once per test process, then hard-linked: a copy made on one test
/// thread while another forks to spawn a compositor leaves the copy's
/// write descriptor in that child until it execs, and an exec of the fresh
/// copy then fails with `ETXTBSY`. Linked, not symlinked: Landlock checks a
/// symlink's target, and the build directory is granted to nothing.
pub fn install_compositor(dest: &Path) {
    static ONCE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let (_, source) = ONCE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deckjack-stream");
        std::fs::copy(env!("CARGO_BIN_EXE_deckjack-stream"), &path).unwrap();
        (dir, path)
    });
    if std::fs::hard_link(source, dest).is_err() {
        // Another filesystem: a copy, which this process makes before any
        // compositor of this rig is spawned.
        std::fs::copy(source, dest).unwrap();
    }
}

/// A native app's declaration with no needs: the compositor and its app
/// get the launcher's own grants and nothing from the working directory.
const MANIFEST: &str = r#"
[app]
name = "native"
version = "0.1.0"
protocol = 1
title = "Native"
"#;

pub fn have(paths: &[&str]) -> bool {
    let missing: Vec<&&str> = paths.iter().filter(|p| !Path::new(p).exists()).collect();
    if !missing.is_empty() {
        eprintln!("skipped: not installed on this host: {missing:?}");
    }
    missing.is_empty()
}

impl Rig {
    /// `None`, having said why, on a host that cannot confine.
    pub fn new() -> Option<Rig> {
        if !sandbox::availability().is_enforcing() {
            eprintln!(
                "skipped: this host cannot confine ({:?})",
                sandbox::availability()
            );
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("install");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let native = install.join("native");
        std::fs::create_dir_all(&native).unwrap();
        install_compositor(&native.join("deckjack-stream"));
        let manifest = Manifest::parse(Path::new("native.toml"), MANIFEST).unwrap();
        Some(Rig {
            env: OsEnv::new(),
            install,
            work,
            manifest,
            _dir: dir,
        })
    }

    /// The plan exactly as the core builds one for a native app running
    /// `spec`, handed the link at `link_fd`.
    pub fn plan(&self, spec: &LaunchSpec, link_fd: i32) -> LaunchPlan {
        let app = NativeApp {
            manifest: self.manifest.clone(),
            native: NativeDecl {
                exec: spec.exec.clone(),
                x11: spec.x11,
            },
        };
        native_plan(
            &app,
            &self.install,
            &self.work,
            Confinement::Enforced,
            &self.env,
            link_fd,
        )
        .expect("a plan for a real install")
    }

    pub fn home(&self) -> PathBuf {
        let spec = LaunchSpec {
            app: "native".into(),
            exec: vec!["/usr/bin/true".into()],
            x11: false,
        };
        let plan = self.plan(&spec, 3);
        PathBuf::from(&plan.env.iter().find(|(k, _)| k == "HOME").unwrap().1)
    }

    /// Spawn the compositor confined for `spec`, with the link a socket
    /// pair as the core makes it.
    pub fn launch(&self, spec: &LaunchSpec) -> Running {
        use std::os::fd::AsRawFd as _;
        let (stream, theirs) = UnixStream::pair().unwrap();
        let mut plan = self.plan(spec, theirs.as_raw_fd());
        if std::env::var_os("DECKJACK_STREAM_TIMING").is_some() {
            plan.env.push(("DECKJACK_STREAM_TIMING".into(), "1".into()));
        }
        let started = Instant::now();
        let pid = self
            .env
            .spawn(&plan)
            .expect("the confined compositor starts");
        drop(theirs);
        Running {
            pid,
            stream,
            started,
        }
    }
}

pub struct Running {
    pub pid: u32,
    pub stream: UnixStream,
    pub started: Instant,
}

/// One frame from the compositor.
#[derive(Debug)]
pub enum Got {
    Event(StreamEvent),
    Video(VideoHeader, Vec<u8>),
}

impl Running {
    /// The next frame, or `None` at the end of the stream or after
    /// `timeout`.
    pub fn next(&mut self, timeout: Duration) -> Option<Got> {
        self.stream.set_read_timeout(Some(timeout)).unwrap();
        let (kind, body) = deckjack_stream::link::read_frame(&mut self.stream).ok()??;
        match kind {
            KIND_CONTROL => Some(Got::Event(serde_json::from_slice(&body).unwrap())),
            KIND_VIDEO => {
                let (header, h264) = VideoHeader::parse_link(&body).unwrap();
                Some(Got::Video(header, h264.to_vec()))
            }
            other => panic!("unknown frame kind {other}"),
        }
    }

    /// Read until `want` matches, within `timeout`, handing everything
    /// read on the way to `seen`.
    pub fn until(
        &mut self,
        timeout: Duration,
        mut want: impl FnMut(&Got) -> bool,
        mut seen: impl FnMut(&Got),
    ) -> Option<Got> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            let got = self.next(left.max(Duration::from_millis(1)))?;
            if want(&got) {
                return Some(got);
            }
            seen(&got);
        }
        None
    }

    pub fn send(&mut self, command: &StreamCommand) {
        use std::io::Write as _;
        let json = serde_json::to_vec(command).unwrap();
        self.stream
            .write_all(&link_frame(KIND_CONTROL, &json))
            .unwrap();
    }

    /// Type ASCII text as evdev key presses (US layout).
    pub fn type_text(&mut self, surface: u32, text: &str) {
        for c in text.chars() {
            let (code, shift) = evdev_for(c).unwrap_or_else(|| panic!("no key for {c:?}"));
            let press = |me: &mut Running, code: u32, pressed: bool| {
                me.send(&StreamCommand::Input {
                    surface,
                    input: deckjack_protocol::stream::Input::Key { code, pressed },
                });
            };
            if shift {
                press(self, 42, true);
            }
            press(self, code, true);
            press(self, code, false);
            if shift {
                press(self, 42, false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// The evdev code (and whether Shift is needed) for one ASCII character
/// on a US layout: just what the tests type.
fn evdev_for(c: char) -> Option<(u32, bool)> {
    const LETTERS: [u32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    Some(match c {
        'a'..='z' => (LETTERS[(c as u8 - b'a') as usize], false),
        'A'..='Z' => (LETTERS[(c as u8 - b'A') as usize], true),
        '1'..='9' => (2 + (c as u8 - b'1') as u32, false),
        '0' => (11, false),
        ' ' => (57, false),
        '\n' => (28, false),
        '-' => (12, false),
        '.' => (52, false),
        '/' => (53, false),
        '$' => (5, true),
        '_' => (12, true),
        '>' => (52, true),
        _ => return None,
    })
}

impl Drop for Running {
    fn drop(&mut self) {
        // Only the pid this test spawned, never a pattern.
        unsafe {
            libc::kill(self.pid as i32, libc::SIGKILL);
        }
    }
}
