#![forbid(unsafe_code)]

//! Streams that arrive as SECS-II messages over HSMS. One data message is
//! one Stream: the stream and function that address it travel in the
//! origin URI, and the body — SECS-II items, encoded — is what Xmip carries.
//!
//! SECS/GEM is how semiconductor equipment talks to the factory host: SEMI
//! E37 (HSMS) carries SEMI E5 (SECS-II) messages over TCP, conventionally
//! on port 5000, addressed by stream and function — S1F13 establishes
//! communication, S6F11 reports an event, S2F41 sends a remote command —
//! with a wait bit that says a reply is expected under the next function
//! up. A Receive Location is the passive side: it listens, accepts one
//! equipment's select, and hands each data message up. A Send Location is
//! the active side: it connects, selects, sends one message as the stream
//! and function the target names, waits for the reply if the wait bit is
//! set, and separates. [`Connection`] is either side for a Journey that
//! must reply, and [`item`] reads and writes the SECS-II bodies.
//!
//! The origin URI carries what the header knew:
//! `secs-gem://peer?session=1&stream=6&function=11`. A target is
//! `secs-gem://host:5000?stream=6&function=11&wait=1`, S6F11 without a
//! query, or a bare `host:port`.
//!
//! **How a receive acknowledges.** A primary with the wait bit set is
//! answered after the whole receive cycle, on the connection it came on:
//! [`Verdict::Accepted`] replies under the next function up with
//! [`ACCEPTED`], `<B 0>`, the code SECS-II's acknowledge items share for
//! accepted; [`Verdict::Refused`] replies under the next function up with
//! [`NOT_ACCEPTED`], `<B 1>`, the code those items give a message received
//! and not accepted (SEMI E5, section 10, the data item dictionary: ACKC5,
//! ACKC6 *1–63 error, not accepted*), so the equipment does not send it
//! again; [`Verdict::Failed`] replies `SxF0`, the abort SEMI E5 (section
//! 9.2, function 0) gives a receiver that will not complete the
//! transaction, so the equipment sends it again. A receive stops at such a
//! primary and keeps the connection, so the next receive goes on reading it
//! once the reply is written. A message
//! without the wait bit expects nothing back: acceptance is at-most-once
//! there ([`AT_MOST_ONCE`]). Each message arrives whole.

pub mod connection;
pub mod hsms;
pub mod item;

use std::net::TcpListener;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

pub use connection::Connection;
pub use hsms::{Header, Message, SType};
pub use item::Item;
use net::Target;
use transport::ArrivalIdentity;
use transport::error::{Result, TransportError, protocol_error};
use transport::kept::Kept;
use transport::listening::{Accepting, Listening};
use transport::loopback::{FarEnd, LOOPBACK_TIMEOUT, Loopback};
use transport::socket;
use transport::{Acknowledgement, Taken, Verdict};
use transport::{Arrived, Configured, Directions, Transport};
use xcore::settings::{Applies, Fixed, Kind, Presence, Read, Setting, Settings};

/// The body of the reply to an accepted primary: `<B 0>`, accepted, the
/// code ACKC5, ACKC6, ACKC10, HCACK and the other acknowledge items share.
pub const ACCEPTED: [u8; 3] = [0x21, 0x01, 0x00];

/// The body of the reply to a refused primary: `<B 1>`, error, not
/// accepted, the first of the codes 1–63 the acknowledge items give a
/// message the receiver will not take.
pub const NOT_ACCEPTED: [u8; 3] = [0x21, 0x01, 0x01];

/// Why a message without the wait bit cannot be acknowledged after the
/// receive cycle.
pub const AT_MOST_ONCE: &str = "a SECS-II message without the wait bit expects no reply, so the \
                                equipment is never told how the receive cycle ended";

/// The connection a receive stopped on, its primary waiting for a reply.
type Open = Arc<Mutex<Option<Connection>>>;

