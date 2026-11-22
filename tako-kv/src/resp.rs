//! The Redis serialization protocol: requests (arrays of bulk strings, or
//! inline commands) and replies in RESP2 or RESP3.

use bytes::Bytes;

/// Arguments in one array request, as Redis limits them.
const MAX_ARGUMENTS: i64 = 1024 * 1024;
/// Longest inline command or array header line.
const MAX_INLINE: usize = 64 * 1024;

/// A request that cannot be parsed. The connection replies with it and closes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ProtocolError(pub(crate) String);

impl ProtocolError {
    fn new(message: impl Into<String>) -> Self {
        Self(format!("ERR Protocol error: {}", message.into()))
    }
}

/// A complete request at the start of a buffer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
    /// Argument ranges within the first `consumed` bytes.
    Array {
        args: Vec<(usize, usize)>,
        consumed: usize,
    },
    /// An inline command, whose arguments are unescaped copies.
    Inline { args: Vec<Bytes>, consumed: usize },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Parsed {
    Frame(Frame),
    /// More bytes are needed; at least `needed` in total, when known.
    Incomplete {
        needed: usize,
    },
}

fn line(buf: &[u8], from: usize) -> Option<usize> {
    buf.get(from..)?
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|at| from + at)
}

fn integer(bytes: &[u8]) -> Option<i64> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// Parses the request at the start of `buf`. A request whose arguments add up
/// to more than `max_request` bytes is refused before it is buffered.
pub(crate) fn parse(buf: &[u8], max_request: usize) -> Result<Parsed, ProtocolError> {
    if buf.is_empty() {
        return Ok(Parsed::Incomplete { needed: 1 });
    }
    if buf[0] != b'*' {
        return parse_inline(buf);
    }
    let Some(end) = line(buf, 1) else {
        if buf.len() > 32 {
            return Err(ProtocolError::new("invalid multibulk length"));
        }
        return Ok(Parsed::Incomplete {
            needed: buf.len() + 1,
        });
    };
    let count = integer(&buf[1..end])
        .filter(|count| *count <= MAX_ARGUMENTS)
        .ok_or_else(|| ProtocolError::new("invalid multibulk length"))?;
    let mut at = end + 2;
    if count <= 0 {
        return Ok(Parsed::Frame(Frame::Array {
            args: Vec::new(),
            consumed: at,
        }));
    }
    let mut args = Vec::with_capacity(count.min(1024) as usize);
    let mut total = 0usize;
    for _ in 0..count {
        let Some(&first) = buf.get(at) else {
            return Ok(Parsed::Incomplete { needed: at + 1 });
        };
        if first != b'$' {
            return Err(ProtocolError::new(format!(
                "expected '$', got '{}'",
                char::from(first).escape_default()
            )));
        }
        let Some(end) = line(buf, at + 1) else {
            if buf.len() - at > 32 {
                return Err(ProtocolError::new("invalid bulk length"));
            }
            return Ok(Parsed::Incomplete {
                needed: buf.len() + 1,
            });
        };
        let len = integer(&buf[at + 1..end])
            .and_then(|len| usize::try_from(len).ok())
            .ok_or_else(|| ProtocolError::new("invalid bulk length"))?;
        total = total.saturating_add(len);
        if total > max_request {
            return Err(ProtocolError::new(format!(
                "request exceeds {} bytes",
                max_request
            )));
        }
        let start = end + 2;
        let stop = start + len;
        if buf.len() < stop + 2 {
            return Ok(Parsed::Incomplete { needed: stop + 2 });
        }
        if &buf[stop..stop + 2] != b"\r\n" {
            return Err(ProtocolError::new("bulk string is not terminated"));
        }
        args.push((start, stop));
        at = stop + 2;
    }
    Ok(Parsed::Frame(Frame::Array { args, consumed: at }))
}

fn parse_inline(buf: &[u8]) -> Result<Parsed, ProtocolError> {
    let Some(newline) = buf.iter().position(|b| *b == b'\n') else {
        if buf.len() > MAX_INLINE {
            return Err(ProtocolError::new("too big inline request"));
        }
        return Ok(Parsed::Incomplete {
            needed: buf.len() + 1,
        });
    };
    let mut text = &buf[..newline];
    if text.last() == Some(&b'\r') {
        text = &text[..text.len() - 1];
    }
    let args = split_inline(text)?;
    Ok(Parsed::Frame(Frame::Inline {
        args,
        consumed: newline + 1,
    }))
}

