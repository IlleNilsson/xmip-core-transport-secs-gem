//! An active side's session, kept open at a Receive Location between its
//! messages.
//!
//! The select is the session's first turn (`transport::serving::Turn`), a
//! control message answered on the way — a linktest — one of its own, and
//! every data message after one turn handed up as a Stream. A primary with
//! the wait bit is answered once the receive cycle has ended, on the
//! connection it came on, and the connection is busy until then
//! (`transport::serving::Open::busy`): what the active side sends
//! meanwhile waits in the socket. Until 2026-10-21 a receive read one
//! connection's messages until it separated or a primary waited, and every
//! send selected a session of its own.

use std::net::{SocketAddr, TcpStream};

use transport::answer::{Answer, Busy};
use transport::error::Result;
use transport::serving::{Open, Turn};
use transport::{Acknowledgement, Arrived, Verdict};

use crate::connection::{Connection, Step, abort_of, reply_to};
use crate::{ACCEPTED, AT_MOST_ONCE, NOT_ACCEPTED, hsms};

/// One active side's session, selected on its first turn.
pub struct Peer {
    connection: Connection,
    selected: bool,
    /// A waiting primary taken and its reply not yet written.
    busy: Busy,
}

impl Peer {
    /// An active side's new connection, from `peer`, not yet selected.
    ///
    /// # Errors
    /// Where the connection could not be cloned for its reads.
    pub fn new(stream: TcpStream, peer: SocketAddr) -> Result<Self> {
        Ok(Self {
            connection: Connection::accepted(stream, peer)?,
            selected: false,
            busy: Busy::new(),
        })
    }

    /// One turn: the select, where the session has not had it, a control
    /// message answered, or the next data message. A primary with the wait
    /// bit is answered after the cycle — `<B 0>` on accepted, `<B 1>` on
    /// refused, `SxF0` on failed; one without it is at-most-once
    /// ([`AT_MOST_ONCE`]).
    ///
    /// # Errors
    /// Where the session did not open with Select.req, the connection
    /// broke, or the peer sent what is not HSMS.
    pub fn turn(&mut self) -> Result<Turn<Arrived>> {
        if !self.selected {
            self.selected = self.connection.grant_select()?;
            return Ok(if self.selected {
                Turn::Nothing
            } else {
                Turn::Closed
            });
        }
        let message = match self.connection.step()? {
            Step::Data(message) => message,
            Step::Answered => return Ok(Turn::Nothing),
            Step::Ended => return Ok(Turn::Closed),
        };
        let header = message.header;
        let acknowledgement = if header.wait {
            // Let go without a verdict, the connection is shut, and the
            // active side sends the primary again.
            let answer = Answer::held(self.connection.socket())?.busy(&self.busy);
            Acknowledgement::deferred(move |verdict| {
                let reply = match verdict {
                    Verdict::Accepted => reply_to(header, &ACCEPTED),
                    Verdict::Refused(_) => reply_to(header, &NOT_ACCEPTED),
                    Verdict::Failed => abort_of(header),
                };
                answer.with(|stream| hsms::write_message(stream, &reply))
            })
        } else {
            Acknowledgement::at_most_once(AT_MOST_ONCE)
        };
        let peer = self.connection.peer();
        let origin = self.connection.origin(&header);
        Ok(Turn::Taken(
            Arrived::whole(origin, message.body, acknowledgement).from_peer(peer),
        ))
    }
}

impl Open for Peer {
    fn socket(&self) -> &TcpStream {
        self.connection.socket()
    }

    /// A message the last read took beside its own waits in the buffer.
    fn waiting(&self) -> bool {
        self.connection.buffered()
    }

    fn busy(&self) -> Option<&Busy> {
        Some(&self.busy)
    }
}
