//! The single place where pocketdock touches the system: running commands and
//! writing or removing files. Everything goes through [`Sys`] so that dry-run,
//! logging and error reporting behave the same everywhere.
//!
//! Two kinds of command exist:
//! - [`Sys::run`] changes system state. In dry-run mode it is recorded and
//!   printed, never executed.
//! - [`Sys::query`] only reads state (e.g. `ip -j addr`). It always executes,
//!   because dry-run plans depend on what is actually there.

use std::ffi::OsString;
use std::fmt;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SysError {
    #[error("could not run `{cmd}`")]
    Spawn {
        cmd: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{cmd}` failed ({status}){}", fmt_stderr(stderr))]
    Failed {
        cmd: String,
        status: String,
        stderr: String,
    },
    #[error("{action} {path}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn fmt_stderr(stderr: &str) -> String {
    let s = stderr.trim();
    if s.is_empty() {
        String::new()
    } else {
        format!(": {s}")
    }
}

impl SysError {
    /// The stderr of a failed command, if this is one.
    pub fn stderr(&self) -> Option<&str> {
        match self {
            SysError::Failed { stderr, .. } => Some(stderr),
            _ => None,
        }
    }
}

/// A command line. Displayed shell-quoted so dry-run output can be pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    program: String,
    args: Vec<String>,
    stdin: Option<String>,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
            stdin: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Data written to the command's stdin.
    pub fn stdin(mut self, data: impl Into<String>) -> Self {
        self.stdin = Some(data.into());
        self
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", shell_quote(&self.program))?;
        for a in &self.args {
            write!(f, " {}", shell_quote(a))?;
        }
        if self.stdin.is_some() {
            write!(f, " <<stdin")?;
        }
        Ok(())
    }
}

fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '@' | ',' | '+' | '%')
        });
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// One state-changing action, as recorded in the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Run(String),
    Write { path: PathBuf, mode: u32 },
    Remove(PathBuf),
    RemoveTree(PathBuf),
    Mkdir { path: PathBuf, mode: u32 },
    Chown { path: PathBuf, owner: String },
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Run(c) => write!(f, "run    {c}"),
            Action::Write { path, mode } => write!(f, "write  {} ({mode:04o})", path.display()),
            Action::Remove(p) => write!(f, "remove {}", p.display()),
            Action::RemoveTree(p) => write!(f, "rmtree {}", p.display()),
            Action::Mkdir { path, mode } => write!(f, "mkdir  {} ({mode:04o})", path.display()),
            Action::Chown { path, owner } => write!(f, "chown  {owner} {}", path.display()),
        }
    }
}

pub struct Sys {
    dry_run: bool,
    /// Print each action to stdout as it is planned (dry-run CLI use).
    echo: bool,
    plan: Mutex<Vec<Action>>,
}

impl Sys {
    pub fn real() -> Self {
        Sys {
            dry_run: false,
            echo: false,
            plan: Mutex::new(Vec::new()),
        }
    }

    pub fn dry_run() -> Self {
        Sys {
            dry_run: true,
            echo: true,
            plan: Mutex::new(Vec::new()),
        }
    }

    /// Dry-run that records the plan without printing it (for tests).
    pub fn recording() -> Self {
        Sys {
            dry_run: true,
            echo: false,
            plan: Mutex::new(Vec::new()),
        }
    }

    pub fn new(dry_run: bool) -> Self {
        if dry_run { Sys::dry_run() } else { Sys::real() }
    }

    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// The actions performed (or, in dry-run, that would have been).
    pub fn plan(&self) -> Vec<Action> {
        self.plan.lock().map(|p| p.clone()).unwrap_or_default()
    }

    fn record(&self, action: Action) {
        if self.echo {
            println!("[dry-run] {action}");
        }
        if let Ok(mut p) = self.plan.lock() {
            p.push(action);
        }
    }

    /// Run a state-changing command. Skipped (but recorded) in dry-run.
    pub fn run(&self, cmd: &Cmd) -> Result<Output, SysError> {
        self.record(Action::Run(cmd.to_string()));
        if self.dry_run {
            return Ok(empty_output());
        }
        exec(cmd)
    }

    /// Run a read-only command. Always executes, even in dry-run.
    pub fn query(&self, cmd: &Cmd) -> Result<Output, SysError> {
        exec(cmd)
    }

    /// `query` returning stdout as a string.
    pub fn query_stdout(&self, cmd: &Cmd) -> Result<String, SysError> {
        let out = self.query(cmd)?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Write a file atomically (temp file + rename) with the given mode.
    pub fn write_file(&self, path: &Path, contents: &str, mode: u32) -> Result<(), SysError> {
        self.record(Action::Write {
            path: path.to_path_buf(),
            mode,
        });
        if self.dry_run {
            return Ok(());
        }
        let io = |action, source| SysError::Io {
            action,
            path: path.to_path_buf(),
            source,
        };
        let dir = path.parent().unwrap_or(Path::new("/"));
        let mut tmp_name = OsString::from(".");
        tmp_name.push(path.file_name().unwrap_or_default());
        tmp_name.push(".pocketdock-tmp");
        let tmp = dir.join(tmp_name);
        // Start at 0600 so secrets are never briefly readable, then widen.
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| io("create", e))?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| io("chmod", e))?;
        f.write_all(contents.as_bytes())
            .map_err(|e| io("write", e))?;
        f.sync_all().map_err(|e| io("sync", e))?;
        std::fs::rename(&tmp, path).map_err(|e| io("rename into", e))
    }

    /// Remove a file. A missing file is not an error.
    pub fn remove_file(&self, path: &Path) -> Result<(), SysError> {
        self.record(Action::Remove(path.to_path_buf()));
        if self.dry_run {
            return Ok(());
        }
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(SysError::Io {
                action: "remove",
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Remove a directory tree. A missing directory is not an error.
    pub fn remove_dir_all(&self, path: &Path) -> Result<(), SysError> {
        self.record(Action::RemoveTree(path.to_path_buf()));
        if self.dry_run {
            return Ok(());
        }
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(SysError::Io {
                action: "remove",
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Create a directory (and parents) and set its mode.
    pub fn create_dir(&self, path: &Path, mode: u32) -> Result<(), SysError> {
        self.record(Action::Mkdir {
            path: path.to_path_buf(),
            mode,
        });
        if self.dry_run {
            return Ok(());
        }
        let io = |action, source| SysError::Io {
            action,
            path: path.to_path_buf(),
            source,
        };
        std::fs::create_dir_all(path).map_err(|e| io("create directory", e))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| io("chmod", e))
    }

    /// Change owner and group by name (via `chown`, so names resolve through NSS).
    pub fn chown(&self, path: &Path, owner: &str) -> Result<(), SysError> {
        self.record(Action::Chown {
            path: path.to_path_buf(),
            owner: owner.to_string(),
        });
        if self.dry_run {
            return Ok(());
        }
        exec(&Cmd::new("chown").arg(owner).arg(path.to_string_lossy()))?;
        Ok(())
    }
}

fn empty_output() -> Output {
    Output {
        status: std::process::ExitStatus::default(),
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

fn exec(cmd: &Cmd) -> Result<Output, SysError> {
    tracing::debug!(cmd = %cmd, "exec");
    let mut c = Command::new(&cmd.program);
    c.args(&cmd.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if cmd.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let spawn_err = |source| SysError::Spawn {
        cmd: cmd.to_string(),
        source,
    };
    let mut child = c.spawn().map_err(spawn_err)?;
    if let (Some(data), Some(mut stdin)) = (&cmd.stdin, child.stdin.take()) {
        stdin.write_all(data.as_bytes()).map_err(spawn_err)?;
    }
    let out = child.wait_with_output().map_err(spawn_err)?;
    if !out.status.success() {
        return Err(SysError::Failed {
            cmd: cmd.to_string(),
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        let c = Cmd::new("nft").args(["add", "element", "inet pocketdock", "it's"]);
        assert_eq!(
            c.to_string(),
            r"nft add element 'inet pocketdock' 'it'\''s'"
        );
    }

    #[test]
    fn dry_run_records_and_does_not_execute() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.conf");
        let sys = Sys::recording();
        sys.run(&Cmd::new("false")).unwrap();
        sys.write_file(&path, "hello", 0o600).unwrap();
        sys.remove_file(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(
            sys.plan(),
            vec![
                Action::Run("false".into()),
                Action::Write {
                    path: path.clone(),
                    mode: 0o600
                },
                Action::Remove(path),
            ]
        );
    }

    #[test]
    fn query_runs_even_in_dry_run() {
        let sys = Sys::recording();
        let out = sys.query_stdout(&Cmd::new("echo").arg("hi")).unwrap();
        assert_eq!(out, "hi\n");
        assert!(sys.plan().is_empty());
    }

    #[test]
    fn failure_carries_stderr() {
        let sys = Sys::real();
        let err = sys
            .run(&Cmd::new("sh").args(["-c", "echo boom >&2; exit 3"]))
            .unwrap_err();
        assert_eq!(err.stderr(), Some("boom\n"));
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn stdin_is_passed() {
        let sys = Sys::real();
        let out = sys.run(&Cmd::new("cat").stdin("piped")).unwrap();
        assert_eq!(out.stdout, b"piped");
    }

    #[test]
    fn real_write_sets_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        Sys::real().write_file(&path, "x", 0o600).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
    }
}