/// Splits an inline command like Redis does: on whitespace, with double
/// quotes (supporting escapes) and single quotes.
fn split_inline(text: &[u8]) -> Result<Vec<Bytes>, ProtocolError> {
    let unbalanced = || ProtocolError::new("unbalanced quotes in request");
    let mut args = Vec::new();
    let mut i = 0;
    while i < text.len() {
        while i < text.len() && text[i].is_ascii_whitespace() {
            i += 1;
        }
        if i == text.len() {
            break;
        }
        let mut arg = Vec::new();
        match text[i] {
            b'"' => {
                i += 1;
                loop {
                    let Some(&c) = text.get(i) else {
                        return Err(unbalanced());
                    };
                    match c {
                        b'\\'
                            if i + 3 < text.len()
                                && text[i + 1] == b'x'
                                && text[i + 2].is_ascii_hexdigit()
                                && text[i + 3].is_ascii_hexdigit() =>
                        {
                            let hex = std::str::from_utf8(&text[i + 2..i + 4]).unwrap();
                            arg.push(u8::from_str_radix(hex, 16).unwrap());
                            i += 4;
                        }
                        b'\\' if i + 1 < text.len() => {
                            arg.push(match text[i + 1] {
                                b'n' => b'\n',
                                b'r' => b'\r',
                                b't' => b'\t',
                                b'b' => 0x08,
                                b'a' => 0x07,
                                other => other,
                            });
                            i += 2;
                        }
                        b'"' => {
                            i += 1;
                            if text.get(i).is_some_and(|c| !c.is_ascii_whitespace()) {
                                return Err(unbalanced());
                            }
                            break;
                        }
                        _ => {
                            arg.push(c);
                            i += 1;
                        }
                    }
                }
            }
            b'\'' => {
                i += 1;
                loop {
                    let Some(&c) = text.get(i) else {
                        return Err(unbalanced());
                    };
                    if c == b'\\' && text.get(i + 1) == Some(&b'\'') {
                        arg.push(b'\'');
                        i += 2;
                    } else if c == b'\'' {
                        i += 1;
                        if text.get(i).is_some_and(|c| !c.is_ascii_whitespace()) {
                            return Err(unbalanced());
                        }
                        break;
                    } else {
                        arg.push(c);
                        i += 1;
                    }
                }
            }
            _ => {
                while i < text.len() && !text[i].is_ascii_whitespace() {
                    arg.push(text[i]);
                    i += 1;
                }
            }
        }
        args.push(Bytes::from(arg));
    }
    Ok(args)
}

/// A reply, encoded for the connection's protocol version.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Ok,
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Bytes),
    /// A missing value: `$-1` in RESP2.
    Nil,
    /// A missing array, such as an aborted transaction: `*-1` in RESP2.
    NilArray,
    Array(Vec<Reply>),
    /// A map in RESP3, a flat array of keys and values in RESP2.
    Map(Vec<(Reply, Reply)>),
}

impl Reply {
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    pub(crate) fn bulk(bytes: impl Into<Bytes>) -> Self {
        Self::Bulk(bytes.into())
    }

    pub(crate) fn bulk_str(text: &str) -> Self {
        Self::Bulk(Bytes::copy_from_slice(text.as_bytes()))
    }

    /// Approximately the bytes this reply takes to send.
    pub(crate) fn size(&self) -> usize {
        match self {
            Self::Ok | Self::Nil | Self::NilArray | Self::Int(_) => 16,
            Self::Simple(text) | Self::Error(text) => text.len() + 3,
            Self::Bulk(bytes) => bytes.len() + 16,
            Self::Array(items) => 16 + items.iter().map(Self::size).sum::<usize>(),
            Self::Map(items) => {
                16 + items
                    .iter()
                    .map(|(k, v)| k.size() + v.size())
                    .sum::<usize>()
            }
        }
    }

