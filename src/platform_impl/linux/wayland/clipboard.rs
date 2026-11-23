//! Copy and paste through the seat's `wl_data_device`, the one winit also uses for drag and drop.
//!
//! A client should keep one data device per seat. Some compositors, Hyprland among them, send
//! the selection and drags to only one data device of a client, so a toolkit that binds a second
//! device for its clipboard stops receiving the selection once winit binds one for drops. Winit
//! therefore offers the clipboard on its own device.

use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags};
use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
use tracing::{debug, warn};

use sctk::data_device_manager::data_device::DataDeviceData;
use sctk::data_device_manager::data_source::DataSourceData;
use sctk::data_device_manager::WritePipe;
use sctk::primary_selection::{
    device::{PrimarySelectionDevice, PrimarySelectionDeviceHandler},
    selection::{PrimarySelectionSource, PrimarySelectionSourceHandler},
    PrimarySelectionManagerState,
};
use sctk::reexports::client::backend::ObjectId;
use sctk::reexports::client::globals::GlobalList;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::WlDataDeviceManager;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};
use sctk::reexports::protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
};

use crate::platform_impl::wayland::state::WinitState;

/// The text types winit offers, and accepts in this order of preference.
const TEXT_MIME_TYPES: [&str; 5] =
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "TEXT", "STRING"];

/// How long a paste waits for the source to send all of its data before giving up.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// The most bytes read from another client's selection.
const MAX_CLIPBOARD_BYTES: usize = 64 * 1024 * 1024;
const MAX_ACTIVE_SENDS: usize = 4;

/// Every live clipboard, so one can be found from the display handle a toolkit is given.
static CLIPBOARDS: Mutex<Vec<Weak<ClipboardState>>> = Mutex::new(Vec::new());

/// The clipboard of one event loop, shared between the loop and its users.
#[derive(Debug)]
pub struct ClipboardState {
    connection: Connection,
    queue_handle: QueueHandle<WinitState>,
    manager: Option<WlDataDeviceManager>,
    primary_manager: Option<PrimarySelectionManagerState>,
    /// The `wl_display` pointer, to match a raw display handle.
    display: usize,
    /// A marker type offered with our own selection, so we recognise it when it comes back.
    marker: String,
    inner: Mutex<Inner>,
    active_sends: Arc<AtomicUsize>,
}

#[derive(Debug, Default)]
struct Inner {
    seats: Vec<SeatClipboard>,
    /// The seat that last gave one of our windows input.
    active_seat: Option<ObjectId>,
    /// The selection we offer, until another client takes the selection.
    owned: Option<OwnedSelection>,
}

#[derive(Debug)]
struct SeatClipboard {
    seat: ObjectId,
    device: Option<WlDataDevice>,
    primary: Option<PrimarySelectionDevice>,
    // SCTK's PrimarySelectionSource destroys its proxy when dropped. This also covers replacing
    // the selection, a compositor cancellation, and removal of the entire seat.
    owned_primary: Option<(PrimarySelectionSource, Arc<str>)>,
    /// The serial of the latest input event on this seat, which setting the selection needs.
    serial: Option<u32>,
}

#[derive(Debug)]
struct OwnedSelection {
    source: WlDataSource,
    text: Arc<str>,
}

impl ClipboardState {
    pub fn new(
        connection: &Connection,
        queue_handle: &QueueHandle<WinitState>,
        manager: Option<WlDataDeviceManager>,
        globals: &GlobalList,
    ) -> Arc<Self> {
        let display = connection.display().id().as_ptr() as usize;
        let state = Arc::new(Self {
            connection: connection.clone(),
            queue_handle: queue_handle.clone(),
            manager,
            primary_manager: PrimarySelectionManagerState::bind(globals, queue_handle).ok(),
            display,
            marker: format!(
                "application/x-winit-selection;pid={};display={display:x}",
                std::process::id()
            ),
            inner: Mutex::new(Inner::default()),
            active_sends: Arc::default(),
        });

        let mut clipboards = CLIPBOARDS.lock().unwrap();
        clipboards.retain(|clipboard| clipboard.strong_count() > 0);
        clipboards.push(Arc::downgrade(&state));
        state
    }

