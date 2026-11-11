//! keelOS's L2 protocol, host side (keelOS docs/113; the user 2026-11-09: a Mac should show in
//! kRDPclient's Connection > Discover keelOS like a keelOS host does, kRDPclient unchanged).
//! Two things, as keel's own keel/internal/l2/server.go does them:
//!
//! - DISCOVER (a broadcast to UDP 7385) is answered with an OFFER: this Mac's uuid, name,
//!   version, the answering interface's MAC and IPv4, the web face's port, and the SHA-256 of
//!   the certificate's public key, which kRDPclient pins.
//! - OPEN starts a session: a reliable byte stream over KL2 packets (stream.rs), joined here to
//!   a TCP connection to the web face on 127.0.0.1. The TLS inside is the web face's own, end
//!   to end, so what kRDPclient does over it (POST /login, POST /v1/tokens) is web.rs as is.
//!
//! What differs from keel: keel writes whole frames through BPF so that it answers without an
//! IP address; a Mac has one, and BPF needs root here, so this is a plain UDP socket. The
//! answers go to the asker's address and port as ordinary unicast when the asker is on one of
//! our subnets, else as a broadcast to its port (a PC with only a 169.254 address still hears
//! us). The client's packets come as broadcasts carrying our interface's MAC (§4.1), which is
//! how a session is told from another host's.
//!
//! MACRDP_L2=0 turns it off. Only the same broadcast domain is reached; that is the protocol.

pub mod proto;
pub mod stream;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use proto::{DISCOVER, Header, Mac, OFFER, OPEN, OPEN_FAIL, OPEN_OK, Packet, RESET, TAG_NONCE, TAG_REASON, TAG_SERVICE};
use stream::{Conn, FIN_LINGER};

/// keel's limits (server.go): sessions at once, OFFERs a second per asker, the window we take.
const MAX_SESSIONS: usize = 16;
const OFFERS_PER_SECOND: u32 = 10;
const WINDOW: u16 = 64;
/// The interface list is read again when it is older than this (cable, Wi-Fi, DHCP change).
const IFACES_FOR: Duration = Duration::from_secs(5);
const COPY_BUFFER: usize = 16 * 1024;

/// What every OFFER says of this Mac.
pub struct Identity {
    pub uuid: String,
    pub hostname: String,
    pub version: String,
    pub model: String,
    pub https_port: u16,
    pub spki: Option<[u8; 32]>,
}

/// An interface that can answer: up, not loopback, broadcast-capable, with a MAC and IPv4.
#[derive(Clone, Debug)]
struct Iface {
    name: String,
    mac: Mac,
    ipv4: Vec<(Ipv4Addr, u8)>,
}

fn interfaces() -> Vec<Iface> {
    let mut found: Vec<Iface> = Vec::new();
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            let flags = ifa.ifa_flags as i32;
            if ifa.ifa_addr.is_null()
                || flags & libc::IFF_UP == 0
                || flags & libc::IFF_LOOPBACK != 0
                || flags & libc::IFF_BROADCAST == 0
            {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            let at = match found.iter().position(|i| i.name == name) {
                Some(at) => at,
                None => {
                    found.push(Iface { name, mac: [0; 6], ipv4: Vec::new() });
                    found.len() - 1
                }
            };
            match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_LINK => {
                    let dl = &*(ifa.ifa_addr as *const libc::sockaddr_dl);
                    if dl.sdl_alen == 6 {
                        // the address follows the name inside sdl_data (which runs past its
                        // declared 12 bytes for long names: the sockaddr is sa_len long)
                        let p = (dl.sdl_data.as_ptr() as *const u8).add(dl.sdl_nlen as usize);
                        std::ptr::copy_nonoverlapping(p, found[at].mac.as_mut_ptr(), 6);
                    }
                }
                libc::AF_INET if !ifa.ifa_netmask.is_null() => {
                    let v4 = |sa: *const libc::sockaddr| u32::from_be((*(sa as *const libc::sockaddr_in)).sin_addr.s_addr);
                    let prefix = v4(ifa.ifa_netmask).count_ones() as u8;
                    found[at].ipv4.push((Ipv4Addr::from(v4(ifa.ifa_addr)), prefix));
                }
                _ => {}
            }
        }
        libc::freeifaddrs(list);
    }
    found.retain(|i| i.mac != [0; 6] && !i.ipv4.is_empty());
    found
}

