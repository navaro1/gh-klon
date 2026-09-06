//! The DNS rescue listener of the pasta namespace (C23):
//! `gh-klon __dns-forward`. A namespace shares the host's
//! `/etc/resolv.conf`, and a loopback resolver on that file is dead inside
//! the namespace: nothing listens there, and pasta never sees a query that
//! the kernel keeps on `lo`. This command fills the gap. It binds the stub
//! address inside the namespace and relays every query to a routable
//! upstream resolver, whose packets travel out through pasta like any other
//! traffic.
//!
//! The listener is plain plumbing, like pasta itself: it holds no state, it
//! parses nothing (DNS packets pass through as opaque bytes), and it starts
//! and stops with the command it serves (see `cli::fence_exec`). A person
//! never types it.

use crate::{Error, Result};
use std::io;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::thread;
use std::time::Duration;

/// How long one upstream may take to answer a UDP query. glibc retries, so a
/// lost query costs latency, not correctness.
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// One upstream word: a socket address, or a bare address on port 53. The
/// bare form must go through `IpAddr`, so an IPv6 literal becomes
/// `[::1]:53` and not the unparseable `::1:53`.
fn parse_upstream(text: &str) -> Result<SocketAddr> {
    text.parse::<SocketAddr>()
        .or_else(|_| text.parse::<IpAddr>().map(|ip| SocketAddr::new(ip, 53)))
        .map_err(|_| Error::klon(format!("{text} is not a literal address")))
}

#[derive(clap::Args)]
pub struct Args {
    /// The address and port to listen on, for example `127.0.0.53:53`.
    #[arg(long, value_name = "ADDR")]
    pub bind: SocketAddr,
    /// The upstream resolvers, in try order. A plain address means port 53.
    #[arg(value_name = "UPSTREAM")]
    pub upstreams: Vec<String>,
}

pub fn run(args: Args) -> Result<()> {
    if args.upstreams.is_empty() {
        return Err(Error::klon("name at least one upstream resolver"));
    }
    // Upstreams must be literal addresses: this listener is the resolver, so
    // it cannot resolve names itself.
    let upstreams: Vec<SocketAddr> = args
        .upstreams
        .iter()
        .map(String::as_str)
        .map(parse_upstream)
        .collect::<Result<_>>()?;
    let udp = match UdpSocket::bind(args.bind) {
        Ok(udp) => udp,
        // A port under 1024 needs a capability that only the namespace's user
        // namespace grants here. The listener is a rescue: without it the
        // command runs as before, and its resolver simply fails.
        Err(err) => {
            eprintln!(
                "klon: cannot bind the DNS rescue listener {}: {err}",
                args.bind
            );
            return Ok(());
        }
    };
    let tcp = match TcpListener::bind(args.bind) {
        Ok(tcp) => Some(tcp),
        Err(err) => {
            eprintln!(
                "klon: cannot bind the DNS rescue TCP socket {}: {err}",
                args.bind
            );
            None
        }
    };
    // One thread per query. A resolver sees a handful of queries per command,
    // and each query needs its own upstream socket so replies cannot mix.
    let upstreams_for_udp = upstreams.clone();
    let reply_socket = udp.try_clone().map_err(Error::io("share the UDP socket"))?;
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match udp.recv_from(&mut buf) {
                Ok((len, peer)) => {
                    let query = buf[..len].to_vec();
                    let ups = upstreams_for_udp.clone();
                    // A fresh socket per query keeps the replies apart.
                    if let Ok(replies) = reply_socket.try_clone() {
                        thread::spawn(move || relay_udp(&replies, &query, peer, &ups));
                    }
                }
                Err(_) => continue,
            }
        }
    });
    if let Some(tcp) = tcp {
        for accepted in tcp.incoming() {
            let Ok(client) = accepted else {
                continue;
            };
            let ups = upstreams.clone();
            thread::spawn(move || relay_tcp(client, &ups));
        }
        Ok(())
    } else {
        // No TCP side, but the UDP side still works. The process must stay
        // alive: its end comes from the parent's exit (PR_SET_PDEATHSIG),
        // not from this function returning.
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }
}

