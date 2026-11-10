//! Text clipboard (the CLIPBOARD selection) of one X server.
//!
//! X has no clipboard storage: the owner of the selection hands the data to
//! whoever asks. So an unmapped window on its own connection does both
//! sides: XFixes tells it when another client takes the selection, and it
//! asks that owner for the text (UTF8_STRING, in INCR pieces when large);
//! when a client sets text it takes the selection itself and answers the
//! requests of pasting applications.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use remothing_core::platform::{Clipboard, ClipboardSink};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Longest text accepted from another client (INCR transfers are cut here).
const MAX_TEXT: usize = 1 << 20;

struct Atoms {
    clipboard: Atom,
    utf8_string: Atom,
    /// MIME spelling of the same, asked for by some toolkits.
    text_plain_utf8: Atom,
    targets: Atom,
    incr: Atom,
    /// Property on our window where owners deliver the text.
    transfer: Atom,
}

struct Inner {
    conn: RustConnection,
    window: Window,
    atoms: Atoms,
    /// Text we own the selection with (None: someone else owns it).
    text: Mutex<Option<String>>,
    on_change: Mutex<Option<ClipboardSink>>,
    stop: AtomicBool,
}

pub struct X11Clipboard {
    inner: Arc<Inner>,
}

impl X11Clipboard {
    pub fn new(display: &str) -> Result<Self> {
        let (conn, screen) = x11rb::connect(Some(display)).context("connect to X for the clipboard")?;
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            // INCR transfers arrive as changes of the transfer property.
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let atom = |name: &[u8]| -> Result<Atom> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
        let atoms = Atoms {
            clipboard: atom(b"CLIPBOARD")?,
            utf8_string: atom(b"UTF8_STRING")?,
            text_plain_utf8: atom(b"text/plain;charset=utf-8")?,
            targets: atom(b"TARGETS")?,
            incr: atom(b"INCR")?,
            transfer: atom(b"REMOTHING_CLIPBOARD")?,
        };
        conn.xfixes_query_version(5, 0)?.reply().context("XFixes unavailable")?;
        conn.xfixes_select_selection_input(window, atoms.clipboard, SelectionEventMask::SET_SELECTION_OWNER)?;
        conn.flush()?;
        let inner = Arc::new(Inner {
            conn,
            window,
            atoms,
            text: Mutex::new(None),
            on_change: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let thread_inner = inner.clone();
        std::thread::Builder::new().name("clipboard".into()).spawn(move || thread_inner.run())?;
        Ok(Self { inner })
    }
}

impl Clipboard for X11Clipboard {
    fn set_text(&self, text: &str) -> Result<()> {
        let inner = &self.inner;
        *inner.text.lock().unwrap() = Some(text.to_string());
        inner.conn.set_selection_owner(inner.window, inner.atoms.clipboard, x11rb::CURRENT_TIME)?;
        inner.conn.flush()?;
        Ok(())
    }

    fn watch(&self, on_change: ClipboardSink) {
        let inner = &self.inner;
        *inner.on_change.lock().unwrap() = Some(on_change);
        // What is on the clipboard now is the first report (no answer comes
        // when nobody owns the selection).
        let a = &inner.atoms;
        let asked = inner.conn.convert_selection(inner.window, a.clipboard, a.utf8_string, a.transfer, x11rb::CURRENT_TIME);
        if asked.is_ok() {
            let _ = inner.conn.flush();
        }
    }
}

impl Drop for X11Clipboard {
    fn drop(&mut self) {
        // Wakes the thread out of wait_for_event: an event with an empty
        // mask goes to the client that created the window, which is us.
        self.inner.stop.store(true, Ordering::Release);
        let wake = ClientMessageEvent::new(32, self.inner.window, self.inner.atoms.transfer, [0u32; 5]);
        let _ = self.inner.conn.send_event(false, self.inner.window, EventMask::NO_EVENT, wake);
        let _ = self.inner.conn.flush();
    }
}

impl Inner {
    fn run(&self) {
        // Pieces of an INCR transfer in progress.
        let mut incoming: Option<Vec<u8>> = None;
        loop {
            let Ok(event) = self.conn.wait_for_event() else { return };
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            if let Err(e) = self.handle(event, &mut incoming) {
                eprintln!("clipboard: {e:#}");
            }
        }
    }

    fn handle(&self, event: Event, incoming: &mut Option<Vec<u8>>) -> Result<()> {
        let a = &self.atoms;
        match event {
            // Another client copied something: ask it for the text.
            Event::XfixesSelectionNotify(e) if e.owner != self.window && e.owner != x11rb::NONE => {
                *self.text.lock().unwrap() = None;
                self.conn.convert_selection(self.window, a.clipboard, a.utf8_string, a.transfer, e.selection_timestamp)?;
                self.conn.flush()?;
            }
            // Its answer (property NONE: it has no text to offer).
            Event::SelectionNotify(e) if e.property == a.transfer => {
                let reply = self.conn.get_property(false, self.window, a.transfer, AtomEnum::ANY, 0, u32::MAX)?.reply()?;
                // Deleting the property also tells an INCR owner to send the first piece.
                self.conn.delete_property(self.window, a.transfer)?;
                self.conn.flush()?;
                if reply.type_ == a.incr {
                    *incoming = Some(Vec::new());
                } else {
                    self.changed(reply.value);
                }
            }
            Event::PropertyNotify(e) if e.atom == a.transfer && e.state == Property::NEW_VALUE => {
                let Some(text) = incoming else { return Ok(()) };
                let piece = self.conn.get_property(true, self.window, a.transfer, AtomEnum::ANY, 0, u32::MAX)?.reply()?.value;
                self.conn.flush()?;
                if piece.is_empty() {
                    // An empty piece ends the transfer.
                    self.changed(incoming.take().unwrap_or_default());
                } else if text.len() + piece.len() <= MAX_TEXT {
                    text.extend_from_slice(&piece);
                }
            }
            Event::SelectionRequest(e) => self.answer(e)?,
            Event::SelectionClear(_) => *self.text.lock().unwrap() = None,
            _ => {}
        }
        Ok(())
    }

    fn changed(&self, text: Vec<u8>) {
        if let Some(on_change) = self.on_change.lock().unwrap().as_ref() {
            on_change(String::from_utf8_lossy(&text).into_owned());
        }
    }

    /// An application pastes (or asks what we offer) while we own the selection.
    fn answer(&self, e: SelectionRequestEvent) -> Result<()> {
        let a = &self.atoms;
        // Obsolete clients give no property: the target names it.
        let property = if e.property == x11rb::NONE { e.target } else { e.property };
        let text = self.text.lock().unwrap().clone().filter(|_| e.selection == a.clipboard);
        let delivered = match text {
            Some(_) if e.target == a.targets => {
                let offered = [a.targets, a.utf8_string, a.text_plain_utf8];
                self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &offered)?;
                true
            }
            // One request carries it whole (BIG-REQUESTS allows megabytes).
            Some(text)
                if (e.target == a.utf8_string || e.target == a.text_plain_utf8)
                    && text.len() + 64 <= self.conn.maximum_request_bytes() =>
            {
                self.conn.change_property8(PropMode::REPLACE, e.requestor, property, e.target, text.as_bytes())?;
                true
            }
            _ => false,
        };
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if delivered { property } else { x11rb::NONE },
        };
        self.conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
        self.conn.flush()?;
        Ok(())
    }
}
