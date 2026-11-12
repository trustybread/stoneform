//! A minimal protobuf wire-format reader and writer: enough to read and
//! write Battle.net's `product.db` records without a `.proto` definition.
//!
//! The reader borrows its bytes and keeps every field, so a record can be
//! changed field by field with everything else (unknown fields included)
//! passed through byte for byte.

use std::collections::BTreeMap;

use crate::{Error, Result};

/// A field's value, as the wire format has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    /// Length-delimited: a string, bytes, or an embedded message.
    Bytes(&'a [u8]),
    Fixed32(u32),
}

impl<'a> Value<'a> {
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match *self {
            Value::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// The bytes as UTF-8; `None` for anything else.
    pub fn as_str(&self) -> Option<&'a str> {
        std::str::from_utf8(self.as_bytes()?).ok()
    }

    /// The bytes read as an embedded message.
    pub fn as_message(&self) -> Option<Reader<'a>> {
        self.as_bytes().map(Reader::new)
    }

    pub fn as_varint(&self) -> Option<u64> {
        match *self {
            Value::Varint(value) => Some(value),
            _ => None,
        }
    }
}

/// Every field of a message by number; repeated fields keep every
/// occurrence, in order.
pub type Fields<'a> = BTreeMap<u32, Vec<Value<'a>>>;

/// Reads a message's fields front to back.
#[derive(Debug, Clone, Copy)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn is_at_end(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// The next field; `None` at the end of the message.
    pub fn next_field(&mut self) -> Result<Option<(u32, Value<'a>)>> {
        if self.is_at_end() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = u32::try_from(key >> 3).map_err(|_| Error::malformed("protobuf field number"))?;
        let value = match key & 7 {
            0 => Value::Varint(self.varint()?),
            1 => Value::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| truncated())?)),
            2 => {
                let length = self.varint()?;
                let length = usize::try_from(length).ok().filter(|&length| length <= self.data.len() - self.pos);
                Value::Bytes(self.take(length.ok_or_else(truncated)?)?)
            }
            5 => Value::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| truncated())?)),
            wire_type => return Err(Error::unsupported(format!("protobuf wire type {wire_type}"))),
        };
        Ok(Some((field, value)))
    }

    /// Every field of the message.
    pub fn fields(&self) -> Result<Fields<'a>> {
        let mut reader = *self;
        let mut fields = Fields::new();
        while let Some((field, value)) = reader.next_field()? {
            fields.entry(field).or_default().push(value);
        }
        Ok(fields)
    }

    /// The message with each length-delimited `field` transformed, every
    /// other byte (unknown fields too, in their order) kept as it was. A
    /// field the transform leaves unchanged keeps its original encoding.
    pub fn map_bytes(&self, field: u32, mut transform: impl FnMut(&[u8]) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
        let mut reader = *self;
        let mut out = Vec::with_capacity(self.data.len());
        loop {
            let start = reader.pos;
            let Some((number, value)) = reader.next_field()? else { break };
            let raw = &self.data[start..reader.pos];
            if number != field {
                out.extend_from_slice(raw);
                continue;
            }
            let bytes = value.as_bytes().ok_or_else(|| Error::malformed(format!("protobuf field {field}")))?;
            let updated = transform(bytes)?;
            if updated == bytes {
                out.extend_from_slice(raw);
            } else {
                let mut writer = Writer::new();
                writer.bytes(field, &updated);
                out.extend(writer.into_bytes());
            }
        }
        Ok(out)
    }

    /// The message with every `field` set to `bytes`, or with the field
    /// added at the end when it has none.
    pub fn replace_bytes(&self, field: u32, bytes: &[u8]) -> Result<Vec<u8>> {
        let mut found = false;
        let mut out = self.map_bytes(field, |_| {
            found = true;
            Ok(bytes.to_vec())
        })?;
        if !found {
            let mut writer = Writer::new();
            writer.bytes(field, bytes);
            out.extend(writer.into_bytes());
        }
        Ok(out)
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let bytes = self.data.get(self.pos..self.pos + count).ok_or_else(truncated)?;
        self.pos += count;
        Ok(bytes)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.data.get(self.pos).ok_or_else(truncated)?;
            self.pos += 1;
            // A u64 has one bit left for the tenth byte: more, or a
            // continuation, would silently lose bits.
            if shift == 63 && byte > 1 {
                return Err(Error::malformed("protobuf varint (overflow)"));
            }
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::malformed("protobuf varint (overflow)"))
    }
}

