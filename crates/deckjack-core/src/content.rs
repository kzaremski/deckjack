//! The content route: working-directory bytes, over HTTP, with range requests.
//!
//! # Why this exists
//!
//! Until this module the only way a file's content reached the browser was
//! `files.read`, which returns it inside a JSON command response — base64
//! when the bytes are not valid UTF-8 — capped at
//! [`crate::files::DEFAULT_MAX_READ`]. That is right for a README and wrong
//! for everything else: a photograph arrives a third larger than it is, a
//! video cannot arrive at all, and nothing can be seeked. `files.rs` has said
//! so beside the cap for as long as the cap has existed — *"anything bigger
//! than this should be streamed from [`crate::files::Resolved::real_path`]
//! instead of buffered"*.
//!
//! So: a route that hands the browser the file and lets the browser do what
//! it is already good at. An `<img>` decodes its own JPEG, a `<video>` seeks
//! by sending `Range`, and a PDF opens in the viewer the browser ships. That
//! is the whole of Quick Look's rendering, and none of it needs a decoder of
//! ours.
//!
//! # Why serving the operator's own files is the sharpest edge in the product
//!
//! Everything else on this origin is content Deckjack wrote. This is content
//! the operator's working directory happens to contain, which is to say
//! content that arrived by download, email, sync or an agent — and it is
//! served from the same origin as the shell, to a browser carrying the
//! session cookie. An HTML file served inline with no policy would run its
//! script as the operator.
//!
//! Three headers answer that, and all three are asserted in
//! `tests/content.rs`:
//!
//! - `Content-Security-Policy: sandbox`, with **no** allow tokens, which puts
//!   the response in an opaque origin with scripting off. It is the whole
//!   reason an HTML or SVG preview is safe to show at all.
//! - `X-Content-Type-Options: nosniff`, so a file served as `text/plain`
//!   cannot be re-interpreted as HTML by a browser that disagrees.
//! - `Cache-Control: private`, so no shared cache is authorized to keep a
//!   payslip. Never `public`, whatever the file.
//!
//! The path is resolved by [`crate::files::Namespace`] and nothing else, so
//! the working-directory boundary and the `.deckjack/` denial are enforced by
//! the code that already enforces them for every other reader. A second
//! resolver here would be a second place for those rules to drift, which is
//! how the predecessor project ended up with a decorative root.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

/// Where the route lives. Behind the session gate, like everything that reads
/// the working directory.
pub const MOUNT: &str = "/content";

/// How much of a file is read per piece of a streamed body.
///
/// Not a limit on what can be *served*. The body is streamed in pieces this
/// size, so one request for a 4 GB file holds a quarter of a megabyte at a
/// time and the socket's back-pressure decides how fast the rest is read.
///
/// It used to be a cap on the whole answer: the route read one 8 MiB window
/// and sent it with a 200, or with a `Content-Range` that still claimed the
/// whole span. So any file larger than the window arrived truncated while
/// looking complete, and a `<video>`'s opening `Range: bytes=0-` got a header
/// that disagreed with its own body.
const PIECE: u64 = 256 * 1024;

#[derive(Clone)]
struct Ctx {
    http: Arc<crate::http::Ctx>,
    files: Arc<crate::files::Namespace>,
}

#[derive(Debug, Deserialize)]
struct Ask {
    /// A `NodePath`: `node:/path`, or a bare `/path` for this node.
    path: String,
    /// Present and truthy for a download rather than a preview.
    #[serde(default)]
    download: Option<String>,
}

impl Ask {
    fn as_attachment(&self) -> bool {
        matches!(self.download.as_deref(), Some("1" | "true" | "yes"))
    }
}

/// The route, to be merged **behind the session gate**.
///
/// It applies `require_session` itself rather than trusting a caller to wrap
/// it, because the one mistake that matters here — mounting this where the
/// install assets are mounted — would publish the operator's whole working
/// directory, and a layer applied inside the function cannot be forgotten at
/// the call site.
pub fn router(http: Arc<crate::http::Ctx>, files: Arc<crate::files::Namespace>) -> axum::Router {
    let ctx = Ctx {
        http: Arc::clone(&http),
        files,
    };
    axum::Router::new()
        .route(MOUNT, get(serve))
        .with_state(ctx)
        .layer(axum::middleware::from_fn_with_state(
            http,
            crate::http::require_session,
        ))
}

fn refuse(status: StatusCode, why: &str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        format!("{why}\n"),
    )
        .into_response()
}

