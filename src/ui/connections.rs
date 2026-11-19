//! Keeps down the number of connections held open to the server, and keeps it taking new ones.
//!
//! tiny_http gives every connection a thread from the moment it is accepted, sets no limit and
//! lets nobody into its accepting, so idle connections alone could use up threads and file
//! descriptors. Its accepting stops for good at the first accept that fails, and a panic ends
//! it where a new connection cannot be cloned (no file descriptor left). What it has accepted is
//! found here among the sockets of this process, and those over the limit are closed, the ones
//! that matter least first; and the listening socket is held here, so that another tiny_http
//! server can be started on it when the one in use stops accepting.

use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::{Duration, Instant};

/// More than a browser or two have any use for: six connections per page, and the desktop
/// window's own.
const MOST: usize = 64;

const LOOK_EVERY: Duration = Duration::from_millis(500);

/// How often the server in use is checked to be still accepting connections.
const CHECK_EVERY: Duration = Duration::from_secs(5);

/// How long a connection made to check that gets to be accepted: a server that has not taken
/// it by then is not accepting any more.
const ACCEPT_WITHIN: Duration = Duration::from_secs(3);

/// The pause after a failed accept before a new server takes over, so that a failure that
/// lasts (no file descriptor left) does not turn into a loop that uses up the processor.
const AFTER_A_FAILURE: Duration = Duration::from_millis(250);

/// How long a server that no longer accepts keeps answering requests on the connections it had,
/// once none has come: a browser closes a connection it has not used for a few minutes.
const LAST_CALL: Duration = Duration::from_secs(600);

/// The state Linux gives in `tcp_info` for a connection in use. The libc crate does not name it.
const ESTABLISHED: u8 = 1;

const REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// What a connection is to the person, in the order they are closed when there are too many:
/// another user's first, then ones that have not sent a whole request (opened and left idle,
/// or sending one byte at a time), then ones that have, and a long answer to a signed-in page
/// (its event stream, a video, a picture) only when nothing else is left.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    OtherUser,
    NoRequest,
    Heard,
    Answering,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Connection {
    socket: RawFd,
    standing: Standing,
    /// How long nothing has gone either way on it.
    quiet_millis: u32,
    /// How long it has been seen open.
    open_millis: u64,
}

/// Which of `open` to close to leave `most`: by their standing, and within one the oldest (for
/// those that have sent no request) or the quietest (for those that have), so that connections
/// held open doing nothing go before the ones in use.
fn over_the_limit(mut open: Vec<Connection>, most: usize) -> Vec<RawFd> {
    let excess = open.len().saturating_sub(most);
    open.sort_by_key(|c| {
        let waited = match c.standing {
            Standing::OtherUser | Standing::NoRequest => c.open_millis,
            Standing::Heard | Standing::Answering => u64::from(c.quiet_millis),
        };
        (c.standing, std::cmp::Reverse(waited))
    });
    open.iter().take(excess).map(|c| c.socket).collect()
}

/// What the server has had of each connection, by the address of its far end.
#[derive(Default)]
struct Heard {
    /// When the last whole request came on it.
    at: Option<Instant>,
    /// How many long answers to signed-in requests are being sent on it.
    answering: usize,
}

static HEARD: LazyLock<Mutex<HashMap<SocketAddrV4, Heard>>> = LazyLock::new(Mutex::default);

fn v4(peer: Option<&SocketAddr>) -> Option<SocketAddrV4> {
    match peer {
        Some(SocketAddr::V4(a)) => Some(*a),
        _ => None,
    }
}

/// Notes that the connection from `peer` has sent a whole request.
pub fn heard(peer: Option<&SocketAddr>) {
    if let Some(peer) = v4(peer) {
        HEARD.lock().unwrap().entry(peer).or_default().at = Some(Instant::now());
    }
}

/// Notes, until it is dropped, that the connection from `peer` is in the middle of a long answer
/// to a signed-in request.
pub struct Answering(Option<SocketAddrV4>);

pub fn answering(peer: Option<&SocketAddr>) -> Answering {
    let peer = v4(peer);
    if let Some(peer) = peer {
        HEARD.lock().unwrap().entry(peer).or_default().answering += 1;
    }
    Answering(peer)
}

impl Drop for Answering {
    fn drop(&mut self) {
        if let Some(peer) = self.0
            && let Some(h) = HEARD.lock().unwrap().get_mut(&peer)
        {
            h.answering = h.answering.saturating_sub(1);
        }
    }
}

