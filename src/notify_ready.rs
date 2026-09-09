//! Send systemd readiness after config loading and compositor setup.
//! The sd_notify(3) protocol uses one datagram sent to NOTIFY_SOCKET.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};

/// Send READY=1 if NOTIFY_SOCKET is set. Log delivery errors at debug level.
pub fn ready() {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    if let Err(e) = send(path.as_bytes(), b"READY=1") {
        log::debug!("sd_notify: {e}");
    }
}

fn send(socket: &[u8], msg: &[u8]) -> std::io::Result<()> {
    // A leading '@' denotes an abstract Unix socket; it is not part of the name.
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

    /// Check that '@' selects the abstract namespace and is excluded from the name.
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
