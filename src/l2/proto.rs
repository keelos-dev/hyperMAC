//! Copied from kRDPclient src/l2/proto.rs (bd3795f), which follows keelOS docs/113 §4 and
//! keel/internal/l2/proto.go; the OFFER encoder at the end is hyperMAC's (keel's HostInfo.Marshal).
#![allow(dead_code)]

//! The packets of keelOS's L2 protocol (keelOS docs/113 §4): the 32-byte header, the TLVs, and
//! the packet types. No sockets here. Tested against keel's own vectors (testdata/vectors.json).

use std::net::{Ipv4Addr, Ipv6Addr};

/// UDP port of the protocol on both sides (docs/113 §4.1).
pub const PORT: u16 = 7385;
/// `"KL2"` and version 1.
pub const MAGIC: [u8; 4] = *b"KL2\x01";
pub const HEADER_LEN: usize = 32;
pub const NONCE_LEN: usize = 16;
/// A UDP payload at most (docs/113 §4.1), and so what one DATA carries.
pub const MAX_UDP: usize = 1400;
pub const MAX_DATA: usize = MAX_UDP - HEADER_LEN;

pub const DISCOVER: u8 = 1;
pub const OFFER: u8 = 2;
pub const OPEN: u8 = 3;
pub const OPEN_OK: u8 = 4;
pub const OPEN_FAIL: u8 = 5;
pub const DATA: u8 = 6;
pub const ACK: u8 = 7;
pub const FIN: u8 = 8;
pub const RESET: u8 = 9;
pub const PING: u8 = 10;

pub const TAG_NONCE: u8 = 1;
const TAG_UUID: u8 = 2;
const TAG_HOSTNAME: u8 = 3;
const TAG_VERSION: u8 = 4;
const TAG_MODEL: u8 = 5;
const TAG_SETUP: u8 = 6;
const TAG_MANAGED: u8 = 7;
const TAG_IFNAME: u8 = 8;
const TAG_IFMAC: u8 = 9;
const TAG_IPV4: u8 = 10;
const TAG_IPV6: u8 = 11;
const TAG_HTTPS_PORT: u8 = 12;
const TAG_SPKI: u8 = 13;
pub const TAG_SERVICE: u8 = 14;
pub const TAG_REASON: u8 = 15;

pub type Mac = [u8; 6];
pub const BROADCAST_MAC: Mac = [0xff; 6];

/// Why a packet was not taken (the words keel's vectors use).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Short,
    Magic,
    Version,
    Length,
    Tlv,
    /// Not the type asked for, or an OFFER without a uuid.
    Unexpected,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::Short => "shorter than the header",
            Error::Magic => "not KL2",
            Error::Version => "a KL2 version other than 1",
            Error::Length => "length past the end",
            Error::Tlv => "a TLV runs past the end",
            Error::Unexpected => "not the packet expected",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub kind: u8,
    pub flags: u8,
    pub target: Mac,
    pub session: u32,
    pub seq: u32,
    pub ack: u32,
    pub window: u16,
}

/// A packet: its header and its payload (TLVs, or stream bytes for DATA).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub header: Header,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn encode(&self) -> Vec<u8> {
        let h = &self.header;
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&MAGIC);
        out.push(h.kind);
        out.push(h.flags);
        out.extend_from_slice(&h.target);
        out.extend_from_slice(&h.session.to_be_bytes());
        out.extend_from_slice(&h.seq.to_be_bytes());
        out.extend_from_slice(&h.ack.to_be_bytes());
        out.extend_from_slice(&h.window.to_be_bytes());
        out.extend_from_slice(&(self.payload.len() as u16).to_be_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn parse(packet: &[u8]) -> Result<Self, Error> {
        if packet.len() < HEADER_LEN {
            return Err(Error::Short);
        }
        if packet[..3] != MAGIC[..3] {
            return Err(Error::Magic);
        }
        if packet[3] != MAGIC[3] {
            return Err(Error::Version);
        }
        let be32 = |at: usize| u32::from_be_bytes(packet[at..at + 4].try_into().expect("4 bytes"));
        let length = u16::from_be_bytes([packet[26], packet[27]]) as usize;
        let payload = packet.get(HEADER_LEN..HEADER_LEN + length).ok_or(Error::Length)?;
        Ok(Self {
            header: Header {
                kind: packet[4],
                flags: packet[5],
                target: packet[6..12].try_into().expect("6 bytes"),
                session: be32(12),
                seq: be32(16),
                ack: be32(20),
                window: u16::from_be_bytes([packet[24], packet[25]]),
            },
            payload: payload.to_vec(),
        })
    }

    /// The first value of `tag` in a TLV payload.
    pub fn tlv(&self, tag: u8) -> Option<&[u8]> {
        tlvs(&self.payload).ok()?.into_iter().find(|(t, _)| *t == tag).map(|(_, v)| v)
    }
}