async fn serve(State(ctx): State<Ctx>, Query(ask): Query<Ask>, headers: HeaderMap) -> Response {
    let _ = &ctx.http;
    let address = match crate::files::NodePath::parse(&ask.path) {
        Ok(a) => a,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, &e.to_string()),
    };

    // One resolution, by the namespace, so the boundary and the hidden-state
    // denial are the same ones every other reader gets.
    let resolved = match ctx.files.resolve(&address) {
        Ok(r) => r,
        Err(e) => {
            let status = match &e {
                crate::files::FileError::NotFound { .. } => StatusCode::NOT_FOUND,
                crate::files::FileError::NotLocal { .. } => StatusCode::NOT_IMPLEMENTED,
                _ => StatusCode::FORBIDDEN,
            };
            return refuse(status, &e.to_string());
        }
    };

    let real = resolved.real_path().to_path_buf();
    // `std::fs` on a blocking task rather than `tokio::fs`: this workspace
    // does not enable tokio's `fs` feature. Opened first and judged by the
    // open descriptor, so what is checked is what is served: a stat followed
    // by an open by name could be raced by a FIFO swapped in between.
    let (file, meta) = match blocking(&real, open_examined).await {
        Ok(opened) => opened,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return refuse(StatusCode::NOT_FOUND, "there is no file at that address");
        }
        Err(e) => return refuse(StatusCode::FORBIDDEN, &e.to_string()),
    };
    if meta.is_dir() {
        return refuse(
            StatusCode::BAD_REQUEST,
            "that is a directory, so there is nothing to show; list it instead",
        );
    }
    // A FIFO, socket or device has no length and no end. Its open did not
    // wait (see `open_examined`), and nothing is ever read from it.
    if !meta.is_file() {
        return refuse(
            StatusCode::BAD_REQUEST,
            "that is not a regular file, so there is nothing to show",
        );
    }

    let size = meta.len();
    let name = address.path.name().unwrap_or("file").to_string();
    let mime = mime_guess::from_path(&name)
        .first_or_octet_stream()
        .to_string();

    // Weak, and deliberately: it is derived from length and mtime rather than
    // from the bytes, so producing it costs one stat instead of a read of the
    // whole file. That is the right trade for revalidating a 2 GB video.
    let etag = etag_for(&meta, size);

    let already_held = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|seen| seen.split(',').any(|c| c.trim() == etag));
    if already_held {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag.as_str()),
                (header::CACHE_CONTROL, "private, no-cache"),
            ],
        )
            .into_response();
    }

    let wanted = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let range = match wanted.map(|r| parse_range(r, size)) {
        None => None,
        Some(Ok(r)) => Some(r),
        Some(Err(())) => {
            // 416 with `Content-Range: bytes */len` is how a player learns the
            // real length and asks again. Answering a bad range with the whole
            // file instead is how one ends up decoding a header where it
            // expected the middle of a stream.
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{size}"))],
                "that range is not inside the file\n",
            )
                .into_response();
        }
    };

    let (start, end) = range.unwrap_or((0, size.saturating_sub(1)));
    let length = if size == 0 { 0 } else { end - start + 1 };

    // Opened and positioned before any header is written, so a file that
    // vanished or cannot be read is a status code rather than a body that
    // stops after the headers promised it.
    let opened = match tokio::task::spawn_blocking(move || seek_to(file, start)).await {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let disposition = if ask.as_attachment() {
        format!("attachment; filename=\"{}\"", sanitize(&name))
    } else {
        format!("inline; filename=\"{}\"", sanitize(&name))
    };

    let mut response = Response::builder()
        .status(if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, length)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, "private, no-cache")
        .header(header::CONTENT_DISPOSITION, disposition)
        // The three that make serving the operator's own files safe. See the
        // module header; none of them is decoration.
        .header(header::CONTENT_SECURITY_POLICY, "sandbox")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer");

    if range.is_some() {
        response = response.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }

    response
        .body(stream_body(opened, length))
        .unwrap_or_else(|_| {
            refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the response could not be built",
            )
        })
}

fn etag_for(meta: &std::fs::Metadata, size: u64) -> String {
    let stamp = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("W/\"{size:x}-{stamp:x}\"")
}

/// A filename for a header, with the two characters that would end the quoted
/// string removed. Not an encoding: a name that needs one is rare enough that
/// losing the exotic characters beats emitting a header a browser rejects.
fn sanitize(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '"' && *c != '\\' && !c.is_control())
        .collect()
}

/// `bytes=start-end`, `bytes=start-`, `bytes=-suffix`. One range only.
///
/// A multipart answer to several ranges is a different response shape and no
/// browser needs it to play a file, so asking for several is treated as asking
/// for something this route does not do rather than answered wrongly.
fn parse_range(header: &str, size: u64) -> Result<(u64, u64), ()> {
    let spec = header.trim().strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') {
        return Err(());
    }
    let (from, to) = spec.split_once('-').ok_or(())?;
    let (start, end) = match (from.trim(), to.trim()) {
        ("", "") => return Err(()),
        // A suffix range: the last N bytes, which is how some players read a
        // container's trailing index.
        ("", n) => {
            let n: u64 = n.parse().map_err(|_| ())?;
            if n == 0 || size == 0 {
                return Err(());
            }
            (size.saturating_sub(n), size - 1)
        }
        (s, "") => (s.parse().map_err(|_| ())?, size.saturating_sub(1)),
        (s, e) => (s.parse().map_err(|_| ())?, e.parse().map_err(|_| ())?),
    };
    if size == 0 || start >= size || end < start {
        return Err(());
    }
    Ok((start, end.min(size - 1)))
}

