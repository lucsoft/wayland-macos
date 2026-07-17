//! Clipboard bridge: connects the Wayland selection (`wl_data_device`) to the
//! macOS pasteboard (`NSPasteboard`).
//!
//! Two directions, both routed through the macOS pasteboard so that even
//! Wayland-to-Wayland copy/paste works consistently:
//!
//! * **Copy (Wayland → macOS).** A client offers data via `wl_data_source` and
//!   makes it the selection with `wl_data_device.set_selection`. We ask the
//!   client to write the bytes into a pipe (`wl_data_source.send`), read them on
//!   a background thread, and hand the text to [`mac::set_clipboard`].
//!
//! * **Paste (macOS → Wayland).** [`mac::start_clipboard_watch`] polls the
//!   pasteboard and pushes its text onto the input bus as
//!   [`InputEvent::MacClipboard`]. The Wayland loop forwards that to
//!   [`Clipboard::set_mac_selection`], which advertises a fresh `wl_data_offer`
//!   to every client. When a client pastes (`wl_data_offer.receive`) we write
//!   the snapshot into the client's pipe.
//!
//! Only plain UTF-8 text is bridged; every advertised MIME maps to the same
//! bytes. Drag-and-drop (`start_drag`) is not handled.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use wayland_server::backend::ObjectId;
use wayland_server::protocol::{
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::{self, WlDataDeviceManager},
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::mac;
use crate::wayland::State;

/// Text MIME types we advertise to clients for the macOS pasteboard, and accept
/// from clients when they copy. In descending order of preference; all map to
/// the same UTF-8 bytes.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// User data for a server-created `wl_data_offer`: a snapshot of the macOS
/// selection it represents. Shared (via `Arc`) across every client's offer.
pub struct OfferData {
    bytes: Vec<u8>,
}

/// State of the clipboard bridge. Lives on `state.bridges.clipboard`.
#[derive(Default)]
pub struct Clipboard {
    /// Live client data devices; new selections are advertised to each.
    devices: Vec<WlDataDevice>,
    /// MIME types each client `wl_data_source` advertised (copy direction).
    source_mimes: HashMap<ObjectId, Vec<String>>,
    /// Current macOS pasteboard selection offered to clients (paste direction).
    mac_offer: Option<Arc<OfferData>>,
    /// The client source that currently owns the Wayland selection, if any. It
    /// is `cancelled` when the macOS pasteboard replaces it.
    active_source: Option<WlDataSource>,
    /// Write-ends handed to clients via `wl_data_source.send`, kept open until
    /// the next `flush_clients` transmits the fd (see [`Clipboard::flush_done`]),
    /// then dropped so the reader thread sees EOF.
    pending_close: Vec<OwnedFd>,
}

impl Clipboard {
    /// Register a freshly created client data device.
    ///
    /// We deliberately do NOT advertise the selection here: a client creates its
    /// data device during its very first roundtrip, before its toolkit's display
    /// is fully initialized. Sending a selection that early crashes GTK
    /// (`gdk_display_should_use_portal: display != NULL`). Instead we advertise
    /// when the client gains keyboard focus — which is also when real compositors
    /// do it. See [`Clipboard::advertise_to_client`].
    fn add_device(&mut self, _dh: &DisplayHandle, device: WlDataDevice) {
        self.devices.push(device);
    }

    /// Advertise the current macOS selection to a client's data devices. Called
    /// when the client gains keyboard focus.
    pub fn advertise_to_client(&self, dh: &DisplayHandle, client: &Client) {
        for device in &self.devices {
            if device.is_alive() && device.client().map(|c| c.id()) == Some(client.id()) {
                self.advertise_to(dh, device);
            }
        }
    }

    /// The macOS pasteboard changed: refresh the offer and advertise it to every
    /// live client. Called from the Wayland loop for [`InputEvent::MacClipboard`].
    pub fn set_mac_selection(&mut self, dh: &DisplayHandle, text: Option<String>) {
        self.mac_offer = text.map(|t| {
            Arc::new(OfferData {
                bytes: t.into_bytes(),
            })
        });
        // The macOS pasteboard now owns the clipboard; release any client source
        // so it stops advertising itself as the selection owner.
        if let Some(src) = self.active_source.take() {
            if src.is_alive() {
                src.cancelled();
            }
        }
        self.devices.retain(|d| d.is_alive());
        for device in self.devices.clone() {
            self.advertise_to(dh, &device);
        }
    }

    /// Advertise the current macOS selection (if any) to a single device.
    fn advertise_to(&self, dh: &DisplayHandle, device: &WlDataDevice) {
        if !device.is_alive() {
            return;
        }
        let Some(offer_data) = &self.mac_offer else {
            device.selection(None);
            return;
        };
        let Some(client) = device.client() else {
            return;
        };
        let offer = match client.create_resource::<WlDataOffer, Arc<OfferData>, State>(
            dh,
            device.version(),
            offer_data.clone(),
        ) {
            Ok(offer) => offer,
            Err(e) => {
                eprintln!("[clipboard] failed to create data offer: {e}");
                return;
            }
        };
        // Order matters: introduce the offer, describe it, then set it.
        device.data_offer(&offer);
        for mime in TEXT_MIMES {
            offer.offer((*mime).to_string());
        }
        device.selection(Some(&offer));
    }

    /// A client made one of its sources the selection (copy direction).
    fn set_selection(&mut self, source: Option<WlDataSource>) {
        let Some(source) = source else {
            // Client cleared its own selection; leave the macOS pasteboard as-is.
            self.active_source = None;
            return;
        };
        // Cancel any previous client source this one replaces.
        if let Some(prev) = self.active_source.take() {
            if prev.is_alive() && prev != source {
                prev.cancelled();
            }
        }
        // Pull the bytes from the client for the best text MIME it offered.
        let mimes = self
            .source_mimes
            .get(&source.id())
            .cloned()
            .unwrap_or_default();
        if let Some(mime) = pick_text_mime(&mimes) {
            if let Some((read, write)) = pipe() {
                source.send(mime, write.as_fd());
                // The backend transmits the fd at the next flush, so keep our
                // write end open until then (see `flush_done`); dropping it early
                // would send a stale fd. The reader blocks meanwhile.
                self.pending_close.push(write);
                read_text_async(read);
            }
        }
        self.active_source = Some(source);
    }

    /// Called by the Wayland loop right after `flush_clients`: the queued `send`
    /// fds have been transmitted, so our copies can be dropped, letting the
    /// reader threads observe EOF once the clients finish writing.
    pub fn flush_done(&mut self) {
        self.pending_close.clear();
    }
}

/// Pick the client-offered MIME type we prefer to pull text from.
fn pick_text_mime(offered: &[String]) -> Option<String> {
    for pref in TEXT_MIMES {
        if let Some(m) = offered.iter().find(|o| o.eq_ignore_ascii_case(pref)) {
            return Some(m.clone());
        }
    }
    // Fall back to any other text/* the client offers.
    offered.iter().find(|o| o.starts_with("text/")).cloned()
}

/// Read a client's clipboard bytes to EOF on a background thread, then hand the
/// text to the macOS pasteboard.
fn read_text_async(fd: OwnedFd) {
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(fd);
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
            mac::set_clipboard(String::from_utf8_lossy(&buf).into_owned());
        }
    });
}

