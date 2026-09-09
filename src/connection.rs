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
        let mut connection = Self::over(stream, peer, ANY_SESSION)?;
        match connection.read()? {
            Some(message) if message.header.stype == SType::SelectReq => {
                let header = message.header;
                connection.session_id = header.session_id;
                connection.write(&control(
                    header.session_id,
                    SType::SelectRsp,
                    0,
                    header.system,
                ))?;
            }
            _ => return Err(protocol_error("the peer did not open with Select.req")),
        }
        Ok(connection)
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
        self.write(&Message {
            header: primary.header.reply(),
            body: body.to_vec(),
        })
    }

    /// The next data message, or `None` when the peer separated, deselected
    /// or closed. Linktest requests are answered on the way.
    ///
    /// # Errors
    /// Where the connection broke, or the peer sent what is not HSMS.
    pub fn next_data(&mut self) -> Result<Option<Message>> {
        loop {
            let Some(message) = self.read()? else {
                return Ok(None);
            };
            let header = message.header;
            match header.stype {
                SType::Data => return Ok(Some(message)),
                SType::LinktestReq => {
                    self.write(&control(
                        header.session_id,
                        SType::LinktestRsp,
                        0,
                        header.system,
                    ))?;
                }
                SType::DeselectReq => {
                    self.write(&control(
                        header.session_id,
                        SType::DeselectRsp,
                        0,
                        header.system,
                    ))?;
                    return Ok(None);
                }
                SType::SeparateReq => return Ok(None),
                SType::SelectReq => {
                    self.write(&control(
                        header.session_id,
                        SType::SelectRsp,
                        1,
                        header.system,
                    ))?;
                }
                SType::LinktestRsp | SType::SelectRsp | SType::DeselectRsp | SType::RejectReq => {}
            }
        }
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
