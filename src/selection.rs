//! Clipboard payloads stored as bytes by MIME type.

use std::fmt;

/// Private MIME marking our output. Checked before reading to avoid self-rewrites.
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

/// Offers advertising these MIME types are skipped before reading.
/// Source applications may omit the hint even for secrets.
pub const SECRET_MIMES: &[&str] = &["x-kde-passwordManagerHint"];

/// Rich types used by `is_plain_only` and included in clipboard reads.
pub const RICH_MIMES: &[&str] = &[HTML_MIME, URL_MIME, "text/uri-list", "text/rtf"];

/// An ordered list of clipboard flavours. Some clients use the first supported type.
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

    /// Whether a flavour arrived at all, without copying its bytes out.
    pub fn has(&self, mime: &str) -> bool {
        self.flavours.iter().any(|(m, _)| m == mime)
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

    /// Decode the first present TEXT_MIMES entry as UTF-8.
    /// An invalid payload returns None without trying lower-priority entries.
    pub fn text(&self) -> Option<&str> {
        TEXT_MIMES
            .iter()
            .find_map(|m| self.get(m))
            .and_then(|b| std::str::from_utf8(b).ok())
    }

    /// True when no rich MIME type is present.
    pub fn is_plain_only(&self) -> bool {
        !self.mimes().any(|m| RICH_MIMES.contains(&m))
    }

    /// Advertise text first, then RICH_MIMES order, then other types by name.
    /// Lua table iteration order varies between runs; some clients paste the
    /// first supported type.
    pub fn canonical_order(&mut self) -> &mut Self {
        fn rank(mime: &str) -> usize {
            if let Some(i) = TEXT_MIMES.iter().position(|m| *m == mime) {
                return i;
            }
            match RICH_MIMES.iter().position(|m| *m == mime) {
                Some(i) => TEXT_MIMES.len() + i,
                None => TEXT_MIMES.len() + RICH_MIMES.len(),
            }
        }
        self.flavours
            .sort_by(|a, b| rank(&a.0).cmp(&rank(&b.0)).then_with(|| a.0.cmp(&b.0)));
        self
    }
}

/// Content formatter for `--debug`. The regular `Debug` impl prints only sizes.
pub fn dump(sel: &Selection) -> String {
    /// Limit each payload preview to keep large selections from filling the log.
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
