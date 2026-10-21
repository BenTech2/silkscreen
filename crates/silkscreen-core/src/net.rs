//! Socket helpers tuned for low-latency LAN streaming.

use std::io;
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};

/// Large socket buffers absorb keyframe bursts (a 4K IDR is ~1-2 MB).
const SOCKET_BUFFER: usize = 8 * 1024 * 1024;

pub fn udp_socket(bind: SocketAddr) -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::for_address(bind), Type::DGRAM, Some(Protocol::UDP))?;
    // Best effort: the OS may cap these (Linux: net.core.rmem_max).
    let _ = s.set_recv_buffer_size(SOCKET_BUFFER);
    let _ = s.set_send_buffer_size(SOCKET_BUFFER);
    // Mark video as interactive traffic (DSCP AF41) for Wi-Fi WMM queues.
    #[cfg(not(windows))]
    let _ = s.set_tos_v4(0x88);
    s.bind(&bind.into())?;
    Ok(s.into())
}

pub fn tune_tcp(s: &TcpStream) -> io::Result<()> {
    s.set_nodelay(true)?;
    let sock = socket2::SockRef::from(s);
    let _ = sock.set_keepalive(true);
    let _ = sock.set_tcp_keepalive(&socket2::TcpKeepalive::new().with_time(Duration::from_secs(5)));
    Ok(())
}

pub fn now_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// This machine's LAN address (the one on the default route). No packets
/// are sent: connecting a UDP socket only picks a route.
pub fn local_ip() -> Option<std::net::IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip())
}
