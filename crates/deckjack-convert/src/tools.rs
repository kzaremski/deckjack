//! The `ffmpeg` and `ffprobe` binaries: where they are, and how every child
//! is started.
//!
//! Both are found at fixed absolute paths, never through `PATH` (the launcher
//! clears the environment anyway). Each child inherits this process's
//! Landlock and seccomp confinement, so it can open only what the app was
//! granted. On top of that every input is opened with
//! `-protocol_whitelist file` and `-format_whitelist` naming the demuxers in
//! [`INPUT_FORMATS`]: a playlist or concat file that tries to make ffmpeg read
//! other files is refused by ffmpeg itself, before it reads anything.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Where ffmpeg is looked for, in order. All under `/usr` or `/bin`, which the
/// launcher grants read-and-execute.
pub const FFMPEG_PATHS: &[&str] = &["/usr/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/bin/ffmpeg"];
pub const FFPROBE_PATHS: &[&str] = &["/usr/bin/ffprobe", "/usr/local/bin/ffprobe", "/bin/ffprobe"];

/// The demuxers an input may be opened with. Containers of ordinary media
/// files only: no `hls`, `concat`, `ffconcat`, `image2`, `tee` or any other
/// format whose file names other files (lane brief, R4).
pub const INPUT_FORMATS: &str =
    "mov,matroska,mp3,ogg,flac,wav,avi,mpegts,aac,flv,asf,aiff,w64,mpeg";

/// How much nicer than Convert itself every child runs (capped at 19).
/// Neither sandbox layer limits CPU, and a laptop at full load for an hour is
/// hot and loud.
pub const NICE: i32 = 10;

/// The encoders and filters the presets and the estimate use, as
/// `convert.status` reports them.
pub const WANTED_ENCODERS: &[&str] = &["libx264", "libsvtav1", "libopus", "aac", "flac"];
pub const WANTED_FILTERS: &[&str] = &["libvmaf", "ssim", "scale"];

#[derive(Debug, Clone)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// Threads one encode may use: half the cores, at least one.
    pub threads: usize,
}

impl Tools {
    /// Find both binaries, or say how to install them.
    pub fn locate() -> Result<Tools, String> {
        let first = |paths: &[&str]| paths.iter().map(PathBuf::from).find(|p| p.is_file());
        match (first(FFMPEG_PATHS), first(FFPROBE_PATHS)) {
            (Some(ffmpeg), Some(ffprobe)) => Ok(Tools {
                ffmpeg,
                ffprobe,
                threads: threads_for(std::thread::available_parallelism().map_or(1, |n| n.get())),
            }),
            _ => Err(format!(
                "ffmpeg and ffprobe were not found in {}. Install them on this node \
                 ({}), then reopen Convert",
                FFMPEG_PATHS.join(", "),
                install_hint()
            )),
        }
    }

    /// The input options every child uses, before `-i`.
    pub fn input_guard() -> Vec<String> {
        [
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            INPUT_FORMATS,
        ]
        .map(String::from)
        .to_vec()
    }

    /// An `ffprobe` command for one file, guarded, with JSON output.
    pub fn ffprobe(&self, path: &Path, extra: &[&str]) -> Command {
        let mut cmd = Command::new(&self.ffprobe);
        cmd.args(["-hide_banner", "-v", "error"])
            .args(Tools::input_guard())
            .args(extra)
            .arg("-i")
            .arg(file_url(path));
        cmd
    }

    /// What `convert.status` says: versions, the encoders and filters the
    /// presets need, and the hardware accelerations this build lists (which
    /// no preset uses in v1).
    pub fn status(&self) -> Value {
        let text = |args: &[&str]| {
            let mut cmd = Command::new(&self.ffmpeg);
            cmd.arg("-hide_banner").args(args);
            capture(cmd, Duration::from_secs(10))
                .map(|o| o.stdout)
                .unwrap_or_default()
        };
        let version = text(&["-version"]).lines().next().unwrap_or("").to_string();
        let encoders = text(&["-encoders"]);
        let filters = text(&["-filters"]);
        let hwaccels: Vec<String> = text(&["-hwaccels"])
            .lines()
            .skip_while(|l| !l.starts_with("Hardware acceleration methods"))
            .skip(1)
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let listed = |list: &str, name: &str| {
            list.lines()
                .any(|l| l.split_whitespace().nth(1) == Some(name))
        };
        let encoders: serde_json::Map<String, Value> = WANTED_ENCODERS
            .iter()
            .map(|e| (e.to_string(), Value::Bool(listed(&encoders, e))))
            .collect();
        let filters: serde_json::Map<String, Value> = WANTED_FILTERS
            .iter()
            .map(|f| (f.to_string(), Value::Bool(listed(&filters, f))))
            .collect();
        json!({
            "found": true,
            "ffmpeg": self.ffmpeg,
            "ffprobe": self.ffprobe,
            "version": version,
            "encoders": encoders,
            "filters": filters,
            "hwaccels": hwaccels,
            "threads": self.threads,
        })
    }
}

