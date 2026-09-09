//! SECS-II items, SEMI E5: the self-describing values every data message
//! is built from. A format byte — six bits of format code, two bits saying
//! how many length bytes follow — then the length in bytes, then the value;
//! a list's length counts elements rather than bytes and its elements
//! follow, each an item.
//!
//! Numbers are big-endian; an item of a numeric format holds as many values
//! as its length divides into. A message body is one item, usually a list.

use transport::error::{Result, protocol_error};

/// One item, and by recursion the whole body.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    List(Vec<Item>),
    Ascii(String),
    Binary(Vec<u8>),
    Boolean(Vec<bool>),
    I1(Vec<i8>),
    I2(Vec<i16>),
    I4(Vec<i32>),
    I8(Vec<i64>),
    U1(Vec<u8>),
    U2(Vec<u16>),
    U4(Vec<u32>),
    U8(Vec<u64>),
    F4(Vec<f32>),
    F8(Vec<f64>),
}

const LIST: u8 = 0o00;
const BINARY: u8 = 0o10;
const BOOLEAN: u8 = 0o11;
const ASCII: u8 = 0o20;
const I8: u8 = 0o30;
const I1: u8 = 0o31;
const I2: u8 = 0o32;
const I4: u8 = 0o34;
const F8: u8 = 0o40;
const F4: u8 = 0o44;
const U8: u8 = 0o50;
const U1: u8 = 0o51;
const U2: u8 = 0o52;
const U4: u8 = 0o54;

/// How deep a list may nest before it is refused as hostile.
const MAX_DEPTH: usize = 64;

/// `item` as bytes on the wire.
#[must_use]
pub fn encode(item: &Item) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(item, &mut out);
    out
}

fn encode_into(item: &Item, out: &mut Vec<u8>) {
    match item {
        Item::List(items) => {
            head(LIST, items.len(), out);
            for element in items {
                encode_into(element, out);
            }
        }
        Item::Ascii(text) => value(ASCII, text.as_bytes(), out),
        Item::Binary(bytes) => value(BINARY, bytes, out),
        Item::U1(bytes) => value(U1, bytes, out),
        Item::Boolean(flags) => {
            let bytes: Vec<u8> = flags.iter().map(|flag| u8::from(*flag)).collect();
            value(BOOLEAN, &bytes, out);
        }
        Item::I1(v) => numbers(I1, v.iter().map(|n| n.to_be_bytes().to_vec()), out),
        Item::I2(v) => numbers(I2, v.iter().map(|n| n.to_be_bytes().to_vec()), out),
        Item::I4(v) => numbers(I4, v.iter().map(|n| n.to_be_bytes().to_vec()), out),
        Item::I8(v) => numbers(I8, v.iter().map(|n| n.to_be_bytes().to_vec()), out),
        Item::U2(v) => numbers(U2, v.iter().map(|n| n.to_be_bytes().to_vec()), out),
        Item::U4(v) => numbers(U4, v.iter().map(|n| n.to_be_bytes().to_vec()), out),
        Item::U8(v) => numbers(U8, v.iter().map(|n| n.to_be_bytes().to_vec()), out),
        Item::F4(v) => numbers(F4, v.iter().map(|n| n.to_be_bytes().to_vec()), out),
        Item::F8(v) => numbers(F8, v.iter().map(|n| n.to_be_bytes().to_vec()), out),
    }
}

fn numbers(format: u8, values: impl Iterator<Item = Vec<u8>>, out: &mut Vec<u8>) {
    let bytes: Vec<u8> = values.flatten().collect();
    value(format, &bytes, out);
}

fn value(format: u8, bytes: &[u8], out: &mut Vec<u8>) {
    head(format, bytes.len(), out);
    out.extend_from_slice(bytes);
}

/// The format byte and as few length bytes as `length` needs.
fn head(format: u8, length: usize, out: &mut Vec<u8>) {
    let length = u32::try_from(length).unwrap_or(0x00FF_FFFF) & 0x00FF_FFFF;
    let bytes = length.to_be_bytes();
    let count: u8 = if length > 0xFFFF {
        3
    } else if length > 0xFF {
        2
    } else {
        1
    };
    out.push((format << 2) | count);
    out.extend_from_slice(&bytes[4 - usize::from(count)..]);
}

/// The one item `bytes` holds, all of them.
///
/// # Errors
/// A format code E5 does not define, a length past the end, a numeric item
/// whose length is not a whole number of values, or bytes left over.
pub fn decode(bytes: &[u8]) -> Result<Item> {
    let mut at = 0;
    let item = decode_at(bytes, &mut at, 0)?;
    if at != bytes.len() {
        return Err(protocol_error("bytes after the item"));
    }
    Ok(item)
}

