//! Clipboard (the CLIPBOARD selection) of one X server: text, pictures
//! (`image/png`), and the files a file manager copies (`text/uri-list`,
//! read only).
//!
//! X has no clipboard storage: the owner of the selection hands the data to
//! whoever asks. So an unmapped window on its own connection does both
//! sides: XFixes tells it when another client takes the selection, and it
//! asks that owner what it offers (TARGETS), then for the file list, else
//! the text (UTF8_STRING), else the picture, in INCR pieces when large;
//! when a client sets text or a picture it takes the selection itself and
//! answers the requests of pasting applications, in INCR pieces what does
//! not fit one request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use remothing_core::capture::Clip;
use remothing_core::files::file_uri_path;
use remothing_core::platform::{Clipboard, ClipboardSink, Copied, MAX_IMAGE};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, Property, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Longest text (or file list) accepted from another client (INCR transfers are cut here).
const MAX_TEXT: usize = 1 << 20;
/// Size of the pieces an INCR transfer to a pasting client goes in.
const PIECE: usize = 256 << 10;

struct Atoms {
    clipboard: Atom,
    utf8_string: Atom,
    /// MIME spelling of the same, asked for by some toolkits.
    text_plain_utf8: Atom,
    png: Atom,
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
    /// What we own the selection with (None: someone else owns it).
    owned: Mutex<Option<Clip>>,
    on_change: Mutex<Option<ClipboardSink>>,
    /// The next text received is what was there when watching started.
    found: AtomicBool,
    stop: AtomicBool,
}

/// An INCR transfer from the owner: its target and the pieces so far.
struct Incoming {
    target: Atom,
    data: Vec<u8>,
    /// More came than is accepted: text is cut, a picture dropped.
    cut: bool,
}

/// An INCR transfer to a pasting client: the next piece goes when it
/// deleted the last one from `property` of `requestor`.
struct Outgoing {
    requestor: Window,
    property: Atom,
    target: Atom,
    data: Clip,
    sent: usize,
}

#[derive(Default)]
struct Transfers {
    incoming: Option<Incoming>,
    outgoing: Vec<Outgoing>,
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
            png: atom(b"image/png")?,
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
            owned: Mutex::new(None),
            on_change: Mutex::new(None),
            found: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let thread_inner = inner.clone();
        std::thread::Builder::new().name("clipboard".into()).spawn(move || thread_inner.run())?;
        Ok(Self { inner })
    }

    /// Takes the selection, to answer pasting clients with `clip`.
    fn own(&self, clip: Clip) -> Result<()> {
        let inner = &self.inner;
        *inner.owned.lock().unwrap() = Some(clip);
        inner.conn.set_selection_owner(inner.window, inner.atoms.clipboard, x11rb::CURRENT_TIME)?;
        // A round trip: the X server has made us the owner before a paste
        // pressed next (on the input connection) asks the owner.
        inner.conn.get_selection_owner(inner.atoms.clipboard)?.reply()?;
        Ok(())
    }
}

impl Clipboard for X11Clipboard {
    fn set_text(&self, text: &str) -> Result<()> {
        self.own(Clip::Text(Arc::from(text)))
    }

