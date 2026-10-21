//! One HSMS session: selected on connect, data messages either way,
//! linktests answered while waiting, and separate to end it.
//!
//! HSMS has an active side that connects and selects, and a passive side
//! that listens and accepts the select. Which is the host and which the
//! equipment is a matter of configuration, not protocol — E37 lets either
//! be either — so one type is both, and it is the constructor that says
//! which side this one is.

use std::io::BufReader;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use transport::error::{Result, protocol_error};
use transport::socket;

use crate::hsms::{self, Header, Message, SType};

/// The session id a select is made under when the equipment has not said.
pub const ANY_SESSION: u16 = 0xFFFF;

/// A selected HSMS connection, either side.
pub struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    peer: SocketAddr,
    session_id: u16,
    next_system: u32,
}

impl Connection {
    /// Connect to `address` as the active side and select `session_id`.
    ///
    /// # Errors
    /// Where the peer could not be reached, did not answer Select.req, or
    /// answered with a status other than communication established.
    pub fn connect(address: &str, session_id: u16, timeout: Option<Duration>) -> Result<Self> {
        let stream = socket::connect_tcp(address, timeout)?;
        let peer = stream
            .peer_addr()
            .map_err(|e| transport::error::classify("reading the peer address", &e))?;
        let mut connection = Self::over(stream, peer, session_id)?;
        let system = connection.system();
        connection.write(&control(session_id, SType::SelectReq, 0, system))?;
        match connection.read()? {
            Some(message) if message.header.stype == SType::SelectRsp => {
                if message.header.function != 0 {
                    return Err(protocol_error(format!(
                        "select refused with status {}",
                        message.header.function
                    )));
                }
                connection.session_id = message.header.session_id;
            }
            _ => return Err(protocol_error("the peer did not answer Select.req")),
        }
        Ok(connection)
    }

    /// Accept one active side on `listener` and grant its select.
    ///
    /// # Errors
    /// Where the connection could not be accepted or did not open with
    /// Select.req.
    pub fn accept(listener: &TcpListener, timeout: Option<Duration>) -> Result<Self> {
        let (stream, peer) = socket::accept_tcp(listener, timeout)?;
        let mut connection = Self::accepted(stream, peer)?;
        if !connection.grant_select()? {
            return Err(protocol_error("the peer closed before Select.req"));
        }
        Ok(connection)
    }

    /// The passive side of a connection an active side opened from `peer`,
    /// its select not yet read ([`Connection::grant_select`]).
    ///
    /// # Errors
    /// Where the connection could not be cloned for its reads.
    pub fn accepted(stream: TcpStream, peer: SocketAddr) -> Result<Self> {
        Self::over(stream, peer, ANY_SESSION)
    }

    /// Read the active side's Select.req and grant it: `false` where it
    /// closed before saying anything — a poke, not a peer.
    ///
    /// # Errors
    /// Where the connection did not open with Select.req.
    pub fn grant_select(&mut self) -> Result<bool> {
        match self.read()? {
            Some(message) if message.header.stype == SType::SelectReq => {
                let header = message.header;
                self.session_id = header.session_id;
                self.write(&control(
                    header.session_id,
                    SType::SelectRsp,
                    0,
                    header.system,
                ))?;
                Ok(true)
            }
            None => Ok(false),
            Some(_) => Err(protocol_error("the peer did not open with Select.req")),
        }
    }

    /// The socket the connection is, for its readiness and for an answer
    /// written from elsewhere (`transport::answer::Answer`).
    #[must_use]
    pub const fn socket(&self) -> &TcpStream {
        &self.writer
    }

    /// Whether a message already read waits in this side's buffer.
    #[must_use]
    pub fn buffered(&self) -> bool {
        !self.reader.buffer().is_empty()
    }

    /// Whether the peer still holds the connection open and has sent
    /// nothing since it was last read: what an idle active side must be to
    /// carry the next message.
    #[must_use]
    pub fn idle(&self) -> bool {
        !self.buffered() && transport::pool::quiet(&self.writer)
    }