fn truncated() -> Error {
    Error::malformed("protobuf (unexpected end)")
}

/// Writes a message field by field.
#[derive(Debug, Clone, Default)]
pub struct Writer {
    data: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    pub fn varint(&mut self, field: u32, value: u64) {
        self.raw_varint(u64::from(field) << 3);
        self.raw_varint(value);
    }

    pub fn fixed64(&mut self, field: u32, value: u64) {
        self.raw_varint(u64::from(field) << 3 | 1);
        self.data.extend(value.to_le_bytes());
    }

    pub fn bytes(&mut self, field: u32, value: &[u8]) {
        self.raw_varint(u64::from(field) << 3 | 2);
        self.raw_varint(value.len() as u64);
        self.data.extend_from_slice(value);
    }

    pub fn string(&mut self, field: u32, value: &str) {
        self.bytes(field, value.as_bytes());
    }

    /// An embedded message, written by `build`.
    pub fn message(&mut self, field: u32, build: impl FnOnce(&mut Writer)) {
        let mut sub = Writer::new();
        build(&mut sub);
        self.bytes(field, &sub.data);
    }

    fn raw_varint(&mut self, mut value: u64) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                self.data.push(byte);
                return;
            }
            self.data.push(byte | 0x80);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_wire_type() {
        let mut w = Writer::new();
        w.varint(1, 300);
        w.fixed64(2, u64::MAX);
        w.string(3, "hé");
        w.message(4, |m| m.varint(1, 0));
        w.varint(99, u64::MAX);
        w.bytes(101, &[]);
        let data = w.into_bytes();
        assert_eq!(data[..3], [0x08, 0xAC, 0x02]);
        let fields = Reader::new(&data).fields().unwrap();
        assert_eq!(fields[&1], [Value::Varint(300)]);
        assert_eq!(fields[&2], [Value::Fixed64(u64::MAX)]);
        assert_eq!(fields[&3][0].as_str(), Some("hé"));
        assert_eq!(fields[&4][0].as_message().unwrap().fields().unwrap()[&1], [Value::Varint(0)]);
        assert_eq!(fields[&99][0].as_varint(), Some(u64::MAX));
        assert_eq!(fields[&101][0].as_bytes(), Some(&[][..]));
        let fixed32 = Reader::new(&[0x0D, 1, 0, 0, 0]).fields().unwrap();
        assert_eq!(fixed32[&1], [Value::Fixed32(1)]);
    }

    /// Ports of Waypoint's `rejectsInvalidProtobufLengths` and
    /// `rejectsOverflowingOrTruncatedProtobufVarints`.
    #[test]
    fn rejects_invalid_lengths_and_varints() {
        let cases: [&[u8]; 8] = [
            // A length of u64::MAX, and one of i64::MAX.
            &[0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
            &[0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f],
            &[0x0a, 0x02, 0x00],
            &[0x0a, 0x80],
            // The tenth byte can hold only bit 63, not bit 64 or a continuation.
            &[0x30, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02],
            &[0x30, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00],
            &[0x30, 0x80],
            &[0x0b],
        ];
        for data in cases {
            assert!(Reader::new(data).fields().is_err(), "{data:x?}");
        }
    }

    #[test]
    fn maps_one_field_and_keeps_every_other_byte() {
        let mut w = Writer::new();
        w.string(1, "keep");
        w.string(9, "Other");
        w.varint(90, 123);
        w.string(9, "Other");
        let data = w.into_bytes();
        let reader = Reader::new(&data);
        assert_eq!(reader.map_bytes(9, |bytes| Ok(bytes.to_vec())).unwrap(), data);
        let changed = reader.map_bytes(9, |bytes| Ok([bytes, b" HighRes"].concat())).unwrap();
        let fields = Reader::new(&changed).fields().unwrap();
        assert_eq!(fields[&9].iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>(), ["Other HighRes"; 2]);
        assert_eq!((fields[&1][0].as_str(), fields[&90][0].as_varint()), (Some("keep"), Some(123)));

        let added = reader.replace_bytes(13, b"_forever_").unwrap();
        assert!(added.starts_with(&data));
        assert_eq!(Reader::new(&added).fields().unwrap()[&13][0].as_str(), Some("_forever_"));
        assert!(reader.map_bytes(90, |bytes| Ok(bytes.to_vec())).is_err());
    }
}