/// Answer one UDP query from the first upstream that replies. The reply
/// leaves through `replies`, the listener's own socket: a DNS client drops a
/// reply whose source differs from the address and port it queried (the
/// resolver-port defense), so a fresh socket for the upstream leg is not
/// allowed to answer the client.
fn relay_udp(replies: &UdpSocket, query: &[u8], peer: SocketAddr, upstreams: &[SocketAddr]) {
    let family = if peer.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let Ok(udp) = UdpSocket::bind(family) else {
        return;
    };
    let _ = udp.set_read_timeout(Some(REPLY_TIMEOUT));
    for upstream in upstreams {
        if udp.send_to(query, *upstream).is_err() {
            continue;
        }
        let mut reply = [0u8; 4096];
        if let Ok((len, _)) = udp.recv_from(&mut reply) {
            let _ = replies.send_to(&reply[..len], peer);
            return;
        }
    }
}

/// Relay one TCP connection byte for byte in both directions. DNS over TCP
/// carries a two-byte length prefix, which a plain pump forwards correctly.
fn relay_tcp(mut client: TcpStream, upstreams: &[SocketAddr]) {
    let mut upstream = None;
    for candidate in upstreams {
        if let Ok(stream) = TcpStream::connect(candidate) {
            upstream = Some(stream);
            break;
        }
    }
    let Some(upstream) = upstream else {
        return;
    };
    let Ok(mut client_read) = client.try_clone() else {
        return;
    };
    let Ok(mut upstream_read) = upstream.try_clone() else {
        return;
    };
    let Ok(mut upstream_write) = upstream.try_clone() else {
        return;
    };
    let to_upstream = thread::spawn(move || {
        let copied = io::copy(&mut client_read, &mut upstream_write);
        // The client is done: end our write side, so the upstream sees an EOF
        // and closes, and the reply pump below ends with it.
        let _ = upstream_write.shutdown(Shutdown::Write);
        copied
    });
    let _ = io::copy(&mut upstream_read, &mut client);
    let _ = to_upstream.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn an_upstream_word_takes_a_bare_address_or_a_socket_address() {
        let bare = parse_upstream("1.1.1.1").unwrap();
        assert_eq!(bare, "1.1.1.1:53".parse().unwrap());
        // The IPv6 form must keep its brackets parseable.
        let v6 = parse_upstream("::1").unwrap();
        assert_eq!(v6, "[::1]:53".parse().unwrap());
        let with_port = parse_upstream("10.0.0.1:5353").unwrap();
        assert_eq!(with_port, "10.0.0.1:5353".parse().unwrap());
        assert!(parse_upstream("example.com").is_err());
    }

    #[test]
    fn udp_queries_reach_the_upstream_and_the_reply_returns() {
        let upstream = UdpSocket::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 64];
            while let Ok((len, peer)) = upstream.recv_from(&mut buf) {
                let _ = upstream.send_to(&buf[..len], peer);
            }
        });
        let checker = UdpSocket::bind("127.0.0.1:0").unwrap();
        let checker_addr = checker.local_addr().unwrap();
        let query: &[u8] = b"\x12\x34\x01\x00query";
        let upstreams = vec![upstream_addr];
        let replies = UdpSocket::bind("127.0.0.1:0").unwrap();
        thread::spawn(move || relay_udp(&replies, query, checker_addr, &upstreams));
        checker
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reply = [0u8; 64];
        let len = checker.recv(&mut reply).unwrap();
        assert_eq!(&reply[..len], query, "the reply must be the query echoed");
    }

    #[test]
    fn tcp_connections_pump_both_directions() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        thread::spawn(move || {
            for accepted in upstream.incoming().flatten() {
                thread::spawn(move || {
                    let mut write = accepted.try_clone().unwrap();
                    let mut read = accepted;
                    let _ = io::copy(&mut read, &mut write);
                });
            }
        });
        let client_side = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(client_side.local_addr().unwrap()).unwrap();
        let (accepted, _) = client_side.accept().unwrap();
        let upstreams = vec![upstream_addr];
        thread::spawn(move || relay_tcp(accepted, &upstreams));
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client.write_all(b"\x00\x1cping").unwrap();
        let mut reply = [0u8; 6];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"\x00\x1cping");
    }
}
