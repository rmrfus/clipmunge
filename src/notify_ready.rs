//! Telling systemd the daemon is actually up.
//!
//! `Type=simple` calls a unit started the moment the process forks, so
//! everything ordered after it races the part that matters: binding to the
//! compositor and loading the rules. With `Type=notify` the unit is not
//! started until this module says so, which also turns "no compositor" from a
//! unit that started and then died into a start that failed and said why.
//!
//! Hand-rolled rather than a crate. The protocol is one datagram of `READY=1`
//! to the socket named in `$NOTIFY_SOCKET`; std has had abstract-namespace
//! Unix addresses since 1.70, so there is nothing left for a dependency to do.
//! See sd_notify(3) for the wire format.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};

/// Send `READY=1`, or do nothing at all when not running under systemd.
///
/// Failure here is never fatal: the daemon works fine outside systemd, and a
/// notification that cannot be delivered says something about the supervisor,
/// not about the clipboard.
pub fn ready() {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    if let Err(e) = send(path.as_bytes(), b"READY=1") {
        log::debug!("sd_notify: {e}");
    }
}

fn send(socket: &[u8], msg: &[u8]) -> std::io::Result<()> {
    // A leading '@' means the abstract namespace, where the name is the bytes
    // after it - not a filesystem path, so joining or canonicalising it is
    // wrong. systemd uses this form for system units; user units usually get
    // a real path under $XDG_RUNTIME_DIR.
    let addr = match socket.split_first() {
        Some((b'@', name)) => SocketAddr::from_abstract_name(name)?,
        _ => SocketAddr::from_pathname(std::path::Path::new(
            std::str::from_utf8(socket).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "NOTIFY_SOCKET is not UTF-8",
                )
            })?,
        ))?,
    };
    let sock = UnixDatagram::unbound()?;
    sock.send_to_addr(msg, &addr)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The abstract-namespace form, which is what systemd hands a system unit.
    /// Worth a test of its own because the leading '@' is a namespace marker
    /// and not part of the name: treating the whole string as a path binds
    /// nothing and fails silently, which is the failure mode this module was
    /// written to avoid.
    #[test]
    fn ready_reaches_an_abstract_socket() {
        let name = format!("clipmunge-test-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixDatagram::bind_addr(&addr).unwrap();

        send(format!("@{name}").as_bytes(), b"READY=1").unwrap();

        let mut buf = [0u8; 64];
        let n = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
    }

    #[test]
    fn a_socket_nobody_is_listening_on_is_an_error_not_a_panic() {
        let err = send(b"/nonexistent/clipmunge.sock", b"READY=1").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn a_non_utf8_pathname_is_rejected() {
        let err = send(&[0xff, 0xfe], b"READY=1").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
