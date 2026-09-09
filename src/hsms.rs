//! HSMS, SEMI E37: the message as TCP carries it — a four-byte length, a
//! ten-byte header, and the SECS-II body.
//!
//! The header: session id (2), stream with the wait bit on top (1), function
//! (1), presentation type (1, always 0 for SECS-II), session type (1), and
//! the system bytes (4) that pair a reply with its primary. Control messages
//! — select, deselect, linktest, reject, separate — carry no body, and their
//! bytes 2 and 3 hold a status instead of stream and function; the same two
//! fields carry it here.

use std::io::{Read, Write};

use transport::error::{Result, classify, protocol_error};

/// The header, always.
pub const HEADER_LEN: usize = 10;
/// The most a message may say it is before it is refused.
pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// Session type: what kind of message the header is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SType {
    /// A data message: stream, function and a SECS-II body.
    Data,
    SelectReq,
    SelectRsp,
    DeselectReq,
    DeselectRsp,
    LinktestReq,
    LinktestRsp,
    RejectReq,
    SeparateReq,
}

impl SType {
    /// From the byte on the wire.
    ///
    /// # Errors
    /// A session type E37 does not define.
    pub fn from_byte(byte: u8) -> Result<Self> {
        Ok(match byte {
            0 => Self::Data,
            1 => Self::SelectReq,
            2 => Self::SelectRsp,
            3 => Self::DeselectReq,
            4 => Self::DeselectRsp,
            5 => Self::LinktestReq,
            6 => Self::LinktestRsp,
            7 => Self::RejectReq,
            9 => Self::SeparateReq,
            other => return Err(protocol_error(format!("session type {other} is not HSMS"))),
        })
    }

    /// The byte on the wire.
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Data => 0,
            Self::SelectReq => 1,
            Self::SelectRsp => 2,
            Self::DeselectReq => 3,
            Self::DeselectRsp => 4,
            Self::LinktestReq => 5,
            Self::LinktestRsp => 6,
            Self::RejectReq => 7,
            Self::SeparateReq => 9,
        }
    }
}

/// The ten bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub session_id: u16,
    /// Stream on a data message; the high status byte on a control one.
    pub stream: u8,
    /// The wait bit: a reply is expected.
    pub wait: bool,
    /// Function on a data message; the status on a control one.
    pub function: u8,
    pub ptype: u8,
    pub stype: SType,
    pub system: u32,
}

impl Header {
    /// A data message header, S`stream`F`function`.
    #[must_use]
    pub const fn data(session_id: u16, stream: u8, function: u8, wait: bool, system: u32) -> Self {
        Self {
            session_id,
            stream: stream & 0x7F,
            wait,
            function,
            ptype: 0,
            stype: SType::Data,
            system,
        }
    }

    /// A control message header with `status` in byte 3.
    #[must_use]
    pub const fn control(session_id: u16, stype: SType, status: u8, system: u32) -> Self {
        Self {
            session_id,
            stream: 0,
            wait: false,
            function: status,
            ptype: 0,
            stype,
            system,
        }
    }

    /// The header for the reply to this primary: the next function up, the
    /// wait bit clear, the same system bytes.
    #[must_use]
    pub const fn reply(self) -> Self {
        Self {
            wait: false,
            function: self.function + 1,
            ..self
        }
    }
}

/// One message, header and body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub header: Header,
    pub body: Vec<u8>,
}

/// `message` as bytes on the wire, length prefix included.
#[must_use]
pub fn encode(message: &Message) -> Vec<u8> {
    let header = &message.header;
    let length = u32::try_from(HEADER_LEN + message.body.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(4 + HEADER_LEN + message.body.len());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&header.session_id.to_be_bytes());
    out.push(header.stream | if header.wait { 0x80 } else { 0 });
    out.push(header.function);
    out.push(header.ptype);
    out.push(header.stype.byte());
    out.extend_from_slice(&header.system.to_be_bytes());
    out.extend_from_slice(&message.body);
    out
}