    fn over(stream: TcpStream, peer: SocketAddr, session_id: u16) -> Result<Self> {
        let (reader, writer) = socket::split(stream)?;
        Ok(Self {
            reader,
            writer,
            peer,
            session_id,
            next_system: 0,
        })
    }

    /// The far end's address.
    #[must_use]
    pub const fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// The session id selected.
    #[must_use]
    pub const fn session_id(&self) -> u16 {
        self.session_id
    }

    /// Where a data message on this connection came from, as an origin URI:
    /// `secs-gem://peer?session=1&stream=6&function=11`.
    #[must_use]
    pub fn origin(&self, header: &Header) -> String {
        format!(
            "secs-gem://{}?session={}&stream={}&function={}",
            self.peer, header.session_id, header.stream, header.function
        )
    }

    /// Send S`stream`F`function` with `body`; with `wait` set, take the reply
    /// to it, answering linktests on the way.
    ///
    /// # Errors
    /// Where the peer went away, or separated before replying.
    pub fn send_data(
        &mut self,
        stream: u8,
        function: u8,
        wait: bool,
        body: &[u8],
    ) -> Result<Option<Message>> {
        let system = self.system();
        self.write(&Message {
            header: Header::data(self.session_id, stream, function, wait, system),
            body: body.to_vec(),
        })?;
        if !wait {
            return Ok(None);
        }
        loop {
            match self.next_data()? {
                Some(message) if message.header.system == system => return Ok(Some(message)),
                Some(_) => {}
                None => return Err(protocol_error("the peer separated before replying")),
            }
        }
    }

    /// Answer `primary` with `body` under the next function up.
    ///
    /// # Errors
    /// Where the peer went away.
    pub fn reply(&mut self, primary: &Message, body: &[u8]) -> Result<()> {
        self.write(&reply_to(primary.header, body))
    }

    /// Abort the transaction `primary` opened ([`abort_of`]).
    ///
    /// # Errors
    /// Where the peer went away.
    pub fn abort(&mut self, primary: Header) -> Result<()> {
        self.write(&abort_of(primary))
    }

    /// The next data message, or `None` when the peer separated, deselected
    /// or closed. Linktest requests are answered on the way.
    ///
    /// # Errors
    /// Where the connection broke, or the peer sent what is not HSMS.
    pub fn next_data(&mut self) -> Result<Option<Message>> {
        loop {
            match self.step()? {
                Step::Data(message) => return Ok(Some(message)),
                Step::Answered => {}
                Step::Ended => return Ok(None),
            }
        }
    }

    /// The next message, one: a data message, a control message answered
    /// on the way, or the session's end — the peer separated, deselected
    /// or closed. What a kept connection reads one turn at a time, so a
    /// linktest on it never waits for the data after.
    ///
    /// # Errors
    /// Where the connection broke, or the peer sent what is not HSMS.
    pub fn step(&mut self) -> Result<Step> {
        let Some(message) = self.read()? else {
            return Ok(Step::Ended);
        };
        let header = message.header;
        let answer = |stype, status| control(header.session_id, stype, status, header.system);
        match header.stype {
            SType::Data => return Ok(Step::Data(message)),
            SType::LinktestReq => self.write(&answer(SType::LinktestRsp, 0))?,
            SType::DeselectReq => {
                self.write(&answer(SType::DeselectRsp, 0))?;
                return Ok(Step::Ended);
            }
            SType::SeparateReq => return Ok(Step::Ended),
            SType::SelectReq => self.write(&answer(SType::SelectRsp, 1))?,
            SType::LinktestRsp | SType::SelectRsp | SType::DeselectRsp | SType::RejectReq => {}
        }
        Ok(Step::Answered)
    }

    /// Ask whether the peer is still there, and wait for it to say so.
    ///
    /// # Errors
    /// Where the peer did not answer Linktest.req before the timeout.
    pub fn linktest(&mut self) -> Result<()> {
        let system = self.system();
        self.write(&control(self.session_id, SType::LinktestReq, 0, system))?;
        loop {
            match self.read()? {
                Some(message) if message.header.stype == SType::LinktestRsp => return Ok(()),
                Some(message) if message.header.stype == SType::LinktestReq => {
                    let header = message.header;
                    self.write(&control(
                        header.session_id,
                        SType::LinktestRsp,
                        0,
                        header.system,
                    ))?;
                }
                Some(_) => {}
                None => return Err(protocol_error("the peer closed during a linktest")),
            }
        }
    }

