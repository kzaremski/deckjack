//! The compositor's end of the link to the core.
//!
//! One Unix socket, framed as `deckjack_protocol::stream` describes. A
//! reader thread turns the core's frames into [`StreamCommand`]s for the
//! event loop; a writer thread owns the write half, so neither the event
//! loop nor an encoder thread ever blocks on the socket.
//!
//! Control frames are never dropped. Pictures are, when the writer is
//! behind (a slow viewer, a busy core): the window is then marked as owing
//! a keyframe, because the pictures after a dropped one reference it.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use deckjack_protocol::stream::{
    KIND_CONTROL, KIND_VIDEO, StreamCommand, StreamEvent, check_link_len, link_frame,
};

/// Pictures queued for the socket before new ones are dropped. A few
/// frames: enough to ride out a scheduling hiccup, few enough that a viewer
/// is never shown something seconds old.
pub const VIDEO_QUEUE: usize = 8;

/// Read one frame: `(kind, payload)`. `Ok(None)` at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = check_link_len(u32::from_le_bytes(len))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let kind = body[0];
    body.remove(0);
    Ok(Some((kind, body)))
}

/// Control frames queued before [`LinkSender::event`] waits for the writer.
const CONTROL_QUEUE: usize = 256;

/// One frame for the writer, and whether it is a picture (counted against
/// [`VIDEO_QUEUE`]).
struct Out {
    frame: Vec<u8>,
    video: bool,
}

/// The writer's one queue, in order: control frames are never dropped (a
/// sender waits for room), pictures are once [`VIDEO_QUEUE`] of them are
/// waiting. One queue rather than two so the writer can sleep in a
/// blocking receive instead of polling, and so a window's `Mapped` always
/// goes out before its first picture.
#[derive(Clone)]
pub struct LinkSender {
    tx: SyncSender<Out>,
    videos: Arc<AtomicUsize>,
}

impl LinkSender {
    fn new() -> (LinkSender, Receiver<Out>) {
        let (tx, rx) = sync_channel::<Out>(CONTROL_QUEUE + VIDEO_QUEUE);
        (
            LinkSender {
                tx,
                videos: Arc::new(AtomicUsize::new(0)),
            },
            rx,
        )
    }

    pub fn event(&self, event: &StreamEvent) {
        match serde_json::to_vec(event) {
            Ok(json) => {
                let _ = self.tx.send(Out {
                    frame: link_frame(KIND_CONTROL, &json),
                    video: false,
                });
            }
            Err(e) => tracing::error!(error = %e, "cannot encode a stream event"),
        }
    }

    /// Queue a picture. False when it was dropped, in which case the
    /// caller owes the viewer a keyframe.
    pub fn video(&self, link_payload: &[u8]) -> bool {
        if self.videos.fetch_add(1, Ordering::SeqCst) >= VIDEO_QUEUE {
            self.videos.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        let out = Out {
            frame: link_frame(KIND_VIDEO, link_payload),
            video: true,
        };
        match self.tx.try_send(out) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.videos.fetch_sub(1, Ordering::SeqCst);
                false
            }
        }
    }
}

/// Start the writer thread over `stream`.
pub fn spawn_writer(stream: UnixStream, gone: Arc<AtomicBool>) -> io::Result<LinkSender> {
    let (sender, rx) = LinkSender::new();
    let videos = Arc::clone(&sender.videos);
    std::thread::Builder::new()
        .name("link-writer".into())
        .spawn(move || write_loop(stream, rx, videos, gone))?;
    Ok(sender)
}

fn write_loop(
    mut stream: UnixStream,
    rx: Receiver<Out>,
    videos: Arc<AtomicUsize>,
    gone: Arc<AtomicBool>,
) {
    // Blocks until there is something to write, and ends when every sender
    // (the event loop and the encoders) is gone.
    while let Ok(out) = rx.recv() {
        let written = stream.write_all(&out.frame);
        if out.video {
            videos.fetch_sub(1, Ordering::SeqCst);
        }
        if written.is_err() {
            gone.store(true, Ordering::SeqCst);
            return;
        }
    }
    let _ = stream.flush();
}