pub fn push_tlv(out: &mut Vec<u8>, tag: u8, value: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
}

/// The TLVs in order; unknown tags are the caller's to skip.
pub fn tlvs(mut payload: &[u8]) -> Result<Vec<(u8, &[u8])>, Error> {
    let mut out = Vec::new();
    while !payload.is_empty() {
        if payload.len() < 3 {
            return Err(Error::Tlv);
        }
        let len = u16::from_be_bytes([payload[1], payload[2]]) as usize;
        let value = payload.get(3..3 + len).ok_or(Error::Tlv)?;
        out.push((payload[0], value));
        payload = &payload[3 + len..];
    }
    Ok(out)
}

/// A DISCOVER: broadcast target MAC, session 0, the nonce as its only TLV.
pub fn discover(nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(3 + NONCE_LEN);
    push_tlv(&mut payload, TAG_NONCE, nonce);
    let header = Header { kind: DISCOVER, target: BROADCAST_MAC, ..Header::default() };
    Packet { header, payload }.encode()
}

/// An OPEN of an `https` session to the host interface `target`.
pub fn open(target: Mac, session: u32, nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    let mut payload = Vec::new();
    push_tlv(&mut payload, TAG_SERVICE, b"https");
    push_tlv(&mut payload, TAG_NONCE, nonce);
    let header = Header { kind: OPEN, target, session, ..Header::default() };
    Packet { header, payload }.encode()
}

/// One host interface's answer to a DISCOVER (docs/113 §4.4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Offer {
    pub nonce: Vec<u8>,
    pub uuid: String,
    pub hostname: String,
    pub version: String,
    pub model: String,
    /// Not set up yet: no administrator password.
    pub setup: bool,
    /// How many keel managers manage it.
    pub managed: u8,
    pub ifname: String,
    pub ifmac: Mac,
    pub ipv4: Vec<(Ipv4Addr, u8)>,
    pub ipv6: Vec<(Ipv6Addr, u8)>,
    pub https_port: u16,
    /// SHA-256 of the certificate's public key (SPKI).
    pub spki: Option<[u8; 32]>,
}

/// An OFFER packet (header included); known tags of the wrong size count as absent.
pub fn parse_offer(packet: &[u8]) -> Result<Offer, Error> {
    let p = Packet::parse(packet)?;
    if p.header.kind != OFFER {
        return Err(Error::Unexpected);
    }
    let text = |v: &[u8]| String::from_utf8_lossy(v).into_owned();
    let mut offer = Offer::default();
    for (tag, v) in tlvs(&p.payload)? {
        match (tag, v.len()) {
            (TAG_NONCE, _) => offer.nonce = v.to_vec(),
            (TAG_UUID, _) => offer.uuid = text(v).to_ascii_lowercase(),
            (TAG_HOSTNAME, _) => offer.hostname = text(v),
            (TAG_VERSION, _) => offer.version = text(v),
            (TAG_MODEL, _) => offer.model = text(v),
            (TAG_SETUP, 1) => offer.setup = v[0] == 1,
            (TAG_MANAGED, 1) => offer.managed = v[0],
            (TAG_IFNAME, _) => offer.ifname = text(v),
            (TAG_IFMAC, 6) => offer.ifmac.copy_from_slice(v),
            (TAG_IPV4, 5) => offer.ipv4.push((Ipv4Addr::new(v[0], v[1], v[2], v[3]), v[4])),
            (TAG_IPV6, 17) => {
                let a: [u8; 16] = v[..16].try_into().expect("16 bytes");
                offer.ipv6.push((Ipv6Addr::from(a), v[16]));
            }
            (TAG_HTTPS_PORT, 2) => offer.https_port = u16::from_be_bytes([v[0], v[1]]),
            (TAG_SPKI, 32) => offer.spki = Some(v.try_into().expect("32 bytes")),
            _ => {}
        }
    }
    if offer.uuid.is_empty() {
        return Err(Error::Unexpected);
    }
    Ok(offer)
}