    /// End the session with Separate.req.
    ///
    /// # Errors
    /// Where the peer had already gone.
    pub fn separate(mut self) -> Result<()> {
        self.separating()
    }

    /// Say Separate.req, the connection left to whoever holds it: what a
    /// session kept between messages says when it is let go.
    ///
    /// # Errors
    /// Where the peer had already gone.
    pub fn separating(&mut self) -> Result<()> {
        let system = self.system();
        self.write(&control(self.session_id, SType::SeparateReq, 0, system))
    }

    fn system(&mut self) -> u32 {
        self.next_system = self.next_system.wrapping_add(1);
        self.next_system
    }

    fn write(&mut self, message: &Message) -> Result<()> {
        hsms::write_message(&mut self.writer, message)
    }

    fn read(&mut self) -> Result<Option<Message>> {
        hsms::read_message(&mut self.reader)
    }
}

/// What one message read came to ([`Connection::step`]).
#[derive(Debug)]
pub enum Step {
    /// A data message.
    Data(Message),
    /// A control message, answered where it asks for an answer.
    Answered,
    /// The peer separated, deselected or closed.
    Ended,
}

/// The reply to `primary` with `body`, under the next function up.
#[must_use]
pub fn reply_to(primary: Header, body: &[u8]) -> Message {
    Message {
        header: primary.reply(),
        body: body.to_vec(),
    }
}

/// The abort of the transaction `primary` opened: `SxF0`, header only, the
/// same system bytes — SEMI E5's answer from a receiver that will not
/// complete it.
#[must_use]
pub const fn abort_of(primary: Header) -> Message {
    Message {
        header: Header {
            wait: false,
            function: 0,
            ..primary
        },
        body: Vec::new(),
    }
}

fn control(session_id: u16, stype: SType, status: u8, system: u32) -> Message {
    Message {
        header: Header::control(session_id, stype, status, system),
        body: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selected_pair_exchanges_data_linktests_and_separates() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address").to_string();
        let active = std::thread::spawn(move || {
            let mut host = Connection::connect(&address, 1, Some(secs(2))).expect("select");
            assert_eq!(host.session_id(), 1);
            let reply = host
                .send_data(1, 1, true, &[])
                .expect("S1F1")
                .expect("S1F2");
            assert_eq!((reply.header.stream, reply.header.function), (1, 2));
            assert_eq!(reply.body, b"MDLN");
            host.linktest().expect("linktest");
            assert!(
                host.send_data(6, 11, false, b"event")
                    .expect("S6F11")
                    .is_none()
            );
            host.separate().expect("separate");
        });
        let mut equipment = Connection::accept(&listener, Some(secs(2))).expect("accept");
        assert_eq!(equipment.session_id(), 1);
        let primary = equipment.next_data().expect("S1F1").expect("one");
        assert!(primary.header.wait);
        assert_eq!(
            equipment.origin(&primary.header),
            format!(
                "secs-gem://{}?session=1&stream=1&function=1",
                equipment.peer()
            )
        );
        equipment.reply(&primary, b"MDLN").expect("S1F2");
        let event = equipment.next_data().expect("S6F11").expect("one");
        assert_eq!(event.body, b"event");
        assert!(equipment.next_data().expect("separate").is_none());
        active.join().expect("thread");
    }

    #[test]
    fn a_peer_that_does_not_select_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address").to_string();
        let caller = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect");
            let data = Message {
                header: Header::data(1, 1, 1, false, 1),
                body: Vec::new(),
            };
            hsms::write_message(&mut stream, &data).expect("data first");
            std::thread::sleep(Duration::from_millis(100));
        });
        let error = Connection::accept(&listener, Some(secs(2)))
            .err()
            .expect("refused");
        assert!(!error.retryable);
        caller.join().expect("thread");
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }
}
