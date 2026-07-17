//! zwp_primary_selection_v1 (X11-style middle-click "primary" selection)
//!
//! The primary selection is a second, implicit clipboard: selecting text arms
//! it, a middle-click pastes it. Many Linux apps rely on it. macOS has no
//! equivalent, so this is purely client-to-client — the selection is offered
//! among Wayland clients and never touches the macOS pasteboard (that is what
//! `src/wayland/clipboard.rs` does for the regular `wl_data_device`).
//!
//! The flow mirrors the clipboard: a client offers a `..._source_v1` and
//! makes it the selection with `set_selection`; we advertise a fresh
//! `..._offer_v1` to every client's device; when a client pastes
//! (`offer.receive`) we ask the owning source to write the bytes into the pasted
//! fd (`source.send`), keeping our copy of the fd open until the next flush.
//!
//! `use super::*` pulls in `State`, the shared records, and the protocol imports.

use super::*;
use std::os::fd::OwnedFd;
use wayland_protocols::wp::primary_selection::zv1::server::{
    zwp_primary_selection_device_manager_v1::{self, ZwpPrimarySelectionDeviceManagerV1},
    zwp_primary_selection_device_v1::{self, ZwpPrimarySelectionDeviceV1},
    zwp_primary_selection_offer_v1::{self, ZwpPrimarySelectionOfferV1},
    zwp_primary_selection_source_v1::{self, ZwpPrimarySelectionSourceV1},
};

/// Primary-selection state. Lives on `State::primary_selection`.
#[derive(Default)]
pub struct PrimarySelection {
    /// Live client devices; a new selection is advertised to each.
    devices: Vec<ZwpPrimarySelectionDeviceV1>,
    /// MIME types each `..._source_v1` advertised (via `source.offer`).
    source_mimes: HashMap<ObjectId, Vec<String>>,
    /// The source that currently owns the primary selection, if any.
    active_source: Option<ZwpPrimarySelectionSourceV1>,
    /// Write-ends handed to a source via `source.send`, kept open until the next
    /// `flush_clients` transmits the fd, then dropped so the reader sees EOF.
    pending_close: Vec<OwnedFd>,
}

impl PrimarySelection {
    fn add_device(&mut self, dh: &DisplayHandle, device: ZwpPrimarySelectionDeviceV1) {
        // If a selection already exists, let the newcomer see it immediately.
        if self.active_source.is_some() {
            self.advertise_to(dh, &device);
        }
        self.devices.push(device);
    }

    /// A client made one of its sources the primary selection.
    fn set_selection(&mut self, dh: &DisplayHandle, source: Option<ZwpPrimarySelectionSourceV1>) {
        // Cancel the previous owner if it is being replaced.
        if let Some(prev) = self.active_source.take() {
            if prev.is_alive() && Some(&prev) != source.as_ref() {
                prev.cancelled();
            }
        }
        self.active_source = source;
        self.devices.retain(|d| d.is_alive());
        for device in self.devices.clone() {
            self.advertise_to(dh, &device);
        }
    }

    /// The owning source went away — destroyed by its client, or the client
    /// disconnected. Forget it and, if it held the selection, tell every live
    /// device the primary selection is now empty. Without this, clients keep a
    /// stale offer backed by a dead source: a middle-click paste then writes
    /// nothing (see the `source.is_alive()` guard in `receive`) instead of
    /// pasting the current selection. Mirrors wlroots/Smithay, which clear the
    /// seat's primary selection when its source is destroyed. Idempotent, so it
    /// is safe to call from both the `Destroy` request and the `destroyed` hook.
    fn forget_source(&mut self, source_id: &ObjectId) {
        self.source_mimes.remove(source_id);
        if self.active_source.as_ref().map(|s| s.id()).as_ref() == Some(source_id) {
            self.active_source = None;
            self.devices.retain(|d| d.is_alive());
            for device in &self.devices {
                device.selection(None);
            }
        }
    }