    /// The clipboard of the event loop connected through this `wl_display`.
    pub fn for_display(display: *mut std::ffi::c_void) -> Option<Arc<Self>> {
        let clipboards = CLIPBOARDS.lock().unwrap();
        clipboards
            .iter()
            .filter_map(Weak::upgrade)
            .find(|clipboard| clipboard.display == display as usize)
    }

    pub fn add_seat(&self, seat: &WlSeat, device: Option<WlDataDevice>) {
        let mut inner = self.inner.lock().unwrap();
        let seat_id = seat.id();
        if inner.seats.iter().any(|entry| entry.seat == seat_id) {
            return;
        }
        let primary = self
            .primary_manager
            .as_ref()
            .map(|manager| manager.get_selection_device(&self.queue_handle, seat));
        inner.seats.push(SeatClipboard {
            seat: seat_id,
            device,
            primary,
            owned_primary: None,
            serial: None,
        });
    }

    pub fn remove_seat(&self, seat: &ObjectId) {
        let mut inner = self.inner.lock().unwrap();
        inner.seats.retain(|entry| &entry.seat != seat);
        if inner.active_seat.as_ref() == Some(seat) {
            inner.active_seat = None;
        }
    }

    /// Records input on a seat: its serial, and that this seat is the one in use.
    pub fn note_input(&self, seat: &ObjectId, serial: u32) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.seats.iter_mut().find(|entry| &entry.seat == seat) {
            entry.serial = Some(serial);
            inner.active_seat = Some(seat.clone());
        }
    }

    /// The text of our own selection for a source the compositor asks to send.
    pub fn owned_text(&self, source: &WlDataSource) -> Option<Arc<str>> {
        let inner = self.inner.lock().unwrap();
        inner.owned.as_ref().filter(|owned| &owned.source == source).map(|owned| owned.text.clone())
    }

    /// Forgets our selection once another client has taken it.
    pub fn source_cancelled(&self, source: &WlDataSource) {
        let mut inner = self.inner.lock().unwrap();
        if inner.owned.as_ref().is_some_and(|owned| &owned.source == source) {
            inner.owned = None;
        }
        source.destroy();
    }

    /// Sends our selection's text through the pipe the compositor gave us.
    pub fn send(&self, source: &WlDataSource, mime_type: &str, pipe: WritePipe) {
        let Some(text) = self.owned_text(source) else { return };
        self.send_text(text, mime_type, pipe);
    }

    fn send_text(&self, text: Arc<str>, mime_type: &str, pipe: WritePipe) {
        if !TEXT_MIME_TYPES.contains(&mime_type) && mime_type != self.marker {
            return;
        }
        let file = File::from(OwnedFd::from(pipe));
        if self
            .active_sends
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < MAX_ACTIVE_SENDS).then(|| count + 1)
            })
            .is_err()
        {
            debug!(
                "Refusing a Wayland clipboard send: {MAX_ACTIVE_SENDS} transfers already active"
            );
            return;
        }
        let permit = SendPermit(self.active_sends.clone());
        // The reader may be slow, or be this very process: write off the event loop.
        let spawned = std::thread::Builder::new().name("winit-clipboard".into()).spawn(move || {
            let _permit = permit;
            if let Err(error) = write_with_timeout(file, text.as_bytes()) {
                if error.kind() != ErrorKind::BrokenPipe {
                    warn!("Failed to send the clipboard: {error}");
                }
            }
        });
        if let Err(error) = spawned {
            warn!("Failed to send the clipboard: {error}");
        }
    }

    fn active_device(inner: &Inner) -> Option<&SeatClipboard> {
        inner
            .active_seat
            .as_ref()
            .and_then(|seat| inner.seats.iter().find(|entry| &entry.seat == seat))
            .or_else(|| inner.seats.first())
    }

    pub fn load_primary_text(&self) -> io::Result<String> {
        let inner = self.inner.lock().map_err(|_| io::Error::other("clipboard lock poisoned"))?;
        let entry =
            Self::active_device(&inner).ok_or_else(|| io::Error::from(ErrorKind::NotFound))?;
        let device =
            entry.primary.as_ref().ok_or_else(|| io::Error::from(ErrorKind::Unsupported))?;
        let owned = entry.owned_primary.as_ref().map(|(_, text)| text);
        let Some(offer) = device.data().selection_offer() else {
            return owned
                .map(|text| text.to_string())
                .ok_or_else(|| io::Error::from(ErrorKind::NotFound));
        };
        let (is_ours, mime) = offer.with_mime_types(|types| {
            (types.iter().any(|mime| mime == &self.marker), preferred_text_type(types))
        });
        if let (true, Some(text)) = (is_ours, owned) {
            return Ok(text.to_string());
        }
        let mime = mime.ok_or_else(|| io::Error::from(ErrorKind::NotFound))?;
        let pipe = offer.receive(mime.to_string())?;
        drop(inner);
        self.read_text(OwnedFd::from(pipe))
    }

    pub fn store_primary_text(&self, text: String) -> io::Result<()> {
        if text.len() > MAX_CLIPBOARD_BYTES {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "the primary selection is too large",
            ));
        }
        let manager =
            self.primary_manager.as_ref().ok_or_else(|| io::Error::from(ErrorKind::Unsupported))?;
        let mut inner =
            self.inner.lock().map_err(|_| io::Error::other("clipboard lock poisoned"))?;
        let seat = Self::active_device(&inner).map(|entry| entry.seat.clone());
        let entry = inner
            .seats
            .iter_mut()
            .find(|entry| Some(&entry.seat) == seat.as_ref())
            .ok_or_else(|| io::Error::from(ErrorKind::NotFound))?;
        let device =
            entry.primary.as_ref().ok_or_else(|| io::Error::from(ErrorKind::Unsupported))?;
        let serial = entry
            .serial
            .ok_or_else(|| io::Error::other("no input to set the primary selection for"))?;
        let types = TEXT_MIME_TYPES.into_iter().chain(std::iter::once(self.marker.as_str()));
        let source = manager.create_selection_source(&self.queue_handle, types);
        source.set_selection(device, serial);
        entry.owned_primary = Some((source, text.into()));
        drop(inner);
        self.connection.flush().map_err(io::Error::other)
    }

    pub fn load_text(&self) -> io::Result<String> {
        let inner = self.inner.lock().unwrap();
        let offer = Self::active_device(&inner)
            .and_then(|entry| entry.device.as_ref()?.data::<DataDeviceData>())
            .and_then(|data| data.selection_offer());

        let Some(offer) = offer else {
            // The compositor hasn't told us about a selection, but one we set is still ours.
            return match &inner.owned {
                Some(owned) => Ok(owned.text.to_string()),
                None => Err(io::Error::new(ErrorKind::NotFound, "the clipboard is empty")),
            };
        };

        let types = offer.with_mime_types(|types| types.to_vec());
        if types.iter().any(|mime| mime == &self.marker) {
            // Our own selection: reading it through the compositor would wait on this thread.
            if let Some(owned) = &inner.owned {
                return Ok(owned.text.to_string());
            }
        }

        let Some(mime) = preferred_text_type(&types) else {
            return Err(io::Error::new(ErrorKind::InvalidData, "the clipboard holds no text"));
        };

        let pipe = offer.receive(mime.to_string()).map_err(|error| match error {
            sctk::data_device_manager::data_offer::DataOfferError::Io(error) => error,
            other => io::Error::new(ErrorKind::Other, other.to_string()),
        })?;
        drop(inner);
        self.read_text(OwnedFd::from(pipe))
    }

    fn read_text(&self, pipe: OwnedFd) -> io::Result<String> {
        // The source only writes once the compositor has our request.
        self.connection.flush().map_err(|error| io::Error::new(ErrorKind::Other, error))?;

        let bytes = read_with_timeout(File::from(pipe), MAX_CLIPBOARD_BYTES)?;
        let text = String::from_utf8_lossy(&bytes);
        // `text/*` types use CRLF line endings, but applications expect LF.
        Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
    }

    /// The first of `mimes` (in the caller's order of preference) that the other client's
    /// selection offers, with its bytes, at most `max_bytes` of them. Our own selection is text
    /// alone, so it never answers here: the caller asks for text with `load_text`.
    pub fn load_mime(&self, mimes: &[&str], max_bytes: usize) -> io::Result<(String, Vec<u8>)> {
        let inner = self.inner.lock().unwrap();
        let offer = Self::active_device(&inner)
            .and_then(|entry| entry.device.as_ref()?.data::<DataDeviceData>())
            .and_then(|data| data.selection_offer());
        let Some(offer) = offer else {
            return Err(io::Error::new(ErrorKind::NotFound, "the clipboard is empty"));
        };

        let types = offer.with_mime_types(|types| types.to_vec());
        if types.iter().any(|mime| mime == &self.marker) {
            return Err(io::Error::new(ErrorKind::NotFound, "the clipboard holds our own text"));
        }
        let Some(mime) = mimes.iter().find(|mime| types.iter().any(|t| t == **mime)) else {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                "the clipboard holds none of those types",
            ));
        };

        let pipe = offer.receive((*mime).to_string()).map_err(|error| match error {
            sctk::data_device_manager::data_offer::DataOfferError::Io(error) => error,
            other => io::Error::new(ErrorKind::Other, other.to_string()),
        })?;
        drop(inner);
        self.connection.flush().map_err(|error| io::Error::new(ErrorKind::Other, error))?;

        let bytes =
            read_with_timeout(File::from(OwnedFd::from(pipe)), max_bytes.min(MAX_CLIPBOARD_BYTES))?;
        Ok(((*mime).to_string(), bytes))
    }

    /// Whether the other client's selection offers any of `mimes`. Nothing is requested from the
    /// source: only the types the compositor announced are looked at.
    pub fn offers(&self, mimes: &[&str]) -> bool {
        let inner = self.inner.lock().unwrap();
        let offer = Self::active_device(&inner)
            .and_then(|entry| entry.device.as_ref()?.data::<DataDeviceData>())
            .and_then(|data| data.selection_offer());
        let Some(offer) = offer else { return false };
        let types = offer.with_mime_types(|types| types.to_vec());
        if types.iter().any(|mime| mime == &self.marker) {
            return false;
        }
        mimes.iter().any(|mime| types.iter().any(|t| t == *mime))
    }

    pub fn store_text(&self, text: String) -> io::Result<()> {
        let Some(manager) = &self.manager else {
            return Err(io::Error::new(ErrorKind::Unsupported, "no data device manager"));
        };
        let mut inner = self.inner.lock().unwrap();
        let Some(entry) = Self::active_device(&inner) else {
            return Err(io::Error::new(ErrorKind::NotFound, "no seat"));
        };
        let Some(serial) = entry.serial else {
            return Err(io::Error::new(ErrorKind::Other, "no input to set the clipboard for"));
        };

        let device =
            entry.device.as_ref().ok_or_else(|| io::Error::from(ErrorKind::Unsupported))?;
        let source = manager.create_data_source(&self.queue_handle, DataSourceData::default());
        for mime in TEXT_MIME_TYPES {
            source.offer(mime.to_owned());
        }
        source.offer(self.marker.clone());
        device.set_selection(Some(&source), serial);

        if let Some(previous) = inner.owned.replace(OwnedSelection { source, text: text.into() }) {
            previous.source.destroy();
        }
        drop(inner);
        self.connection.flush().map_err(|error| io::Error::new(ErrorKind::Other, error))
    }
}

