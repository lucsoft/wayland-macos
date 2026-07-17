//! wl_shm, wl_shm_pool, wl_buffer (shared-memory buffers)
//!
//! Protocol dispatch split out of the parent `wayland` module; `use super::*`
//! pulls in `State`, the shared records, and the protocol/`wayland_server` imports.

use super::*;


// ---------------------------------------------------------------------------
// wl_shm
// ---------------------------------------------------------------------------

impl GlobalDispatch<WlShm, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlShm>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let shm = data_init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl Dispatch<WlShm, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlShm,
        request: wl_shm::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, fd, size } = request {
            data_init.init(id, Arc::new(map_pool(fd, size)));
        }
    }
}

impl Dispatch<WlShmPool, Arc<PoolMem>> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlShmPool,
        request: wl_shm_pool::Request,
        data: &Arc<PoolMem>,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_shm_pool::Request::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                let format = match format {
                    WEnum::Value(f) => f as u32,
                    WEnum::Unknown(v) => v,
                };
                data_init.init(
                    id,
                    BufferData {
                        kind: BufferKind::Shm {
                            pool: data.clone(),
                            offset,
                            width,
                            height,
                            stride,
                            format,
                        },
                    },
                );
            }
            wl_shm_pool::Request::Resize { size } => {
                data.resize(size);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlBuffer, BufferData> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlBuffer,
        _request: wl_buffer::Request,
        _data: &BufferData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}