#[derive(Clone)]
pub struct SecsGemTransport {
    bind: String,
    session_id: u16,
    wait: bool,
    timeout: Option<Duration>,
    /// The listener the first receive binds, and every receive takes from.
    receiving: Kept<TcpListener>,
    /// The connection a receive stopped on at a waiting primary, which the
    /// verdict replies on and the next receive goes on reading.
    open: Open,
}

impl SecsGemTransport {
    /// Listen at `bind`; `0.0.0.0:5000` is the conventional port. Selects
    /// are made under session id 0xFFFF until [`Self::with_session`] says
    /// otherwise, and a message sent does not wait for a reply unless the
    /// target or [`Self::awaiting_reply`] says so.
    #[must_use]
    pub fn new(bind: impl Into<String>) -> Self {
        Self {
            bind: bind.into(),
            session_id: connection::ANY_SESSION,
            wait: false,
            timeout: None,
            receiving: Kept::new(),
            open: Open::default(),
        }
    }

    /// The session id — the device id — a select is made under.
    #[must_use]
    const fn with_session(mut self, session_id: u16) -> Self {
        self.session_id = session_id;
        self
    }

    /// Set the wait bit on what is sent and take the reply, unless the
    /// target says `wait=0`.
    #[must_use]
    const fn awaiting_reply(mut self) -> Self {
        self.wait = true;
        self
    }

    /// Give up on a peer that stops mid-message or never replies.
    #[must_use]
    pub const fn timing_out_after(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Bind the listener and report the address actually assigned.
    ///
    /// # Errors
    /// Where the address is taken, malformed, or not permitted.
    pub fn bind(&self) -> Result<(TcpListener, String)> {
        socket::bind_tcp(&self.bind)
    }

    /// Accept one active side on an already-bound listener and grant its
    /// select.
    ///
    /// # Errors
    /// Where the connection could not be accepted or did not select.
    pub fn accept_one(&self, listener: &TcpListener) -> Result<Connection> {
        Connection::accept(listener, self.timeout)
    }

    /// Connect to `address` as the active side and select.
    ///
    /// # Errors
    /// Where the peer could not be reached or refused the select.
    pub fn connect(&self, address: &str) -> Result<Connection> {
        Connection::connect(address, self.session_id, self.timeout)
    }

    /// What a target names: the address, the stream and function, and
    /// whether to wait. `secs-gem://host:5000?stream=6&function=11&wait=1`,
    /// or `host:port` for S6F11.
    ///
    /// # Errors
    /// A stream, function or wait in the query that is not a number.
    pub fn resolve(&self, target: &str) -> Result<(String, u8, u8, bool)> {
        let Some(named) = Target::under(&["secs-gem"], target) else {
            return Ok((target.to_string(), 6, 11, self.wait));
        };
        let (mut stream, mut function, mut wait) = (6u8, 11u8, self.wait);
        for (key, value) in named.query() {
            let number: u8 = value
                .parse()
                .map_err(|_| protocol_error(format!("{key}={value:?} is not a number")))?;
            match key.as_str() {
                "stream" => stream = number,
                "function" => function = number,
                "wait" => wait = number != 0,
                _ => {}
            }
        }
        Ok((named.authority().to_string(), stream, function, wait))
    }
}

impl Transport for SecsGemTransport {
    fn name(&self) -> &'static str {
        "secs-gem"
    }

    fn directions(&self) -> Directions {
        Directions::BOTH
    }

    fn arrivals(&self) -> transport::Arrivals {
        transport::Arrivals::Ordered("the connection is held by the message waiting for its reply")
    }

    fn waiting(&self) -> transport::Waiting {
        transport::Waiting::Waits("the accept waits for equipment, the read for its messages")
    }