/// Start the reader thread: each command from the core is passed to
/// `deliver`; at the end of the stream (the core went away) `deliver` gets
/// `None` once and the thread ends.
pub fn spawn_reader(
    mut stream: UnixStream,
    mut deliver: impl FnMut(Option<StreamCommand>) + Send + 'static,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("link-reader".into())
        .spawn(move || {
            loop {
                match read_frame(&mut stream) {
                    Ok(Some((KIND_CONTROL, body))) => {
                        match serde_json::from_slice::<StreamCommand>(&body) {
                            Ok(command) => deliver(Some(command)),
                            Err(e) => tracing::warn!(error = %e, "ignoring a malformed command"),
                        }
                    }
                    Ok(Some((kind, _))) => {
                        tracing::warn!(kind, "ignoring a link frame of unknown kind")
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "the link to the core failed");
                        break;
                    }
                }
            }
            deliver(None);
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_a_hostile_length_is_refused() {
        let mut bytes = link_frame(KIND_CONTROL, br#"{"command":"keyframe","surface":3}"#);
        bytes.extend(link_frame(KIND_VIDEO, &[1, 2, 3]));
        let mut r = &bytes[..];
        let (kind, body) = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(kind, KIND_CONTROL);
        let command: StreamCommand = serde_json::from_slice(&body).unwrap();
        assert_eq!(command, StreamCommand::Keyframe { surface: 3 });
        assert_eq!(
            read_frame(&mut r).unwrap().unwrap(),
            (KIND_VIDEO, vec![1, 2, 3])
        );
        assert!(read_frame(&mut r).unwrap().is_none());

        let hostile = u32::MAX.to_le_bytes();
        let err = read_frame(&mut &hostile[..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// When the socket is not read, pictures stop being queued (and the
    /// caller is told); a control frame is still queued behind them, and
    /// when even the control room is full, a control frame waits for the
    /// writer rather than being dropped.
    #[test]
    fn a_full_video_queue_drops_pictures_but_never_a_control_frame() {
        let (sender, rx) = LinkSender::new();
        for _ in 0..VIDEO_QUEUE {
            assert!(sender.video(&[0; 16]));
        }
        assert!(
            !sender.video(&[0; 16]),
            "a picture past the queue must be refused"
        );
        sender.event(&StreamEvent::Unmapped { surface: 3 });
        let frames: Vec<Out> = rx.try_iter().collect();
        assert_eq!(frames.len(), VIDEO_QUEUE + 1);
        assert!(
            !frames[VIDEO_QUEUE].video,
            "the control frame was not queued"
        );

        // Fill the whole queue with control frames: the next one waits.
        let (sender, rx) = LinkSender::new();
        for _ in 0..CONTROL_QUEUE + VIDEO_QUEUE {
            sender.event(&StreamEvent::Unmapped { surface: 1 });
        }
        let late = sender.clone();
        let waiting = std::thread::spawn(move || {
            late.event(&StreamEvent::Unmapped { surface: 2 });
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !waiting.is_finished(),
            "a control frame was dropped, not queued"
        );
        assert!(rx.recv().is_ok());
        waiting.join().unwrap();
        let last = rx.try_iter().last().expect("the late control frame");
        assert!(String::from_utf8_lossy(&last.frame).contains("\"surface\":2"));
    }

    /// A picture written to the socket frees its place in the queue.
    #[test]
    fn a_written_picture_frees_its_place() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let sender = spawn_writer(ours, Arc::new(AtomicBool::new(false))).unwrap();
        for round in 0..3 {
            for _ in 0..VIDEO_QUEUE {
                assert!(sender.video(&[round; 16]), "round {round}");
            }
            for _ in 0..VIDEO_QUEUE {
                assert_eq!(read_frame(&mut theirs).unwrap().unwrap().0, KIND_VIDEO);
            }
            // The writer decrements after the write returns.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while sender.videos.load(Ordering::SeqCst) != 0 {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
        }
    }
}
