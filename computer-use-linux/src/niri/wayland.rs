use anyhow::{bail, Context, Result};
use std::{
    collections::HashMap,
    ffi::CString,
    fs::File,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::{Duration, Instant},
};
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_keyboard, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    },
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::{
    screencopy::v1::client::{
        zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
        zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
    virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    },
};

const WAYLAND_OPERATION_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub(crate) struct ProtocolCapability {
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub(crate) struct NiriWaylandCapabilities {
    pub screencopy: ProtocolCapability,
    pub virtual_pointer: ProtocolCapability,
    pub virtual_keyboard: ProtocolCapability,
}

pub(crate) fn probe_capabilities() -> NiriWaylandCapabilities {
    match WaylandClient::connect() {
        Ok(mut client) => {
            let mut capabilities = client.capabilities();
            if capabilities.virtual_keyboard.ok {
                match client.request_keyboard_keymap() {
                    Ok(()) => capabilities
                        .virtual_keyboard
                        .detail
                        .push_str(" and the active XKB keymap is available"),
                    Err(error) => {
                        capabilities.virtual_keyboard.ok = false;
                        capabilities.virtual_keyboard.detail = format!(
                            "zwp_virtual_keyboard_manager_v1 is available, but the active XKB keymap could not be read: {error:#}"
                        );
                    }
                }
            }
            capabilities
        }
        Err(error) => {
            let detail = format!("failed to inspect the active Wayland connection: {error:#}");
            NiriWaylandCapabilities {
                screencopy: ProtocolCapability {
                    ok: false,
                    detail: detail.clone(),
                },
                virtual_pointer: ProtocolCapability {
                    ok: false,
                    detail: detail.clone(),
                },
                virtual_keyboard: ProtocolCapability { ok: false, detail },
            }
        }
    }
}

pub(super) fn create_named_memfd(name: &str, size: usize, purpose: &str) -> Result<File> {
    let name = CString::new(name).context("Niri memfd name contained a NUL byte")?;
    // SAFETY: `name` is a live NUL-terminated string and no other pointer is passed.
    let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to create {purpose} memfd"));
    }
    // SAFETY: a successful `memfd_create` returns a new owned file descriptor.
    let file = unsafe { File::from_raw_fd(raw_fd) };
    file.set_len(
        u64::try_from(size).with_context(|| format!("{purpose} memfd size is too large"))?,
    )
    .with_context(|| format!("failed to size {purpose} memfd"))?;
    Ok(file)
}

pub(super) fn monotonic_millis() -> u32 {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `timestamp` is writable for the duration of the call.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp) };
    if result != 0 {
        return 0;
    }
    let millis = i128::from(timestamp.tv_sec) * 1_000 + i128::from(timestamp.tv_nsec) / 1_000_000;
    millis as u32
}

#[derive(Debug, Clone)]
struct BoundOutput {
    proxy: wl_output::WlOutput,
    name: Option<String>,
}

#[derive(Debug)]
struct OutputData {
    global_name: u32,
}

#[derive(Debug)]
pub(super) struct KeymapData {
    pub(super) fd: OwnedFd,
    pub(super) size: u32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct BufferOffer {
    pub(super) format: wl_shm::Format,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stride: u32,
}

#[derive(Debug, Default)]
pub(super) struct CaptureEvents {
    pub(super) offer: Option<BufferOffer>,
    pub(super) buffer_done: bool,
    pub(super) y_invert: bool,
    pub(super) ready: bool,
    pub(super) failed: bool,
}

#[derive(Default)]
pub(super) struct ClientState {
    sync_done: u32,
    pub(super) shm: Option<wl_shm::WlShm>,
    pub(super) screencopy_manager: Option<ZwlrScreencopyManagerV1>,
    pub(super) screencopy_version: Option<u32>,
    virtual_pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    virtual_pointer_version: Option<u32>,
    pub(super) virtual_keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
    virtual_keyboard_version: Option<u32>,
    pub(super) seat: Option<wl_seat::WlSeat>,
    outputs: HashMap<u32, BoundOutput>,
    pub(super) keymap: Option<KeymapData>,
    pub(super) capture: Option<CaptureEvents>,
}

pub(super) struct WaylandClient {
    connection: Connection,
    queue: EventQueue<ClientState>,
    pub(super) qh: QueueHandle<ClientState>,
    pub(super) state: ClientState,
    next_sync: u32,
}

impl WaylandClient {
    pub(super) fn connect() -> Result<Self> {
        let connection = Connection::connect_to_env()
            .context("failed to connect to the active Wayland compositor")?;
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        let display = connection.display();
        let _registry = display.get_registry(&qh, ());
        let _initial_sync = display.sync(&qh, 1);
        let mut state = ClientState::default();
        dispatch_until(&mut queue, &mut state, |state| state.sync_done >= 1)?;

        let _bind_sync = display.sync(&qh, 2);
        dispatch_until(&mut queue, &mut state, |state| state.sync_done >= 2)?;
        Ok(Self {
            connection,
            queue,
            qh,
            state,
            next_sync: 3,
        })
    }

