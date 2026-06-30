// Unprivileged ICMP echo with kernel receive timestamps.
//
// Each target gets its own SOCK_DGRAM ICMP socket (no root needed on macOS). We enable
// SO_TIMESTAMP, so every reply carries an SCM_TIMESTAMP control message with the kernel's
// arrival time. RTT = (kernel arrival) − (time captured immediately before sendto), which
// makes the measurement independent of our process scheduling / runtime load / App Nap —
// the reply can sit in the socket buffer while we're busy and the number is still correct.

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;

/// A distinct ICMP identifier per socket. macOS demuxes echo replies to a DGRAM ICMP socket by
/// the id field, so every pinger in this process MUST use a different one — otherwise replies
/// for some targets get delivered to the wrong socket and those targets read as 100% loss.
/// Seeded from the pid to avoid clashing with other processes' ICMP sockets.
fn next_ident() -> u16 {
    static COUNTER: AtomicU16 = AtomicU16::new(0);
    (std::process::id() as u16).wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Pull the sender IP out of a `recvmsg` `msg_name` (sockaddr_storage).
fn src_ip(ss: &libc::sockaddr_storage) -> Option<IpAddr> {
    match ss.ss_family as libc::c_int {
        libc::AF_INET => {
            let sin = ss as *const _ as *const libc::sockaddr_in;
            // s_addr is stored in network order; its in-memory bytes are the IP octets.
            let octets = unsafe { (*sin).sin_addr.s_addr.to_ne_bytes() };
            Some(IpAddr::V4(Ipv4Addr::from(octets)))
        }
        libc::AF_INET6 => {
            let sin6 = ss as *const _ as *const libc::sockaddr_in6;
            let octets = unsafe { (*sin6).sin6_addr.s6_addr };
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

fn now_realtime() -> libc::timeval {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    unsafe { libc::gettimeofday(&mut tv, std::ptr::null_mut()) };
    tv
}

fn delta_ms(send: libc::timeval, recv: libc::timeval) -> f64 {
    let secs = (recv.tv_sec as f64) - (send.tv_sec as f64);
    let usecs = (recv.tv_usec as f64) - (send.tv_usec as f64);
    (secs * 1000.0 + usecs / 1000.0).max(0.0)
}

/// Standard internet checksum (used for ICMPv4; the kernel computes ICMPv6's).
fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn build_echo(v6: bool, ident: u16, seq: u16) -> Vec<u8> {
    let typ: u8 = if v6 { 128 } else { 8 }; // echo request
    let mut pkt = vec![
        typ,
        0,
        0,
        0, // checksum (filled below for v4; kernel fills v6)
        (ident >> 8) as u8,
        ident as u8,
        (seq >> 8) as u8,
        seq as u8,
    ];
    pkt.extend_from_slice(b"speed-daemon-probe");
    if !v6 {
        let c = checksum(&pkt);
        pkt[2] = (c >> 8) as u8;
        pkt[3] = c as u8;
    }
    pkt
}

/// Pull the sequence number out of an echo *reply*, tolerating a leading IPv4 header
/// (some stacks deliver it on DGRAM ICMP, some don't).
fn reply_seq(buf: &[u8]) -> Option<u16> {
    let mut off = 0usize;
    if !buf.is_empty() && (buf[0] >> 4) == 4 {
        off = ((buf[0] & 0x0f) as usize) * 4; // IPv4 IHL
    }
    let icmp = buf.get(off..)?;
    if icmp.len() < 8 {
        return None;
    }
    // echo reply: v4 type 0, v6 type 129
    if icmp[0] == 0 || icmp[0] == 129 {
        Some(u16::from_be_bytes([icmp[6], icmp[7]]))
    } else {
        None
    }
}

/// One recvmsg, returning (sequence, kernel arrival time, from_kernel, source IP) for an echo
/// reply. Err(WouldBlock) signals the caller (via AsyncFd) to wait for more readiness.
fn recv_once(fd: i32) -> io::Result<Option<(u16, libc::timeval, bool, IpAddr)>> {
    let mut buf = [0u8; 1500];
    let mut ctrl = [0u8; 256];
    let mut from: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = &mut from as *mut _ as *mut libc::c_void;
    msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ctrl.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = ctrl.len() as _;

    let n = unsafe { libc::recvmsg(fd, &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let n = n as usize;

    // Find the SCM_TIMESTAMP control message (kernel arrival time).
    let mut rx: Option<libc::timeval> = None;
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            let c = &*cmsg;
            if c.cmsg_level == libc::SOL_SOCKET && c.cmsg_type == libc::SCM_TIMESTAMP {
                let p = libc::CMSG_DATA(cmsg) as *const libc::timeval;
                rx = Some(std::ptr::read_unaligned(p));
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    let src = src_ip(&from).unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    match reply_seq(&buf[..n]) {
        Some(seq) => {
            let (tv, from_kernel) = match rx {
                Some(t) => (t, true),
                None => (now_realtime(), false),
            };
            Ok(Some((seq, tv, from_kernel, src)))
        }
        None => Ok(None),
    }
}

/// A per-target ICMP echo sender/receiver with kernel RX timestamping.
pub struct Pinger {
    fd: AsyncFd<Socket>,
    target: SockAddr,
    v6: bool,
    ident: u16,
}

impl Pinger {
    pub fn new(addr: IpAddr) -> io::Result<Pinger> {
        let v6 = addr.is_ipv6();
        let (domain, proto) = if v6 {
            (Domain::IPV6, Protocol::ICMPV6)
        } else {
            (Domain::IPV4, Protocol::ICMPV4)
        };
        let sock = Socket::new(domain, Type::DGRAM, Some(proto))?;
        sock.set_nonblocking(true)?;

        // Enable kernel receive timestamps.
        let on: libc::c_int = 1;
        let rc = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TIMESTAMP,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Pinger {
            fd: AsyncFd::new(sock)?,
            target: SocketAddr::new(addr, 0).into(),
            v6,
            ident: next_ident(),
        })
    }

    /// Send one echo and return its RTT in ms, or None on timeout/loss.
    pub async fn ping(&self, seq: u16, timeout: Duration) -> Option<f64> {
        let send = now_realtime();
        let pkt = build_echo(self.v6, self.ident, seq);
        if self.fd.get_ref().send_to(&pkt, &self.target).is_err() {
            return None;
        }

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let mut guard = match tokio::time::timeout(remaining, self.fd.readable()).await {
                Ok(Ok(g)) => g,
                _ => return None, // timeout, or readiness error
            };
            match guard.try_io(|s| recv_once(s.get_ref().as_raw_fd())) {
                Ok(Ok(Some((rseq, rx, _, _)))) if rseq == seq => return Some(delta_ms(send, rx)),
                Ok(Ok(_)) => continue,  // a packet, but not our reply
                Ok(Err(_)) => return None, // real recv error
                Err(_would_block) => continue, // spurious readiness; wait again
            }
        }
    }
}

/// Blocking counterpart to [`Pinger`]: the calling thread parks directly in `recvmsg` waiting
/// for the reply (one dedicated OS thread per target). The headless daemon uses THIS, not the
/// async `Pinger`, because a socket whose owning thread is actively blocked in `recvmsg` stays
/// in the OS's "active" network path, whereas a kqueue-parked async socket gets power-save
/// deprioritized (~40ms added on Wi-Fi) once the process isn't the foreground app.
pub struct BlockingPinger {
    sock: Socket,
    target: SockAddr,
    target_ip: IpAddr,
    v6: bool,
    ident: u16,
}

impl BlockingPinger {
    pub fn new(addr: IpAddr) -> io::Result<BlockingPinger> {
        let v6 = addr.is_ipv6();
        let (domain, proto) = if v6 {
            (Domain::IPV6, Protocol::ICMPV6)
        } else {
            (Domain::IPV4, Protocol::ICMPV4)
        };
        // Blocking socket on purpose — do NOT set_nonblocking.
        let sock = Socket::new(domain, Type::DGRAM, Some(proto))?;

        let on: libc::c_int = 1;
        let rc = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TIMESTAMP,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(BlockingPinger {
            sock,
            target: SocketAddr::new(addr, 0).into(),
            target_ip: addr,
            v6,
            ident: next_ident(),
        })
    }

    /// Send one echo and block (up to `timeout`) for its reply; RTT from the kernel RX stamp.
    /// Returns None on timeout/loss.
    pub fn ping(&self, seq: u16, timeout: Duration) -> Option<f64> {
        let send = now_realtime();
        let pkt = build_echo(self.v6, self.ident, seq);
        if self.sock.send_to(&pkt, &self.target).is_err() {
            return None;
        }

        let fd = self.sock.as_raw_fd();
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            // SO_RCVTIMEO, so a lost reply can't block the thread past its budget.
            if self.sock.set_read_timeout(Some(remaining)).is_err() {
                return None;
            }
            match recv_once(fd) {
                // Must be our seq AND from our target: an unconnected ICMP datagram socket also
                // receives other targets' echo replies, and with all targets pinging in lockstep
                // a wrong-target reply can share our seq and yield a bogus reading.
                Ok(Some((rseq, rx, _, src))) if rseq == seq && src == self.target_ip => {
                    let rtt = delta_ms(send, rx);
                    if rtt > 0.0 {
                        return Some(rtt);
                    }
                }
                Ok(_) => continue,     // not ours (wrong seq/source) or non-reply -> keep waiting
                Err(_) => return None, // timeout (EAGAIN) or error -> loss
            }
        }
    }
}