/// Half the cores, at least one: an encode should leave the node usable.
pub fn threads_for(cores: usize) -> usize {
    (cores / 2).max(1)
}

/// The command that installs ffmpeg on this kind of node, for a sentence.
fn install_hint() -> &'static str {
    let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let id = |name: &str| {
        release
            .lines()
            .any(|l| (l.starts_with("ID=") || l.starts_with("ID_LIKE=")) && l.contains(name))
    };
    if id("arch") {
        "sudo pacman -S ffmpeg"
    } else if id("debian") || id("ubuntu") {
        "sudo apt install ffmpeg"
    } else if id("fedora") {
        "sudo dnf install ffmpeg"
    } else {
        "your distribution's ffmpeg package"
    }
}

/// `file:<absolute path>`: ffmpeg reads the name as a file and nothing else,
/// whatever it contains (`%`, `|`, a leading `-`, a `scheme:` prefix).
pub fn file_url(path: &Path) -> String {
    format!("file:{}", path.display())
}

/// Prepare the child about to exec: lower its priority, and tie its life to
/// the thread that starts it. Called between fork and exec, so only plain
/// syscall wrappers: `getpriority`, `setpriority`, `prctl` and `getppid`.
///
/// The death signal is what stops an encode when Convert goes away without
/// cancelling it: the core's SIGTERM on stop, restart or shutdown, a crash,
/// or the frame loop ending when the core's socket closes. Without it the
/// ffmpeg is reparented and keeps encoding for up to an hour, invisible, into
/// a partial the next start removes, while Retry starts a second encode
/// beside it. The kernel sends it when the *thread* that forked exits, which
/// is safe here: every child is waited for by the thread that started it
/// ([`capture`], [`crate::encode::run_with_pid`]).
pub fn niced(cmd: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id();
    // SAFETY: the closure runs in the forked child before exec and makes
    // four syscalls that touch no memory of the parent's; nothing is
    // allocated (the error is built from a raw errno, which does not).
    unsafe {
        cmd.pre_exec(move || {
            // Relative to this process: an absolute value would be no
            // nicer at all for an app the launcher already runs niced.
            let now = libc::getpriority(libc::PRIO_PROCESS, 0);
            libc::setpriority(libc::PRIO_PROCESS, 0, (now + NICE).min(19));
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The parent may have died between fork and prctl, in which case
            // the signal will never come: do not start at all.
            if u32::try_from(libc::getppid()).ok() != Some(parent) {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    cmd
}

/// What a short child printed.
#[derive(Debug)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run a short child (a probe, a version query) to its end, niced, and give
/// up on it after `limit`: a hostile file that makes a probe spin must not
/// hold a command forever.
pub fn capture(mut cmd: Command, limit: Duration) -> Result<Output, String> {
    niced(&mut cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start {:?}: {e}", cmd.get_program()))?;
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                stop(&mut child, Duration::from_millis(500));
                return Err(format!(
                    "{:?} did not finish within {} s and was stopped",
                    cmd.get_program(),
                    limit.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("could not wait for {:?}: {e}", cmd.get_program())),
        }
    };
    Ok(Output {
        success: status.success(),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// Read a pipe to its end on a thread of its own, so a child that fills one
/// pipe while we wait on the other never deadlocks.
fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        text
    })
}

/// Ask a child to stop (SIGTERM, which ffmpeg answers by finishing its
/// current packet and exiting), then kill it if it has not after `grace`.
pub fn stop(child: &mut Child, grace: Duration) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: kill(2) on the pid of a child this process started and has
        // not yet reaped, so the pid cannot have been reused.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The last line ffmpeg wrote to stderr that says something, for a failed
/// job's sentence.
pub fn last_words(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(|l| l.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_encode_takes_half_the_cores_and_at_least_one() {
        assert_eq!(threads_for(24), 12);
        assert_eq!(threads_for(3), 1);
        assert_eq!(threads_for(1), 1);
        assert_eq!(threads_for(0), 1);
    }

    #[test]
    fn every_path_reaches_ffmpeg_as_a_file_and_nothing_else() {
        // A name that starts with '-' or holds a scheme or a pattern is
        // still one file.
        assert_eq!(
            file_url(Path::new("/w/Videos/-i http:%d|x.mp4")),
            "file:/w/Videos/-i http:%d|x.mp4"
        );
    }

    #[test]
    fn the_input_guard_names_no_format_that_reads_other_files() {
        let formats: Vec<&str> = INPUT_FORMATS.split(',').collect();
        for refused in [
            "hls", "concat", "ffconcat", "image2", "tee", "dash", "data", "lavfi",
        ] {
            assert!(!formats.contains(&refused), "{refused} is allowed");
        }
        let guard = Tools::input_guard();
        assert_eq!(guard[0..2], ["-protocol_whitelist", "file"]);
        assert_eq!(guard[2], "-format_whitelist");
    }

    #[test]
    fn a_failure_is_explained_by_its_last_line() {
        assert_eq!(
            last_words("a\nb: Invalid data found\n\n").as_deref(),
            Some("b: Invalid data found")
        );
        assert_eq!(last_words("  \n"), None);
    }

    #[test]
    fn a_child_that_runs_too_long_is_stopped_and_said_so() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"]);
        let started = Instant::now();
        let err = capture(cmd, Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("did not finish"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A process's nice value, field 19 of its stat line.
    fn nice_of(stat: &str) -> i32 {
        // Field 2 is the command in parentheses and may hold spaces.
        let after = &stat[stat.rfind(')').unwrap() + 2..];
        after.split(' ').nth(16).unwrap().parse().unwrap()
    }

    #[test]
    fn a_child_runs_niced() {
        // Relative to this process, which may itself run niced (a CI runner,
        // `nice cargo test`): a child that only inherited that would pass a
        // bare `>= NICE`.
        let own = nice_of(&std::fs::read_to_string("/proc/self/stat").unwrap());
        if own >= 19 {
            eprintln!("skipped: this test already runs at nice {own}, so niced shows nothing");
            return;
        }
        let mut cmd = Command::new("/bin/cat");
        cmd.arg("/proc/self/stat");
        let out = capture(cmd, Duration::from_secs(5)).unwrap();
        let nice = nice_of(&out.stdout);
        assert!(
            nice > own && nice >= (own + NICE).min(19),
            "the child ran at nice {nice}, this test at {own}"
        );
    }

    #[test]
    fn a_child_that_ignores_sigterm_is_killed_after_the_grace() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; exec sleep 30"])
            .spawn()
            .unwrap();
        // Give the shell time to ignore TERM and exec; the ignore survives
        // the exec, so `sleep` is what ignores the SIGTERM.
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        stop(&mut child, Duration::from_millis(300));
        let took = started.elapsed();
        use std::os::unix::process::ExitStatusExt;
        let status = child.try_wait().unwrap().expect("the child was reaped");
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
        assert!(took >= Duration::from_millis(300), "{took:?}");
        assert!(took < Duration::from_secs(5), "{took:?}");
    }

    #[test]
    fn a_child_dies_with_the_thread_that_started_it() {
        // What happens to an encode when Convert exits or is SIGTERMed: the
        // threads that started the children go, and so must the children.
        // A thread stands in for the process here, because the death signal
        // follows the forking thread.
        let child = std::thread::spawn(|| {
            let mut cmd = Command::new("/bin/sleep");
            cmd.arg("30").stdin(Stdio::null());
            niced(&mut cmd).spawn().unwrap()
        })
        .join()
        .unwrap();
        let mut child = child;
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the child outlived the thread that started it");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
    }

    /// Run by [`a_child_dies_when_convert_is_sigtermed`] in a process of its
    /// own: start a niced child, say its pid, and wait to be killed.
    #[test]
    #[ignore = "a helper process for another test"]
    fn helper_starts_a_child_and_waits() {
        if std::env::var_os("DJ_CONVERT_HELPER").is_none() {
            return;
        }
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30").stdin(Stdio::null());
        let mut child = niced(&mut cmd).spawn().unwrap();
        println!("child pid {}", child.id());
        // Waited for by the thread that started it, as every real child is.
        let _ = child.wait();
    }

    #[test]
    fn a_child_dies_when_convert_is_sigtermed() {
        // The whole process this time, stopped the way the core stops an
        // app: SIGTERM, which Convert does not handle.
        use std::io::BufRead;
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::tests::helper_starts_a_child_and_waits",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("DJ_CONVERT_HELPER", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufReader::new(helper.stdout.take().unwrap()).lines();
        let pid: i32 = lines
            .by_ref()
            .map_while(Result::ok)
            // libtest prints the test's name and then, on the same line, what
            // the test printed.
            .find_map(|l| {
                l.split("child pid ")
                    .nth(1)
                    .map(|p| p.trim().parse().unwrap())
            })
            .expect("the helper said its child's pid");
        // SAFETY: kill(2) on the helper, a child of this test not yet reaped.
        unsafe {
            libc::kill(helper.id() as i32, libc::SIGTERM);
        }
        helper.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        // SAFETY: kill(2) with signal 0 only asks whether the pid exists.
        while unsafe { libc::kill(pid, 0) } == 0 {
            if Instant::now() >= deadline {
                // SAFETY: as above; the orphan is cleaned up before failing.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                panic!("the child (pid {pid}) outlived Convert");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