/// Runs one filesystem call on the blocking pool, so a slow disk cannot stall
/// the runtime that is serving every other request.
async fn blocking<T, F>(path: &std::path::Path, f: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&std::path::Path) -> std::io::Result<T> + Send + 'static,
{
    let owned = path.to_path_buf();
    match tokio::task::spawn_blocking(move || f(&owned)).await {
        Ok(result) => result,
        Err(e) => Err(std::io::Error::other(e)),
    }
}

fn seek_to(mut file: std::fs::File, start: u64) -> std::io::Result<std::fs::File> {
    use std::io::Seek as _;

    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start))?;
    }
    Ok(file)
}

/// Opens `path` for reading without ever waiting on it, and says what it is.
///
/// A plain `open(2)` of a FIFO for reading blocks until a writer appears, on a
/// thread nothing can cancel. `O_NONBLOCK` makes that open return at once;
/// `O_NOCTTY` keeps a terminal device from becoming the core's controlling
/// terminal. The type then comes from `fstat` on the descriptor that was
/// opened, never from a separate stat by name, so a file swapped for a FIFO
/// between the two cannot slip through.
///
/// The caller decides what to refuse. For a regular file `O_NONBLOCK` is
/// cleared before it is returned, so reads behave exactly as they would from
/// `File::open`.
pub(crate) fn open_examined(
    path: &std::path::Path,
) -> std::io::Result<(std::fs::File, std::fs::Metadata)> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::io::AsRawFd as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    let meta = file.metadata()?;
    if meta.is_file() {
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is owned by `file` for the whole call; F_GETFL and
        // F_SETFL only read and set its status flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: as above.
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((file, meta))
}

/// Exactly `length` bytes from where `file` is positioned, a [`PIECE`] at a
/// time, each read on the blocking pool.
///
/// A file that shrinks while it is being sent ends the stream with an error
/// rather than with fewer bytes than `Content-Length` promised: hyper then
/// drops the connection, which a browser reports as a failed load. Quietly
/// finishing short would hand a player a body it believes is complete.
fn stream_body(file: std::fs::File, length: u64) -> Body {
    if length == 0 {
        return Body::empty();
    }
    let pieces =
        futures_util::stream::try_unfold((file, length), |(mut file, remaining)| async move {
            if remaining == 0 {
                return Ok(None);
            }
            let want = remaining.min(PIECE) as usize;
            let (file, piece) = tokio::task::spawn_blocking(move || {
                let piece = read_piece(&mut file, want)?;
                Ok::<_, std::io::Error>((file, piece))
            })
            .await
            .map_err(std::io::Error::other)??;
            if piece.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the file got shorter while it was being sent",
                ));
            }
            let left = remaining - piece.len() as u64;
            Ok(Some((piece, (file, left))))
        });
    Body::from_stream(pieces)
}

fn read_piece(file: &mut std::fs::File, want: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;

    let mut buf = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_is_read_the_way_a_player_writes_one() {
        assert_eq!(parse_range("bytes=0-99", 1000), Ok((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Ok((500, 999)));
        // A suffix range is the last N bytes, not a range starting at N.
        assert_eq!(parse_range("bytes=-100", 1000), Ok((900, 999)));
        // Clamped to the file rather than refused: a player that asks for more
        // than is there is asking for the rest, and answering the rest is both
        // what it meant and what the spec says.
        assert_eq!(parse_range("bytes=900-5000", 1000), Ok((900, 999)));
    }

    #[test]
    fn a_range_that_cannot_be_answered_is_refused_rather_than_guessed() {
        assert!(
            parse_range("bytes=1000-", 1000).is_err(),
            "starts past the end"
        );
        assert!(parse_range("bytes=50-20", 1000).is_err(), "inverted");
        assert!(parse_range("bytes=-", 1000).is_err(), "names nothing");
        assert!(parse_range("items=0-10", 1000).is_err(), "not bytes");
        assert!(parse_range("bytes=abc-10", 1000).is_err(), "not a number");
        assert!(
            parse_range("bytes=0-10", 0).is_err(),
            "an empty file has no ranges"
        );
        // Several ranges would need a multipart body, which this route does
        // not produce. Refusing beats answering one of them and calling it the
        // answer.
        assert!(parse_range("bytes=0-10,20-30", 1000).is_err(), "multipart");
    }

    #[test]
    fn a_filename_cannot_break_out_of_its_own_header() {
        assert_eq!(sanitize("report.pdf"), "report.pdf");
        assert_eq!(sanitize("a\"b\\c.txt"), "abc.txt");
        assert_eq!(sanitize("new\nline.txt"), "newline.txt");
    }
}