    fn capabilities(&self) -> NiriWaylandCapabilities {
        let named_output = self
            .state
            .outputs
            .values()
            .any(|output| output.name.is_some());
        let screencopy_ok = self.state.screencopy_manager.is_some()
            && self.state.screencopy_version.unwrap_or_default() >= 3
            && self.state.shm.is_some()
            && named_output;
        let pointer_ok = self.state.virtual_pointer_manager.is_some()
            && self.state.virtual_pointer_version.unwrap_or_default() >= 2
            && named_output;
        let keyboard_ok =
            self.state.virtual_keyboard_manager.is_some() && self.state.seat.is_some();

        NiriWaylandCapabilities {
            screencopy: capability_detail(
                screencopy_ok,
                "zwlr_screencopy_manager_v1",
                self.state.screencopy_version,
                "version 3, wl_shm, and a named wl_output are required",
            ),
            virtual_pointer: capability_detail(
                pointer_ok,
                "zwlr_virtual_pointer_manager_v1",
                self.state.virtual_pointer_version,
                "version 2 and a named wl_output are required",
            ),
            virtual_keyboard: capability_detail(
                keyboard_ok,
                "zwp_virtual_keyboard_manager_v1",
                self.state.virtual_keyboard_version,
                "wl_seat is also required",
            ),
        }
    }

    pub(super) fn output(&self, name: &str) -> Result<wl_output::WlOutput> {
        self.state
            .outputs
            .values()
            .find(|output| output.name.as_deref() == Some(name))
            .map(|output| output.proxy.clone())
            .with_context(|| format!("Wayland did not advertise the Niri output {name:?}"))
    }

    pub(super) fn create_pointer(&self, output_name: Option<&str>) -> Result<ZwlrVirtualPointerV1> {
        let manager = self
            .state
            .virtual_pointer_manager
            .as_ref()
            .context("Niri did not advertise zwlr_virtual_pointer_manager_v1")?;
        if self.state.virtual_pointer_version.unwrap_or_default() < 2 {
            bail!("Niri virtual pointer manager does not support output-scoped pointers");
        }
        match output_name {
            Some(name) => {
                let output = self.output(name)?;
                Ok(manager.create_virtual_pointer_with_output(None, Some(&output), &self.qh, ()))
            }
            None => Ok(manager.create_virtual_pointer(None, &self.qh, ())),
        }
    }

    pub(super) fn dispatch_until(
        &mut self,
        condition: impl Fn(&ClientState) -> bool,
    ) -> Result<()> {
        dispatch_until(&mut self.queue, &mut self.state, condition)
    }

    fn sync_next(&mut self) -> Result<()> {
        let generation = self.next_sync;
        self.next_sync = self.next_sync.wrapping_add(1).max(3);
        let _sync = self.connection.display().sync(&self.qh, generation);
        self.dispatch_until(|state| state.sync_done >= generation)
    }

    pub(super) fn finish_requests(&mut self) -> Result<()> {
        self.sync_next()
    }

