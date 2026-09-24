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

pub mod connection;
pub mod hsms;
pub mod item;

use std::net::TcpListener;
use std::time::Duration;

pub use connection::Connection;
pub use hsms::{Header, Message, SType};
pub use item::Item;
use transport::error::{Result, protocol_error};
use transport::listening::{Accepting, Listening};
use transport::loopback::{FarEnd, LOOPBACK_TIMEOUT, Loopback};
use transport::socket;
use transport::{Arrived, Directions, Transport};

#[derive(Clone)]
pub struct SecsGemTransport {
    bind: String,
    session_id: u16,
    wait: bool,
    timeout: Option<Duration>,
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
        }
    }

    /// The session id — the device id — a select is made under.
    #[must_use]
    pub const fn with_session(mut self, session_id: u16) -> Self {
        self.session_id = session_id;
        self
    }

    /// Set the wait bit on what is sent and take the reply, unless the
    /// target says `wait=0`.
    #[must_use]
    pub const fn awaiting_reply(mut self) -> Self {
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
        let Some((authority, _)) = socket::target("secs-gem", target) else {
            return Ok((target.to_string(), 6, 11, self.wait));
        };
        let (address, query) = authority.split_once('?').unwrap_or((authority, ""));
        let (mut stream, mut function, mut wait) = (6u8, 11u8, self.wait);
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let number: u8 = value
                .parse()
                .map_err(|_| protocol_error(format!("{key}={value:?} is not a number")))?;
            match key {
                "stream" => stream = number,
                "function" => function = number,
                "wait" => wait = number != 0,
                _ => {}
            }
        }
        Ok((address.to_string(), stream, function, wait))
    }
}

impl Transport for SecsGemTransport {
    fn name(&self) -> &'static str {
        "secs-gem"
    }

    fn directions(&self) -> Directions {
        Directions::BOTH
    }

    /// One equipment's data messages until it separates.
    fn receive(&self) -> Result<Vec<Arrived>> {
        let (listener, _) = self.bind()?;
        let mut connection = self.accept_one(&listener)?;
        let mut arrived = Vec::new();
        while let Some(message) = connection.next_data()? {
            arrived.push(Arrived::new(
                connection.origin(&message.header),
                message.body,
            ));
        }
        Ok(arrived)
    }

    /// Connect, select, send one message, take the reply if one is due,
    /// separate.
    fn send(&self, target: &str, bytes: &[u8]) -> Result<()> {
        let (address, stream, function, wait) = self.resolve(target)?;
        let mut connection = self.connect(&address)?;
        connection.send_data(stream, function, wait, bytes)?;
        connection.separate()
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
    fn take_one(self, listener: &TcpListener) -> Result<Arrived> {
        let mut connection = self.accept_one(listener)?;
        let message = connection
            .next_data()?
            .ok_or_else(|| protocol_error("the host separated without a message"))?;
        // See the Separate.req that follows, so the goodbye is read.
        connection.next_data()?;
        let origin = connection.origin(&message.header);
        Ok(Arrived::new(origin, message.body))
    }
}

impl Loopback for SecsGemTransport {
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
        let (listener, address) = node().bind().expect("a free port");
        drop(listener);
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