fn decode_at(bytes: &[u8], at: &mut usize, depth: usize) -> Result<Item> {
    if depth > MAX_DEPTH {
        return Err(protocol_error("a list nested past what is reasonable"));
    }
    let format_byte = *bytes
        .get(*at)
        .ok_or_else(|| protocol_error("an item cut off at its format byte"))?;
    let count = usize::from(format_byte & 0b11);
    if count == 0 {
        return Err(protocol_error("a format byte with no length bytes"));
    }
    let length_bytes = bytes
        .get(*at + 1..*at + 1 + count)
        .ok_or_else(|| protocol_error("an item cut off in its length"))?;
    let length = length_bytes
        .iter()
        .fold(0usize, |length, byte| (length << 8) | usize::from(*byte));
    *at += 1 + count;
    let format = format_byte >> 2;
    if format == LIST {
        let mut items = Vec::with_capacity(length.min(1024));
        for _ in 0..length {
            items.push(decode_at(bytes, at, depth + 1)?);
        }
        return Ok(Item::List(items));
    }
    let data = bytes
        .get(*at..*at + length)
        .ok_or_else(|| protocol_error("an item longer than what follows"))?;
    *at += length;
    let item = match format {
        ASCII => Item::Ascii(String::from_utf8_lossy(data).into_owned()),
        BINARY => Item::Binary(data.to_vec()),
        BOOLEAN => Item::Boolean(data.iter().map(|byte| *byte != 0).collect()),
        U1 => Item::U1(data.to_vec()),
        I1 => Item::I1(data.iter().map(|byte| i8::from_be_bytes([*byte])).collect()),
        I2 => Item::I2(split::<2>(data)?.map(i16::from_be_bytes).collect()),
        I4 => Item::I4(split::<4>(data)?.map(i32::from_be_bytes).collect()),
        I8 => Item::I8(split::<8>(data)?.map(i64::from_be_bytes).collect()),
        U2 => Item::U2(split::<2>(data)?.map(u16::from_be_bytes).collect()),
        U4 => Item::U4(split::<4>(data)?.map(u32::from_be_bytes).collect()),
        U8 => Item::U8(split::<8>(data)?.map(u64::from_be_bytes).collect()),
        F4 => Item::F4(split::<4>(data)?.map(f32::from_be_bytes).collect()),
        F8 => Item::F8(split::<8>(data)?.map(f64::from_be_bytes).collect()),
        other => {
            return Err(protocol_error(format!(
                "format code {other:#o} is not one E5 defines"
            )));
        }
    };
    Ok(item)
}

/// `data` as whole `N`-byte values.
fn split<const N: usize>(data: &[u8]) -> Result<impl Iterator<Item = [u8; N]> + '_> {
    if !data.len().is_multiple_of(N) {
        return Err(protocol_error(format!(
            "a numeric item of {} bytes where each value is {N}",
            data.len()
        )));
    }
    Ok(data.chunks_exact(N).map(|chunk| {
        let mut value = [0u8; N];
        value.copy_from_slice(chunk);
        value
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_format_round_trips() {
        let body = Item::List(vec![
            Item::U4(vec![1]),
            Item::List(vec![Item::List(vec![
                Item::Ascii("LOT-42".into()),
                Item::Binary(vec![0, 255]),
                Item::Boolean(vec![true, false]),
                Item::I1(vec![-1, 127]),
                Item::I2(vec![-2, 300]),
                Item::I4(vec![-70_000]),
                Item::I8(vec![i64::MIN]),
                Item::U1(vec![200]),
                Item::U2(vec![65_535]),
                Item::U4(vec![4_000_000_000]),
                Item::U8(vec![u64::MAX]),
                Item::F4(vec![1.5]),
                Item::F8(vec![-0.25]),
                Item::List(Vec::new()),
            ])]),
        ]);
        let bytes = encode(&body);
        assert_eq!(&bytes[..2], &[1, 2], "a list of two");
        assert_eq!(&bytes[2..8], &[0o54 << 2 | 1, 4, 0, 0, 0, 1], "U4 1");
        assert_eq!(decode(&bytes).expect("decode"), body);
        let long = Item::Binary(vec![7u8; 70_000]);
        let bytes = encode(&long);
        assert_eq!(&bytes[..4], &[0o10 << 2 | 3, 0x01, 0x11, 0x70]);
        assert_eq!(decode(&bytes).expect("three length bytes"), long);
        let mid = encode(&Item::Ascii("x".repeat(300)));
        assert_eq!(&mid[..3], &[0o20 << 2 | 2, 0x01, 0x2C]);
    }

    #[test]
    fn what_is_not_secs_ii_is_refused() {
        assert!(decode(&[]).is_err(), "empty");
        assert!(decode(&[0o20 << 2]).is_err(), "no length bytes");
        assert!(decode(&[0o20 << 2 | 2, 1]).is_err(), "cut in length");
        assert!(decode(&[0o20 << 2 | 1, 5, b'a']).is_err(), "short");
        assert!(
            decode(&[0o52 << 2 | 1, 3, 0, 0, 0]).is_err(),
            "U2 of 3 bytes"
        );
        assert!(decode(&[0o77 << 2 | 1, 0]).is_err(), "no such format");
        assert!(decode(&[1, 1]).is_err(), "a list of one, empty");
        assert!(decode(&[0o51 << 2 | 1, 1, 9, 9]).is_err(), "left over");
        let mut deep = [1, 1].repeat(70);
        deep.extend_from_slice(&[1, 0]);
        assert!(!decode(&deep).expect_err("too deep").retryable);
    }
}
