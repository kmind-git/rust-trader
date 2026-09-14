//! FIX 4.2 tag=value framing: parse a message off a stream, and serialize one
//! back with correct BodyLength (9) and CheckSum (10) fields.

use std::io::{BufRead, Error, ErrorKind};

pub const SOH: u8 = 0x01;

/// one parsed FIX message
#[derive(Debug, Clone)]
pub struct FixMessage {
    pub begin_string: String,
    pub fields: Vec<(u32, String)>,
    /// the message as it appeared on the wire (8=..|9=..|body|10=..), for the
    /// FIX message log
    pub raw: String,
}

impl FixMessage {
    pub fn get(&self, tag: u32) -> Option<&str> {
        self.fields
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, v)| v.as_str())
    }

    pub fn msg_type(&self) -> Option<&str> {
        self.get(35)
    }

    pub fn seq(&self) -> Option<u64> {
        self.get(34).and_then(|v| v.parse().ok())
    }
}

/// Maximum supported wire frame; checked before allocating the body.
pub const MAX_MESSAGE_SIZE: usize = 1024 * 1024;
const MAX_PREFIX_SIZE: usize = 64;

/// A persistent decoder. A socket timeout never discards a partially received
/// frame. Reads stop at frame boundaries, so the convenience function below
/// can also safely be called repeatedly on a buffered stream.
pub struct FrameReader<R> {
    reader: R,
    pending: Vec<u8>,
    total: Option<usize>,
}

