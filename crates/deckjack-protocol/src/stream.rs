//! Streamed native apps: what the compositor, the core and the browser say
//! to each other (docs/lanes/8-stream.md).
//!
//! Three parties and two links:
//!
//! - **The link**, compositor and core, over one end of a Unix socket pair
//!   the core makes and the compositor inherits ([`ENV_LINK_FD`]). Each frame is a
//!   little-endian `u32` length (of everything after it), one kind byte and
//!   the payload: [`KIND_CONTROL`] carries JSON ([`StreamEvent`] one way,
//!   [`StreamCommand`] the other), [`KIND_VIDEO`] carries one encoded
//!   picture for one surface ([`VideoHeader`] then H.264 in Annex B).
//! - **The stream socket**, core and browser, a WebSocket at [`WS_PATH`],
//!   one per streamed window. Binary messages are pictures for that window
//!   only, with the surface number left out ([`BROWSER_HEADER_LEN`]); text
//!   messages from the browser are [`BrowserMessage`] JSON. The core fills
//!   in which surface a message is for from the window the socket was
//!   opened for, so a browser can never address another window's surface.
//!
//! None of this rides the command bus. A picture is tens of kilobytes sixty
//! times a second, and the bus is JSON with progress frames that may be
//! dropped; a dropped P-frame corrupts every picture until the next
//! keyframe. Launching, listing and stopping are bus commands (`stream.*`).
//!
//! Every size and coordinate here is in **logical pixels at scale 1**:
//! the compositor presents one 1920x1080 output at 96 dpi, and the browser
//! scales the picture. The app never learns how dense the screen looking at
//! it is.

use serde::{Deserialize, Serialize};

/// The stream socket's path on the core's origin.
pub const WS_PATH: &str = "/stream";

/// The environment variable carrying the link's descriptor number, set by
/// the core on the compositor's launch plan. The link is one end of a
/// socket pair the compositor inherits, not a path it connects to: a path
/// would be a rendezvous any other local process could race for.
pub const ENV_LINK_FD: &str = "DECKJACK_STREAM_FD";

/// The environment variable carrying the [`LaunchSpec`] as JSON.
pub const ENV_SPEC: &str = "DECKJACK_STREAM_SPEC";

/// The one output every streamed app sees, in logical pixels.
pub const OUTPUT_WIDTH: u32 = 1920;
pub const OUTPUT_HEIGHT: u32 = 1080;

/// Largest link frame either side accepts. A 1920x1080 keyframe from the
/// encoder at its configured rate is well under 1 MiB; the cap is there so
/// a corrupt length cannot make the reader allocate gigabytes.
pub const MAX_LINK_FRAME: usize = 8 << 20;

/// Link frame kinds.
pub const KIND_CONTROL: u8 = 1;
pub const KIND_VIDEO: u8 = 2;

/// [`VideoHeader::flags`]: this picture is an IDR and decodes on its own.
pub const FLAG_KEYFRAME: u8 = 1;

/// Bytes of [`VideoHeader`] on the link: surface, flags, width, height,
/// timestamp.
pub const LINK_HEADER_LEN: usize = 4 + 1 + 2 + 2 + 4;

/// Bytes of [`VideoHeader`] in a browser message: the link header without
/// the surface.
pub const BROWSER_HEADER_LEN: usize = 1 + 2 + 2 + 4;

/// What the core tells the compositor to run, as JSON in [`ENV_SPEC`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSpec {
    /// The native app's name, for logs.
    pub app: String,
    /// Program and arguments. An absolute program path; nothing is looked
    /// up on a `PATH`.
    pub exec: Vec<String>,
    /// Start XWayland and give the app a `DISPLAY` as well as a
    /// `WAYLAND_DISPLAY`.
    #[serde(default)]
    pub x11: bool,
}

/// What kind of window a surface is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SurfaceKind {
    /// An xdg toplevel or a managed X11 window.
    Toplevel,
    /// An xdg popup or an override-redirect X11 window (menus, tooltips).
    Popup,
}

