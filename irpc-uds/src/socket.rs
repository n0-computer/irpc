//! Unix datagram socket implementing quinn's `AsyncUdpSocket`.
//!
//! Maps Unix socket paths to fake `SocketAddr` values so quinn can route
//! QUIC packets as if they were UDP.

use std::{
    collections::HashMap,
    fmt::Debug,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroUsize,
    os::unix::net::UnixDatagram as StdUnixDatagram,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc, Mutex,
    },
    task::{ready, Context, Poll},
};

use quinn::AsyncUdpSocket;
use quinn::udp::{RecvMeta, Transmit};
use tokio::net::UnixDatagram;

/// The fake IP used for all addresses in the UDS transport.
const FAKE_IP: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Counter for assigning unique fake ports.
static NEXT_FAKE_PORT: AtomicU16 = AtomicU16::new(100);

/// Counter for unique client socket paths.
static NEXT_CLIENT_ID: AtomicU16 = AtomicU16::new(0);

/// Desired socket buffer size (2MB). The kernel may cap this lower.
const SOCKET_BUF_SIZE: usize = 2 * 1024 * 1024;

fn next_fake_addr() -> SocketAddr {
    SocketAddr::new(FAKE_IP, NEXT_FAKE_PORT.fetch_add(1, Ordering::Relaxed))
}

/// Try to increase socket send/receive buffers for better throughput.
fn set_socket_buffers(sock: &StdUnixDatagram) {
    use std::os::fd::AsRawFd;
    let fd = sock.as_raw_fd();
    let size = SOCKET_BUF_SIZE as libc::c_int;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &size as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &size as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// Bidirectional mapping between Unix socket paths and fake `SocketAddr`s.
#[derive(Debug, Default, Clone)]
pub(crate) struct AddrMap {
    path_to_addr: HashMap<PathBuf, SocketAddr>,
    addr_to_path: HashMap<SocketAddr, PathBuf>,
}

impl AddrMap {
    fn get_or_insert_addr(&mut self, path: &Path) -> SocketAddr {
        if let Some(&addr) = self.path_to_addr.get(path) {
            return addr;
        }
        let addr = next_fake_addr();
        self.path_to_addr.insert(path.to_owned(), addr);
        self.addr_to_path.insert(addr, path.to_owned());
        addr
    }

    fn get_path(&self, addr: &SocketAddr) -> Option<&Path> {
        self.addr_to_path.get(addr).map(|p| p.as_path())
    }
}

/// A Unix datagram socket that implements quinn's [`AsyncUdpSocket`].
pub struct UdsSocket {
    io: UnixDatagram,
    local_addr: SocketAddr,
    /// Path this socket is bound to (for cleanup on drop).
    bound_path: Option<PathBuf>,
    /// Address mapping shared between the socket and its senders.
    addr_map: Arc<Mutex<AddrMap>>,
}

impl Debug for UdsSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdsSocket")
            .field("local_addr", &self.local_addr)
            .field("bound_path", &self.bound_path)
            .finish()
    }
}