fn same_net(a: Ipv4Addr, prefix: u8, b: Ipv4Addr) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
    u32::from(a) & mask == u32::from(b) & mask
}

/// Where an answer to `from` goes: to it, or to its port as a broadcast when it is on none of
/// our subnets (ordinary routing would send that to the gateway, not to the asker).
fn answer_to(ifaces: &[Iface], from: SocketAddrV4) -> SocketAddrV4 {
    let near = ifaces.iter().any(|i| i.ipv4.iter().any(|&(a, p)| same_net(a, p, *from.ip())));
    if near { from } else { SocketAddrV4::new(Ipv4Addr::BROADCAST, from.port()) }
}

struct Session {
    conn: Conn,
    nonce: Vec<u8>,
}

struct Server {
    socket: Arc<UdpSocket>,
    id: Identity,
    web_port: u16,
    ifaces: Vec<Iface>,
    ifaces_at: Instant,
    /// by the asker's address and port (each session has its own port, dial.rs) and its id
    sessions: HashMap<(SocketAddrV4, u32), Session>,
    /// OFFERs this second, per asker
    rate: HashMap<Ipv4Addr, (Instant, u32)>,
}

/// Starts the responder on its own thread. Its failure (the port taken) leaves the rest alone:
/// the Mac is then added by address, as before.
pub fn serve(id: Identity, web_port: u16) -> anyhow::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, proto::PORT))?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    info!("L2 discovery on UDP {} (kRDPclient: Connection > Discover keelOS)", proto::PORT);
    let mut server = Server {
        socket: Arc::new(socket),
        id,
        web_port,
        ifaces: interfaces(),
        ifaces_at: Instant::now(),
        sessions: HashMap::new(),
        rate: HashMap::new(),
    };
    std::thread::Builder::new().name("l2".into()).spawn(move || server.run())?;
    Ok(())
}