/// Write an offer's bytes into a client's pipe on a background thread, then close
/// it (dropping the fd) so the client sees EOF.
fn serve_offer(data: &Arc<OfferData>, fd: OwnedFd) {
    let bytes = data.bytes.clone();
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(fd);
        let _ = file.write_all(&bytes);
    });
}

/// A blocking pipe `(read, write)`. Blocking is intentional: the reader thread
/// should wait for the client to finish writing.
fn pipe() -> Option<(OwnedFd, OwnedFd)> {
    rustix::pipe::pipe()
        .map_err(|e| eprintln!("[clipboard] pipe failed: {e}"))
        .ok()
}

// ---------------------------------------------------------------------------
// Wayland dispatch: wl_data_device_manager / _source / _device / _offer
// ---------------------------------------------------------------------------

impl GlobalDispatch<WlDataDeviceManager, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlDataDeviceManager>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WlDataDeviceManager,
        request: wl_data_device_manager::Request,
        _data: &(),
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_device_manager::Request::CreateDataSource { id } => {
                let source = data_init.init(id, ());
                state
                    .bridges
                    .clipboard
                    .source_mimes
                    .insert(source.id(), Vec::new());
            }
            wl_data_device_manager::Request::GetDataDevice { id, .. } => {
                let device = data_init.init(id, ());
                state.bridges.clipboard.add_device(dh, device);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataSource, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        source: &WlDataSource,
        request: wl_data_source::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_data_source::Request::Offer { mime_type } => {
                state
                    .bridges
                    .clipboard
                    .source_mimes
                    .entry(source.id())
                    .or_default()
                    .push(mime_type);
            }
            wl_data_source::Request::Destroy => {
                let cb = &mut state.bridges.clipboard;
                cb.source_mimes.remove(&source.id());
                if cb.active_source.as_ref() == Some(source) {
                    cb.active_source = None;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _device: &WlDataDevice,
        request: wl_data_device::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // start_drag (DnD) and release are not handled.
        if let wl_data_device::Request::SetSelection { source, .. } = request {
            state.bridges.clipboard.set_selection(source);
        }
    }
}

impl Dispatch<WlDataOffer, Arc<OfferData>> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _offer: &WlDataOffer,
        request: wl_data_offer::Request,
        data: &Arc<OfferData>,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // accept / finish / set_actions / destroy need no action here.
        if let wl_data_offer::Request::Receive { fd, .. } = request {
            serve_offer(data, fd);
        }
    }
}