impl PrimarySelectionDeviceHandler for WinitState {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionDeviceV1,
    ) {
        // SCTK already recorded the offer on its device.
    }
}

impl PrimarySelectionSourceHandler for WinitState {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        mime: String,
        pipe: WritePipe,
    ) {
        let text = self.clipboard.inner.lock().ok().and_then(|inner| {
            inner
                .seats
                .iter()
                .filter_map(|seat| seat.owned_primary.as_ref())
                .find(|(owned, _)| owned.inner() == source)
                .map(|(_, text)| text.clone())
        });
        if let Some(text) = text {
            self.clipboard.send_text(text, &mime, pipe);
        }
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
    ) {
        if let Ok(mut inner) = self.clipboard.inner.lock() {
            if let Some(entry) = inner.seats.iter_mut().find(|seat| {
                seat.owned_primary.as_ref().is_some_and(|(owned, _)| owned.inner() == source)
            }) {
                entry.owned_primary = None;
            }
        }
    }
}

sctk::delegate_primary_selection!(WinitState);

fn preferred_text_type(types: &[String]) -> Option<&'static str> {
    TEXT_MIME_TYPES.into_iter().find(|mime| types.iter().any(|offered| offered == mime))
}

impl Drop for ClipboardState {
    fn drop(&mut self) {
        if let Some(owned) = self.inner.get_mut().unwrap().owned.take() {
            owned.source.destroy();
        }
    }
}

