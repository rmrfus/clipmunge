//! The ext-data-control-v1 plumbing: watch the selection, read it, put a new
//! one back.
//!
//! This protocol is what makes clipmunge possible at all. A data source may
//! advertise several MIME types and answer each `send` with different bytes,
//! so the plain text can stay a bare token while text/html carries a link.
//! wl-copy cannot do that - it serves the same buffer for every type it
//! advertises - and upstream has declined twice (wl-clipboard#71, #248).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::pipe::{PipeFlags, pipe_with};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

use crate::selection::{MARKER_MIME, RICH_MIMES, Selection, TEXT_MIMES, dump};

/// Refuse to slurp a selection larger than this. Text rules do not need more,
/// and an unbounded read is a way for any application to make the daemon eat
/// all of memory. Image flavours will need their own, much larger, budget.
const READ_LIMIT: usize = 256 * 1024;
/// Per flavour, and then a ceiling on the lot. The event loop is blocked for
/// the whole of a read - nothing else is dispatched - so a selection that
/// advertises a dozen flavours must not be able to buy a dozen timeouts.
const READ_TIMEOUT: Duration = Duration::from_millis(500);
const READ_BUDGET: Duration = Duration::from_millis(1000);
/// How long to keep pushing bytes at a client that asked for them. See
/// `send_payload`; the number only matters when something is wrong.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Flavours worth the pipe round trip.
///
/// A LibreOffice selection advertises a dozen private types, some of them
/// megabytes, and nothing above this layer can look at any of them: a handler
/// is given capture groups from the plain text, and `is_plain_only` reads the
/// announced MIME list rather than any content. Reading the rest is pure
/// latency on every copy.
///
/// The rich types are in here rather than just the text family so that the
/// day a rule wants to see the incoming HTML, the bytes are already there and
/// only the handler API has to change.
fn worth_reading(mime: &str) -> bool {
    TEXT_MIMES.contains(&mime) || RICH_MIMES.contains(&mime)
}

/// What a rule produced: the new selection, and optionally something to tell
/// the user about it.
pub struct Rewrite {
    pub selection: Selection,
    pub notify: Option<String>,
}

/// What a rule engine has to provide. Returning None means "not mine, leave
/// the clipboard alone".
pub trait Rewriter {
    fn rewrite(&mut self, incoming: &Selection) -> Option<Rewrite>;

    /// Called only after the rewrite actually reached the clipboard. Notifying
    /// about a result we then dropped would be a lie, and results do get
    /// dropped when the clipboard moves on mid-rule.
    fn notify(&self, _text: &str) {}

    /// Whether an offer advertising these flavours is one to leave alone.
    ///
    /// Asked of the *announced* list, because the answer has to arrive before
    /// anything is read: once the bytes are in the process they are in the
    /// journal too, for anybody running with `--debug`. That is the whole
    /// point of the guard, so it cannot be a rule's decision - a handler is
    /// only reached after the text has already been pulled out of the pipe.
    fn is_secret(&self, _mimes: &[String]) -> bool {
        false
    }
}

pub struct Clipboard {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    /// Log what went in and what came out, in full. Off unless asked for:
    /// this writes the clipboard to the log, passwords included.
    log_contents: bool,
}

#[derive(Default)]
struct State {
    manager: Option<ExtDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    device: Option<ExtDataControlDeviceV1>,

    /// Offers announced but not yet claimed by a `selection` event, keyed by
    /// object id: the proxy, and the MIME types named for it so far. The proxy
    /// is kept because an offer we drop on the floor has to be destroyed -
    /// see the `Selection` arm.
    pending: HashMap<u32, (ExtDataControlOfferV1, Vec<String>)>,
    /// The offer the compositor last told us is the selection.
    current: Option<(ExtDataControlOfferV1, Vec<String>)>,

    /// Our live source, kept alive so the compositor can still call back for
    /// reads, plus the bytes to answer those calls with.
    source: Option<ExtDataControlSourceV1>,
    payload: Selection,