/// Write one message.
///
/// # Errors
/// Where the peer went away.
pub fn write_message(writer: &mut impl Write, message: &Message) -> Result<()> {
    writer
        .write_all(&encode(message))
        .map_err(|e| classify("writing an HSMS message", &e))?;
    writer
        .flush()
        .map_err(|e| classify("flushing an HSMS message", &e))
}

/// Read one message, or `None` when the peer closed between messages.
///
/// # Errors
/// A length shorter than the header or over [`MAX_MESSAGE`], a session type
/// E37 does not define, or a connection that closes mid-message.
pub fn read_message(reader: &mut impl Read) -> Result<Option<Message>> {
    let mut length = [0u8; 4];
    let first = reader
        .read(&mut length[..1])
        .map_err(|e| classify("reading an HSMS length", &e))?;
    if first == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut length[1..])
        .map_err(|e| classify("reading an HSMS length", &e))?;
    let length = usize::try_from(u32::from_be_bytes(length)).unwrap_or(usize::MAX);
    if length < HEADER_LEN {
        return Err(protocol_error("an HSMS message shorter than its header"));
    }
    if length > MAX_MESSAGE {
        return Err(protocol_error("an HSMS message over what Xmip will read"));
    }
    let mut bytes = vec![0u8; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|e| classify("reading an HSMS message", &e))?;
    let header = Header {
        session_id: u16::from_be_bytes([bytes[0], bytes[1]]),
        stream: bytes[2] & 0x7F,
        wait: bytes[2] & 0x80 != 0,
        function: bytes[3],
        ptype: bytes[4],
        stype: SType::from_byte(bytes[5])?,
        system: u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]),
    };
    Ok(Some(Message {
        header,
        body: bytes[HEADER_LEN..].to_vec(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_data_message_and_a_control_message_round_trip() {
        let primary = Message {
            header: Header::data(1, 6, 11, true, 0x0102_0304),
            body: vec![0x01, 0x00],
        };
        let bytes = encode(&primary);
        assert_eq!(
            bytes,
            [0, 0, 0, 12, 0, 1, 0x86, 11, 0, 0, 1, 2, 3, 4, 0x01, 0x00]
        );
        let back = read_message(&mut bytes.as_slice())
            .expect("read")
            .expect("one");
        assert_eq!(back, primary);
        let reply = primary.header.reply();
        assert_eq!(
            (reply.function, reply.wait, reply.system),
            (12, false, 0x0102_0304)
        );
        let select = Message {
            header: Header::control(0xFFFF, SType::SelectReq, 0, 7),
            body: Vec::new(),
        };
        let mut wire = Vec::new();
        write_message(&mut wire, &select).expect("write");
        assert_eq!(&wire[..4], &[0, 0, 0, 10]);
        assert_eq!(wire[9], 1, "select.req");
        let back = read_message(&mut wire.as_slice())
            .expect("read")
            .expect("one");
        assert_eq!(back, select);
        assert!(read_message(&mut &b""[..]).expect("closed").is_none());
        for stype in [SType::Data, SType::LinktestRsp, SType::SeparateReq] {
            assert_eq!(SType::from_byte(stype.byte()).expect("byte"), stype);
        }
    }

    #[test]
    fn what_is_not_hsms_is_refused() {
        assert!(
            read_message(&mut &[0u8, 0, 0, 4, 1, 2, 3, 4][..]).is_err(),
            "short"
        );
        assert!(
            read_message(&mut &[1u8, 0, 0, 0][..]).is_err(),
            "16 MiB and more"
        );
        assert!(read_message(&mut &[0u8, 0, 0, 10, 0, 1, 0, 0, 0, 8, 0, 0, 0, 0][..]).is_err());
        assert!(
            read_message(&mut &[0u8, 0, 0, 10, 0, 1][..]).is_err(),
            "cut off"
        );
        assert!(SType::from_byte(8).is_err());
        assert!(!read_message(&mut &[0u8, 0][..]).expect_err("cut").retryable);
    }
}