    fn set_image(&self, png: &[u8]) -> Result<()> {
        self.own(Clip::Image(Arc::from(png)))
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
        let mut transfers = Transfers::default();
        loop {
            let Ok(event) = self.conn.wait_for_event() else { return };
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            if let Err(e) = self.handle(event, &mut transfers) {
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

    fn handle(&self, event: Event, transfers: &mut Transfers) -> Result<()> {
        let a = &self.atoms;
        match event {
            // Another client copied something: ask it what it offers.
            Event::XfixesSelectionNotify(e) if e.owner != self.window && e.owner != x11rb::NONE => {
                *self.owned.lock().unwrap() = None;
                self.ask(a.targets, e.selection_timestamp)?;
            }
            // Its answer.
            Event::SelectionNotify(e) if e.property == a.transfer => {
                let reply = self.conn.get_property(false, self.window, a.transfer, AtomEnum::ANY, 0, u32::MAX)?.reply()?;
                // Deleting the property also tells an INCR owner to send the first piece.
                self.conn.delete_property(self.window, a.transfer)?;
                self.conn.flush()?;
                if reply.type_ == a.incr {
                    transfers.incoming = Some(Incoming { target: e.target, data: Vec::new(), cut: false });
                } else if e.target == a.targets {
                    // Files (a file manager's copy) when offered, else text,
                    // else a picture: an office program offers a picture of
                    // the text it copies besides it.
                    let offered = reply.value32().map(|atoms| atoms.collect::<Vec<_>>()).unwrap_or_default();
                    let text = [a.utf8_string, a.text_plain_utf8, AtomEnum::STRING.into()].iter().any(|t| offered.contains(t));
                    let target = if offered.contains(&a.uri_list) {
                        a.uri_list
                    } else if !text && offered.contains(&a.png) {
                        a.png
                    } else {
                        a.utf8_string
                    };
                    self.ask(target, e.time)?;
                } else {
                    self.received(e.target, reply.value, e.time)?;
                }
            }
            // An owner too old to list its targets may still give text.
            Event::SelectionNotify(e) if e.target == a.targets => self.ask(a.utf8_string, e.time)?,
            // Nothing to give, or no owner: then the first report is that nothing is there.
            Event::SelectionNotify(_) => self.found.store(false, Ordering::Relaxed),
            Event::PropertyNotify(e) if e.window == self.window && e.atom == a.transfer && e.state == Property::NEW_VALUE => {
                let Some(incoming) = &mut transfers.incoming else { return Ok(()) };
                let piece = self.conn.get_property(true, self.window, a.transfer, AtomEnum::ANY, 0, u32::MAX)?.reply()?.value;
                self.conn.flush()?;
                let limit = if incoming.target == a.png { MAX_IMAGE } else { MAX_TEXT };
                if piece.is_empty() {
                    // An empty piece ends the transfer.
                    let Incoming { target, data, cut } = transfers.incoming.take().unwrap();
                    if cut && target == a.png {
                        eprintln!("clipboard: a picture of more than {MAX_IMAGE} bytes was copied: not taken");
                        self.found.store(false, Ordering::Relaxed);
                    } else {
                        self.received(target, data, e.time)?;
                    }
                } else if incoming.data.len() + piece.len() <= limit {
                    incoming.data.extend_from_slice(&piece);
                } else {
                    incoming.cut = true;
                }
            }
            // A pasting client took the last piece of an INCR transfer from us.
            Event::PropertyNotify(e) if e.window != self.window && e.state == Property::DELETE => {
                let Some(at) = transfers.outgoing.iter().position(|o| o.requestor == e.window && o.property == e.atom) else {
                    return Ok(());
                };
                let out = &mut transfers.outgoing[at];
                let piece = &out.data.bytes()[out.sent..(out.sent + PIECE).min(out.data.bytes().len())];
                // The empty piece after the last one ends it.
                self.conn.change_property8(PropMode::REPLACE, out.requestor, out.property, out.target, piece)?;
                out.sent += piece.len();
                if piece.is_empty() {
                    let done = transfers.outgoing.swap_remove(at);
                    self.conn.change_window_attributes(done.requestor, &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT))?;
                }
                self.conn.flush()?;
            }
            Event::SelectionRequest(e) => self.answer(e, &mut transfers.outgoing)?,
            Event::SelectionClear(_) => *self.owned.lock().unwrap() = None,
            _ => {}
        }
        Ok(())
    }

    /// The owner's text, picture, or its `text/uri-list`: one URI a line,
    /// `#` lines are comments. A list without local files is asked for as text.
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
        } else if target == self.atoms.png {
            Copied::Image(data)
        } else {
            Copied::Text(String::from_utf8_lossy(&data).into_owned())
        };
        let changed = !self.found.swap(false, Ordering::Relaxed);
        if let Some(on_change) = self.on_change.lock().unwrap().as_ref() {
            on_change(copied, changed);
        }
        Ok(())
    }

    /// An application pastes (or asks what we offer) while we own the
    /// selection. What does not fit one request starts an INCR transfer.
    fn answer(&self, e: SelectionRequestEvent, outgoing: &mut Vec<Outgoing>) -> Result<()> {
        let a = &self.atoms;
        // Obsolete clients give no property: the target names it.
        let property = if e.property == x11rb::NONE { e.target } else { e.property };
        let owned = self.owned.lock().unwrap().clone().filter(|_| e.selection == a.clipboard);
        let given = match &owned {
            Some(Clip::Text(_)) if e.target == a.targets => {
                let offered = [a.targets, a.utf8_string, a.text_plain_utf8];
                self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &offered)?;
                None
            }
            Some(Clip::Image(_)) if e.target == a.targets => {
                self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &[a.targets, a.png])?;
                None
            }
            Some(text @ Clip::Text(_)) if e.target == a.utf8_string || e.target == a.text_plain_utf8 => Some(text),
            Some(png @ Clip::Image(_)) if e.target == a.png => Some(png),
            _ => None,
        };
        let delivered = match given {
            // One request carries it whole (BIG-REQUESTS allows megabytes).
            Some(clip) if clip.bytes().len() + 64 <= self.conn.maximum_request_bytes() => {
                self.conn.change_property8(PropMode::REPLACE, e.requestor, property, e.target, clip.bytes())?;
                true
            }
            Some(clip) => {
                // The size first, as the type INCR; the pieces follow as the
                // client deletes each one, which we hear of on its window.
                self.conn.change_window_attributes(e.requestor, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE))?;
                self.conn.change_property32(PropMode::REPLACE, e.requestor, property, a.incr, &[clip.bytes().len() as u32])?;
                outgoing.retain(|o| o.requestor != e.requestor || o.property != property);
                outgoing.push(Outgoing { requestor: e.requestor, property, target: e.target, data: clip.clone(), sent: 0 });
                true
            }
            None => e.target == a.targets && owned.is_some(),
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