    pub fn encode(&self, proto: u8, out: &mut Vec<u8>) {
        match self {
            Self::Ok => out.extend_from_slice(b"+OK\r\n"),
            Self::Simple(text) => {
                out.push(b'+');
                push_line(out, text.as_bytes());
            }
            Self::Error(text) => {
                out.push(b'-');
                push_line(out, text.as_bytes());
            }
            Self::Int(value) => {
                out.push(b':');
                out.extend_from_slice(value.to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Self::Bulk(bytes) => {
                out.push(b'$');
                out.extend_from_slice(bytes.len().to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(bytes);
                out.extend_from_slice(b"\r\n");
            }
            Self::Nil if proto >= 3 => out.extend_from_slice(b"_\r\n"),
            Self::Nil => out.extend_from_slice(b"$-1\r\n"),
            Self::NilArray if proto >= 3 => out.extend_from_slice(b"_\r\n"),
            Self::NilArray => out.extend_from_slice(b"*-1\r\n"),
            Self::Array(items) => {
                out.push(b'*');
                out.extend_from_slice(items.len().to_string().as_bytes());
                out.extend_from_slice(b"\r\n");
                for item in items {
                    item.encode(proto, out);
                }
            }
            Self::Map(items) => {
                if proto >= 3 {
                    out.push(b'%');
                    out.extend_from_slice(items.len().to_string().as_bytes());
                } else {
                    out.push(b'*');
                    out.extend_from_slice((items.len() * 2).to_string().as_bytes());
                }
                out.extend_from_slice(b"\r\n");
                for (key, value) in items {
                    key.encode(proto, out);
                    value.encode(proto, out);
                }
            }
        }
    }
}

/// Simple strings and errors cannot contain line breaks.
fn push_line(out: &mut Vec<u8>, text: &[u8]) {
    out.extend(text.iter().map(|b| match b {
        b'\r' | b'\n' => b' ',
        b => *b,
    }));
    out.extend_from_slice(b"\r\n");
}

/// A reply read from another server, for forwarding a connection's
/// handshake. Only the first line is examined.
pub(crate) fn first_line(buf: &[u8]) -> Option<(&[u8], usize)> {
    let end = line(buf, 0)?;
    Some((&buf[..end], end + 2))
}

/// Encodes a request as an array of bulk strings.
pub fn encode_request<A: AsRef<[u8]>>(args: &[A], out: &mut Vec<u8>) {
    out.push(b'*');
    out.extend_from_slice(args.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    for arg in args {
        let arg = arg.as_ref();
        out.push(b'$');
        out.extend_from_slice(arg.len().to_string().as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(buf: &[u8]) -> Vec<Vec<u8>> {
        match parse(buf, 1 << 20).unwrap() {
            Parsed::Frame(Frame::Array { args, .. }) => {
                args.into_iter().map(|(s, e)| buf[s..e].to_vec()).collect()
            }
            Parsed::Frame(Frame::Inline { args, .. }) => {
                args.into_iter().map(|a| a.to_vec()).collect()
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_arrays_incrementally() {
        let full = b"*2\r\n$3\r\nGET\r\n$5\r\nhello\r\n";
        for cut in 0..full.len() {
            assert!(matches!(
                parse(&full[..cut], 1 << 20).unwrap(),
                Parsed::Incomplete { .. }
            ));
        }
        assert_eq!(args(full), vec![b"GET".to_vec(), b"hello".to_vec()]);
        // The size of a pending bulk string is known before it arrives.
        assert_eq!(
            parse(b"*1\r\n$100\r\n", 1 << 20).unwrap(),
            Parsed::Incomplete { needed: 112 }
        );
    }

    #[test]
    fn refuses_malformed_and_oversized_requests() {
        assert!(parse(b"*x\r\n", 1024).is_err());
        assert!(parse(b"*1\r\n:1\r\n", 1024).is_err());
        assert!(parse(b"*1\r\n$-5\r\n", 1024).is_err());
        assert!(parse(b"*1\r\n$3\r\nabcde", 1024).is_err());
        assert!(parse(b"*1\r\n$2000\r\n", 1024).is_err());
        assert!(parse(&[b'*'; 40], 1024).is_err());
        assert_eq!(
            parse(b"*0\r\n", 1024).unwrap(),
            Parsed::Frame(Frame::Array {
                args: vec![],
                consumed: 4
            })
        );
    }

    #[test]
    fn parses_inline_commands_with_quotes() {
        assert_eq!(
            args(b"SET  k \"a b\\x41\\n\" 'it\\'s'\r\n"),
            vec![
                b"SET".to_vec(),
                b"k".to_vec(),
                b"a bA\n".to_vec(),
                b"it's".to_vec()
            ]
        );
        assert!(parse(b"SET \"a\r\n", 1024).is_err());
        assert!(parse(b"SET \"a\"b\r\n", 1024).is_err());
        assert_eq!(args(b"\r\n"), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn encodes_both_protocols() {
        let reply = Reply::Array(vec![
            Reply::Nil,
            Reply::Map(vec![(Reply::bulk_str("a"), Reply::Int(1))]),
            Reply::NilArray,
        ]);
        let mut two = Vec::new();
        reply.encode(2, &mut two);
        assert_eq!(two, b"*3\r\n$-1\r\n*2\r\n$1\r\na\r\n:1\r\n*-1\r\n");
        let mut three = Vec::new();
        reply.encode(3, &mut three);
        assert_eq!(three, b"*3\r\n_\r\n%1\r\n$1\r\na\r\n:1\r\n_\r\n");
        let mut error = Vec::new();
        Reply::error("ERR a\r\nb").encode(2, &mut error);
        assert_eq!(error, b"-ERR a  b\r\n");
    }
}
