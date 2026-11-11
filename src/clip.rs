//! The clipboard channel (CLIPRDR), text and pictures both ways (design.md "剪贴板"; keel-rdp
//! clip.rs is the model, minus the guest agent and files; pictures: clipimg.rs):
//!
//! - Mac → client: a thread watches NSPasteboard's changeCount; a new string there is offered
//!   as CF_UNICODETEXT (+ CF_TEXT), a picture as CF_DIB + "PNG"; the client asks for the data
//!   when the user pastes.
//! - Client → server: the client's format list arrives (on_remote_copy); text is taken first,
//!   else a picture ("PNG", then CF_DIBV5 / CF_DIB). We fetch it at once (SendInitiatePaste)
//!   and put it on NSPasteboard, so a paste on the Mac finds it. What we put there is
//!   remembered by changeCount so the watcher does not offer it back.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse,
    FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::impl_as_any;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

struct Inner {
    sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    /// the client's CLIPRDR is up (on_ready)
    ready: AtomicBool,
    /// NSPasteboard.changeCount last seen by the watcher, or produced by our own write
    seen: AtomicIsize,
    /// the format we asked the client for (on_remote_copy), answered in on_format_data_response
    asked: Mutex<Option<(ClipboardFormatId, Kind)>>,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Text,
    Png,
    Dib,
}

/// The id we give the registered "PNG" format in our own format lists (any id from 0xC000 up;
/// the client maps it by name).
fn png_id() -> ClipboardFormatId {
    ClipboardFormatId::new(0xC0F0)
}

/// One connection's clipboard; the watcher thread holds a Weak and ends with it.
#[derive(Clone)]
pub struct Clip(Arc<Inner>);

pub struct Factory(Clip);

/// The Mac's general pasteboard text, if any.
fn pasteboard_string() -> Option<String> {
    let pb = NSPasteboard::generalPasteboard();
    let s = unsafe { pb.stringForType(NSPasteboardTypeString) }?;
    Some(s.to_string())
}

fn change_count() -> isize {
    NSPasteboard::generalPasteboard().changeCount()
}

impl Clip {
    pub fn new() -> (Self, Factory) {
        let c = Clip(Arc::new(Inner {
            sender: Mutex::new(None),
            ready: AtomicBool::new(false),
            seen: AtomicIsize::new(change_count()),
            asked: Mutex::new(None),
        }));
        let weak = Arc::downgrade(&c.0);
        std::thread::Builder::new()
            .name("clip-watch".into())
            .spawn(move || watch(weak))
            .expect("clipboard watcher thread");
        (c.clone(), Factory(c))
    }

    fn send(&self, m: ClipboardMessage) {
        if let Some(tx) = &*self.0.sender.lock().unwrap() {
            let _ = tx.send(ServerEvent::Clipboard(m));
        }
    }

    /// Offer the Mac's current text to the client (its "copy" from our side).
    fn offer(&self) {
        if !self.0.ready.load(Ordering::Relaxed) {
            return;
        }
        let mut formats = Vec::new();
        if pasteboard_string().is_some() {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_TEXT));
        }
        if crate::clipimg::mac_has_image() {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
            formats.push(ClipboardFormat::new(png_id()).with_name(ClipboardFormatName::new_static("PNG")));
        }
        if !formats.is_empty() {
            debug!(n = formats.len(), "CLIPRDR: offering the Mac's clipboard");
            self.send(ClipboardMessage::SendInitiateCopy(formats));
        }
    }

    /// The client's text goes onto the Mac's pasteboard; our own change is remembered.
    fn put(&self, text: &str) {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        if !unsafe { pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString) } {
            warn!("CLIPRDR: NSPasteboard refused the text");
        }
        self.0.seen.store(change_count(), Ordering::Relaxed);
        info!(chars = text.chars().count(), "CLIPRDR: the client's text is on the Mac's clipboard");
    }

    /// The client's picture goes onto the Mac's pasteboard (PNG + TIFF).
    fn put_image(&self, data: &[u8], kind: Kind) {
        let count = match kind {
            Kind::Png => crate::clipimg::put_png(data),
            _ => crate::clipimg::put_dib(data),
        };
        match count {
            Some(c) => {
                self.0.seen.store(c, Ordering::Relaxed);
                info!(bytes = data.len(), ?kind, "CLIPRDR: the client's picture is on the Mac's clipboard");
            }
            None => warn!(bytes = data.len(), ?kind, "CLIPRDR: could not take the client's picture"),
        }
    }
}

/// Polls NSPasteboard: a change that is not ours is offered to the client.
fn watch(weak: Weak<Inner>) {
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let Some(inner) = weak.upgrade() else { return };
        let now = change_count();
        if now != inner.seen.load(Ordering::Relaxed) {
            inner.seen.store(now, Ordering::Relaxed);
            Clip(inner).offer();
        }
    }
}

impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.0 .0.sender.lock().unwrap() = Some(sender);
    }
}

impl CliprdrBackendFactory for Factory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(Backend(self.0.clone()))
    }
}

impl CliprdrServerFactory for Factory {}

struct Backend(Clip);

impl_as_any!(Backend);

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clipboard").field("ready", &self.0 .0.ready.load(Ordering::Relaxed)).finish()
    }
}

/// UTF-16LE with the terminating NUL (CF_UNICODETEXT), or bytes with NUL (CF_TEXT: UTF-8
/// here; Windows reads it as the ANSI code page, so ASCII is what survives — the client
/// prefers CF_UNICODETEXT anyway).
fn encode(text: &str, format: ClipboardFormatId) -> Option<Vec<u8>> {
    match format {
        ClipboardFormatId::CF_UNICODETEXT => {
            let mut b: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
            b.extend_from_slice(&[0, 0]);
            Some(b)
        }
        ClipboardFormatId::CF_TEXT | ClipboardFormatId::CF_OEMTEXT => {
            let mut b = text.as_bytes().to_vec();
            b.push(0);
            Some(b)
        }
        _ => None,
    }
}

fn decode(data: &[u8], format: ClipboardFormatId) -> Option<String> {
    match format {
        ClipboardFormatId::CF_UNICODETEXT => {
            let u: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
            Some(String::from_utf16_lossy(&u))
        }
        ClipboardFormatId::CF_TEXT | ClipboardFormatId::CF_OEMTEXT => {
            let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
            Some(String::from_utf8_lossy(&data[..end]).into_owned())
        }
        _ => None,
    }
}

impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        info!("CLIPRDR: ready");
        self.0 .0.ready.store(true, Ordering::Relaxed);
        self.0 .0.seen.store(change_count(), Ordering::Relaxed);
        // what is on the Mac's clipboard now is pasteable on the client at once
        self.0.offer();
    }

    fn on_request_format_list(&mut self) {
        self.0.offer();
    }

    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
        let has = |id: ClipboardFormatId| formats.iter().any(|f| f.id() == id);
        let text = [ClipboardFormatId::CF_UNICODETEXT, ClipboardFormatId::CF_TEXT, ClipboardFormatId::CF_OEMTEXT]
            .into_iter()
            .find(|w| has(*w))
            .map(|id| (id, Kind::Text));
        let png = formats
            .iter()
            .find(|f| f.name().is_some_and(|n| n.value().eq_ignore_ascii_case("PNG")))
            .map(|f| (f.id(), Kind::Png));
        let dib = [ClipboardFormatId::CF_DIBV5, ClipboardFormatId::CF_DIB].into_iter().find(|w| has(*w)).map(|id| (id, Kind::Dib));
        match text.or(png).or(dib) {
            Some((id, kind)) => {
                debug!(?id, ?kind, "CLIPRDR: the client copied something we take, fetching it");
                *self.0 .0.asked.lock().unwrap() = Some((id, kind));
                self.0.send(ClipboardMessage::SendInitiatePaste(id));
            }
            None => debug!(n = formats.len(), "CLIPRDR: the client copied nothing we take (files?)"),
        }
    }

    fn on_format_data_request(&mut self, r: FormatDataRequest) {
        let reply = match r.format {
            ClipboardFormatId::CF_DIB => crate::clipimg::mac_dib(),
            f if f == png_id() => crate::clipimg::mac_png(),
            f => pasteboard_string().and_then(|t| encode(&t, f)),
        };
        self.0.send(ClipboardMessage::SendFormatData(match reply {
            Some(b) => FormatDataResponse::new_data(b),
            None => FormatDataResponse::new_error(),
        }));
    }

    fn on_format_data_response(&mut self, r: FormatDataResponse<'_>) {
        let Some((id, kind)) = self.0 .0.asked.lock().unwrap().take() else { return };
        if r.is_error() {
            debug!("CLIPRDR: the client had no data after all");
            return;
        }
        match kind {
            Kind::Text => match decode(r.data(), id) {
                Some(t) => self.0.put(&t),
                None => debug!(?id, "CLIPRDR: data in a format we do not take"),
            },
            Kind::Png | Kind::Dib => self.0.put_image(r.data(), kind),
        }
    }

    fn on_file_contents_request(&mut self, _: FileContentsRequest) {
        debug!("CLIPRDR: files are not shared");
    }

    fn on_file_contents_response(&mut self, _: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _: LockDataId) {}

    fn on_unlock(&mut self, _: LockDataId) {}
}
