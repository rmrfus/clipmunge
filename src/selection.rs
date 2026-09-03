//! What a clipboard selection looks like to the rest of the program.
//!
//! Deliberately bytes-and-MIME rather than a string. Rules today only touch
//! text, but an image flavour is a `Vec<u8>` with a different MIME and nothing
//! above this layer should have to change to carry one.

use std::fmt;

/// The private flavour every selection we publish carries. Seeing it on an
/// incoming offer means we are looking at our own work coming back, which is
/// the loop guard. It is a real advertised MIME rather than internal state so
/// that `wl-paste -l` shows who touched the clipboard.
pub const MARKER_MIME: &str = "application/x-clipmunge";

pub const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

pub const HTML_MIME: &str = "text/html";
pub const URL_MIME: &str = "chromium/x-source-url";

/// Flavours whose presence means "this is a password, leave it alone".
///
/// `x-kde-passwordManagerHint` is klipper's, and the name has outlived its
/// desktop - Firefox carries the same atom. Its documented value is `secret`,
/// and we do not read it: fetching a six-byte flavour costs a pipe round trip
/// with a source that may never answer (Firefox advertises `COMPOUND_TEXT`
/// and then serves nothing, measured at a four-second timeout), and a source
/// that bothered to advertise the hint at all has said what it means.
///
/// Partial by construction: this is opt-in for whoever owns the selection, and
/// most of them do not opt in. Measured on Firefox 154 - copying from
/// about:logins sets it, copying out of an `<input type=password>` does not,
/// and the 1Password browser extension does not.
pub const SECRET_MIMES: &[&str] = &["x-kde-passwordManagerHint"];

/// Flavours that mean somebody has already described this selection better
/// than a guess from its plain text would. Also, with `TEXT_MIMES`, the whole
/// set the daemon bothers to read: see `Clipboard::read_offer`.
pub const RICH_MIMES: &[&str] = &[HTML_MIME, URL_MIME, "text/uri-list", "text/rtf"];

/// One clipboard selection: an ordered list of flavours.
///
/// Ordered, not a map: the order we advertise flavours in is the order a
/// pasting client sees them, and some clients take the first one they
/// recognise rather than the best one.
#[derive(Clone, Default)]
pub struct Selection {
    flavours: Vec<(String, Vec<u8>)>,
}

impl Selection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, mime: impl Into<String>, data: impl Into<Vec<u8>>) -> &mut Self {
        let mime = mime.into();
        let data = data.into();
        match self.flavours.iter_mut().find(|(m, _)| *m == mime) {
            Some(slot) => slot.1 = data,
            None => self.flavours.push((mime, data)),
        }
        self
    }

    /// Same bytes under several names, for the text/plain family.
    pub fn set_text(&mut self, text: &str) -> &mut Self {
        for mime in TEXT_MIMES {
            self.set(*mime, text.as_bytes());
        }
        self
    }

    pub fn get(&self, mime: &str) -> Option<&[u8]> {
        self.flavours
            .iter()
            .find(|(m, _)| m == mime)
            .map(|(_, d)| d.as_slice())
    }

    pub fn mimes(&self) -> impl Iterator<Item = &str> {
        self.flavours.iter().map(|(m, _)| m.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.flavours
            .iter()
            .map(|(m, d)| (m.as_str(), d.as_slice()))
    }

    pub fn is_empty(&self) -> bool {
        self.flavours.is_empty()
    }

    /// The text/plain payload as UTF-8, if there is one and it decodes.
    pub fn text(&self) -> Option<&str> {
        TEXT_MIMES
            .iter()
            .find_map(|m| self.get(m))
            .and_then(|b| std::str::from_utf8(b).ok())
    }

    /// True when nobody has attached a rich flavour. A browser copying a link
    /// already provides text/html, and overwriting that would throw away a
    /// perfectly good href in favour of our guess.
    pub fn is_plain_only(&self) -> bool {
        !self.mimes().any(|m| RICH_MIMES.contains(&m))
    }

    /// Put the flavours in the order they should be advertised: the text
    /// family first, then HTML, then the source URL, then anything a rule
    /// invented, by name.
    ///
    /// Not cosmetic. A handler hands back a Lua table, Lua seeds its string
    /// hash per process, and `pairs` walks the hash part in that order - so
    /// without this the same rule advertises text/html first on one boot and
    /// text/plain first on the next. Measured: six daemon starts, five
    /// different orders. A client that takes the first flavour it recognises
    /// then pastes something different depending on when the daemon last
    /// restarted, which is the worst kind of bug to be handed in a ticket.
    pub fn canonical_order(&mut self) -> &mut Self {
        fn rank(mime: &str) -> usize {
            if let Some(i) = TEXT_MIMES.iter().position(|m| *m == mime) {
                return i;
            }
            match RICH_MIMES.iter().position(|m| *m == mime) {
                Some(i) => TEXT_MIMES.len() + i,
                // Everything unknown after the lot, and the name breaks the
                // tie so two of them cannot swap places either.
                None => TEXT_MIMES.len() + RICH_MIMES.len(),
            }
        }
        self.flavours
            .sort_by(|a, b| rank(&a.0).cmp(&rank(&b.0)).then_with(|| a.0.cmp(&b.0)));
        self
    }
}

/// Every flavour with its content, for `--debug`.
///
/// Kept well away from the `Debug` impl, which prints sizes only. This one
/// puts whatever you copied into the log, and that is the whole point of it
/// being opt-in behind a flag that says so.
pub fn dump(sel: &Selection) -> String {
    /// Long enough for any realistic rule to be debugged, short enough that a
    /// pasted document does not fill the journal.
    const MAX: usize = 512;

    let mut out = String::new();
    for (mime, data) in sel.iter() {
        out.push_str(&format!("\n      {mime}: "));
        match std::str::from_utf8(data) {
            Ok(s) if s.len() <= MAX => out.push_str(&s.replace('\n', "\\n")),
            Ok(s) => {
                let cut = s
                    .char_indices()
                    .map(|(i, _)| i)
                    .take_while(|i| *i <= MAX)
                    .last()
                    .unwrap_or(0);
                out.push_str(&s[..cut].replace('\n', "\\n"));
                out.push_str(&format!("… ({} bytes total)", data.len()));
            }
            Err(_) => out.push_str(&format!("<{} bytes, not utf-8>", data.len())),
        }
    }
    out
}

impl fmt::Debug for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Selection");
        for (mime, data) in self.iter() {
            d.field(mime, &format_args!("{} bytes", data.len()));
        }
        d.finish()
    }
}