impl UdsSocket {
    /// Create a new server socket bound to the given path.
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        // Remove stale socket file if it exists
        let _ = std::fs::remove_file(path);
        let std_sock = StdUnixDatagram::bind(path)?;
        std_sock.set_nonblocking(true)?;
        set_socket_buffers(&std_sock);
        let io = UnixDatagram::from_std(std_sock)?;
        let local_addr = next_fake_addr();
        let mut addr_map = AddrMap::default();
        addr_map.path_to_addr.insert(path.to_owned(), local_addr);
        addr_map.addr_to_path.insert(local_addr, path.to_owned());
        Ok(Self {
            io,
            local_addr,
            bound_path: Some(path.to_owned()),
            addr_map: Arc::new(Mutex::new(addr_map)),
        })
    }

    /// Create a client socket that will communicate with the given server path.
    ///
    /// Binds to a temporary path in the same directory for replies.
    /// Returns the socket and the fake server address (for use with `Endpoint::connect_with`).
    pub fn connect(server_path: impl AsRef<Path>) -> io::Result<(Self, SocketAddr)> {
        let server_path = server_path.as_ref();
        let dir = server_path.parent().unwrap_or(Path::new("/tmp"));

        // Create a unique temp path for this client socket
        let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
        let client_path = dir.join(format!(
            ".irpc-uds-client-{}-{}",
            std::process::id(),
            client_id,
        ));
        let _ = std::fs::remove_file(&client_path);

        let std_sock = StdUnixDatagram::bind(&client_path)?;
        std_sock.set_nonblocking(true)?;
        set_socket_buffers(&std_sock);
        let io = UnixDatagram::from_std(std_sock)?;

        let local_addr = next_fake_addr();
        let server_addr = next_fake_addr();

        let mut addr_map = AddrMap::default();
        addr_map
            .path_to_addr
            .insert(client_path.clone(), local_addr);
        addr_map
            .addr_to_path
            .insert(local_addr, client_path.clone());
        addr_map
            .path_to_addr
            .insert(server_path.to_owned(), server_addr);
        addr_map
            .addr_to_path
            .insert(server_addr, server_path.to_owned());

        Ok((
            Self {
                io,
                local_addr,
                bound_path: Some(client_path),
                addr_map: Arc::new(Mutex::new(addr_map)),
            },
            server_addr,
        ))
    }
}

impl Drop for UdsSocket {
    fn drop(&mut self) {
        if let Some(path) = &self.bound_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn dup_datagram(io: &UnixDatagram) -> io::Result<UnixDatagram> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let raw = io.as_raw_fd();
    let duped = unsafe { libc::dup(raw) };
    if duped < 0 {
        return Err(io::Error::last_os_error());
    }
    let std_sock = unsafe { StdUnixDatagram::from_raw_fd(duped) };
    std_sock.set_nonblocking(true)?;
    UnixDatagram::from_std(std_sock)
}

impl AsyncUdpSocket for UdsSocket {
    fn create_sender(&self) -> Pin<Box<dyn quinn::UdpSender>> {
        let io = dup_datagram(&self.io).expect("failed to dup UDS fd for sender");
        Box::pin(UdsSender {
            io,
            addr_map: self.addr_map.clone(),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        debug_assert!(!bufs.is_empty() && !meta.is_empty());
        loop {
            ready!(self.io.poll_recv_ready(cx))?;
            let buf = &mut bufs[0];
            match self.io.try_recv_from(buf) {
                Ok((len, src_addr)) => {
                    let fake_addr = if let Some(path) = src_addr.as_pathname() {
                        self.addr_map.lock().unwrap().get_or_insert_addr(path)
                    } else {
                        next_fake_addr()
                    };
                    let mut recv_meta = RecvMeta::default();
                    recv_meta.addr = fake_addr;
                    recv_meta.len = len;
                    recv_meta.stride = len;
                    recv_meta.ecn = None;
                    recv_meta.dst_ip = Some(FAKE_IP);
                    meta[0] = recv_meta;
                    return Poll::Ready(Ok(1));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    fn may_fragment(&self) -> bool {
        false
    }

    fn max_receive_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
}

struct UdsSender {
    io: UnixDatagram,
    addr_map: Arc<Mutex<AddrMap>>,
}

impl Debug for UdsSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdsSender").finish()
    }
}

impl quinn::UdpSender for UdsSender {
    fn poll_send(
        self: Pin<&mut Self>,
        transmit: &Transmit<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        let dest_path = {
            let map = this.addr_map.lock().unwrap();
            map.get_path(&transmit.destination).map(|p| p.to_owned())
        };
        let Some(dest_path) = dest_path else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("no UDS path for addr {}", transmit.destination),
            )));
        };

        loop {
            ready!(this.io.poll_send_ready(cx))?;
            match this.io.try_send_to(transmit.contents, &dest_path) {
                Ok(_) => return Poll::Ready(Ok(())),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
}