/// Reads a pipe to its end, giving up if it isn't all there within [`READ_TIMEOUT`] of the start.
fn read_with_timeout(file: File, max_bytes: usize) -> io::Result<Vec<u8>> {
    read_within(file, max_bytes, READ_TIMEOUT)
}

fn read_within(mut file: File, max_bytes: usize, limit: Duration) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + limit;
    nonblocking(&file)?;

    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        remaining(deadline)?;
        match file.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(read) => {
                if bytes.len().saturating_add(read) > max_bytes {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "the clipboard is too large",
                    ));
                }
                if let Some(read_bytes) = chunk.get(..read) {
                    bytes.extend_from_slice(read_bytes);
                }
            },
            Err(error) if error.kind() == ErrorKind::Interrupted => {},
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                wait_for_pipe(&file, PollFlags::IN, deadline)?;
            },
            Err(error) => return Err(error),
        }
    }
}

struct SendPermit(Arc<AtomicUsize>);

impl Drop for SendPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn nonblocking(file: &File) -> io::Result<()> {
    let flags = fcntl_getfl(file)?;
    Ok(fcntl_setfl(file, flags | OFlags::NONBLOCK)?)
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::Error::new(ErrorKind::TimedOut, "clipboard transfer deadline expired"));
    }
    Ok(left)
}

fn wait_for_pipe(file: &File, flags: PollFlags, deadline: Instant) -> io::Result<()> {
    use rustix::io::Errno;
    let millis = remaining(deadline)?.as_millis().clamp(1, i32::MAX as u128) as i32;
    match poll(&mut [PollFd::new(file, flags)], millis) {
        Ok(_) | Err(Errno::INTR) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn write_with_timeout(file: File, bytes: &[u8]) -> io::Result<()> {
    write_within(file, bytes, READ_TIMEOUT)
}

fn write_within(mut file: File, mut bytes: &[u8], limit: Duration) -> io::Result<()> {
    let deadline = Instant::now() + limit;
    nonblocking(&file)?;
    while !bytes.is_empty() {
        remaining(deadline)?;
        match file.write(bytes) {
            Ok(0) => return Err(io::Error::from(ErrorKind::WriteZero)),
            Ok(written) => {
                bytes =
                    bytes.get(written..).ok_or_else(|| io::Error::from(ErrorKind::InvalidData))?
            },
            Err(error) if error.kind() == ErrorKind::Interrupted => {},
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                wait_for_pipe(&file, PollFlags::OUT, deadline)?
            },
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "clipboard_tests.rs"]
mod tests;
