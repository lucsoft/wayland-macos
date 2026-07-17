//! Process-boundary transport for `--multiplex` mode.
//!
//! Normally the Wayland thread hands `WinCmd`s to the AppKit main thread inside
//! the same process. In `--multiplex` mode the compositor spawns one *window
//! host* child process per Wayland app (see `src/host.rs`), so each app owns its
//! own `NSApplication` — its own Dock tile and Cmd-Tab entry — while the
//! compositor process itself is an `Accessory` (hidden) app.
//!
//! The seam is unchanged in spirit: the compositor sends `WinCmd`s *down* to the
//! host, and the host sends `InputEvent`s back *up*. This module is only the wire
//! format: length-prefixed `bincode` frames over a `UnixStream`.
//!
//! ```text
//! compositor (Accessory)                 window-host N (Regular, one per app)
//!   wayland thread ──Downlink::Cmd──────────> AppKit main thread (NSWindow)
//!   InputBus       <──────Uplink::Input────── NSEvent handlers
//! ```

use crate::input::InputEvent;
use crate::mac::WinCmd;
use std::io::{self, Read, Write};

/// Compositor → window-host.
#[derive(serde::Serialize, serde::Deserialize)]
pub enum Downlink {
    /// First message on the socket: environment the host needs before it creates
    /// any window. `regular` decides the host's activation policy (a toplevel app
    /// is `Regular` → Dock tile; a layer-shell bar is `Accessory` → no tile).
    Hello {
        scale: i32,
        out_w: i32,
        out_h: i32,
        /// Reserved work-area insets (top, right, bottom, left) at spawn time.
        insets: (i32, i32, i32, i32),
        regular: bool,
        /// Human-facing app name for the Dock/Cmd-Tab entry.
        name: String,
    },
    /// A window operation to run on this host's AppKit main thread.
    Cmd(WinCmd),
    /// Docked-bar reserved insets changed; update this host's copy so its windows
    /// keep avoiding the bar (which lives in a different host).
    Insets(i32, i32, i32, i32),
    /// The owning client is gone; the host should exit.
    Shutdown,
}

/// Window-host → compositor.
#[derive(serde::Serialize, serde::Deserialize)]
pub enum Uplink {
    /// Sent once the host's AppKit loop is up and it has installed its input sink.
    Ready,
    /// A normalized input event, to be pushed into the compositor's `InputBus`.
    Input(InputEvent),
}

/// Serialize `msg` into a complete length-prefixed frame (`u32` little-endian
/// length header + `bincode` payload). Encode once, then write the same bytes to
/// several sockets when broadcasting.
pub fn encode_frame<T: serde::Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let bytes = bincode::serialize(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&bytes);
    Ok(out)
}

/// Write one length-prefixed `bincode` frame.
pub fn write_frame<W: Write, T: serde::Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let frame = encode_frame(msg)?;
    w.write_all(&frame)?;
    w.flush()
}

/// Read one length-prefixed `bincode` frame. Returns `Ok(None)` on a clean EOF at
/// a frame boundary (peer closed the socket).
pub fn read_frame<R: Read, T: serde::de::DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let msg = bincode::deserialize(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(msg))
}