impl Server {
    fn run(&mut self) {
        let mut buf = [0u8; 2048];
        loop {
            // ended sessions stay FIN_LINGER longer, so a peer whose last ACK was lost gets it
            // again instead of a RESET (server.go `ended`)
            self.sessions.retain(|_, s| !s.conn.ended_at().is_some_and(|t| t.elapsed() >= FIN_LINGER));
            let (n, from) = match self.socket.recv_from(&mut buf) {
                Ok((n, SocketAddr::V4(from))) => (n, from),
                Ok(_) => continue,
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                Err(e) => {
                    debug!(error = %e, "l2 recv");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            let Ok(p) = Packet::parse(&buf[..n]) else { continue };
            if self.ifaces_at.elapsed() >= IFACES_FOR {
                self.ifaces = interfaces();
                self.ifaces_at = Instant::now();
            }
            self.handle(p, from);
        }
    }

    fn send(&self, to: SocketAddrV4, p: Packet) {
        if let Err(e) = self.socket.send_to(&p.encode(), to) {
            debug!(error = %e, %to, "l2 send");
        }
    }

    fn allow(&mut self, asker: Ipv4Addr) -> bool {
        let now = Instant::now();
        if self.rate.len() > 4096 {
            self.rate.retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(1));
        }
        let slot = self.rate.entry(asker).or_insert((now, 0));
        if now.duration_since(slot.0) >= Duration::from_secs(1) {
            *slot = (now, 0);
        }
        slot.1 += 1;
        slot.1 <= OFFERS_PER_SECOND
    }

    fn handle(&mut self, p: Packet, from: SocketAddrV4) {
        let to = answer_to(&self.ifaces, from);
        if p.header.kind == DISCOVER {
            if !self.allow(*from.ip()) {
                return;
            }
            let nonce = p.tlv(TAG_NONCE).unwrap_or_default().to_vec();
            // the interfaces on the asker's subnet; an asker on none of them hears every one
            let near: Vec<&Iface> =
                self.ifaces.iter().filter(|i| i.ipv4.iter().any(|&(a, pre)| same_net(a, pre, *from.ip()))).collect();
            let answering: Vec<&Iface> = if near.is_empty() { self.ifaces.iter().collect() } else { near };
            for i in answering {
                let payload = proto::offer_payload(
                    &nonce,
                    &self.id.uuid,
                    &self.id.hostname,
                    &self.id.version,
                    &self.id.model,
                    &i.name,
                    i.mac,
                    &i.ipv4,
                    self.id.https_port,
                    self.id.spki.as_ref(),
                );
                self.send(to, Packet { header: Header { kind: OFFER, target: i.mac, ..Header::default() }, payload });
            }
            debug!(%from, "l2 DISCOVER answered");
            return;
        }
        if p.header.session == 0 || !self.ifaces.iter().any(|i| i.mac == p.header.target) {
            return; // another host's
        }
        let key = (from, p.header.session);
        if p.header.kind == OPEN {
            return self.open(p, key, to);
        }
        match self.sessions.get(&key) {
            Some(s) => s.conn.input(&p),
            None if p.header.kind != RESET => self.refuse(RESET, &p, to, "unknown_session"),
            None => {}
        }
    }

    fn refuse(&self, kind: u8, p: &Packet, to: SocketAddrV4, reason: &str) {
        let mut payload = Vec::new();
        proto::push_tlv(&mut payload, TAG_REASON, reason.as_bytes());
        let header = Header { kind, target: p.header.target, session: p.header.session, ..Header::default() };
        self.send(to, Packet { header, payload });
    }

    fn open(&mut self, p: Packet, key: (SocketAddrV4, u32), to: SocketAddrV4) {
        let nonce = p.tlv(TAG_NONCE).unwrap_or_default().to_vec();
        let ok = |server: &Server| {
            let mut payload = Vec::new();
            proto::push_tlv(&mut payload, TAG_NONCE, &nonce);
            let header = Header {
                kind: OPEN_OK,
                target: p.header.target,
                session: p.header.session,
                window: WINDOW,
                ..Header::default()
            };
            server.send(to, Packet { header, payload });
        };
        if let Some(s) = self.sessions.get(&key) {
            if s.nonce == nonce && s.conn.ended_at().is_none() {
                ok(self); // our OPEN_OK was lost
            }
            return;
        }
        if p.tlv(TAG_SERVICE) != Some(b"https") {
            return self.refuse(OPEN_FAIL, &p, to, "unknown_service");
        }
        if self.sessions.values().filter(|s| s.conn.ended_at().is_none()).count() >= MAX_SESSIONS {
            return self.refuse(OPEN_FAIL, &p, to, "too_many_sessions");
        }
        let tcp = match TcpStream::connect((Ipv4Addr::LOCALHOST, self.web_port)) {
            Ok(tcp) => tcp,
            Err(e) => {
                warn!(error = %e, "l2: the web face does not answer on 127.0.0.1:{}", self.web_port);
                return self.refuse(OPEN_FAIL, &p, to, "disabled");
            }
        };
        let socket = self.socket.clone();
        let send: stream::SendFn = Box::new(move |q: Packet| {
            if let Err(e) = socket.send_to(&q.encode(), to) {
                debug!(error = %e, "l2 send");
            }
        });
        let conn = Conn::new(p.header.session, p.header.target, WINDOW, send);
        self.sessions.insert(key, Session { conn: conn.clone(), nonce: nonce.clone() });
        ok(self);
        info!(from = %key.0, "l2 session opened");
        std::thread::spawn(move || forward(conn, tcp));
    }
}

/// One session joined to one TCP connection to the web face, until both sides are done
/// (kRDPclient's tunnel.rs, the other way round).
fn forward(conn: Conn, tcp: TcpStream) {
    let Ok(mut up) = tcp.try_clone() else {
        return conn.abort("closed");
    };
    let from_web = {
        let conn = conn.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; COPY_BUFFER];
            loop {
                match up.read(&mut buf) {
                    Ok(0) => return conn.close_write(),
                    Ok(n) => {
                        if conn.write_all(&buf[..n]).is_err() {
                            let _ = up.shutdown(Shutdown::Both);
                            return;
                        }
                    }
                    Err(_) => return conn.abort("closed"),
                }
            }
        })
    };
    let mut tcp = tcp;
    let mut buf = vec![0u8; COPY_BUFFER];
    loop {
        match conn.read(&mut buf) {
            Ok(0) => {
                let _ = tcp.shutdown(Shutdown::Write);
                break;
            }
            Ok(n) => {
                if tcp.write_all(&buf[..n]).is_err() {
                    conn.abort("closed");
                    break;
                }
            }
            Err(_) => {
                let _ = tcp.shutdown(Shutdown::Both);
                break;
            }
        }
    }
    let _ = from_web.join();
    conn.close();
    debug!("l2 session ended");
}

