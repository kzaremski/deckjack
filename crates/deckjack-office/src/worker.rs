//! The LibreOffice worker: one process per job.
//!
//! The app re-executes its own binary as `deckjack-office --worker <dir>`.
//! `<dir>` holds `job.json`; the worker writes `result.json` beside it and
//! exits. Everything LibreOffice does happens in that child, so:
//!
//! - a document that crashes the engine kills one job, not the app;
//! - the limits the sandbox does not set (it bounds neither memory nor CPU,
//!   `sandbox.rs` module docs) are set here, with `setrlimit`, before the
//!   library is even loaded;
//! - a cancel or a timeout is `SIGKILL` to the worker's process group, which
//!   is the only stop LibreOffice reliably obeys.
//!
//! The child inherits the app's Landlock and seccomp confinement (a confined
//! process cannot shed it), so the engine reads only what the app could.
//!
//! The source document is copied into the job directory first and the copy is
//! loaded. LibreOffice writes a `.~lock.<name>#` file beside whatever it
//! opens, and beside the operator's file that is litter at best and a refused
//! write at worst.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;
use tokio::sync::{Semaphore, watch};

use crate::lok;

/// The flag that turns this binary into a worker.
pub const WORKER_FLAG: &str = "--worker";

/// The worker's environment, beyond its private `HOME` and `TMPDIR`.
///
/// Landlock (ABI 4) does not govern `connect()` to a pathname Unix socket,
/// and a confined LibreOffice was measured connecting to CUPS, the D-Bus
/// system bus and systemd-resolved (docs/lanes/6-office.md, S1). The two
/// that a client library reads an address for from the environment are
/// pointed at a path that does not exist, so the connect fails before it
/// starts. Measured with strace, that removed every one of those connects,
/// the resolver's included (why that one went too was not traced).
/// It is a mitigation for this app only: Landlock still does not govern the
/// sockets, so any other app can reach them (a platform gap).
pub const WORKER_ENV: &[(&str, &str)] = &[
    // Headless rendering, no display connection attempted.
    ("SAL_USE_VCLPLUGIN", "svp"),
    // Belt to the profile's `UseDocumentOOoLockfile=false`.
    ("SAL_ENABLE_FILE_LOCKING", "0"),
    ("CUPS_SERVER", "/nonexistent/deckjack-office/cups.sock"),
    (
        "DBUS_SYSTEM_BUS_ADDRESS",
        "unix:path=/nonexistent/deckjack-office/system_bus_socket",
    ),
];

const JOB_FILE: &str = "job.json";
const RESULT_FILE: &str = "result.json";

/// What one worker is asked to do. Written by the app, read by the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    /// LibreOffice's program directory.
    pub install: PathBuf,
    /// The user installation (profile) directory, already seeded.
    pub profile: PathBuf,
    pub input: PathBuf,
    pub output: PathBuf,
    /// A LibreOffice short format name: `pdf`.
    pub format: String,
    /// `RLIMIT_AS`, bytes.
    pub address_space: u64,
    /// `RLIMIT_DATA`, bytes.
    pub data: u64,
    /// `RLIMIT_CPU`, seconds.
    pub cpu_seconds: u64,
}

/// What the worker reports back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// LibreOffice's version JSON, when it booted.
    #[serde(default)]
    pub version: Option<String>,
    /// Milliseconds from boot to saved.
    #[serde(default)]
    pub millis: u64,
    /// The worker's peak resident set, KiB, as the kernel counted it.
    #[serde(default)]
    pub peak_rss_kib: u64,
}

// ---------------------------------------------------------------------------
// The worker's side
// ---------------------------------------------------------------------------

/// The worker's whole life. Returns the process exit code.
pub fn worker_main(dir: &Path) -> i32 {
    let result = match run_job(dir) {
        Ok(result) => result,
        Err(error) => JobResult {
            ok: false,
            error: Some(error),
            version: None,
            millis: 0,
            peak_rss_kib: 0,
        },
    };
    let mut result = result;
    result.peak_rss_kib = peak_rss_kib();
    let ok = result.ok;
    let written = serde_json::to_vec(&result)
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            let tmp = dir.join(format!("{RESULT_FILE}.tmp"));
            std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, dir.join(RESULT_FILE)).map_err(|e| e.to_string())
        });
    let code = if written.is_ok() && ok { 0 } else { 1 };
    // `_exit`, not `exit`: LibreOffice's static destructors and atexit
    // handlers are where a headless office hangs on the way out, and the job
    // is done.
    // SAFETY: terminates the process; nothing after it runs.
    unsafe { libc::_exit(code) }
}