    /// Advertise the current primary selection (if any) to a single device.
    fn advertise_to(&self, dh: &DisplayHandle, device: &ZwpPrimarySelectionDeviceV1) {
        if !device.is_alive() {
            return;
        }
        // Guard on the source still being alive too: a source whose client
        // vanished before `forget_source` ran would otherwise back a live offer.
        let Some(source) = self.active_source.as_ref().filter(|s| s.is_alive()) else {
            device.selection(None);
            return;
        };
        let Some(client) = device.client() else {
            return;
        };
        let offer = match client
            .create_resource::<ZwpPrimarySelectionOfferV1, ZwpPrimarySelectionSourceV1, State>(
                dh,
                device.version(),
                source.clone(),
            ) {
            Ok(offer) => offer,
            Err(e) => {
                error!(target: "primary", "failed to create offer: {e}");
                return;
            }
        };
        // Order matters: introduce the offer, describe its MIME types, set it.
        device.data_offer(&offer);
        if let Some(mimes) = self.source_mimes.get(&source.id()) {
            for mime in mimes {
                offer.offer(mime.clone());
            }
        }
        device.selection(Some(&offer));
    }

    /// Called by the Wayland loop right after `flush_clients` (see the
    /// clipboard): the queued `send` fds have been transmitted, so drop our copies.
    pub fn flush_done(&mut self) {
        self.pending_close.clear();
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

impl GlobalDispatch<ZwpPrimarySelectionDeviceManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpPrimarySelectionDeviceManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwpPrimarySelectionDeviceManagerV1,
        request: zwp_primary_selection_device_manager_v1::Request,
        _data: &(),
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_primary_selection_device_manager_v1::Request::CreateSource { id } => {
                let source = data_init.init(id, ());
                state
                    .primary_selection
                    .source_mimes
                    .insert(source.id(), Vec::new());
            }
            zwp_primary_selection_device_manager_v1::Request::GetDevice { id, .. } => {
                let device = data_init.init(id, ());
                state.primary_selection.add_device(dh, device);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpPrimarySelectionSourceV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        source: &ZwpPrimarySelectionSourceV1,
        request: zwp_primary_selection_source_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_primary_selection_source_v1::Request::Offer { mime_type } => {
                state
                    .primary_selection
                    .source_mimes
                    .entry(source.id())
                    .or_default()
                    .push(mime_type);
            }
            zwp_primary_selection_source_v1::Request::Destroy => {
                state.primary_selection.forget_source(&source.id());
            }
            _ => {}
        }
    }

    /// Also fires on abrupt client disconnect, so a dead source never keeps
    /// holding (and being advertised as) the primary selection.
    fn destroyed(state: &mut Self, _c: ClientId, source: &ZwpPrimarySelectionSourceV1, _d: &()) {
        state.primary_selection.forget_source(&source.id());
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _device: &ZwpPrimarySelectionDeviceV1,
        request: zwp_primary_selection_device_v1::Request,
        _data: &(),
        dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_primary_selection_device_v1::Request::SetSelection { source, .. } = request {
            state.primary_selection.set_selection(dh, source);
        }
    }
}

/// A `..._offer_v1` carries the source it represents as user data.
impl Dispatch<ZwpPrimarySelectionOfferV1, ZwpPrimarySelectionSourceV1> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _offer: &ZwpPrimarySelectionOfferV1,
        request: zwp_primary_selection_offer_v1::Request,
        source: &ZwpPrimarySelectionSourceV1,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_primary_selection_offer_v1::Request::Receive { mime_type, fd } = request {
            // Ask the owning source to write its bytes into the paster's fd. The
            // backend transmits the fd at the next flush, so keep it open until
            // then (see `flush_done`); dropping it early would send a stale fd.
            if source.is_alive() {
                source.send(mime_type, fd.as_fd());
                state.primary_selection.pending_close.push(fd);
            }
        }
    }
}