/// Compositor to core, as [`KIND_CONTROL`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum StreamEvent {
    /// The compositor is up and the app has been started.
    Ready {
        /// The X display the app was given, when it asked for X11.
        #[serde(default)]
        x11_display: Option<u32>,
    },
    /// A surface became a window. For a popup, `x` and `y` are its offset
    /// from its parent's top-left corner; for a toplevel they are zero.
    Mapped {
        surface: u32,
        kind: SurfaceKind,
        #[serde(default)]
        parent: Option<u32>,
        title: String,
        width: u32,
        height: u32,
        x: i32,
        y: i32,
    },
    /// A mapped surface changed its title, size or (a popup) its offset.
    Changed {
        surface: u32,
        title: String,
        width: u32,
        height: u32,
        x: i32,
        y: i32,
    },
    /// A surface is no longer a window.
    Unmapped { surface: u32 },
    /// The app has exited and the compositor is about to.
    Exited { status: String },
}

/// Core to compositor, as [`KIND_CONTROL`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum StreamCommand {
    /// Input for one surface, from the device holding the seat.
    Input { surface: u32, input: Input },
    /// The window around a toplevel was resized: ask the app for this size.
    Resize {
        surface: u32,
        width: u32,
        height: u32,
    },
    /// Ask the app to close a toplevel (the app may ask to save first).
    Close { surface: u32 },
    /// Encode the next picture of this surface as a keyframe: a viewer has
    /// attached, or the seat moved to another device.
    Keyframe { surface: u32 },
    /// Release every key and pointer button the app still sees held: the
    /// device that pressed them can no longer send the releases (the seat
    /// moved, or its last panel went away).
    ReleaseAll,
}

/// One input event, in window-local logical pixels.
///
/// Buttons are numbered as the DOM numbers `MouseEvent.button` (0 primary,
/// 1 middle, 2 secondary, 3 back, 4 forward); the compositor maps them to
/// evdev. Keys are evdev key codes, mapped by the browser from
/// `KeyboardEvent.code`, so the layout is the compositor's (US) and not the
/// browser's: what a key does is decided where the app runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Input {
    PointerMove {
        x: f64,
        y: f64,
    },
    PointerButton {
        x: f64,
        y: f64,
        button: u8,
        pressed: bool,
    },
    /// Scroll, in logical pixels (positive is down and right).
    Scroll {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    Key {
        code: u32,
        pressed: bool,
    },
    /// The pointer left the window.
    PointerLeave,
    /// The window was focused in the shell: give it the keyboard.
    Focus,
}

/// Browser to core, as a text message on the stream socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum BrowserMessage {
    Input { input: Input },
    Resize { width: u32, height: u32 },
    Keyframe,
}

/// The header in front of every picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    pub surface: u32,
    pub flags: u8,
    /// The visible size. The encoded picture may be one pixel larger in
    /// each direction (H.264 4:2:0 wants even sizes); the browser draws this
    /// much of it.
    pub width: u16,
    pub height: u16,
    /// Milliseconds since the compositor started, wrapping.
    pub timestamp: u32,
}

impl VideoHeader {
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    /// The link form: header then `payload`.
    pub fn link_bytes(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(LINK_HEADER_LEN + payload.len());
        out.extend_from_slice(&self.surface.to_le_bytes());
        self.put_rest(&mut out);
        out.extend_from_slice(payload);
        out
    }

    /// Read a link video payload: the header and the H.264 after it.
    pub fn parse_link(bytes: &[u8]) -> Option<(VideoHeader, &[u8])> {
        if bytes.len() < LINK_HEADER_LEN {
            return None;
        }
        let surface = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
        let (flags, width, height, timestamp) = VideoHeader::get_rest(&bytes[4..])?;
        Some((
            VideoHeader {
                surface,
                flags,
                width,
                height,
                timestamp,
            },
            &bytes[LINK_HEADER_LEN..],
        ))
    }

    /// The browser form of a link payload: the same bytes without the
    /// surface, which the browser's socket already implies.
    pub fn browser_bytes(link_payload: &[u8]) -> Option<&[u8]> {
        (link_payload.len() >= LINK_HEADER_LEN).then(|| &link_payload[4..])
    }

    fn put_rest(&self, out: &mut Vec<u8>) {
        out.push(self.flags);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.timestamp.to_le_bytes());
    }

    fn get_rest(bytes: &[u8]) -> Option<(u8, u16, u16, u32)> {
        if bytes.len() < BROWSER_HEADER_LEN {
            return None;
        }
        Some((
            bytes[0],
            u16::from_le_bytes(bytes[1..3].try_into().ok()?),
            u16::from_le_bytes(bytes[3..5].try_into().ok()?),
            u32::from_le_bytes(bytes[5..9].try_into().ok()?),
        ))
    }
}

