//! Clipboard (the CLIPBOARD selection) of one X server: text, and the
//! files a file manager copies (`text/uri-list`, read only).
//!
//! X has no clipboard storage: the owner of the selection hands the data to
//! whoever asks. So an unmapped window on its own connection does both
//! sides: XFixes tells it when another client takes the selection, and it
//! asks that owner what it offers (TARGETS), then for the file list or else
//! the text (UTF8_STRING), in INCR pieces when large;
//! when a client sets text it takes the selection itself and answers the
//! requests of pasting applications.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use remothing_core::files::file_uri_path;
use remothing_core::platform::{Clipboard, ClipboardSink, Copied};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Longest text (or file list) accepted from another client (INCR transfers are cut here).
const MAX_TEXT: usize = 1 << 20;

struct Atoms {
    clipboard: Atom,
    utf8_string: Atom,
    /// MIME spelling of the same, asked for by some toolkits.
    text_plain_utf8: Atom,
    targets: Atom,
    incr: Atom,
    /// What file managers offer for copied files.
    uri_list: Atom,
    /// Property on our window where owners deliver what we ask for.
    transfer: Atom,
}

struct Inner {
    conn: RustConnection,
    window: Window,
    atoms: Atoms,
    /// Text we own the selection with (None: someone else owns it).
    text: Mutex<Option<String>>,
    on_change: Mutex<Option<ClipboardSink>>,
    /// The next text received is what was there when watching started.
    found: AtomicBool,
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
            uri_list: atom(b"text/uri-list")?,
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
            found: AtomicBool::new(false),
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
        // A round trip: the X server has made us the owner before a paste
        // pressed next (on the input connection) asks the owner.
        inner.conn.get_selection_owner(inner.atoms.clipboard)?.reply()?;
        Ok(())
    }

    fn watch(&self, on_change: ClipboardSink) {
        let inner = &self.inner;
        *inner.on_change.lock().unwrap() = Some(on_change);
        // What is on the clipboard now is the first report; with no owner
        // the answers are SelectionNotify events without a property.
        inner.found.store(true, Ordering::Relaxed);
        if let Err(e) = inner.ask(inner.atoms.targets, x11rb::CURRENT_TIME) {
            eprintln!("clipboard: {e:#}");
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
        // An INCR transfer in progress: its target and the pieces so far.
        let mut incoming: Option<(Atom, Vec<u8>)> = None;
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

    /// Asks the owner of the selection for `target`, delivered into `transfer`.
    fn ask(&self, target: Atom, time: u32) -> Result<()> {
        self.conn.convert_selection(self.window, self.atoms.clipboard, target, self.atoms.transfer, time)?;
        self.conn.flush()?;
        Ok(())
    }

    fn handle(&self, event: Event, incoming: &mut Option<(Atom, Vec<u8>)>) -> Result<()> {
        let a = &self.atoms;
        match event {
            // Another client copied something: ask it what it offers.
            Event::XfixesSelectionNotify(e) if e.owner != self.window && e.owner != x11rb::NONE => {
                *self.text.lock().unwrap() = None;
                self.ask(a.targets, e.selection_timestamp)?;
            }
            // Its answer.
            Event::SelectionNotify(e) if e.property == a.transfer => {
                let reply = self.conn.get_property(false, self.window, a.transfer, AtomEnum::ANY, 0, u32::MAX)?.reply()?;
                // Deleting the property also tells an INCR owner to send the first piece.
                self.conn.delete_property(self.window, a.transfer)?;
                self.conn.flush()?;
                if reply.type_ == a.incr {
                    *incoming = Some((e.target, Vec::new()));
                } else if e.target == a.targets {
                    // Files (a file manager's copy) when offered, else text.
                    let offered = reply.value32().map(|atoms| atoms.collect::<Vec<_>>()).unwrap_or_default();
                    self.ask(if offered.contains(&a.uri_list) { a.uri_list } else { a.utf8_string }, e.time)?;
                } else {
                    self.received(e.target, reply.value, e.time)?;
                }
            }
            // An owner too old to list its targets may still give text.
            Event::SelectionNotify(e) if e.target == a.targets => self.ask(a.utf8_string, e.time)?,
            // Nothing to give, or no owner: then the first report is that nothing is there.
            Event::SelectionNotify(_) => self.found.store(false, Ordering::Relaxed),
            Event::PropertyNotify(e) if e.atom == a.transfer && e.state == Property::NEW_VALUE => {
                let Some((target, data)) = incoming else { return Ok(()) };
                let piece = self.conn.get_property(true, self.window, a.transfer, AtomEnum::ANY, 0, u32::MAX)?.reply()?.value;
                self.conn.flush()?;
                if piece.is_empty() {
                    // An empty piece ends the transfer.
                    let target = *target;
                    let data = incoming.take().map(|(_, data)| data).unwrap_or_default();
                    self.received(target, data, e.time)?;
                } else if data.len() + piece.len() <= MAX_TEXT {
                    data.extend_from_slice(&piece);
                }
            }
            Event::SelectionRequest(e) => self.answer(e)?,
            Event::SelectionClear(_) => *self.text.lock().unwrap() = None,
            _ => {}
        }
        Ok(())
    }

    /// The owner's text, or its `text/uri-list`: one URI a line, `#` lines
    /// are comments. A list without local files is asked for as text.
    fn received(&self, target: Atom, data: Vec<u8>, time: u32) -> Result<()> {
        let copied = if target == self.atoms.uri_list {
            let files: Vec<_> = String::from_utf8_lossy(&data)
                .lines()
                .filter(|line| !line.starts_with('#'))
                .filter_map(file_uri_path)
                .collect();
            if files.is_empty() {
                return self.ask(self.atoms.utf8_string, time);
            }
            Copied::Files(files)
        } else {
            Copied::Text(String::from_utf8_lossy(&data).into_owned())
        };
        let changed = !self.found.swap(false, Ordering::Relaxed);
        if let Some(on_change) = self.on_change.lock().unwrap().as_ref() {
            on_change(copied, changed);
        }
        Ok(())
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