impl<R: std::io::Read> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            pending: Vec::new(),
            total: None,
        }
    }

    /// Exact bytes retained for diagnostics after an invalid/truncated frame.
    pub fn buffered_bytes(&self) -> &[u8] {
        &self.pending
    }

    pub fn read_message(&mut self) -> std::io::Result<Option<FixMessage>> {
        loop {
            if self.total.is_none() {
                let ends: Vec<usize> = self
                    .pending
                    .iter()
                    .enumerate()
                    .filter_map(|(i, b)| (*b == SOH).then_some(i))
                    .take(2)
                    .collect();
                if ends.len() == 2 {
                    let begin = &self.pending[..ends[0]];
                    if !begin.starts_with(b"8=") || begin.len() <= 2 {
                        return Err(invalid("expected 8=BeginString"));
                    }
                    let length = &self.pending[ends[0] + 1..ends[1]];
                    if !length.starts_with(b"9=")
                        || length.len() <= 2
                        || !length[2..].iter().all(u8::is_ascii_digit)
                    {
                        return Err(invalid("expected numeric 9=BodyLength"));
                    }
                    let body_len = std::str::from_utf8(&length[2..])
                        .unwrap()
                        .parse::<usize>()
                        .map_err(|_| invalid("bad BodyLength"))?;
                    let total = (ends[1] + 1)
                        .checked_add(body_len)
                        .and_then(|n| n.checked_add(7))
                        .filter(|n| *n <= MAX_MESSAGE_SIZE)
                        .ok_or_else(|| invalid("FIX message exceeds maximum size"))?;
                    if body_len < 5 {
                        return Err(invalid("BodyLength too small"));
                    }
                    self.total = Some(total);
                } else if self.pending.len() >= MAX_PREFIX_SIZE {
                    return Err(invalid("FIX length header exceeds maximum size"));
                }
            }
            if let Some(total) = self.total {
                if self.pending.len() == total {
                    let message = parse_wire(&self.pending)?;
                    self.pending.clear();
                    self.total = None;
                    return Ok(Some(message));
                }
            }
            let mut buf = [0u8; 8192];
            let want = self
                .total
                .map(|n| (n - self.pending.len()).min(buf.len()))
                .unwrap_or(1);
            match self.reader.read(&mut buf[..want]) {
                Ok(0) if self.pending.is_empty() => return Ok(None),
                Ok(0) => return Err(broken()),
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

pub fn read_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<FixMessage>> {
    FrameReader::new(reader).read_message()
}

fn parse_wire(wire: &[u8]) -> std::io::Result<FixMessage> {
    if !wire.iter().all(|b| *b == SOH || (0x20..=0x7e).contains(b)) {
        return Err(invalid("only ASCII FIX fields are supported"));
    }
    let raw = std::str::from_utf8(wire).map_err(|_| invalid("invalid FIX text"))?;
    let begin_end = wire.iter().position(|b| *b == SOH).ok_or_else(broken)?;
    let length_end = begin_end
        + 1
        + wire[begin_end + 1..]
            .iter()
            .position(|b| *b == SOH)
            .ok_or_else(broken)?;
    let checksum_start = wire.len() - 7;
    let trailer = &wire[checksum_start..];
    if &trailer[..3] != b"10=" || trailer[6] != SOH || !trailer[3..6].iter().all(u8::is_ascii_digit)
    {
        return Err(invalid("expected three-digit CheckSum after body"));
    }
    let actual = wire[..checksum_start]
        .iter()
        .fold(0u8, |sum, b| sum.wrapping_add(*b));
    let declared = std::str::from_utf8(&trailer[3..6])
        .unwrap()
        .parse::<u16>()
        .unwrap();
    if declared != u16::from(actual) {
        return Err(invalid("CheckSum mismatch"));
    }
    let body = &raw[length_end + 1..checksum_start];
    if !body.starts_with("35=") || !body.ends_with(SOH as char) {
        return Err(invalid(
            "MsgType must be third field and body must end with SOH",
        ));
    }
    let fields = parse_fields(body)?;
    if fields.iter().any(|(t, _)| matches!(t, 8 | 9 | 10)) {
        return Err(invalid("framing fields inside FIX body"));
    }
    Ok(FixMessage {
        begin_string: raw[2..begin_end].to_owned(),
        fields,
        raw: raw.to_owned(),
    })
}

fn broken() -> Error {
    Error::new(ErrorKind::UnexpectedEof, "truncated FIX message")
}
fn invalid(msg: &str) -> Error {
    Error::new(ErrorKind::InvalidData, msg)
}

fn parse_fields(body: &str) -> std::io::Result<Vec<(u32, String)>> {
    let mut fields = Vec::new();
    for part in body
        .strip_suffix(SOH as char)
        .unwrap_or(body)
        .split(SOH as char)
    {
        let (tag, value) = part.split_once('=').ok_or_else(|| invalid("bad field"))?;
        if tag.is_empty() || !tag.bytes().all(|b| b.is_ascii_digit()) || value.is_empty() {
            return Err(invalid("invalid tag or empty field value"));
        }
        let tag = tag.parse::<u32>().map_err(|_| invalid("bad tag"))?;
        if tag == 0 {
            return Err(invalid("invalid zero tag"));
        }
        fields.push((tag, value.to_string()));
    }
    Ok(fields)
}

/// parse an already received raw body into fields (used by tests)
pub fn parse_body(body: &str) -> Vec<(u32, String)> {
    parse_fields(body).unwrap_or_default()
}

/// SendingTime (52) format, e.g. "20260912-19:24:59.056"
pub fn utc_timestamp() -> String {
    chrono::Utc::now().format("%Y%m%d-%H:%M:%S%.3f").to_string()
}

/// serialize a message: 8=begin|9=len|35=type|34=seq|49=sender|56=target|52=ts|fields|10=chk|
pub fn frame(
    begin_string: &str,
    msg_type: &str,
    seq: u64,
    sender: &str,
    target: &str,
    fields: &[(u32, String)],
) -> Vec<u8> {
    let mut body = String::new();
    body.push_str(&format!("35={}\x01", msg_type));
    body.push_str(&format!("34={}\x01", seq));
    body.push_str(&format!("49={}\x01", sender));
    body.push_str(&format!("56={}\x01", target));
    body.push_str(&format!("52={}\x01", utc_timestamp()));
    for (tag, value) in fields {
        body.push_str(&format!("{}={}\x01", tag, value));
    }
    let mut message = format!("8={}\x019={}\x01", begin_string, body.len());
    message.push_str(&body);
    let checksum: u32 = message.bytes().map(|b| b as u32).sum::<u32>() % 256;
    message.push_str(&format!("10={:03}\x01", checksum));
    message.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_checksum_and_oversized_length_are_rejected() {
        let mut bytes = frame("FIX.4.2", "0", 1, "A", "B", &[]);
        let n = bytes.len();
        bytes[n - 2] = if bytes[n - 2] == b'0' { b'1' } else { b'0' };
        assert_eq!(
            read_message(&mut std::io::Cursor::new(bytes))
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidData
        );
        for input in [
            b"8=FIX.4.2\x019=99999999999999999999\x01".as_slice(),
            b"8=FIX.4.2\x019=-1\x01",
        ] {
            assert_eq!(
                read_message(&mut std::io::Cursor::new(input))
                    .unwrap_err()
                    .kind(),
                ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn decoder_retains_partial_message_across_timeouts() {
        struct InterruptedStream {
            data: std::io::Cursor<Vec<u8>>,
            timed_out: bool,
        }
        impl std::io::Read for InterruptedStream {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if self.data.position() >= 25 && !self.timed_out {
                    self.timed_out = true;
                    return Err(Error::new(ErrorKind::TimedOut, "simulated socket timeout"));
                }
                let len = out.len().min(3);
                std::io::Read::read(&mut self.data, &mut out[..len])
            }
        }
        let bytes = frame("FIX.4.2", "0", 1, "A", "B", &[]);
        let mut reader = FrameReader::new(InterruptedStream {
            data: std::io::Cursor::new(bytes.clone()),
            timed_out: false,
        });
        assert_eq!(
            reader.read_message().unwrap_err().kind(),
            ErrorKind::TimedOut
        );
        assert!(!reader.buffered_bytes().is_empty());
        assert_eq!(
            reader.read_message().unwrap().unwrap().raw.as_bytes(),
            bytes
        );
        assert!(reader.read_message().unwrap().is_none());
    }

    #[test]
    fn raw_preserves_length_field_spelling() {
        let bytes = frame("FIX.4.2", "0", 1, "A", "B", &[]);
        let text = String::from_utf8(bytes)
            .unwrap()
            .replacen("\x019=", "\x019=00", 1);
        let prefix = &text[..text.len() - 7];
        let sum = prefix.bytes().fold(0u8, u8::wrapping_add);
        let raw = format!("{prefix}10={sum:03}\x01");
        assert_eq!(
            read_message(&mut std::io::Cursor::new(raw.as_bytes()))
                .unwrap()
                .unwrap()
                .raw,
            raw
        );
    }

    #[test]
    fn test_frame_and_parse_roundtrip() {
        let fields = vec![(11u32, "1".to_string()), (55u32, "IBM".to_string())];
        let bytes = frame("FIX.4.2", "D", 1, "CLIENT", "GOX", &fields);
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with("8=FIX.4.2\x019="));
        assert!(text.ends_with(""));
        // verify checksum
        let without_checksum = text.rfind("10=").unwrap();
        let sum: u32 = text[..without_checksum].bytes().map(|b| b as u32).sum();
        assert_eq!(
            sum % 256,
            text[without_checksum + 3..without_checksum + 6]
                .parse::<u32>()
                .unwrap()
        );

        let mut reader = std::io::Cursor::new(bytes);
        let message = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(message.begin_string, "FIX.4.2");
        assert_eq!(message.msg_type(), Some("D"));
        assert_eq!(message.seq(), Some(1));
        assert_eq!(message.get(55), Some("IBM"));
    }

    #[test]
    fn test_body_length_and_checksum() {
        let bytes = frame("FIX.4.2", "0", 1, "A", "B", &[]);
        let text = String::from_utf8(bytes).unwrap();
        let checksum_start = text.rfind("10=").unwrap();
        // 9= must equal the number of bytes between it and the checksum field
        let len_start = text.find("9=").unwrap() + 2;
        let len_end = text[len_start..].find('\u{1}').unwrap() + len_start;
        let declared: usize = text[len_start..len_end].parse().unwrap();
        assert_eq!(declared, checksum_start - (len_end + 1));
        // checksum covers everything before 10=
        let sum: u32 = text[..checksum_start]
            .bytes()
            .map(|b| b as u32)
            .sum::<u32>()
            % 256;
        assert_eq!(
            text[checksum_start + 3..checksum_start + 6]
                .parse::<u32>()
                .unwrap(),
            sum
        );
        // required header: SendingTime present
        assert!(text.contains("52="));
    }

    #[test]
    fn test_read_multiple_messages() {
        let m1 = frame("FIX.4.2", "0", 1, "A", "B", &[]);
        let m2 = frame("FIX.4.2", "0", 2, "A", "B", &[]);
        let mut data = m1;
        data.extend_from_slice(&m2);
        let mut reader = std::io::Cursor::new(data);
        let first = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(first.seq(), Some(1));
        let second = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(second.seq(), Some(2));
        assert!(read_message(&mut reader).unwrap().is_none());
    }
}