    pub(super) fn request_keyboard_keymap(&mut self) -> Result<()> {
        let seat = self
            .state
            .seat
            .clone()
            .context("Niri did not advertise wl_seat")?;
        let _keyboard = seat.get_keyboard(&self.qh, ());
        self.sync_next()?;
        self.state
            .keymap
            .as_ref()
            .context("Niri did not provide the active wl_keyboard XKB keymap")?;
        Ok(())
    }
}

fn capability_detail(
    ok: bool,
    protocol: &str,
    version: Option<u32>,
    requirement: &str,
) -> ProtocolCapability {
    ProtocolCapability {
        ok,
        detail: if ok {
            format!(
                "{protocol} version {} is available",
                version.unwrap_or_default()
            )
        } else if let Some(version) = version {
            format!("{protocol} version {version} is insufficient; {requirement}")
        } else {
            format!("{protocol} is unavailable; {requirement}")
        },
    }
}

fn dispatch_until(
    queue: &mut EventQueue<ClientState>,
    state: &mut ClientState,
    condition: impl Fn(&ClientState) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + WAYLAND_OPERATION_TIMEOUT;
    loop {
        queue
            .dispatch_pending(state)
            .context("failed to dispatch pending Wayland events")?;
        if condition(state) {
            return Ok(());
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("timed out waiting for Niri Wayland protocol events")?;
        queue.flush().context("failed to flush Wayland requests")?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        poll_readable(guard.connection_fd().as_raw_fd(), remaining)?;
        match guard.read() {
            Ok(_) => {}
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).context("failed to read Wayland events"),
        }
    }
}

fn poll_readable(fd: i32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("timed out waiting for Niri Wayland protocol events")?;
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: `poll_fd` is a valid one-element array for the duration of the call.
        let result = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if result > 0 {
            if poll_fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                bail!("Wayland connection closed while waiting for Niri protocol events");
            }
            return Ok(());
        }
        if result == 0 {
            bail!("timed out waiting for Niri Wayland protocol events");
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("failed to poll the Wayland connection");
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClientState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
                "wl_output" => {
                    let proxy =
                        registry.bind(name, version.min(4), qh, OutputData { global_name: name });
                    state
                        .outputs
                        .insert(name, BoundOutput { proxy, name: None });
                }
                "wl_seat" => state.seat = Some(registry.bind(name, version.min(7), qh, ())),
                "zwlr_screencopy_manager_v1" => {
                    state.screencopy_version = Some(version);
                    state.screencopy_manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.virtual_pointer_version = Some(version);
                    state.virtual_pointer_manager =
                        Some(registry.bind(name, version.min(2), qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.virtual_keyboard_version = Some(version);
                    state.virtual_keyboard_manager =
                        Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.remove(&name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u32> for ClientState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        generation: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.sync_done = state.sync_done.max(*generation);
        }
    }
}

impl Dispatch<wl_output::WlOutput, OutputData> for ClientState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        data: &OutputData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(output) = state.outputs.get_mut(&data.global_name) {
                output.name = Some(name);
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            if matches!(format, WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)) {
                state.keymap = Some(KeymapData { fd, size });
            }
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(capture) = state.capture.as_mut() else {
            return;
        };
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } if matches!(
                format,
                wl_shm::Format::Xrgb8888
                    | wl_shm::Format::Argb8888
                    | wl_shm::Format::Xbgr8888
                    | wl_shm::Format::Abgr8888
            ) =>
            {
                capture.offer = Some(BufferOffer {
                    format,
                    width,
                    height,
                    stride,
                });
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => capture.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                capture.y_invert = matches!(
                    flags,
                    WEnum::Value(flags) if flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert)
                );
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => capture.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => capture.failed = true,
            _ => {}
        }
    }
}

delegate_noop!(ClientState: ignore wl_shm::WlShm);
delegate_noop!(ClientState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(ClientState: ignore wl_buffer::WlBuffer);
delegate_noop!(ClientState: ignore wl_seat::WlSeat);
delegate_noop!(ClientState: ignore ZwlrScreencopyManagerV1);
delegate_noop!(ClientState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(ClientState: ignore ZwlrVirtualPointerV1);
delegate_noop!(ClientState: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(ClientState: ignore ZwpVirtualKeyboardV1);