/// The address of a call of `getsockname` or `getpeername` for `socket`.
fn address_of(socket: RawFd, get: unsafe extern "C" fn(libc::c_int, *mut libc::sockaddr, *mut libc::socklen_t) -> libc::c_int) -> Option<SocketAddrV4> {
    // SAFETY: an all-zero sockaddr_in is valid, and the call writes at most `length` bytes of it.
    let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut length = size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let done = unsafe { get(socket, (&raw mut address).cast(), &raw mut length) };
    (done == 0 && libc::c_int::from(address.sin_family) == libc::AF_INET)
        .then(|| SocketAddrV4::new(std::net::Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)), u16::from_be(address.sin_port)))
}

/// A connection to the server as found among this process's sockets.
#[derive(Clone, Copy, Debug)]
struct Found {
    socket: RawFd,
    /// The socket's number in the system, which stays the same while it is open.
    inode: u64,
    peer: SocketAddrV4,
    quiet_millis: u32,
}

/// `socket` as a connection made to `port` and still established. None for anything else:
/// the listening socket, sockets of other kinds, one already closing.
fn connection_to(socket: RawFd, inode: u64, port: u16) -> Option<Found> {
    if address_of(socket, libc::getsockname)?.port() != port {
        return None;
    }
    let peer = address_of(socket, libc::getpeername)?;
    // SAFETY: an all-zero tcp_info is valid, and the call writes at most `length` bytes of it.
    let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
    let mut length = size_of::<libc::tcp_info>() as libc::socklen_t;
    let done = unsafe { libc::getsockopt(socket, libc::IPPROTO_TCP, libc::TCP_INFO, (&raw mut info).cast(), &raw mut length) };
    (done == 0 && info.tcpi_state == ESTABLISHED)
        .then(|| Found { socket, inode, peer, quiet_millis: info.tcpi_last_data_recv.min(info.tcpi_last_data_sent) })
}

/// The connections open to `port`. tiny_http holds each under two descriptors, one for reading
/// and one for writing: the socket's number in the link's name tells them apart.
fn open_connections(port: u16) -> Vec<Found> {
    let mut seen = HashSet::new();
    let mut open = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd").into_iter().flatten().flatten() {
        let Some(socket) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else { continue };
        let Ok(link) = std::fs::read_link(entry.path()) else { continue };
        let Some(inode) = link.to_str().and_then(|l| l.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()) else { continue };
        if seen.insert(inode) {
            open.extend(connection_to(socket, inode, port));
        }
    }
    open
}

/// The connections to `port`, each with its standing; `first_seen` keeps when each was first
/// found, from one look to the next.
fn look(port: u16, first_seen: &mut HashMap<u64, Instant>) -> Vec<Connection> {
    let found = open_connections(port);
    let now = Instant::now();
    first_seen.retain(|inode, _| found.iter().any(|f| f.inode == *inode));
    // Only read when there are too many: whose program holds the other end.
    let tcp = (found.len() > MOST).then(|| std::fs::read_to_string("/proc/net/tcp").ok()).flatten();
    // SAFETY: getuid has no failure mode and touches no memory.
    let me = unsafe { libc::getuid() };
    let mut heard = HEARD.lock().unwrap();
    // What is known of connections that have closed goes, once they have had time to be found.
    heard.retain(|peer, h| h.answering > 0 || found.iter().any(|f| f.peer == *peer) || h.at.is_some_and(|at| at.elapsed() < 4 * LOOK_EVERY));
    found
        .iter()
        .map(|f| {
            let standing = match (&tcp, heard.get(&f.peer)) {
                (Some(tcp), _) if !super::http::may_connect(tcp, f.peer, port, me) => Standing::OtherUser,
                (_, Some(h)) if h.answering > 0 => Standing::Answering,
                (_, Some(h)) if h.at.is_some() => Standing::Heard,
                _ => Standing::NoRequest,
            };
            let open_millis = now.duration_since(*first_seen.entry(f.inode).or_insert(now)).as_millis() as u64;
            Connection { socket: f.socket, standing, quiet_millis: f.quiet_millis, open_millis }
        })
        .collect()
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
        let mut first_seen = HashMap::new();
        loop {
            std::thread::sleep(LOOK_EVERY);
            for socket in over_the_limit(look(port, &mut first_seen), MOST) {
                refuse(socket);
            }
        }
    });
}

/// Raises the number of files this process may have open to the most the system lets it: each
/// connection holds two, and at the usual 1024 a steady stream of connections from any program
/// of the machine would use them up between two looks at the connections.
pub fn raise_open_file_limit() {
    // SAFETY: getrlimit and setrlimit read and write only the struct given.
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 || limit.rlim_cur >= limit.rlim_max {
            return;
        }
        let wanted = libc::rlimit { rlim_cur: limit.rlim_max, rlim_max: limit.rlim_max };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &wanted) == 0 {
            return;
        }
        // An unlimited hard limit is more than Linux takes: its own ceiling is in nr_open.
        let ceiling = std::fs::read_to_string("/proc/sys/fs/nr_open").ok().and_then(|n| n.trim().parse::<libc::rlim_t>().ok());
        if let Some(ceiling) = ceiling.filter(|c| *c > limit.rlim_cur) {
            libc::setrlimit(libc::RLIMIT_NOFILE, &libc::rlimit { rlim_cur: ceiling.min(limit.rlim_max), rlim_max: limit.rlim_max });
        }
    }
}