/// SHA-256 of the public key (the whole SubjectPublicKeyInfo, as keel's OFFER and kRDPclient's
/// pin take it) of the first certificate in a PEM file.
pub fn spki_sha256_of_pem(pem: &str) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN CERTIFICATE"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    let der = base64(&body)?;
    // Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { [0] version?, serialNumber,
    // signature, issuer, validity, subject, subjectPublicKeyInfo, ... }, ... }
    let (_, cert, _) = tlv(&der)?;
    let (_, mut tbs, _) = tlv(cert)?;
    if tbs.first() == Some(&0xa0) {
        tbs = tlv(tbs)?.2;
    }
    for _ in 0..5 {
        tbs = tlv(tbs)?.2;
    }
    let rest = tlv(tbs)?.2;
    let spki = &tbs[..tbs.len() - rest.len()];
    Some(Sha256::digest(spki).into())
}

/// One DER element at the head of `b`: its tag, its contents, what follows it.
fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = b.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        (rest[..n].iter().fold(0usize, |a, &x| a << 8 | x as usize), &rest[n..])
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnets() {
        let a = Ipv4Addr::new(192, 168, 11, 7);
        assert!(same_net(a, 24, Ipv4Addr::new(192, 168, 11, 200)));
        assert!(!same_net(a, 24, Ipv4Addr::new(192, 168, 12, 7)));
        assert!(!same_net(a, 24, Ipv4Addr::new(169, 254, 3, 4)));
        let ifaces = vec![Iface { name: "en0".into(), mac: [2, 0, 0, 0, 0, 1], ipv4: vec![(a, 24)] }];
        let near = SocketAddrV4::new(Ipv4Addr::new(192, 168, 11, 30), 50000);
        let far = SocketAddrV4::new(Ipv4Addr::new(169, 254, 3, 4), 50001);
        assert_eq!(answer_to(&ifaces, near), near);
        assert_eq!(answer_to(&ifaces, far), SocketAddrV4::new(Ipv4Addr::BROADCAST, 50001));
    }

    /// The OFFER we write reads back through the client's parser (kRDPclient's parse_offer).
    #[test]
    fn our_offer_parses_as_the_client_reads_it() {
        let payload = proto::offer_payload(
            &[7; 16],
            "11111111-2222-4333-8444-555555555555",
            "macmini",
            "0.1.0",
            "Mac16,10",
            "en0",
            [0x1c, 0xf6, 0x4c, 0x5f, 0xba, 0xf3],
            &[(Ipv4Addr::new(192, 168, 11, 7), 24)],
            443,
            Some(&[9; 32]),
        );
        let packet = Packet {
            header: Header { kind: OFFER, target: [0x1c, 0xf6, 0x4c, 0x5f, 0xba, 0xf3], ..Header::default() },
            payload,
        }
        .encode();
        let offer = proto::parse_offer(&packet).unwrap();
        assert_eq!(offer.nonce, vec![7; 16]);
        assert_eq!(offer.hostname, "macmini");
        assert_eq!(offer.model, "Mac16,10");
        assert!(!offer.setup);
        assert_eq!(offer.ifname, "en0");
        assert_eq!(offer.ifmac, [0x1c, 0xf6, 0x4c, 0x5f, 0xba, 0xf3]);
        assert_eq!(offer.ipv4, vec![(Ipv4Addr::new(192, 168, 11, 7), 24)]);
        assert_eq!(offer.https_port, 443);
        assert_eq!(offer.spki, Some([9; 32]));
    }

    /// A self-signed certificate made the way main.rs makes ours: the hash is over the same
    /// bytes rcgen says are the key's SubjectPublicKeyInfo.
    #[test]
    fn spki_of_a_certificate() {
        use sha2::{Digest, Sha256};
        let ck = rcgen::generate_simple_self_signed(vec!["macmini".to_owned()]).unwrap();
        let want: [u8; 32] = Sha256::digest(rcgen::PublicKeyData::subject_public_key_info(&ck.signing_key)).into();
        assert_eq!(spki_sha256_of_pem(&ck.cert.pem()), Some(want));
        assert_eq!(spki_sha256_of_pem("not a certificate"), None);
    }

    /// The responder and a web face on their own for two minutes, for a kRDPclient (or its
    /// l2probe example) on the same network to find and log in to, without the screen capture
    /// the real service needs (so it runs from ssh):
    /// `cargo test --release live_responder -- --ignored --nocapture`.
    /// The password is `l2-live-test`. Needs UDP 7385 free: stop a running hypermac that has L2.
    #[test]
    #[ignore]
    fn live_responder_for_two_minutes() {
        let dir = std::env::temp_dir().join(format!("hypermac-l2-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("credentials.json"), br#"{"user":"test","password":"l2-live-test"}"#).unwrap();
        let ck = rcgen::generate_simple_self_signed(vec!["hypermac-l2-live".to_owned()]).unwrap();
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&cert, ck.cert.pem()).unwrap();
        std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();
        let spki = spki_sha256_of_pem(&ck.cert.pem()).unwrap();
        let tls = ironrdp_server::TlsIdentityCtx::init_from_paths(&cert, &key).unwrap();
        const WEB: u16 = 18443;
        let st = Arc::new(crate::web::State {
            uuid: "00000000-0000-4000-8000-0000000012ff".into(),
            alias: "l2-live-test".into(),
            os_version: "0".into(),
            rdp_port: 3389,
            tokens: dir.join("tokens.json"),
            credentials: dir.join("credentials.json"),
            sessions: Default::default(),
            fails: Default::default(),
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let acceptor = tls.make_acceptor().unwrap();
        rt.spawn(crate::web::serve((Ipv4Addr::LOCALHOST, WEB).into(), acceptor, st.clone()));
        serve(
            Identity {
                uuid: st.uuid.clone(),
                hostname: st.alias.clone(),
                version: "test".into(),
                model: "test".into(),
                https_port: WEB,
                spki: Some(spki),
            },
            WEB,
        )
        .unwrap();
        println!("spki {}", crate::web::hex(&spki));
        for i in interfaces() {
            println!("iface {} {} {:?}", i.name, proto::mac_text(&i.mac), i.ipv4);
        }
        std::thread::sleep(Duration::from_secs(120));
        println!("tokens made: {}", crate::web::read_tokens(&dir.join("tokens.json")).sha256.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_machine_has_an_interface() {
        // not asserted beyond shape: a build machine may be offline
        for i in interfaces() {
            assert_ne!(i.mac, [0; 6]);
            assert!(!i.ipv4.is_empty());
        }
    }
}