    /// Bumped on every selection event. A rewrite computed for generation N is
    /// dropped if the clipboard has moved on: today's text rules are
    /// microseconds, but resizing an image will not be.
    generation: u64,

    got_selection: bool,
    finished: bool,
}

impl Clipboard {
    pub fn connect() -> Result<Self> {
        let conn =
            Connection::connect_to_env().context("no Wayland display (is WAYLAND_DISPLAY set?)")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let mut state = State::default();

        let display = conn.display();
        display.get_registry(&qh, ());
        queue.roundtrip(&mut state)?;

        let manager = state.manager.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "compositor does not implement ext-data-control-v1; \
                 sway 1.11+, or river/hyprland with the staging protocol"
            )
        })?;
        let seat = state
            .seat
            .clone()
            .ok_or_else(|| anyhow::anyhow!("compositor announced no wl_seat"))?;

        state.device = Some(manager.get_data_device(&seat, &qh, ()));
        queue.roundtrip(&mut state)?;

        Ok(Self {
            conn,
            queue,
            state,
            log_contents: false,
        })
    }

    pub fn log_contents(&mut self, yes: bool) {
        self.log_contents = yes;
    }

    /// One turn of the loop: wait for the compositor or for any of `extra` to
    /// become readable, then act on whatever the compositor said.
    ///
    /// Returns the readiness of each `extra` fd, so the caller can own its own
    /// wakeup sources (the config watcher today, a signal fd tomorrow) without
    /// this module knowing about them.
    pub fn tick(
        &mut self,
        rewriter: &mut dyn Rewriter,
        extra: &[BorrowedFd<'_>],
        timeout: Option<Duration>,
    ) -> Result<Vec<bool>> {
        let mut ready = vec![false; extra.len()];
        self.queue.flush()?;

        // A selection picked up by `drain_events` during the previous rewrite
        // is work already in hand. Sleeping on the socket before dealing with
        // it would leave the clipboard unrewritten until something unrelated
        // happened to arrive, which on a quiet desktop is a long time.
        let work_in_hand = self.state.got_selection;

        // Only sleep when there is nothing already queued: prepare_read hands
        // back None precisely when events are pending, and blocking then would
        // deadlock against events we already hold.
        if !work_in_hand && self.queue.dispatch_pending(&mut self.state)? == 0 {
            match self.queue.prepare_read() {
                Some(guard) => {
                    let ts = timeout.map(|d| Timespec {
                        tv_sec: d.as_secs() as _,
                        tv_nsec: d.subsec_nanos() as _,
                    });
                    let wl_ready = {
                        let mut fds = Vec::with_capacity(1 + extra.len());
                        fds.push(PollFd::from_borrowed_fd(
                            guard.connection_fd(),
                            PollFlags::IN,
                        ));
                        for fd in extra {
                            fds.push(PollFd::from_borrowed_fd(*fd, PollFlags::IN));
                        }
                        rustix::event::poll(&mut fds, ts.as_ref())?;
                        for (slot, pfd) in ready.iter_mut().zip(fds.iter().skip(1)) {
                            *slot = !pfd.revents().is_empty();
                        }
                        !fds[0].revents().is_empty()
                    };
                    if wl_ready {
                        guard.read()?;
                        self.queue.dispatch_pending(&mut self.state)?;
                    } else {
                        drop(guard);
                    }
                }
                None => {
                    self.queue.dispatch_pending(&mut self.state)?;
                }
            }
        }

        self.handle_selection(rewriter)?;
        Ok(ready)
    }

    fn handle_selection(&mut self, rewriter: &mut dyn Rewriter) -> Result<()> {
        if self.state.finished {
            bail!("compositor took the data device away");
        }
        if !std::mem::take(&mut self.state.got_selection) {
            return Ok(());
        }

        {
            let Some((offer, mimes)) = self.state.current.clone() else {
                return Ok(());
            };
            let generation = self.state.generation;

            // The loop guard has to fire on the announced MIME list, before a
            // single byte is read. Reading our own offer would deadlock: the
            // compositor delivers the matching `send` to this very queue,
            // which we are not dispatching while blocked on the pipe, so every
            // flavour costs a full read timeout before it gives up.
            if mimes.iter().any(|m| m == MARKER_MIME) {
                log::debug!("skipping our own selection");
                return Ok(());
            }

            // Same place, same reason: on the announced list, before a byte is
            // read. Deliberately says nothing about what was on the clipboard,
            // because the one thing this guard exists to prevent is that
            // string reaching the log.
            if rewriter.is_secret(&mimes) {
                log::info!("selection marked secret by its owner, left alone");
                return Ok(());
            }

            let incoming = match self.read_offer(&offer, &mimes) {
                Ok(sel) => sel,
                Err(e) => {
                    log::warn!("reading selection failed: {e:#}");
                    return Ok(());
                }
            };
            log::debug!("incoming {incoming:?}");

            let Some(Rewrite {
                selection: mut outgoing,
                notify,
            }) = rewriter.rewrite(&incoming)
            else {
                if self.log_contents {
                    log::debug!("no rule matched:{}", dump(&incoming));
                }
                return Ok(());
            };
            if self.log_contents {
                log::debug!(
                    "rewrote\n    from:{}\n    to:{}",
                    dump(&incoming),
                    dump(&outgoing)
                );
            }
            // Ask the compositor whether anything happened while we were
            // busy, then check. Without the drain this comparison could never
            // fail: nothing else dispatches between taking `generation` and
            // getting here, so `state.generation` was frozen and the guard was
            // a comment. Reading a selection is allowed a whole second, which
            // is plenty of time to hit ctrl-c again.
            self.drain_events()?;
            if self.state.generation != generation {
                log::debug!("clipboard moved on while rewriting, dropping result");
                return Ok(());
            }

            outgoing.set(MARKER_MIME, Vec::new());
            self.publish(outgoing)?;
            if let Some(text) = notify {
                rewriter.notify(&text);
            }
            Ok(())
        }
    }

    /// Pull in whatever the compositor has already sent, without waiting for
    /// anything. Same shape as the sleep in `tick`, with a zero timeout.
    fn drain_events(&mut self) -> Result<()> {
        if self.queue.dispatch_pending(&mut self.state)? > 0 {
            return Ok(());
        }
        let Some(guard) = self.queue.prepare_read() else {
            // Events turned up between the two calls; they are ours already.
            self.queue.dispatch_pending(&mut self.state)?;
            return Ok(());
        };
        let mut fds = [PollFd::from_borrowed_fd(
            guard.connection_fd(),
            PollFlags::IN,
        )];
        let now = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let ready = rustix::event::poll(&mut fds, Some(&now))? > 0 && !fds[0].revents().is_empty();
        if ready {
            guard.read()?;
            self.queue.dispatch_pending(&mut self.state)?;
        } else {
            drop(guard);
        }
        Ok(())
    }

    fn read_offer(&mut self, offer: &ExtDataControlOfferV1, mimes: &[String]) -> Result<Selection> {
        let mut sel = Selection::new();
        let budget = Instant::now() + READ_BUDGET;
        for mime in mimes {
            if !worth_reading(mime) {
                log::debug!("{mime}: no rule can see this flavour, not read");
                continue;
            }
            if Instant::now() >= budget {
                log::warn!("selection read budget spent, {mime} and the rest skipped");
                break;
            }
            match self.read_flavour(offer, mime, budget) {
                Ok(Some(bytes)) => {
                    sel.set(mime.clone(), bytes);
                }
                Ok(None) => log::debug!("{mime}: over the size limit, skipped"),
                Err(e) => log::debug!("{mime}: {e:#}"),
            }
        }
        Ok(sel)
    }

    /// Ok(None) means the flavour was too big rather than broken.
    fn read_flavour(
        &mut self,
        offer: &ExtDataControlOfferV1,
        mime: &str,
        budget: Instant,
    ) -> Result<Option<Vec<u8>>> {
        let (read_fd, write_fd) = pipe_with(PipeFlags::CLOEXEC)?;
        offer.receive(mime.to_string(), write_fd.as_fd());
        self.conn.flush()?;
        drop(write_fd);

        let mut buf = Vec::new();
        let deadline = (Instant::now() + READ_TIMEOUT).min(budget);
        let mut chunk = [0u8; 8192];
        let mut file = std::fs::File::from(read_fd);

        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("timed out waiting for the source");
            }
            let ts = rustix::event::Timespec {
                tv_sec: left.as_secs() as _,
                tv_nsec: left.subsec_nanos() as _,
            };
            let mut fds = [PollFd::new(&file, PollFlags::IN)];
            if rustix::event::poll(&mut fds, Some(&ts))? == 0 {
                bail!("timed out waiting for the source");
            }

            let n = file.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            if buf.len() + n > READ_LIMIT {
                return Ok(None);
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Ok(Some(buf))
    }

    fn publish(&mut self, sel: Selection) -> Result<()> {
        if sel.is_empty() {
            bail!("refusing to publish an empty selection");
        }
        let (Some(manager), Some(device)) = (&self.state.manager, &self.state.device) else {
            bail!("no data device");
        };
        let qh = self.queue.handle();

        let source = manager.create_data_source(&qh, ());
        for mime in sel.mimes() {
            source.offer(mime.to_string());
        }
        device.set_selection(Some(&source));
        self.conn.flush()?;

        log::info!("published {sel:?}");
        // Held so the compositor can still ask us for the bytes; dropping the
        // proxy would take the selection down with it.
        self.state.payload = sel;
        self.state.source = Some(source);
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name, interface, ..
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "ext_data_control_manager_v1" => {
                state.manager = Some(registry.bind(name, 1, qh, ()));
            }
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind(name, 1, qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // Announced before `selection`, and the flavours arrive on the
            // offer itself afterwards.
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state
                    .pending
                    .insert(id.id().protocol_id(), (id, Vec::new()));
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                state.generation += 1;
                state.got_selection = true;
                // "The client must destroy the previous selection
                // ext_data_control_offer, if any, upon receiving this event."
                // wayland-rs does not send destructors when a proxy is
                // dropped, so an offer we merely forget stays alive in the
                // compositor until the connection closes - one per copy, for
                // the life of the session.
                if let Some((old, _)) = state.current.take() {
                    old.destroy();
                }
                state.current = id.map(|offer| {
                    // Taken out of `pending` without destroying: this is the
                    // same object, and destroying it twice is a protocol
                    // error.
                    let mimes = state
                        .pending
                        .remove(&offer.id().protocol_id())
                        .map(|(_, mimes)| mimes)
                        .unwrap_or_default();
                    (offer, mimes)
                });
                // Anything left was announced and never claimed.
                for (_, (offer, _)) in state.pending.drain() {
                    offer.destroy();
                }
            }
            // We do not touch PRIMARY: rewriting every mouse selection would
            // be unbearable. Kept explicit so it is a decision, not a gap.
            //
            // Ignoring it still costs an object per mouse drag, though, so the
            // offer has to go back even though we never read it.
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                state.pending.remove(&offer.id().protocol_id());
                offer.destroy();
            }
            ext_data_control_device_v1::Event::PrimarySelection { id: None } => {}
            ext_data_control_device_v1::Event::Finished => state.finished = true,
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let ext_data_control_offer_v1::Event::Offer { mime_type } = event else {
            return;
        };
        let id = offer.id().protocol_id();
        if let Some((_, mimes)) = state.pending.get_mut(&id) {
            mimes.push(mime_type);
        } else if let Some((current, mimes)) = &mut state.current
            && current.id().protocol_id() == id
        {
            mimes.push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                let data = state.payload.get(&mime_type).unwrap_or(&[]).to_vec();
                if let Err(e) = send_payload(fd, &data) {
                    log::debug!("send {mime_type}: {e:#}");
                }
            }
            ext_data_control_source_v1::Event::Cancelled => {
                source.destroy();
                if state.source.as_ref() == Some(source) {
                    state.source = None;
                    state.payload = Selection::new();
                }
            }
            _ => {}
        }
    }
}