    /// One equipment's data messages until it separates, or up to the first
    /// primary with the wait bit: from the connection the last receive
    /// stopped on, or one accepted on the listener the first receive bound
    /// and kept. A waiting primary is answered after the cycle — `<B 0>` on
    /// accepted, `<B 1>` on refused, `SxF0` on failed — and the receive
    /// stops at it, keeping
    /// the connection for the next. A message without the wait bit is
    /// at-most-once ([`AT_MOST_ONCE`]).
    fn receive_up_to(&self, _most: usize) -> Result<Vec<Arrived>> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let mut connection = match open.take() {
            Some(connection) => connection,
            None => self.accept_one(self.receiving.bound(|| self.bind())?)?,
        };
        let mut arrived = Vec::new();
        while let Some(message) = connection.next_data()? {
            let origin = connection.origin(&message.header);
            if message.header.wait {
                let acknowledgement = replied(Arc::clone(&self.open), message.header);
                arrived.push(
                    Arrived::whole(origin, message.body, acknowledgement)
                        .from_peer(connection.peer()),
                );
                *open = Some(connection);
                return Ok(arrived);
            }
            let acknowledgement = Acknowledgement::at_most_once(AT_MOST_ONCE);
            arrived.push(
                Arrived::whole(origin, message.body, acknowledgement).from_peer(connection.peer()),
            );
        }
        Ok(arrived)
    }

    fn sending(&self) -> transport::Sending {
        transport::Sending::Gathered("one HSMS message carries the whole body")
    }

    /// Connect, select, send one message, take the reply if one is due,
    /// separate. A reply under function 0 — the equipment aborted the
    /// transaction — fails the send as retryable; a reply that is a single
    /// non-zero acknowledge code — not accepted — fails it as permanent.
    fn send_from(&self, target: &str, outgoing: transport::Outgoing<'_>) -> Result<()> {
        let bytes: &[u8] = &outgoing.gather()?;
        let (address, stream, function, wait) = self.resolve(target)?;
        let mut connection = self.connect(&address)?;
        let reply = connection.send_data(stream, function, wait, bytes)?;
        connection.separate()?;
        match reply {
            Some(reply) if reply.header.function == 0 => Err(TransportError::retryable(format!(
                "the equipment aborted S{stream}F{function} with S{stream}F0"
            ))),
            Some(reply) if matches!(reply.body[..], [0x21, 0x01, code] if code != 0) => {
                Err(TransportError::permanent(format!(
                    "the equipment did not accept S{stream}F{function}: code {}",
                    reply.body[2]
                )))
            }
            _ => Ok(()),
        }
    }
}

/// The reply a waiting primary under `primary` gets once the cycle has
/// ended, on the connection the receive kept: [`ACCEPTED`] or
/// [`NOT_ACCEPTED`] under the next function up, or `SxF0`. The connection
/// goes when the reply cannot be written.
fn replied(open: Open, primary: Header) -> Acknowledgement {
    Acknowledgement::deferred(move |verdict| {
        let mut open = open.lock().unwrap_or_else(PoisonError::into_inner);
        let connection = open
            .as_mut()
            .ok_or_else(|| protocol_error("the equipment's connection is gone"))?;
        let primary_message = Message {
            header: primary,
            body: Vec::new(),
        };
        let written = match verdict {
            Verdict::Accepted => connection.reply(&primary_message, &ACCEPTED),
            Verdict::Refused(_) => connection.reply(&primary_message, &NOT_ACCEPTED),
            Verdict::Failed => connection.abort(primary),
        };
        if written.is_err() {
            open.take();
        }
        written
    })
}