fn run_job(dir: &Path) -> Result<JobResult, String> {
    let spec: JobSpec = serde_json::from_slice(
        &std::fs::read(dir.join(JOB_FILE)).map_err(|e| format!("cannot read the job: {e}"))?,
    )
    .map_err(|e| format!("the job is not valid: {e}"))?;

    limit(libc::RLIMIT_AS, spec.address_space)?;
    limit(libc::RLIMIT_DATA, spec.data)?;
    limit(libc::RLIMIT_CPU, spec.cpu_seconds)?;
    // A crashing engine must not leave a core file of a confidential
    // document behind.
    limit(libc::RLIMIT_CORE, 0)?;

    let started = Instant::now();
    let office = lok::Lok::boot(&spec.install, &spec.profile).map_err(|e| e.to_string())?;
    let version = office.version_info();
    let doc = office.open(&spec.input).map_err(|e| e.to_string())?;
    doc.save_as(&spec.output, &spec.format)
        .map_err(|e| e.to_string())?;
    drop(doc);
    Ok(JobResult {
        ok: true,
        error: None,
        version,
        millis: started.elapsed().as_millis() as u64,
        peak_rss_kib: 0,
    })
}

fn peak_rss_kib() -> u64 {
    // SAFETY: getrusage fills a zeroed struct it is handed.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
        usage.ru_maxrss.max(0) as u64
    } else {
        0
    }
}

