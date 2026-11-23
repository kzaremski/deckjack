//! One ffmpeg run that writes a file: started niced, watched through its
//! progress stream, and stopped when its [`Cancellation`] says so.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use deckjack_app::Cancellation;

use crate::progress::{Parser, Tick};
use crate::tools::{self, Tools};

/// How long ffmpeg is given to stop on SIGTERM before it is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// How a run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    Done,
    Cancelled,
    /// A sentence: what ffmpeg said last, or how it died.
    Failed(String),
}

/// Run ffmpeg with `args` (from [`crate::presets::argv`]) and call
/// `on_tick` for every progress block, until it exits or `cancel` fires;
/// stopped and failed if it has not finished within `limit`: a
/// sample encode for an estimate or a still, where a hostile file that makes
/// ffmpeg spin must not hold a read-risk command (and its CPU) until a caller
/// thinks to cancel it.
pub fn run_within(
    tools: &Tools,
    args: &[String],
    cancel: &Cancellation,
    limit: Duration,
    on_tick: impl FnMut(&Tick),
) -> Ended {
    run_with_pid(tools, args, cancel, Some(limit), |_| {}, on_tick)
}

/// [`run_within`], told the child's pid once it has started, and with an
/// optional wall-clock limit (a queued job has none: an hour is normal).
pub fn run_with_pid(
    tools: &Tools,
    args: &[String],
    cancel: &Cancellation,
    limit: Option<Duration>,
    on_start: impl FnOnce(u32),
    mut on_tick: impl FnMut(&Tick),
) -> Ended {
    let deadline = limit.map(|l| Instant::now() + l);
    let mut cmd = Command::new(&tools.ffmpeg);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    tools::niced(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Ended::Failed(format!("ffmpeg could not be started: {e}")),
    };
    on_start(child.id());
    let (ticks, heard) = mpsc::channel::<Tick>();
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let Some(stdout) = stdout else { return };
        let mut parser = Parser::default();
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(tick) = parser.line(&line)
                && ticks.send(tick).is_err()
            {
                break;
            }
        }
    });
    let mut stderr = child.stderr.take();
    let errors = std::thread::spawn(move || {
        // The tail is all a sentence needs; a chatty failure is not kept
        // whole in memory.
        let mut tail = Vec::new();
        if let Some(stderr) = stderr.as_mut() {
            let mut buf = [0u8; 8192];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > 64 * 1024 {
                    tail.drain(..tail.len() - 16 * 1024);
                }
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });

    let status = loop {
        match heard.recv_timeout(Duration::from_millis(100)) {
            Ok(tick) => on_tick(&tick),
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
        }
        if cancel.is_cancelled() {
            tools::stop(&mut child, STOP_GRACE);
            let _ = reader.join();
            let _ = errors.join();
            return Ended::Cancelled;
        }
        if let (Some(deadline), Some(limit)) = (deadline, limit)
            && Instant::now() >= deadline
        {
            tools::stop(&mut child, STOP_GRACE);
            let _ = reader.join();
            let _ = errors.join();
            return Ended::Failed(format!(
                "ffmpeg did not finish within {} s and was stopped",
                limit.as_secs()
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => return Ended::Failed(format!("could not wait for ffmpeg: {e}")),
        }
    };
    let _ = reader.join();
    // Ticks still queued when it exited.
    while let Ok(tick) = heard.try_recv() {
        on_tick(&tick);
    }
    let stderr = errors.join().unwrap_or_default();
    if status.success() {
        return Ended::Done;
    }
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        return Ended::Failed(format!(
            "ffmpeg was killed (signal {signal}) before it finished{}",
            if signal == libc::SIGKILL {
                "; the system may have run out of memory"
            } else {
                ""
            }
        ));
    }
    Ended::Failed(match tools::last_words(&stderr) {
        Some(said) => format!("ffmpeg stopped: {said}"),
        None => format!(
            "ffmpeg exited with status {} and said nothing",
            status.code().unwrap_or(-1)
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_past_its_limit_is_stopped_and_said_so() {
        // A stand-in "ffmpeg" that spins, as a hostile file might make the
        // real one.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("spin");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tools = Tools {
            ffmpeg: script.clone(),
            ffprobe: script,
            threads: 1,
        };
        let started = Instant::now();
        let ended = run_within(
            &tools,
            &[],
            &Cancellation::new(),
            Duration::from_millis(300),
            |_| {},
        );
        let took = started.elapsed();
        assert!(
            matches!(&ended, Ended::Failed(e) if e.contains("did not finish within")),
            "{ended:?}"
        );
        assert!(took < Duration::from_secs(10), "{took:?}");
    }
}
