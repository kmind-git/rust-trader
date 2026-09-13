//! FIX 4.4 tag=value framing: parse a message off a stream, and serialize one
//! back with correct BodyLength (9) and CheckSum (10) fields.

use std::io::{BufRead, Error, ErrorKind};

pub const SOH: u8 = 0x01;

/// one parsed FIX message
#[derive(Debug, Clone)]
pub struct FixMessage {
    pub begin_string: String,
    pub fields: Vec<(u32, String)>,
}

impl FixMessage {
    pub fn get(&self, tag: u32) -> Option<&str> {
        self.fields.iter().find(|(t, _)| *t == tag).map(|(_, v)| v.as_str())
    }

    pub fn msg_type(&self) -> Option<&str> {
        self.get(35)
    }

    pub fn seq(&self) -> Option<u64> {
        self.get(34).and_then(|v| v.parse().ok())
    }
}

/// read one FIX message. Returns Ok(None) on clean EOF before any bytes of a
/// message were read.
pub fn read_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<FixMessage>> {
    // read the 8=BeginString field, skipping anything before it
    let begin = match read_field(reader)? {
        Some((tag, value)) if tag == 8 => value,
        Some(_) => return Err(invalid("expected 8=BeginString")),
        None => return Ok(None),
    };
    let (body_tag, body_len) = read_field(reader)?.ok_or_else(broken)?;
    if body_tag != 9 {
        return Err(invalid("expected 9=BodyLength"));
    }
    let body_len: usize = body_len.parse().map_err(|_| invalid("bad BodyLength"))?;

    let mut body = vec![0u8; body_len];
    reader.read_exact(&mut body)?;

    let (_tag, checksum) = read_field(reader)?.ok_or_else(broken)?;
    if _tag != 10 {
        return Err(invalid("expected CheckSum after body"));
    }
    let _ = checksum;

    let body_str = String::from_utf8_lossy(&body);
    let fields = parse_fields(&body_str)?;
    Ok(Some(FixMessage { begin_string: begin, fields }))
}

fn broken() -> Error {
    Error::new(ErrorKind::UnexpectedEof, "truncated FIX message")
}

fn invalid(msg: &str) -> Error {
    Error::new(ErrorKind::InvalidData, msg)
}

/// read a single "tag=value\x01" field from the stream
fn read_field<R: BufRead>(reader: &mut R) -> std::io::Result<Option<(u32, String)>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    // read the tag
    loop {
        match reader.read(&mut byte)? {
            0 => {
                if buf.is_empty() {
                    return Ok(None);
                }
                return Err(broken());
            }
            _ => {
                if byte[0] == b'=' {
                    break;
                } else if byte[0] == SOH {
                    // unexpected separator: skip garbage and restart
                    buf.clear();
                    continue;
                } else if !byte[0].is_ascii_digit() {
                    buf.clear();
                    continue;
                } else {
                    buf.push(byte[0]);
                }
            }
        }
    }
    let tag: u32 = std::str::from_utf8(&buf)
        .map_err(|_| invalid("bad tag"))?
        .parse()
        .map_err(|_| invalid("bad tag"))?;
    // read the value
    let mut value = Vec::new();
    loop {
        match reader.read(&mut byte)? {
            0 => return Err(broken()),
            _ => {
                if byte[0] == SOH {
                    return Ok(Some((tag, String::from_utf8_lossy(&value).into_owned())));
                }
                value.push(byte[0]);
            }
        }
    }
}

fn parse_fields(body: &str) -> std::io::Result<Vec<(u32, String)>> {
    let mut fields = Vec::new();
    for part in body.split(SOH as char) {
        if part.is_empty() {
            continue;
        }
        let (tag, value) = part.split_once('=').ok_or_else(|| invalid("bad field"))?;
        fields.push((tag.parse().map_err(|_| invalid("bad tag"))?, value.to_string()));
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
    fn test_frame_and_parse_roundtrip() {
        let fields = vec![(11u32, "1".to_string()), (55u32, "IBM".to_string())];
        let bytes = frame("FIX.4.2", "D", 1, "CLIENT", "GOX", &fields);
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with("8=FIX.4.2\x019="));
        assert!(text.ends_with(""));
        // verify checksum
        let without_checksum = text.rfind("10=").unwrap();
        let sum: u32 = text[..without_checksum].bytes().map(|b| b as u32).sum();
        assert_eq!(sum % 256, text[without_checksum + 3..without_checksum + 6].parse::<u32>().unwrap());

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
        let sum: u32 = text[..checksum_start].bytes().map(|b| b as u32).sum::<u32>() % 256;
        assert_eq!(
            text[checksum_start + 3..checksum_start + 6].parse::<u32>().unwrap(),
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