/// Hand a flavour to a pasting client without betting the daemon on it.
///
/// The pipe belongs to whoever is pasting, its buffer is 64 KB, and a rewrite
/// is allowed to be four times that. A plain blocking `write_all` here would
/// therefore stall inside the event dispatch until the client got round to
/// reading, with the whole clipboard stopped behind it - and a client that
/// asks for a flavour and then wanders off would stop it for good.
///
/// So: non-blocking, poll for writability, and a deadline. A client that has
/// not drained 64 KB in two seconds is broken, and gets a short read instead
/// of a hostage.
///
/// Walking away mid-read is fine and normal; that closes the pipe and gives us
/// EPIPE, which is the caller's debug line and nobody's problem.
fn send_payload(fd: OwnedFd, data: &[u8]) -> Result<()> {
    send_payload_until(fd, data, Instant::now() + WRITE_TIMEOUT)
}

/// The body of `send_payload`, with the deadline handed in so a test can use
/// one that does not take two seconds to arrive.
fn send_payload_until(fd: OwnedFd, data: &[u8], deadline: Instant) -> Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    fcntl_setfl(&fd, fcntl_getfl(&fd)? | OFlags::NONBLOCK)?;
    let mut file = std::fs::File::from(fd);
    let mut sent = 0;

    while sent < data.len() {
        match file.write(&data[sent..]) {
            Ok(0) => bail!("the reader stopped accepting bytes"),
            Ok(n) => sent += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    bail!("gave up after {sent} of {} bytes", data.len());
                }
                let ts = Timespec {
                    tv_sec: left.as_secs() as _,
                    tv_nsec: left.subsec_nanos() as _,
                };
                let mut fds = [PollFd::new(&file, PollFlags::OUT)];
                if rustix::event::poll(&mut fds, Some(&ts))? == 0 {
                    bail!("gave up after {sent} of {} bytes", data.len());
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ExtDataControlManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    /// A pipe holds 64 KB. Publishing up to `READ_LIMIT` at a client that is
    /// not reading used to park the daemon inside its own event dispatch with
    /// the clipboard stopped behind it, for as long as the client felt like
    /// it. It has to come back instead.
    #[test]
    fn a_reader_that_never_reads_does_not_wedge_the_writer() {
        let (read_fd, write_fd) = pipe_with(PipeFlags::CLOEXEC).unwrap();
        let data = vec![b'A'; READ_LIMIT];

        let started = Instant::now();
        let err = send_payload_until(write_fd, &data, started + Duration::from_millis(150))
            .expect_err("a full pipe with no reader must not succeed");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "gave up after {:?}, which means it blocked",
            started.elapsed()
        );
        assert!(
            err.to_string().contains("gave up after"),
            "unexpected error: {err:#}"
        );
        // The reader end is held open until here on purpose: dropping it early
        // would turn the test into an EPIPE test instead of a full-pipe one.
        drop(read_fd);
    }

    #[test]
    fn a_reader_that_reads_gets_all_of_it() {
        let (read_fd, write_fd) = pipe_with(PipeFlags::CLOEXEC).unwrap();
        let data = vec![b'B'; READ_LIMIT];

        let drain = std::thread::spawn(move || {
            let mut file = std::fs::File::from(read_fd);
            let mut got = Vec::new();
            file.read_to_end(&mut got).unwrap();
            got
        });

        send_payload_until(write_fd, &data, Instant::now() + Duration::from_secs(5)).unwrap();
        let got = drain.join().unwrap();
        assert_eq!(got.len(), data.len());
        assert!(got.iter().all(|b| *b == b'B'));
    }
}
