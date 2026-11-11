//! Copied unchanged from kRDPclient src/l2/stream.rs (bd3795f): the stream is the same on
//! both ends, so the host side is this code too.
#![allow(dead_code)]

//! The reliable stream of keelOS docs/113 §4.5: one session's bytes both ways over KL2 packets.
//! A port of keel's own (keel/internal/l2/stream.go, branch l2 at 75691f74), rule for rule, so
//! the two sides agree: a window of 64 packets, cumulative ACKs (at once for anything out of
//! order, else after 2 packets or 20 ms), RTO by RFC 6298 (200 ms until there is a sample, at
//! least 30 ms, at most 2 s), fast retransmit after 3 duplicate ACKs and NewReno after it, a
//! FIN taking one sequence number, window probes, keepalive, and RESET after 10 s without an
//! ACK. Blocking calls on plain threads: a session carries one HTTPS connection.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::proto::{ACK, DATA, FIN, Header, MAX_DATA, Mac, PING, Packet, RESET, TAG_REASON, push_tlv};

const WINDOW: usize = 64;
const ACK_DELAY: Duration = Duration::from_millis(20);
const RTO_INIT: Duration = Duration::from_millis(200);
const RTO_MAX: Duration = Duration::from_secs(2);
/// With RTT samples: a LAN's RTT is well under 1 ms.
const RTO_MIN: Duration = Duration::from_millis(30);
/// One packet unacknowledged this long: RESET.
const GIVE_UP: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_secs(15);
const DEAD_AFTER: Duration = Duration::from_secs(120);
/// Our FIN acknowledged, the peer's never came: end after this.
pub const FIN_LINGER: Duration = Duration::from_secs(10);
const PROBE_EVERY: Duration = Duration::from_millis(500);

fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn seq_le(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

/// A DATA or FIN in flight. A FIN takes one sequence number, so its ACK is seq+1.
struct Seg {
    seq: u32,
    data: Vec<u8>,
    fin: bool,
    sent: Instant,
    first: Option<Instant>,
    /// Sent again: no RTT sample from it (Karn).
    retx: bool,
}

impl Seg {
    fn end(&self) -> u32 {
        if self.fin {
            self.seq.wrapping_add(1)
        } else {
            self.seq.wrapping_add(self.data.len() as u32)
        }
    }
}

/// Puts one packet on the link.
pub type SendFn = Box<dyn Fn(Packet) + Send + Sync>;

struct State {
    // sending
    snd_una: u32,
    snd_nxt: u32,
    inflight: VecDeque<Seg>,
    peer_wnd: usize,
    /// Now: doubled by timeouts.
    rto: Duration,
    /// From the RTT samples.
    base_rto: Duration,
    srtt: Option<Duration>,
    rttvar: Duration,
    dup_acks: u32,
    /// In recovery (fast retransmit) until this is acknowledged.
    recover: u32,
    in_recovery: bool,
    write_waiting: bool,
    last_probe: Instant,
    fin_seq: u32,
    fin_queued: bool,
    fin_acked_at: Option<Instant>,

    // receiving
    rcv_nxt: u32,
    ooo: HashMap<u32, Vec<u8>>,
    peer_fin_at: Option<u32>,
    got_fin: bool,
    rbuf: VecDeque<u8>,
    /// Packets received since our last ACK.
    unacked: usize,
    ack_due: Instant,
    adv_wnd: usize,

    last_send: Instant,
    last_recv: Instant,
    /// `close` was called.
    closed: bool,
    err: Option<io::ErrorKind>,
    ended_at: Option<Instant>,
}

struct Shared {
    session: u32,
    target: Mac,
    send: SendFn,
    state: Mutex<State>,
    /// Readers and writers wait on it.
    cond: Condvar,
    /// The timer thread waits on it.
    timer: Condvar,
}

/// One session: a reliable byte stream. Clones share it.
#[derive(Clone)]
pub struct Conn(Arc<Shared>);

impl Conn {
    /// A session the host accepted (OPEN_OK said its window); `send` puts packets on the link.
    pub fn new(session: u32, target: Mac, peer_wnd: u16, send: SendFn) -> Self {
        let now = Instant::now();
        let state = State {
            snd_una: 0,
            snd_nxt: 0,
            inflight: VecDeque::new(),
            peer_wnd: peer_wnd as usize,
            rto: RTO_INIT,
            base_rto: RTO_INIT,
            srtt: None,
            rttvar: Duration::ZERO,
            dup_acks: 0,
            recover: 0,
            in_recovery: false,
            write_waiting: false,
            last_probe: now,
            fin_seq: 0,
            fin_queued: false,
            fin_acked_at: None,
            rcv_nxt: 0,
            ooo: HashMap::new(),
            peer_fin_at: None,
            got_fin: false,
            rbuf: VecDeque::new(),
            unacked: 0,
            ack_due: now,
            adv_wnd: WINDOW,
            last_send: now,
            last_recv: now,
            closed: false,
            err: None,
            ended_at: None,
        };
        let conn = Self(Arc::new(Shared {
            session,
            target,
            send,
            state: Mutex::new(state),
            cond: Condvar::new(),
            timer: Condvar::new(),
        }));
        let timers = conn.clone();
        std::thread::spawn(move || timers.timers());
        conn
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// When the session ended (both FINs acknowledged, a RESET, a timeout), if it has.
    pub fn ended_at(&self) -> Option<Instant> {
        self.lock().ended_at
    }

    /// A packet of this session from the link; never blocks on the application.
    pub fn input(&self, p: &Packet) {
        let mut st = self.lock();
        let kind = p.header.kind;
        if st.ended_at.is_some() {
            // our last ACK was lost: the peer sends again
            if st.err.is_none() && matches!(kind, FIN | DATA | PING) {
                let seq = st.snd_nxt;
                self.out(&mut st, ACK, seq, Vec::new());
            }
            return;
        }
        st.last_recv = Instant::now();
        if kind == RESET {
            self.fail(&mut st, io::ErrorKind::ConnectionReset, false);
            return;
        }
        if !matches!(kind, DATA | ACK | FIN | PING) {
            return;
        }
        self.ack(&mut st, p);
        let mut ack_now = false;
        match kind {
            DATA if !p.payload.is_empty() => ack_now = data(&mut st, p.header.seq, &p.payload),
            FIN => {
                st.peer_fin_at.get_or_insert(p.header.seq);
                ack_now = true;
            }
            PING => ack_now = true,
            _ => {}
        }
        if !st.got_fin && st.peer_fin_at == Some(st.rcv_nxt) {
            st.got_fin = true;
            st.rcv_nxt = st.rcv_nxt.wrapping_add(1);
            ack_now = true;
        }
        if ack_now || st.unacked >= 2 {
            let seq = st.snd_nxt;
            self.out(&mut st, ACK, seq, Vec::new());
        } else if st.unacked == 1 {
            st.ack_due = Instant::now() + ACK_DELAY;
        }
        self.0.cond.notify_all();
        self.end_if_done(&mut st);
        self.0.timer.notify_one();
    }

    /// Sends one packet with our ACK and window.
    fn out(&self, st: &mut State, kind: u8, seq: u32, payload: Vec<u8>) {
        let window = rwnd(st);
        st.adv_wnd = window;
        st.unacked = 0;
        st.last_send = Instant::now();
        let header = Header {
            kind,
            target: self.0.target,
            session: self.0.session,
            seq,
            ack: st.rcv_nxt,
            window: window as u16,
            ..Header::default()
        };
        (self.0.send)(Packet { header, payload });
    }

    /// (Re)sends the `index`th segment in flight.
    fn transmit(&self, st: &mut State, index: usize) {
        let now = Instant::now();
        let s = &mut st.inflight[index];
        s.sent = now;
        if s.first.is_none() {
            s.first = Some(now);
        } else {
            s.retx = true;
        }
        let (seq, kind, data) = if s.fin { (s.seq, FIN, Vec::new()) } else { (s.seq, DATA, s.data.clone()) };
        self.out(st, kind, seq, data);
    }

    /// The peer's cumulative ACK and window.
    fn ack(&self, st: &mut State, p: &Packet) {
        st.peer_wnd = p.header.window as usize;
        let a = p.header.ack;
        if seq_lt(st.snd_nxt, a) {
            return; // acknowledges what we never sent
        }
        if seq_lt(st.snd_una, a) {
            st.snd_una = a;
            st.dup_acks = 0;
            let mut last = None;
            while st.inflight.front().is_some_and(|s| seq_le(s.end(), a)) {
                last = st.inflight.pop_front();
            }
            if let Some(last) = last
                && !last.retx
            {
                sample(st, last.sent.elapsed());
            }
            st.rto = st.base_rto;
            if st.fin_queued && st.fin_acked_at.is_none() && seq_le(st.fin_seq.wrapping_add(1), a) {
                st.fin_acked_at = Some(Instant::now());
            }
            if st.in_recovery {
                if seq_lt(a, st.recover) && !st.inflight.is_empty() {
                    self.transmit(st, 0); // a partial ACK: the next hole
                } else {
                    st.in_recovery = false;
                }
            }
            return;
        }
        // a duplicate ACK: the window may shrink with each (the peer buffers what came after the hole)
        if p.header.kind == ACK && !st.inflight.is_empty() && a == st.snd_una {
            st.dup_acks += 1;
            if st.dup_acks == 3 && !st.in_recovery {
                st.in_recovery = true;
                st.recover = st.snd_nxt;
                self.transmit(st, 0);
            }
        }
    }

    /// Retransmission, the delayed ACK, keepalive, the dead peer, window probes.
    fn timers(&self) {
        let mut st = self.lock();
        loop {
            if st.ended_at.is_some() {
                return;
            }
            let now = Instant::now();
            if st.unacked > 0 && now >= st.ack_due {
                let seq = st.snd_nxt;
                self.out(&mut st, ACK, seq, Vec::new());
            }
            if let Some(head) = st.inflight.front() {
                if head.first.is_some_and(|f| now - f >= GIVE_UP) {
                    self.fail(&mut st, io::ErrorKind::TimedOut, true);
                    return;
                }
                if now - head.sent >= st.rto {
                    self.transmit(&mut st, 0);
                    st.rto = (st.rto * 2).min(RTO_MAX);
                    st.in_recovery = true;
                    st.recover = st.snd_nxt;
                    st.dup_acks = 0;
                }
            }
            if st.peer_wnd == 0 && st.write_waiting && now - st.last_probe >= PROBE_EVERY {
                st.last_probe = now;
                let seq = st.snd_nxt;
                self.out(&mut st, PING, seq, Vec::new());
            }
            if now - st.last_send >= KEEPALIVE {
                let seq = st.snd_nxt;
                self.out(&mut st, PING, seq, Vec::new());
            }
            if now - st.last_recv >= DEAD_AFTER {
                self.fail(&mut st, io::ErrorKind::TimedOut, true);
                return;
            }
            if let Some(acked) = st.fin_acked_at
                && !st.got_fin
                && st.closed
                && now - acked >= FIN_LINGER
            {
                self.fail(&mut st, io::ErrorKind::NotConnected, true);
                return;
            }
            let mut next = st.last_send + KEEPALIVE;
            if st.unacked > 0 {
                next = next.min(st.ack_due);
            }
            if let Some(head) = st.inflight.front() {
                next = next.min(head.sent + st.rto);
            }
            if st.peer_wnd == 0 && st.write_waiting {
                next = next.min(st.last_probe + PROBE_EVERY);
            }
            let wait = next.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
            st = self.0.timer.wait_timeout(st, wait).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Ends the session with `err`, with a RESET to the peer when `reset`.
    fn fail(&self, st: &mut State, err: io::ErrorKind, reset: bool) {
        if st.err.is_none() {
            st.err = Some(err);
        }
        if reset {
            let reason: &[u8] = if err == io::ErrorKind::NotConnected { b"closed" } else { b"timeout" };
            self.reset(reason);
        }
        st.inflight.clear();
        self.0.cond.notify_all();
        self.finish(st);
    }

    fn reset(&self, reason: &[u8]) {
        let mut payload = Vec::new();
        push_tlv(&mut payload, TAG_REASON, reason);
        let header = Header { kind: RESET, target: self.0.target, session: self.0.session, ..Header::default() };
        (self.0.send)(Packet { header, payload });
    }

    /// Both sides closed: the session is over.
    fn end_if_done(&self, st: &mut State) {
        if st.got_fin && st.fin_queued && st.fin_acked_at.is_some() {
            self.finish(st);
        }
    }

    fn finish(&self, st: &mut State) {
        if st.ended_at.is_none() {
            st.ended_at = Some(Instant::now());
            self.0.cond.notify_all();
            self.0.timer.notify_one();
        }
    }

    /// RESET with `reason`, and the session ends.
    pub fn abort(&self, reason: &str) {
        let mut st = self.lock();
        if st.ended_at.is_some() {
            return;
        }
        self.reset(reason.as_bytes());
        st.err = Some(io::ErrorKind::ConnectionReset);
        self.finish(&mut st);
    }

    /// Stream bytes into `buf`; 0 at the end of the peer's stream.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut st = self.lock();
        while st.rbuf.is_empty() {
            if st.closed {
                return Err(io::ErrorKind::NotConnected.into());
            }
            if st.got_fin {
                return Ok(0);
            }
            if let Some(err) = st.err {
                return Err(err.into());
            }
            st = self.0.cond.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        let n = buf.len().min(st.rbuf.len());
        for (dst, src) in buf.iter_mut().zip(st.rbuf.drain(..n)) {
            *dst = src;
        }
        // a window update once the reader made room
        if st.adv_wnd < WINDOW / 4 && rwnd(&st) >= st.adv_wnd + WINDOW / 4 {
            let seq = st.snd_nxt;
            self.out(&mut st, ACK, seq, Vec::new());
        }
        Ok(n)
    }

    /// Sends all of `buf`, waiting while the window is full.
    pub fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        let mut st = self.lock();
        while !buf.is_empty() {
            loop {
                if st.closed || st.fin_queued {
                    return Err(io::ErrorKind::NotConnected.into());
                }
                if let Some(err) = st.err {
                    return Err(err.into());
                }
                if st.inflight.len() < WINDOW.min(st.peer_wnd) {
                    break;
                }
                st.write_waiting = true;
                self.0.timer.notify_one();
                st = self.0.cond.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            st.write_waiting = false;
            let k = buf.len().min(MAX_DATA);
            let seq = st.snd_nxt;
            st.snd_nxt = seq.wrapping_add(k as u32);
            st.inflight.push_back(Seg {
                seq,
                data: buf[..k].to_vec(),
                fin: false,
                sent: Instant::now(),
                first: None,
                retx: false,
            });
            let last = st.inflight.len() - 1;
            self.transmit(&mut st, last);
            buf = &buf[k..];
            self.0.timer.notify_one();
        }
        Ok(())
    }

    /// FIN: the peer reads the end after the data. Reading goes on.
    pub fn close_write(&self) {
        let mut st = self.lock();
        self.queue_fin(&mut st);
    }

    fn queue_fin(&self, st: &mut State) {
        if st.fin_queued || st.ended_at.is_some() {
            return;
        }
        st.fin_queued = true;
        st.fin_seq = st.snd_nxt;
        let seq = st.snd_nxt;
        st.snd_nxt = seq.wrapping_add(1);
        st.inflight.push_back(Seg { seq, data: Vec::new(), fin: true, sent: Instant::now(), first: None, retx: false });
        let last = st.inflight.len() - 1;
        self.transmit(st, last);
        self.0.timer.notify_one();
    }

    /// FIN and no more reading; the session ends when both FINs are acknowledged.
    pub fn close(&self) {
        let mut st = self.lock();
        if st.closed {
            return;
        }
        st.closed = true;
        st.rbuf.clear();
        self.queue_fin(&mut st);
        self.0.cond.notify_all();
        self.end_if_done(&mut st);
    }
}

/// Packets the receiver can still take.
fn rwnd(st: &State) -> usize {
    WINDOW.saturating_sub(st.ooo.len() + st.rbuf.len().div_ceil(MAX_DATA))
}

/// One RTT measurement (RFC 6298 2.2–2.4).
fn sample(st: &mut State, r: Duration) {
    match st.srtt {
        None => {
            st.srtt = Some(r);
            st.rttvar = r / 2;
        }
        Some(srtt) => {
            let d = if srtt > r { srtt - r } else { r - srtt };
            st.rttvar = (st.rttvar * 3 + d) / 4;
            st.srtt = Some((srtt * 7 + r) / 8);
        }
    }
    let srtt = st.srtt.unwrap_or(r);
    st.base_rto = (srtt + st.rttvar * 4).clamp(RTO_MIN, RTO_MAX);
}

/// Takes stream bytes at `seq`; true when the ACK should go at once (out of order, a duplicate).
fn data(st: &mut State, mut seq: u32, mut b: &[u8]) -> bool {
    let end = seq.wrapping_add(b.len() as u32);
    if st.got_fin || seq_le(end, st.rcv_nxt) {
        return true; // all old
    }
    if seq_lt(seq, st.rcv_nxt) {
        // the front is old
        b = &b[st.rcv_nxt.wrapping_sub(seq) as usize..];
        seq = st.rcv_nxt;
    }
    if seq != st.rcv_nxt {
        if seq.wrapping_sub(st.rcv_nxt) as usize >= WINDOW * MAX_DATA || st.ooo.len() >= WINDOW {
            return true;
        }
        st.ooo.entry(seq).or_insert_with(|| b.to_vec());
        return true;
    }
    // nobody reads after close: acknowledged and dropped
    if !st.closed {
        st.rbuf.extend(b);
    }
    st.rcv_nxt = st.rcv_nxt.wrapping_add(b.len() as u32);
    st.unacked += 1;
    while !st.ooo.is_empty() {
        let mut moved = false;
        let keys: Vec<u32> = st.ooo.keys().copied().collect();
        for s in keys {
            let len = st.ooo[&s].len() as u32;
            let e = s.wrapping_add(len);
            if seq_le(e, st.rcv_nxt) {
                st.ooo.remove(&s);
                continue;
            }
            if seq_le(s, st.rcv_nxt) {
                let d = st.ooo.remove(&s).expect("present");
                if !st.closed {
                    let skip = st.rcv_nxt.wrapping_sub(s) as usize;
                    st.rbuf.extend(&d[skip..]);
                }
                st.rcv_nxt = e;
                moved = true;
            }
        }
        if !moved {
            return true; // still a hole
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;

    /// Two conns joined by a lossy link: a packet is dropped, duplicated or held back (and so
    /// reordered) by a cheap deterministic generator.
    fn pair(loss_per_mille: u64) -> (Conn, Conn) {
        let (to_b, from_a) = mpsc::channel::<Packet>();
        let (to_a, from_b) = mpsc::channel::<Packet>();
        let target = [2, 0, 0, 0, 0, 1];
        let lossy = move |tx: mpsc::Sender<Packet>| -> SendFn {
            let n = Arc::new(AtomicU64::new(0x9e37_79b9));
            let held = Arc::new(Mutex::new(None::<Packet>));
            Box::new(move |p: Packet| {
                let x = n.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                let roll = (x >> 33) % 1000;
                if roll < loss_per_mille {
                    return;
                }
                let mut held = held.lock().unwrap();
                if roll < loss_per_mille * 2 {
                    // reorder: this one goes after the next
                    if let Some(old) = held.replace(p) {
                        let _ = tx.send(old);
                    }
                    return;
                }
                let _ = tx.send(p.clone());
                if roll < loss_per_mille * 3 {
                    let _ = tx.send(p);
                }
                if let Some(old) = held.take() {
                    let _ = tx.send(old);
                }
            })
        };
        let a = Conn::new(7, target, 64, lossy(to_b));
        let b = Conn::new(7, target, 64, lossy(to_a));
        for (rx, conn) in [(from_a, b.clone()), (from_b, a.clone())] {
            std::thread::spawn(move || {
                for p in rx {
                    conn.input(&p);
                }
            });
        }
        (a, b)
    }

    fn transfer(loss_per_mille: u64, len: usize) {
        let (a, b) = pair(loss_per_mille);
        let sent: Vec<u8> = (0..len).map(|i| (i * 7 + i / 251) as u8).collect();
        let writer = {
            let a = a.clone();
            let sent = sent.clone();
            std::thread::spawn(move || {
                a.write_all(&sent).unwrap();
                a.close_write();
            })
        };
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = b.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        writer.join().unwrap();
        assert_eq!(got.len(), sent.len());
        assert!(got == sent, "the stream differs");
        b.close();
        a.close();
    }

    #[test]
    fn a_megabyte_on_a_clean_link() {
        transfer(0, 1 << 20);
    }

    #[test]
    fn a_megabyte_with_loss_reordering_and_duplicates() {
        transfer(50, 1 << 20);
    }

    #[test]
    fn both_fins_end_the_session() {
        let (a, b) = pair(0);
        a.write_all(b"hello").unwrap();
        a.close();
        let mut buf = [0u8; 16];
        assert_eq!(b.read(&mut buf).unwrap(), 5);
        assert_eq!(b.read(&mut buf).unwrap(), 0);
        b.close();
        let deadline = Instant::now() + Duration::from_secs(2);
        while (a.ended_at().is_none() || b.ended_at().is_none()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(a.ended_at().is_some() && b.ended_at().is_some());
    }

    #[test]
    fn a_reset_ends_the_reader() {
        let (a, b) = pair(0);
        a.abort("test");
        let mut buf = [0u8; 4];
        let err = b.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    }

    #[test]
    fn sequence_numbers_wrap() {
        assert!(seq_lt(u32::MAX - 1, 2));
        assert!(seq_le(5, 5));
        assert!(!seq_lt(2, u32::MAX - 1));
    }
}