/// Whether a server is accepting connections to `port`: a connection made here has to turn up
/// among this process's sockets. None where it cannot be told, when this process can make no
/// socket. One that cannot connect is not accepting: when nobody takes them, connections fill
/// the system's queue for the port and the next ones wait until they give up.
fn accepting(port: u16) -> Option<bool> {
    let probe = match TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), ACCEPT_WITHIN) {
        Ok(probe) => probe,
        Err(e) if matches!(e.raw_os_error(), Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)) => return None,
        Err(_) => return Some(false),
    };
    let SocketAddr::V4(mine) = probe.local_addr().ok()? else { return None };
    let began = Instant::now();
    while began.elapsed() < ACCEPT_WITHIN {
        if open_connections(port).iter().any(|f| f.peer == mine) {
            return Some(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Some(false)
}

/// Answers the requests of the connections to `listener` with `handle`, for as long as the
/// program runs. The listening socket is never closed: tiny_http is given a copy of it, and
/// when its accepting fails or has stopped, after a short pause another server takes over on
/// a new copy. The one before goes on answering the connections it had.
pub fn serve(listener: TcpListener, handle: impl Fn(tiny_http::Request) + Send + Sync + 'static) -> ! {
    let handle: Arc<dyn Fn(tiny_http::Request) + Send + Sync> = Arc::new(handle);
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let (failed, failures) = mpsc::channel::<String>();
    loop {
        let server = listener.try_clone().map_err(|e| e.to_string()).and_then(|l| tiny_http::Server::from_listener(l, None).map_err(|e| e.to_string()));
        let server = match server {
            Ok(server) => server,
            Err(e) => {
                eprintln!("warning: the page cannot take new connections ({e}); trying again in a moment");
                std::thread::sleep(AFTER_A_FAILURE);
                continue;
            }
        };
        let replaced = Arc::new(AtomicBool::new(false));
        let (handle, failed, gone) = (handle.clone(), failed.clone(), replaced.clone());
        std::thread::spawn(move || answer(&server, &*handle, &failed, &gone));
        loop {
            match failures.recv_timeout(CHECK_EVERY) {
                Ok(e) => {
                    eprintln!("warning: a connection to the page could not be taken ({e}); new ones are taken again in a moment");
                    break;
                }
                Err(_) if accepting(port) == Some(false) => {
                    eprintln!("warning: the page had stopped taking new connections; it takes them again");
                    break;
                }
                Err(_) => {}
            }
        }
        replaced.store(true, Ordering::Relaxed);
        std::thread::sleep(AFTER_A_FAILURE);
    }
}

/// Hands each request `server` receives to `handle`. A failed accept, which ends tiny_http's
/// accepting, is told to `failed`; once `replaced`, the server is let go after a while without
/// a request.
fn answer(server: &tiny_http::Server, handle: &(dyn Fn(tiny_http::Request) + Send + Sync), failed: &mpsc::Sender<String>, replaced: &AtomicBool) {
    loop {
        match server.recv_timeout(LAST_CALL) {
            Ok(Some(request)) => handle(request),
            Ok(None) if replaced.load(Ordering::Relaxed) => return,
            Ok(None) => {}
            // Told only while it is the server in use: a later one has nothing to do with it.
            Err(e) if !replaced.load(Ordering::Relaxed) => {
                let _ = failed.send(e.to_string());
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    fn connection(socket: RawFd, standing: Standing, quiet_millis: u32, open_millis: u64) -> Connection {
        Connection { socket, standing, quiet_millis, open_millis }
    }

    #[test]
    fn the_quietest_go_first_and_only_those_over_the_limit() {
        use Standing::Heard;
        let open = vec![connection(1, Heard, 10, 0), connection(2, Heard, 9000, 0), connection(3, Heard, 0, 0), connection(4, Heard, 500, 0)];
        assert_eq!(over_the_limit(open.clone(), 4), Vec::<RawFd>::new());
        assert_eq!(over_the_limit(open.clone(), 9), Vec::<RawFd>::new());
        assert_eq!(over_the_limit(open.clone(), 3), [2]);
        assert_eq!(over_the_limit(open, 1), [2, 4, 1]);
    }

    #[test]
    fn another_users_go_first_then_those_with_no_request_oldest_first_and_a_signed_in_answer_last() {
        use Standing::*;
        let open = vec![
            // An event stream quiet between two messages, the quietest of all.
            connection(1, Answering, 14_000, 60_000),
            connection(2, Heard, 3_000, 50_000),
            // Sending a byte now and then: never quiet for long, but open longest.
            connection(3, NoRequest, 100, 40_000),
            connection(4, NoRequest, 9_000, 9_000),
            connection(5, OtherUser, 0, 1_000),
            connection(6, Heard, 200, 70_000),
        ];
        assert_eq!(over_the_limit(open.clone(), 5), [5]);
        assert_eq!(over_the_limit(open.clone(), 3), [5, 3, 4]);
        assert_eq!(over_the_limit(open.clone(), 1), [5, 3, 4, 2, 6]);
        // Only when nothing else is left.
        assert_eq!(over_the_limit(open, 0), [5, 3, 4, 2, 6, 1]);
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
        assert_eq!(SocketAddr::V4(found[0].peer), client.local_addr().unwrap());
        // The client's own end, here in the same process, is not a connection to this port.
        assert!(connection_to(client.as_raw_fd(), 0, port).is_none());
        assert!(connection_to(listener.as_raw_fd(), 0, port).is_none());

        refuse(found[0].socket);
        let mut said = String::new();
        client.read_to_string(&mut said).unwrap();
        assert!(said.starts_with("HTTP/1.1 503"), "{said}");
        assert!(open_connections(port).is_empty(), "a connection being closed no longer counts");
    }

    #[test]
    fn what_a_connection_has_sent_decides_its_standing() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let clients: Vec<TcpStream> = (0..3).map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap()).collect();
        let _accepted: Vec<TcpStream> = (0..3).map(|_| listener.accept().unwrap().0).collect();
        let peer = |i: usize| clients[i].local_addr().unwrap();
        heard(Some(&peer(1)));
        heard(Some(&peer(2)));
        let long = answering(Some(&peer(2)));
        let standing_of = |first_seen: &mut HashMap<u64, Instant>| {
            let found = open_connections(port);
            let looked = look(port, first_seen);
            (0..3).map(|i| looked[found.iter().position(|f| SocketAddr::V4(f.peer) == peer(i)).unwrap()].standing).collect::<Vec<_>>()
        };
        let mut first_seen = HashMap::new();
        assert_eq!(standing_of(&mut first_seen), [Standing::NoRequest, Standing::Heard, Standing::Answering]);
        drop(long);
        assert_eq!(standing_of(&mut first_seen), [Standing::NoRequest, Standing::Heard, Standing::Heard]);
        // How long each has been open is counted from when it was first found.
        std::thread::sleep(Duration::from_millis(30));
        assert!(look(port, &mut first_seen).iter().all(|c| c.open_millis >= 30));
    }

    #[test]
    fn a_server_that_has_stopped_accepting_is_told_from_one_that_has_not() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        // Nobody accepts: the connection waits in the system's queue and is never found.
        assert_eq!(accepting(port), Some(false));
        // The first one, left in that queue, is taken first; then the one that asks.
        let taker = std::thread::spawn(move || {
            let kept: Vec<TcpStream> = (0..2).map(|_| listener.accept().unwrap().0).collect();
            std::thread::sleep(ACCEPT_WITHIN / 2);
            drop(kept);
            listener
        });
        assert_eq!(accepting(port), Some(true));
        drop(taker.join().unwrap());
        assert_eq!(accepting(port), Some(false), "nothing listens there any more");
    }

    #[test]
    fn a_server_whose_accepting_ended_is_followed_by_another_on_the_same_socket() {
        use std::io::{Read, Write};
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        // One copy of the socket as tiny_http would be given it, whose accepting is ended by
        // dropping it: the socket stays open through the copy kept, and a later server answers.
        let first = tiny_http::Server::from_listener(listener.try_clone().unwrap(), None).unwrap();
        drop(first);
        std::thread::spawn(move || serve(listener, |request| drop(request.respond(tiny_http::Response::from_string("answered")))));
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(client, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let mut said = String::new();
        client.read_to_string(&mut said).unwrap();
        assert!(said.starts_with("HTTP/1.1 200") && said.ends_with("answered"), "{said}");
    }

    #[test]
    fn the_open_file_limit_is_raised_to_the_most_allowed() {
        raise_open_file_limit();
        // SAFETY: getrlimit writes only the struct given.
        let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
        assert!(limit.rlim_cur == limit.rlim_max || limit.rlim_max == libc::RLIM_INFINITY, "{} of {}", limit.rlim_cur, limit.rlim_max);
    }
}