impl Configured for SecsGemTransport {
    /// The address is where a Receive Location listens as the passive side,
    /// `0.0.0.0:5000`; a Send Location's target is each send's own.
    const SETTINGS: &'static Settings = &Settings {
        technology: env!("CARGO_PKG_NAME"),
        settings: &[
            Setting {
                name: "session",
                kind: Kind::Integer {
                    minimum: 0,
                    maximum: 65_535,
                },
                presence: Presence::Default(Fixed::Integer(connection::ANY_SESSION as i64)),
                meaning: "The session id, the device id, a Send Location selects under.",
                applies: Applies::Send,
            },
            Setting {
                name: "wait",
                kind: Kind::Boolean,
                presence: Presence::Optional,
                meaning: "Whether what is sent sets the wait bit and takes the reply, unless the \
                          target says `wait=0`; no reply is waited for when left out.",
                applies: Applies::Send,
            },
            Setting {
                name: "timeout",
                kind: Kind::Duration,
                presence: Presence::Optional,
                meaning: "How long a peer that stops mid-message or never replies is waited on; \
                          unbounded when left out.",
                applies: Applies::Both,
            },
        ],
    };

    fn configured(address: &str, settings: &Read) -> Result<Self> {
        let mut transport = Self::new(address);
        if let Some(session) = settings.optional_integer("session") {
            let session = u16::try_from(session)
                .map_err(|_| protocol_error("the session id is out of range"))?;
            transport = transport.with_session(session);
        }
        if settings.optional_boolean("wait") == Some(true) {
            transport = transport.awaiting_reply();
        }
        if let Some(timeout) = settings.optional_duration("timeout") {
            transport = transport.timing_out_after(timeout);
        }
        Ok(transport)
    }
}

impl SecsGemTransport {
    /// Both ends on this machine: an ephemeral local port for the passive
    /// side, the loopback timeout on every read. The near end sends the
    /// payload as the body of one S6F11 without waiting for a reply.
    #[must_use]
    pub fn loopback() -> Self {
        Self::new("127.0.0.1:0").timing_out_after(LOOPBACK_TIMEOUT)
    }
}

impl Accepting for SecsGemTransport {
    fn take_one(self, listener: &TcpListener) -> Result<Taken> {
        let mut connection = self.accept_one(listener)?;
        let message = connection
            .next_data()?
            .ok_or_else(|| protocol_error("the host separated without a message"))?;
        if message.header.wait {
            connection.reply(&message, &ACCEPTED)?;
        }
        // See the Separate.req that follows, so the goodbye is read.
        connection.next_data()?;
        let origin = connection.origin(&message.header);
        Ok(Taken::new(origin, message.body).from_peer(connection.peer()))
    }
}

impl Loopback for SecsGemTransport {
    fn arrival_identity(&self) -> ArrivalIdentity {
        ArrivalIdentity::PEER
    }

    fn far_end(&self) -> Result<Box<dyn FarEnd>> {
        Ok(Box::new(Listening::new(self.clone(), self.bind()?)))
    }

