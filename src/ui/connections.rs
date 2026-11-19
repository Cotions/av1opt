//! Keeps down the number of connections held open to the server.
//!
//! tiny_http gives every connection a thread from the moment it is accepted, sets no limit and
//! lets nobody into its accepting, so idle connections alone could use up threads and file
//! descriptors. It stops accepting for good once it runs out of those. What it has accepted is
//! found here among the sockets of this process, and those over the limit are closed.

use std::collections::HashSet;
use std::os::fd::RawFd;
use std::time::Duration;

/// More than a browser or two have any use for: six connections per page, and the desktop
/// window's own.
const MOST: usize = 64;

const LOOK_EVERY: Duration = Duration::from_millis(500);

/// The state Linux gives in `tcp_info` for a connection in use. The libc crate does not name it.
const ESTABLISHED: u8 = 1;

const REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

#[derive(Clone, Copy, Debug, PartialEq)]
struct Connection {
    socket: RawFd,
    /// How long nothing has gone either way on it.
    quiet_millis: u32,
}

/// Which of `open` to close to leave `most`: those that have been quiet longest, so that
/// connections held open doing nothing go before the ones in use.
fn over_the_limit(mut open: Vec<Connection>, most: usize) -> Vec<RawFd> {
    let excess = open.len().saturating_sub(most);
    open.sort_by_key(|c| std::cmp::Reverse(c.quiet_millis));
    open.iter().take(excess).map(|c| c.socket).collect()
}

/// The port of the address a call of `getsockname` or `getpeername` gives for `socket`.
fn port_of(socket: RawFd, get: unsafe extern "C" fn(libc::c_int, *mut libc::sockaddr, *mut libc::socklen_t) -> libc::c_int) -> Option<u16> {
    // SAFETY: an all-zero sockaddr_in is valid, and the call writes at most `length` bytes of it.
    let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut length = size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let done = unsafe { get(socket, (&raw mut address).cast(), &raw mut length) };
    (done == 0 && libc::c_int::from(address.sin_family) == libc::AF_INET).then(|| u16::from_be(address.sin_port))
}

/// `socket` as a connection made to `port` and still established. None for anything else:
/// the listening socket, sockets of other kinds, one already closing.
fn connection_to(socket: RawFd, port: u16) -> Option<Connection> {
    if port_of(socket, libc::getsockname)? != port {
        return None;
    }
    port_of(socket, libc::getpeername)?;
    // SAFETY: an all-zero tcp_info is valid, and the call writes at most `length` bytes of it.
    let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
    let mut length = size_of::<libc::tcp_info>() as libc::socklen_t;
    let done = unsafe { libc::getsockopt(socket, libc::IPPROTO_TCP, libc::TCP_INFO, (&raw mut info).cast(), &raw mut length) };
    (done == 0 && info.tcpi_state == ESTABLISHED)
        .then(|| Connection { socket, quiet_millis: info.tcpi_last_data_recv.min(info.tcpi_last_data_sent) })
}

/// The connections open to `port`. tiny_http holds each under two descriptors, one for reading
/// and one for writing: the socket's number in the link's name tells them apart.
fn open_connections(port: u16) -> Vec<Connection> {
    let mut seen = HashSet::new();
    let mut open = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd").into_iter().flatten().flatten() {
        let Some(socket) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else { continue };
        let Ok(link) = std::fs::read_link(entry.path()) else { continue };
        let Some(number) = link.to_str().and_then(|l| l.strip_prefix("socket:[")).map(str::to_owned) else { continue };
        if seen.insert(number) {
            open.extend(connection_to(socket, port));
        }
    }
    open
}

fn refuse(socket: RawFd) {
    // SAFETY: both take a descriptor and plain numbers, and `send` reads only the bytes named.
    // Should the connection have ended a moment ago and its number gone to a file, both fail and
    // change nothing. Should it have gone to a new connection, that one is refused like this one.
    unsafe {
        libc::send(socket, REFUSAL.as_ptr().cast(), REFUSAL.len(), libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL);
        libc::shutdown(socket, libc::SHUT_RDWR);
    }
}

/// Starts looking after the connections to `port`.
pub fn start(port: u16) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(LOOK_EVERY);
            for socket in over_the_limit(open_connections(port), MOST) {
                refuse(socket);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;

    fn connection(socket: RawFd, quiet_millis: u32) -> Connection {
        Connection { socket, quiet_millis }
    }

    #[test]
    fn the_quietest_go_first_and_only_those_over_the_limit() {
        let open = vec![connection(1, 10), connection(2, 9000), connection(3, 0), connection(4, 500)];
        assert_eq!(over_the_limit(open.clone(), 4), Vec::<RawFd>::new());
        assert_eq!(over_the_limit(open.clone(), 9), Vec::<RawFd>::new());
        assert_eq!(over_the_limit(open.clone(), 3), [2]);
        assert_eq!(over_the_limit(open, 1), [2, 4, 1]);
    }

    #[test]
    fn a_connection_is_found_once_and_a_refused_one_hears_why() {
        use std::io::Read;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        // Held twice, as tiny_http holds it.
        let twin = accepted.try_clone().unwrap();
        let found = open_connections(port);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!([accepted.as_raw_fd(), twin.as_raw_fd()].contains(&found[0].socket));
        // The client's own end, here in the same process, is not a connection to this port.
        assert!(connection_to(client.as_raw_fd(), port).is_none());
        assert!(connection_to(listener.as_raw_fd(), port).is_none());

        refuse(found[0].socket);
        let mut said = String::new();
        client.read_to_string(&mut said).unwrap();
        assert!(said.starts_with("HTTP/1.1 503"), "{said}");
        assert!(open_connections(port).is_empty(), "a connection being closed no longer counts");
    }
}