/// One link frame on the wire: length, kind, payload.
pub fn link_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&((payload.len() + 1) as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

/// Why a link frame was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("link frame of {0} bytes is over the {MAX_LINK_FRAME}-byte limit")]
    TooLarge(usize),
    #[error("empty link frame")]
    Empty,
}

/// Check a frame length read off the link, before allocating for it.
pub fn check_link_len(len: u32) -> Result<usize, LinkError> {
    let len = len as usize;
    if len == 0 {
        Err(LinkError::Empty)
    } else if len > MAX_LINK_FRAME {
        Err(LinkError::TooLarge(len))
    } else {
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_picture_becomes_a_browser_picture_without_its_surface() {
        let header = VideoHeader {
            surface: 7,
            flags: FLAG_KEYFRAME,
            width: 1919,
            height: 1081,
            timestamp: 0xdead_beef,
        };
        let payload = [0, 0, 0, 1, 0x67, 0x42];
        let link = header.link_bytes(&payload);
        let (back, h264) = VideoHeader::parse_link(&link).unwrap();
        assert_eq!(back, header);
        assert!(back.is_keyframe());
        assert_eq!(h264, payload);
        let browser = VideoHeader::browser_bytes(&link).unwrap();
        assert_eq!(browser.len(), BROWSER_HEADER_LEN + payload.len());
        assert_eq!(browser[0], FLAG_KEYFRAME);
        assert_eq!(u16::from_le_bytes([browser[1], browser[2]]), 1919);
        assert_eq!(u16::from_le_bytes([browser[3], browser[4]]), 1081);
        assert_eq!(&browser[BROWSER_HEADER_LEN..], payload);
    }

    #[test]
    fn a_short_picture_is_refused_rather_than_read_past_its_end() {
        assert!(VideoHeader::parse_link(&[0; LINK_HEADER_LEN - 1]).is_none());
        // Shorter than the surface number itself.
        assert!(VideoHeader::parse_link(&[0; 2]).is_none());
        assert!(VideoHeader::browser_bytes(&[0; LINK_HEADER_LEN - 1]).is_none());
    }

    #[test]
    fn a_link_length_is_checked_before_anything_is_allocated() {
        assert_eq!(check_link_len(0), Err(LinkError::Empty));
        assert_eq!(
            check_link_len(MAX_LINK_FRAME as u32 + 1),
            Err(LinkError::TooLarge(MAX_LINK_FRAME + 1))
        );
        assert_eq!(check_link_len(5), Ok(5));
        let frame = link_frame(KIND_CONTROL, b"{}");
        assert_eq!(u32::from_le_bytes(frame[0..4].try_into().unwrap()), 3);
        assert_eq!(frame[4], KIND_CONTROL);
    }

    /// `stream.*` starts processes and carries pictures, so no app window
    /// may call it or name it, whatever its manifest says (D6), and no app
    /// may be named after it.
    #[test]
    fn no_app_window_may_reach_the_stream_namespace() {
        assert!(crate::is_hard_denied("stream.launch"));
        assert!(crate::is_hard_denied("stream.*"));
        assert!(crate::is_reserved_namespace("stream"));
        let manifest = "[app]\nname = \"viewer\"\nversion = \"1\"\nprotocol = 1\n\n\
                        [ui]\ninvoke = [\"stream.launch\"]\n";
        let err = crate::Manifest::parse(std::path::Path::new("app.toml"), manifest).unwrap_err();
        assert!(err.to_string().contains("stream.launch"), "{err}");
    }

    #[test]
    fn browser_messages_have_the_shape_the_stream_panel_sends() {
        let m: BrowserMessage = serde_json::from_str(
            r#"{"type":"input","input":{"type":"pointer-button","x":1.5,"y":2,"button":0,"pressed":true}}"#,
        )
        .unwrap();
        assert_eq!(
            m,
            BrowserMessage::Input {
                input: Input::PointerButton {
                    x: 1.5,
                    y: 2.0,
                    button: 0,
                    pressed: true
                }
            }
        );
        let r: BrowserMessage =
            serde_json::from_str(r#"{"type":"resize","width":800,"height":600}"#).unwrap();
        assert_eq!(
            r,
            BrowserMessage::Resize {
                width: 800,
                height: 600
            }
        );
    }
}