pub fn mac_text(mac: &Mac) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// An OFFER's TLVs (docs/113 §4.4), in keel's order (proto.go `HostInfo.Marshal`): a Mac is
/// always set up (setup 0) and under no keel manager (managed 0); no IPv6 in this version.
#[allow(clippy::too_many_arguments)]
pub fn offer_payload(
    nonce: &[u8],
    uuid: &str,
    hostname: &str,
    version: &str,
    model: &str,
    ifname: &str,
    ifmac: Mac,
    ipv4: &[(Ipv4Addr, u8)],
    https_port: u16,
    spki: Option<&[u8; 32]>,
) -> Vec<u8> {
    let mut b = Vec::new();
    if !nonce.is_empty() {
        push_tlv(&mut b, TAG_NONCE, nonce);
    }
    for (tag, text) in [(TAG_UUID, uuid), (TAG_HOSTNAME, hostname), (TAG_VERSION, version), (TAG_MODEL, model)] {
        if !text.is_empty() {
            push_tlv(&mut b, tag, text.as_bytes());
        }
    }
    push_tlv(&mut b, TAG_SETUP, &[0]);
    push_tlv(&mut b, TAG_MANAGED, &[0]);
    if !ifname.is_empty() {
        push_tlv(&mut b, TAG_IFNAME, ifname.as_bytes());
    }
    push_tlv(&mut b, TAG_IFMAC, &ifmac);
    for (addr, prefix) in ipv4 {
        let mut v = addr.octets().to_vec();
        v.push(*prefix);
        push_tlv(&mut b, TAG_IPV4, &v);
    }
    if https_port != 0 {
        push_tlv(&mut b, TAG_HTTPS_PORT, &https_port.to_be_bytes());
    }
    if let Some(spki) = spki {
        push_tlv(&mut b, TAG_SPKI, spki);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// keel's vectors (docs/113 §4.6), copied from keelOS branch l2 at 75691f74
    /// (keel/internal/l2/testdata/vectors.json).
    fn vectors() -> Value {
        serde_json::from_str(include_str!("testdata/vectors.json")).expect("vectors.json")
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn named<'a>(v: &'a Value, name: &str) -> &'a Value {
        v["packets"].as_array().unwrap().iter().find(|p| p["name"] == name).unwrap()
    }

    #[test]
    fn every_vector_parses_to_its_header_and_encodes_back() {
        let v = vectors();
        assert_eq!(v["port"], PORT);
        for p in v["packets"].as_array().unwrap() {
            let bytes = unhex(p["hex"].as_str().unwrap());
            let parsed = Packet::parse(&bytes).unwrap_or_else(|e| panic!("{}: {e}", p["name"]));
            let h = &p["header"];
            assert_eq!(parsed.header.kind, h["type"].as_u64().unwrap() as u8, "{}", p["name"]);
            assert_eq!(mac_text(&parsed.header.target), h["target_mac"].as_str().unwrap());
            assert_eq!(parsed.header.session as u64, h["session"].as_u64().unwrap());
            assert_eq!(parsed.header.seq as u64, h["seq"].as_u64().unwrap());
            assert_eq!(parsed.header.ack as u64, h["ack"].as_u64().unwrap());
            assert_eq!(parsed.header.window as u64, h["window"].as_u64().unwrap());
            assert_eq!(parsed.payload.len() as u64, h["length"].as_u64().unwrap());
            if let Some(data) = p["data"].as_str() {
                assert_eq!(hex(&parsed.payload), data);
            }
            // flags are ignored, so a vector with unknown flag bits does not encode back
            if parsed.header.flags == 0 {
                assert_eq!(parsed.encode(), bytes, "{}", p["name"]);
            }
        }
    }

    #[test]
    fn offers_read_as_keel_says() {
        let v = vectors();
        for p in v["packets"].as_array().unwrap().iter().filter(|p| p["header"]["type"] == OFFER) {
            let o = parse_offer(&unhex(p["hex"].as_str().unwrap())).unwrap();
            let f = &p["fields"];
            assert_eq!(o.uuid, f["uuid"].as_str().unwrap());
            assert_eq!(o.hostname, f["hostname"].as_str().unwrap());
            assert_eq!(o.version, f["version"].as_str().unwrap());
            assert_eq!(o.model, f["model"].as_str().unwrap());
            assert_eq!(o.ifname, f["ifname"].as_str().unwrap());
            assert_eq!(mac_text(&o.ifmac), f["ifmac"].as_str().unwrap());
            assert_eq!(o.setup, f["setup"].as_bool().unwrap());
            assert_eq!(o.managed as u64, f["managed"].as_u64().unwrap());
            assert_eq!(o.https_port as u64, f["https_port"].as_u64().unwrap());
            assert_eq!(hex(&o.nonce), f["nonce"].as_str().unwrap());
            let ipv4: Vec<String> = o.ipv4.iter().map(|(a, n)| format!("{a}/{n}")).collect();
            let want: Vec<&str> = f["ipv4"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
            assert_eq!(ipv4, want, "{}", p["name"]);
            let ipv6: Vec<String> = o.ipv6.iter().map(|(a, n)| format!("{a}/{n}")).collect();
            let want: Vec<&str> = f["ipv6"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
            assert_eq!(ipv6, want, "{}", p["name"]);
            assert_eq!(o.spki.map(|s| hex(&s)).unwrap_or_default(), f["spki"].as_str().unwrap_or(""));
        }
    }

    #[test]
    fn what_we_send_is_byte_for_byte_keels() {
        let v = vectors();
        let nonce = |p: &Value| -> [u8; NONCE_LEN] {
            unhex(p["fields"]["nonce"].as_str().unwrap()).try_into().unwrap()
        };
        let d = named(&v, "discover");
        assert_eq!(hex(&discover(&nonce(d))), d["hex"].as_str().unwrap());
        let o = named(&v, "open");
        let target: Mac = [0x0c, 0xc4, 0x7a, 0x65, 0xf9, 0x2c];
        assert_eq!(hex(&open(target, 464367618, &nonce(o))), o["hex"].as_str().unwrap());
        let ok = Packet::parse(&unhex(named(&v, "open_ok")["hex"].as_str().unwrap())).unwrap();
        assert_eq!(ok.tlv(TAG_NONCE).map(hex), Some(hex(&nonce(o))));
        let fail = Packet::parse(&unhex(named(&v, "open_fail")["hex"].as_str().unwrap())).unwrap();
        assert_eq!(fail.tlv(TAG_REASON), Some(&b"too_many_sessions"[..]));
    }

    #[test]
    fn invalid_vectors_are_refused_for_the_right_reason() {
        for p in vectors()["invalid"].as_array().unwrap() {
            let bytes = unhex(p["hex"].as_str().unwrap());
            let got = match Packet::parse(&bytes) {
                Err(e) => e,
                Ok(_) => parse_offer(&bytes).unwrap_err(),
            };
            let want = match p["error"].as_str().unwrap() {
                "short" => Error::Short,
                "magic" => Error::Magic,
                "version" => Error::Version,
                "length" => Error::Length,
                "tlv" => Error::Tlv,
                other => panic!("unknown error {other}"),
            };
            assert_eq!(got, want, "{}", p["name"]);
        }
    }
}