fn limit(resource: libc::__rlimit_resource_t, value: u64) -> Result<(), String> {
    let rl = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: plain syscall on a stack value.
    if unsafe { libc::setrlimit(resource, &rl) } != 0 {
        return Err(format!(
            "cannot set resource limit {resource}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The app's side
// ---------------------------------------------------------------------------

/// Bounds on one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub timeout: Duration,
    pub address_space: u64,
    pub data: u64,
    pub cpu_seconds: u64,
}

impl Default for Limits {
    /// Measured on LibreOffice 26.8 (docs/lanes/6-office.md, S1): a converted
    /// docx peaks near 0.4 GiB resident, but the engine reserves several GiB
    /// of address space it never touches (it does not boot under 9 GiB of
    /// `RLIMIT_AS` on a 24-core machine). So the memory cap is `data`
    /// (`RLIMIT_DATA`, which a toy docx needs about 512 MiB of), and
    /// `address_space` is only a loose backstop: tightening it breaks every
    /// conversion before it bounds anything. A hostile document that asks
    /// for more data fails its allocation and the job reports it.
    fn default() -> Limits {
        Limits {
            timeout: Duration::from_secs(120),
            address_space: 16 << 30,
            data: 2 << 30,
            cpu_seconds: 120,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("the conversion was cancelled")]
    Cancelled,

    #[error("LibreOffice took longer than {0:?} and was stopped")]
    TimedOut(Duration),

    #[error("{0}")]
    Failed(String),

    #[error("could not prepare the job: {0}")]
    Io(#[from] std::io::Error),
}

/// Where workers run and what they run with.
pub struct Engine {
    /// LibreOffice's program directory.
    pub install: PathBuf,
    /// This binary, re-executed as the worker.
    pub exe: PathBuf,
    /// The app's private `HOME`; profiles live under it.
    pub home: PathBuf,
    /// The app's private `TMPDIR`; job directories live under it.
    pub tmp: PathBuf,
    pub limits: Limits,
    /// Concurrent workers. Each holds a whole office (about 0.4 GiB).
    permits: Semaphore,
    /// Free profile slots. A LibreOffice profile is single-writer, so two
    /// concurrent workers never share one.
    slots: Mutex<Vec<usize>>,
}

/// How many workers may run at once. The brief's figure for the R620.
pub const MAX_WORKERS: usize = 2;

static NEXT_JOB: AtomicU64 = AtomicU64::new(0);

impl Engine {
    pub fn new(
        install: PathBuf,
        exe: PathBuf,
        home: PathBuf,
        tmp: PathBuf,
        limits: Limits,
    ) -> Engine {
        Engine {
            install,
            exe,
            home,
            tmp,
            limits,
            permits: Semaphore::new(MAX_WORKERS),
            slots: Mutex::new((0..MAX_WORKERS).rev().collect()),
        }
    }

    /// Whether LibreOffice is installed where this engine looks.
    pub fn available(&self) -> bool {
        self.install.join(lok::LIBRARY).is_file()
    }

    /// Convert `source` to `format`, writing `dest`. `dest` is replaced if it
    /// exists: it is always a path in the app's own cache or job space, never
    /// the operator's.
    pub async fn convert(
        &self,
        source: &Path,
        format: &str,
        dest: &Path,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<JobResult, JobError> {
        if !self.available() {
            return Err(JobError::Failed(format!(
                "LibreOffice is not installed on this node ({} is missing). Office needs it \
                 to show pages and export; install the libreoffice package, or open this from \
                 a node that has it.",
                self.install.join(lok::LIBRARY).display()
            )));
        }
        let _permit = tokio::select! {
            permit = self.permits.acquire() => permit.map_err(|_| JobError::Cancelled)?,
            _ = wait_cancelled(&mut cancel) => return Err(JobError::Cancelled),
        };
        let slot = self.take_slot();
        let result = self.run(slot.0, source, format, dest, cancel).await;
        drop(slot);
        result
    }

    fn take_slot(&self) -> Slot<'_> {
        let index = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            // The semaphore admits at most MAX_WORKERS, so a slot is always
            // free; a fallback that cannot collide keeps a bug from becoming
            // a shared profile.
            .unwrap_or(MAX_WORKERS + NEXT_JOB.load(Ordering::Relaxed) as usize);
        Slot(index, self)
    }

    async fn run(
        &self,
        slot: usize,
        source: &Path,
        format: &str,
        dest: &Path,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<JobResult, JobError> {
        let profile = self.home.join("lok").join(format!("profile-{slot}"));
        seed_profile(&profile)?;

        let job = JobDir::create(&self.tmp)?;
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| e.chars().all(|c| c.is_ascii_alphanumeric()) && e.len() <= 8)
            .unwrap_or("bin")
            .to_ascii_lowercase();
        let input = job.path.join(format!("input.{ext}"));
        std::fs::copy(source, &input)?;
        let output = job.path.join(format!("output.{format}"));
        let spec = JobSpec {
            install: self.install.clone(),
            profile,
            input,
            output: output.clone(),
            format: format.to_string(),
            address_space: self.limits.address_space,
            data: self.limits.data,
            cpu_seconds: self.limits.cpu_seconds,
        };
        std::fs::write(
            job.path.join(JOB_FILE),
            serde_json::to_vec(&spec).map_err(std::io::Error::other)?,
        )?;

        let mut command = tokio::process::Command::new(&self.exe);
        command
            .arg(WORKER_FLAG)
            .arg(&job.path)
            .env_clear()
            .env("HOME", &self.home)
            .env("TMPDIR", &self.tmp)
            .envs(WORKER_ENV.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Its own process group, so a kill reaches anything the engine
            // started under it.
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let pid = child.id();
        let group = KillGroup(pid);

        let mut stderr = child.stderr.take();
        let tail = tokio::spawn(async move {
            let mut kept = Vec::new();
            if let Some(stream) = stderr.as_mut() {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    kept.extend_from_slice(&buf[..n]);
                    // The last 8 KiB is what explains a failure.
                    if kept.len() > 16 * 1024 {
                        kept.drain(..kept.len() - 8 * 1024);
                    }
                }
            }
            kept
        });

        let status = tokio::select! {
            status = child.wait() => status?,
            _ = tokio::time::sleep(self.limits.timeout) => {
                group.kill();
                let _ = child.wait().await;
                return Err(JobError::TimedOut(self.limits.timeout));
            }
            _ = wait_cancelled(&mut cancel) => {
                group.kill();
                let _ = child.wait().await;
                return Err(JobError::Cancelled);
            }
        };
        // Exited by itself; anything it left in its group goes too.
        group.kill();
        let stderr_tail = tail.await.unwrap_or_default();

        let result: Option<JobResult> = std::fs::read(job.path.join(RESULT_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        match result {
            Some(result) if result.ok && status.success() && output.is_file() => {
                std::fs::rename(&output, dest)
                    .or_else(|_| std::fs::copy(&output, dest).map(|_| ()))?;
                Ok(result)
            }
            Some(result) => {
                Err(JobError::Failed(result.error.unwrap_or_else(|| {
                    "LibreOffice reported success but wrote nothing".into()
                })))
            }
            None => Err(JobError::Failed(describe_death(status, &stderr_tail))),
        }
    }
}

/// Returns a profile slot when the job ends, however it ends.
struct Slot<'a>(usize, &'a Engine);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        if self.0 < MAX_WORKERS {
            self.1
                .slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(self.0);
        }
    }
}

/// `SIGKILL` to a worker's process group. Also on drop, so a handler future
/// dropped mid-job (the core gave up on it) does not leave an office running.
struct KillGroup(Option<u32>);

impl KillGroup {
    fn kill(&self) {
        if let Some(pid) = self.0 {
            // SAFETY: plain syscall. The group id is the worker's own pid
            // (process_group(0)), captured at spawn, so this never names a
            // group this app did not create.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

impl Drop for KillGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A job's private directory, removed when the job ends.
struct JobDir {
    path: PathBuf,
}

impl JobDir {
    fn create(tmp: &Path) -> std::io::Result<JobDir> {
        let jobs = tmp.join("jobs");
        std::fs::create_dir_all(&jobs)?;
        let n = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
        let path = jobs.join(format!("job-{}-{n}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(JobDir { path })
    }
}

impl Drop for JobDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

async fn wait_cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            // The sender is gone: nobody can cancel any more.
            std::future::pending::<()>().await;
        }
    }
}

fn describe_death(status: std::process::ExitStatus, stderr: &[u8]) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    let how = match (status.code(), status.signal()) {
        (_, Some(libc::SIGXCPU)) => "it used up its CPU allowance".to_string(),
        (_, Some(libc::SIGKILL)) => "it was killed".to_string(),
        (_, Some(libc::SIGSEGV)) | (_, Some(libc::SIGABRT)) => {
            "it crashed (often a memory limit reached, or a document the engine cannot handle)"
                .to_string()
        }
        (_, Some(signal)) => format!("it died on signal {signal}"),
        (Some(code), None) => format!("it exited {code} without a result"),
        (None, None) => "it ended without a result".to_string(),
    };
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        format!("LibreOffice failed: {how}")
    } else {
        let last: String = detail.lines().rev().take(3).collect::<Vec<_>>().join(" / ");
        format!("LibreOffice failed: {how} ({last})")
    }
}

// ---------------------------------------------------------------------------
// The profile
// ---------------------------------------------------------------------------

/// The settings written into every worker profile before LibreOffice reads
/// it. Rewritten whole on every job, so nothing the engine recorded last time
/// (its recent-documents list included) survives into the next.
///
/// - Macros: never run. `DisableMacrosExecution` is the switch; the security
///   level at its maximum is the second belt. The CVE-2024-12425/12426 class
///   is about exactly this headless use.
/// - Java: off. Nothing in a conversion needs it, and it starts a JVM.
/// - Lock files: off (`UseDocumentOOoLockfile`, `UseDocumentSystemFileLocking`).
/// - Update checks and crash reports: off. There is no network anyway; this
///   keeps the engine from trying.
/// - Links to untrusted referers: blocked (`BlockUntrustedRefererLinks`).
pub const REGISTRY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<oor:items xmlns:oor="http://openoffice.org/2001/registry" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
<item oor:path="/org.openoffice.Office.Common/Security/Scripting"><prop oor:name="DisableMacrosExecution" oor:op="fuse"><value>true</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Security/Scripting"><prop oor:name="MacroSecurityLevel" oor:op="fuse"><value>3</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Security/Scripting"><prop oor:name="BlockUntrustedRefererLinks" oor:op="fuse"><value>true</value></prop></item>
<item oor:path="/org.openoffice.Office.Java/VirtualMachine"><prop oor:name="Enable" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Misc"><prop oor:name="UseDocumentOOoLockfile" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Misc"><prop oor:name="UseDocumentSystemFileLocking" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Misc"><prop oor:name="FirstRun" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Misc"><prop oor:name="CrashReport" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Jobs/Jobs/org.openoffice.Office.Jobs:Job['UpdateCheck']/Arguments"><prop oor:name="AutoCheckEnabled" oor:op="fuse"><value>false</value></prop></item>
<item oor:path="/org.openoffice.Office.Common/Misc"><prop oor:name="ShowTipOfTheDay" oor:op="fuse"><value>false</value></prop></item>
</oor:items>
"#;

/// Write the profile's settings, and clear a lock a killed worker left.
pub fn seed_profile(profile: &Path) -> std::io::Result<()> {
    let user = profile.join("user");
    std::fs::create_dir_all(&user)?;
    std::fs::write(user.join("registrymodifications.xcu"), REGISTRY)?;
    // A worker killed mid-job leaves `.lock`; the slot is exclusively ours
    // now, so it is stale by construction.
    match std::fs::remove_file(user.join(".lock")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    Ok(())
}

/// A key that changes when the installed engine does: a render made by one
/// LibreOffice build is not what another would make.
pub fn engine_version(install: &Path) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    if let Ok(mut file) = std::fs::File::open(install.join("versionrc")) {
        let mut text = Vec::new();
        let _ = file.read_to_end(&mut text);
        hash.update(&text);
    }
    if let Ok(meta) = std::fs::metadata(install.join(lok::LIBRARY)) {
        use std::os::unix::fs::MetadataExt as _;
        hash.update(meta.len().to_le_bytes());
        hash.update(meta.mtime().to_le_bytes());
    }
    let digest = hash.finalize();
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_a_profile_turns_macros_and_java_off_and_clears_a_stale_lock() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("p");
        std::fs::create_dir_all(profile.join("user")).unwrap();
        std::fs::write(profile.join("user/.lock"), "stale").unwrap();
        std::fs::write(
            profile.join("user/registrymodifications.xcu"),
            "<recent>Secret plan.docx</recent>",
        )
        .unwrap();

        seed_profile(&profile).unwrap();

        let text = std::fs::read_to_string(profile.join("user/registrymodifications.xcu")).unwrap();
        assert!(
            !text.contains("Secret plan"),
            "the engine's own record survived"
        );
        for setting in [
            r#"<prop oor:name="DisableMacrosExecution" oor:op="fuse"><value>true</value>"#,
            r#"<item oor:path="/org.openoffice.Office.Java/VirtualMachine"><prop oor:name="Enable" oor:op="fuse"><value>false</value>"#,
            r#"<prop oor:name="UseDocumentOOoLockfile" oor:op="fuse"><value>false</value>"#,
        ] {
            assert!(text.contains(setting), "missing {setting}");
        }
        assert!(!profile.join("user/.lock").exists());
    }

    #[test]
    fn the_worker_points_cups_and_the_system_bus_at_nothing() {
        // S1 measured a confined LibreOffice connecting to both; these two
        // variables are what stopped it (strace, docs/lanes/6-office.md).
        let env: std::collections::BTreeMap<&str, &str> = WORKER_ENV.iter().copied().collect();
        for key in ["CUPS_SERVER", "DBUS_SYSTEM_BUS_ADDRESS"] {
            let value = env.get(key).unwrap_or_else(|| panic!("{key} is not set"));
            let path = value.trim_start_matches("unix:path=");
            assert!(path.starts_with("/nonexistent/"), "{key}={value}");
            assert!(!std::path::Path::new(path).exists());
        }
        assert_eq!(env.get("SAL_USE_VCLPLUGIN"), Some(&"svp"));
    }

    #[test]
    fn a_worker_death_is_described_by_its_cause() {
        use std::os::unix::process::ExitStatusExt as _;
        let cpu = describe_death(std::process::ExitStatus::from_raw(libc::SIGXCPU), b"");
        assert!(cpu.contains("CPU"), "{cpu}");
        let crashed = describe_death(
            std::process::ExitStatus::from_raw(libc::SIGSEGV),
            b"first\nsecond\nwarn: out of memory\n",
        );
        assert!(
            crashed.contains("crashed") && crashed.contains("out of memory"),
            "{crashed}"
        );
    }
}