    fn send_to(&self, address: &str, payload: &[u8]) -> Result<()> {
        Self::new("127.0.0.1:0")
            .with_session(self.session_id)
            .timing_out_after(LOOPBACK_TIMEOUT)
            .send(address, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use transport::payload::edge_payloads;

    fn node() -> SecsGemTransport {
        SecsGemTransport::new("127.0.0.1:0").timing_out_after(Duration::from_secs(2))
    }

    #[test]
    fn every_receive_takes_from_the_listener_the_first_bound() {
        let receiver = SecsGemTransport::loopback();
        receiver.receiving.bound(|| receiver.bind()).expect("bound");
        let address = receiver.receiving.address().expect("address");
        transport::kept::held_across_receives(&receiver, address, 5, |at, payload| {
            SecsGemTransport::loopback().send_to(at, payload)
        });
    }

    #[test]
    fn secs_gem_declares_its_settings_and_reads_through_them() {
        use xcore::settings::Given;
        assert_eq!(SecsGemTransport::SETTINGS.problems(), Vec::<String>::new());
        let given = [
            ("session".to_string(), Given::Integer(7)),
            ("wait".to_string(), Given::Boolean(true)),
            ("timeout".to_string(), Given::Text("2s".to_string())),
        ];
        let built = SecsGemTransport::open("0.0.0.0:0", Applies::Send, &given).expect("sent");
        assert_eq!(built.session_id, 7);
        assert!(built.wait);
        assert_eq!(built.timeout, Some(Duration::from_secs(2)));
        let plain = SecsGemTransport::open("0.0.0.0:0", Applies::Send, &[]).expect("plain");
        assert_eq!(plain.session_id, connection::ANY_SESSION);
        assert!(!plain.wait);
        let Err(refused) = SecsGemTransport::open("0.0.0.0:0", Applies::Receive, &given) else {
            panic!("a Receive Location selects nothing");
        };
        assert!(refused.message.contains("\"session\""), "{refused}");
    }

    #[test]
    fn the_loopback_sends_one_event_report_and_takes_it() {
        let arrived = SecsGemTransport::loopback().round(b"S6F11").expect("round");
        assert_eq!(arrived.bytes, b"S6F11");
        assert!(arrived.origin_uri.starts_with("secs-gem://127.0.0.1:"));
        assert!(
            arrived
                .origin_uri
                .ends_with("?session=65535&stream=6&function=11")
        );
        let long: Vec<u8> = (0..=255u8).cycle().take(3000).collect();
        assert_eq!(
            SecsGemTransport::loopback()
                .round(&long)
                .expect("long")
                .bytes,
            long
        );
    }

    #[test]
    fn the_loopback_returns_the_edge_payloads_whole() {
        let transport = SecsGemTransport::loopback();
        assert!(transport.ceiling().is_none());
        for (name, bytes) in edge_payloads() {
            assert!(transport.refuses(&bytes).is_none(), "{name}");
            let arrived = transport
                .round(&bytes)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(arrived.bytes, bytes, "{name}");
        }
    }

    #[test]
    fn a_host_sends_to_an_equipment_and_takes_its_reply_when_waiting() {
        let equipment = node();
        let (listener, address) = equipment.bind().expect("binding");
        let body = item::encode(&Item::List(vec![
            Item::U4(vec![1]),
            Item::Ascii("A".into()),
        ]));
        let sent = body.clone();
        let host = std::thread::spawn(move || {
            node().with_session(3).send(&address, &sent)?;
            node().send(
                &format!("secs-gem://{address}?stream=1&function=13&wait=1"),
                b"",
            )
        });
        let mut connection = equipment.accept_one(&listener).expect("accepting");
        assert_eq!(connection.session_id(), 3);
        let event = connection.next_data().expect("S6F11").expect("one");
        assert_eq!(event.body, body);
        assert_eq!(
            item::decode(&event.body).expect("items"),
            Item::List(vec![Item::U4(vec![1]), Item::Ascii("A".into())])
        );
        assert!(
            connection
                .origin(&event.header)
                .ends_with("?session=3&stream=6&function=11")
        );
        assert!(connection.next_data().expect("separate").is_none());
        let mut connection = equipment.accept_one(&listener).expect("second");
        let establish = connection.next_data().expect("S1F13").expect("one");
        assert!(establish.header.wait);
        assert_eq!(
            (establish.header.stream, establish.header.function),
            (1, 13)
        );
        connection.reply(&establish, &[0x01, 0x00]).expect("S1F14");
        assert!(connection.next_data().expect("separate").is_none());
        host.join().expect("thread").expect("sending");
    }

    #[test]
    fn receive_takes_an_equipments_messages_until_it_separates() {
        let address = net::fixture::unheld().to_string();
        let passive = address.clone();
        let receiver = std::thread::spawn(move || {
            SecsGemTransport::new(passive)
                .timing_out_after(Duration::from_secs(2))
                .receive()
        });
        let mut connection = None;
        for _ in 0..100 {
            if let Ok(open) = node().with_session(7).connect(&address) {
                connection = Some(open);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut equipment = connection.expect("the receiver came up");
        equipment.send_data(6, 11, false, b"first").expect("first");
        equipment.linktest().expect("linktest");
        equipment.send_data(5, 1, false, b"alarm").expect("second");
        equipment.separate().expect("separate");
        let arrived = receiver.join().expect("thread").expect("receiving");
        assert_eq!(arrived.len(), 2);
        assert!(!arrived[0].defers(), "no wait bit: at-most-once");
        let arrived: Vec<Taken> = arrived
            .into_iter()
            .map(|one| one.taken().expect("taken"))
            .collect();
        assert_eq!(arrived[0].bytes, b"first");
        assert!(
            arrived[0]
                .origin_uri
                .ends_with("?session=7&stream=6&function=11")
        );
        assert!(
            arrived[1]
                .origin_uri
                .ends_with("?session=7&stream=5&function=1")
        );
    }

    #[test]
    fn a_waiting_primary_is_not_accepted_when_refused_aborted_when_failed_and_acknowledged() {
        let receiver = SecsGemTransport::loopback();
        receiver.receiving.bound(|| receiver.bind()).expect("bound");
        let address = receiver.receiving.address().expect("address");
        let target = format!("secs-gem://{address}?stream=6&function=11&wait=1");
        let host = std::thread::spawn(move || {
            [b"R1", b"C1", b"C1"].map(|body| node().send(&target, body))
        });
        // Refused: S6F12 <B 1>, not accepted, and the send fails for good.
        let mut arrived = receiver.receive().expect("the first");
        assert_eq!(arrived.len(), 1);
        assert!(arrived[0].defers(), "the wait bit waits for the verdict");
        arrived
            .remove(0)
            .refused(transport::Refusal::Unacceptable)
            .expect("not accepted");
        // The kept connection goes on: the host separated after its reply.
        assert!(receiver.receive().expect("separated").is_empty());
        // Failed: the equipment's host is answered S6F0 and sends again.
        let mut arrived = receiver.receive().expect("the second");
        arrived.remove(0).failed().expect("aborted");
        assert!(receiver.receive().expect("separated").is_empty());
        // Accepted: S6F12 <B 0>, and the send succeeds.
        let mut arrived = receiver.receive().expect("sent again");
        let taken = arrived.remove(0).taken().expect("accepted");
        assert_eq!(taken.bytes, b"C1");
        let [refused, failed, accepted] = host.join().expect("thread");
        let refused = refused.expect_err("not accepted");
        assert!(
            !refused.retryable && refused.message.contains("code 1"),
            "{refused}"
        );
        let failed = failed.expect_err("aborted");
        assert!(
            failed.retryable && failed.message.contains("S6F0"),
            "{failed}"
        );
        accepted.expect("acknowledged");
    }

    #[test]
    fn a_target_resolves_and_what_is_not_hsms_is_refused() {
        let (address, stream, function, wait) = node()
            .resolve("secs-gem://tool:5000?stream=2&function=41&wait=1")
            .expect("resolved");
        assert_eq!(
            (address.as_str(), stream, function, wait),
            ("tool:5000", 2, 41, true)
        );
        assert_eq!(
            node().resolve("tool:5000").expect("bare"),
            ("tool:5000".into(), 6, 11, false)
        );
        assert!(
            node()
                .awaiting_reply()
                .resolve("secs-gem://tool")
                .expect("w")
                .3
        );
        assert!(node().resolve("secs-gem://tool?stream=x").is_err());
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address").to_string();
        let far_end = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut select = [0u8; 14];
            let _ = std::io::Read::read(&mut stream, &mut select);
            std::io::Write::write_all(&mut stream, b"220 mail.example ESMTP\r\n").expect("w");
            std::thread::sleep(Duration::from_millis(200));
        });
        let error = node().send(&address, b"x").expect_err("refused");
        assert!(!error.retryable, "{error}");
        far_end.join().expect("thread");
        assert!(node().claims().is_none());
        assert_eq!(node().name(), "secs-gem");
    }
}
