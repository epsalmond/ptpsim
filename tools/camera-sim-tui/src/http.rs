//! Local control HTTP request parsing and bounded socket I/O.
//!
//! Requests permit 16 KiB of headers and 256 KiB in total. Content-Length
//! accepts decimal digits only. Duplicate lengths and Transfer-Encoding are
//! rejected. Reads and writes each use a five-second absolute deadline.
//! Bodies are bytes and need not be UTF-8.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub const MAX_REQUEST_BYTES: usize = 256 * 1024;
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RequestError {
    TooLarge,
    Invalid(&'static str),
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str("request too large"),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RequestError {}

#[derive(Debug)]
pub enum ReadError {
    Parse(RequestError),
    Deadline,
    Io(io::Error),
    Closed,
}

impl ReadError {
    pub fn status(&self) -> &'static str {
        match self {
            Self::Parse(RequestError::TooLarge) => "413 Payload Too Large",
            Self::Deadline => "408 Request Timeout",
            _ => "400 Bad Request",
        }
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => error.fmt(formatter),
            Self::Deadline => formatter.write_str("request deadline exceeded"),
            Self::Io(error) => error.fmt(formatter),
            Self::Closed => formatter.write_str("client closed before full request"),
        }
    }
}

impl std::error::Error for ReadError {}

pub fn read_request(stream: &mut TcpStream) -> Result<Request, ReadError> {
    read_request_using(
        |chunk, remaining| {
            stream.set_read_timeout(Some(remaining))?;
            stream.read(chunk)
        },
        Instant::now,
        REQUEST_TIMEOUT,
    )
}

fn read_request_using(
    mut read: impl FnMut(&mut [u8], Duration) -> io::Result<usize>,
    mut now: impl FnMut() -> Instant,
    timeout: Duration,
) -> Result<Request, ReadError> {
    let deadline = now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let remaining = deadline.saturating_duration_since(now());
        if remaining.is_zero() {
            return Err(ReadError::Deadline);
        }
        let available = chunk.len().min(MAX_REQUEST_BYTES - buf.len());
        let n = match read(&mut chunk[..available], remaining) {
            Ok(n) => n,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(ReadError::Deadline);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ReadError::Io(error)),
        };
        if now() >= deadline {
            return Err(ReadError::Deadline);
        }
        if n == 0 {
            return Err(ReadError::Closed);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(req) = parse_request_if_complete(&buf).map_err(ReadError::Parse)? {
            return Ok(req);
        }
    }
}

pub fn write_json(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let deadline = Instant::now() + RESPONSE_TIMEOUT;
    write_bytes_before(
        response.as_bytes(),
        |remaining, duration| {
            stream.set_write_timeout(Some(duration))?;
            stream.write(remaining)
        },
        Instant::now,
        deadline,
    )?;
    stream.set_write_timeout(Some(write_time_remaining_at(deadline, Instant::now())?))?;
    stream.flush()
}

fn write_bytes_before(
    mut remaining: &[u8],
    mut write: impl FnMut(&[u8], Duration) -> io::Result<usize>,
    mut now: impl FnMut() -> Instant,
    deadline: Instant,
) -> io::Result<()> {
    while !remaining.is_empty() {
        let duration = write_time_remaining_at(deadline, now())?;
        match write(remaining, duration) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => remaining = &remaining[n..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    write_time_remaining_at(deadline, now())?;
    Ok(())
}

fn write_time_remaining_at(deadline: Instant, now: Instant) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(now);
    if remaining.is_zero() {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(remaining)
    }
}

/// Parse one bounded request. Incomplete bodies are left unallocated.
/// Duplicate Content-Length fields are invalid even when their values agree.
pub fn parse_request_if_complete(buf: &[u8]) -> Result<Option<Request>, RequestError> {
    if buf.len() > MAX_REQUEST_BYTES {
        return Err(RequestError::TooLarge);
    }
    let Some(split) = buf.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
        return if buf.len() >= MAX_HEADER_BYTES {
            Err(RequestError::TooLarge)
        } else {
            Ok(None)
        };
    };
    let body_start = split.checked_add(4).ok_or(RequestError::TooLarge)?;
    if body_start > MAX_HEADER_BYTES {
        return Err(RequestError::TooLarge);
    }
    let headers = std::str::from_utf8(&buf[..split])
        .map_err(|_| RequestError::Invalid("request headers are not UTF-8"))?;
    let mut lines = headers.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split(' ');
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if !is_token(method)
        || path.is_empty()
        || !path
            .bytes()
            .all(|byte| byte.is_ascii() && !byte.is_ascii_control())
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || parts.next().is_some()
    {
        return Err(RequestError::Invalid("malformed request line"));
    }
    let mut content_length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(RequestError::Invalid("malformed header"))?;
        if !is_token(name)
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(RequestError::Invalid("malformed header"));
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(RequestError::Invalid("Transfer-Encoding is unsupported"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(RequestError::Invalid("duplicate Content-Length"));
            }
            let value = value.trim_matches([' ', '\t']);
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(RequestError::Invalid("invalid Content-Length"));
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| RequestError::Invalid("invalid Content-Length"))?,
            );
        }
    }
    let body_end = body_start
        .checked_add(content_length.unwrap_or(0))
        .ok_or(RequestError::TooLarge)?;
    if body_end > MAX_REQUEST_BYTES {
        return Err(RequestError::TooLarge);
    }
    if buf.len() < body_end {
        return Ok(None);
    }
    Ok(Some(Request {
        method: method.to_string(),
        path: path.to_string(),
        body: buf[body_start..body_end].to_vec(),
    }))
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_rejects_overflowing_declared_length() {
        let request = format!(
            "POST /state HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            usize::MAX
        );
        assert!(parse_request_if_complete(request.as_bytes()).is_err());
    }

    #[test]
    fn http_rejects_duplicate_lengths_and_transfer_encoding() {
        for headers in [
            "Content-Length: 1\r\nContent-Length: 1",
            "Content-Length: 1\r\ncontent-length: 2",
            "Transfer-Encoding: chunked",
            "Content-Length: 1\r\nTransfer-Encoding: identity",
        ] {
            let request = format!("POST /state HTTP/1.1\r\n{headers}\r\n\r\n");
            assert!(
                parse_request_if_complete(request.as_bytes()).is_err(),
                "{headers}"
            );
        }
    }

    #[test]
    fn http_requires_decimal_content_lengths() {
        for length in [
            "",
            "+1",
            "-1",
            "1.0",
            "0x10",
            "1, 1",
            "1 1",
            "184467440737095516150",
        ] {
            let request = format!("POST /state HTTP/1.1\r\nContent-Length: {length}\r\n\r\n");
            assert!(
                parse_request_if_complete(request.as_bytes()).is_err(),
                "{length}"
            );
        }
        let request = b"POST /state HTTP/1.1\r\ncOnTeNt-LeNgTh: \t003 \t\r\n\r\nabc";
        assert_eq!(
            parse_request_if_complete(request).unwrap().unwrap().body,
            b"abc"
        );
    }

    #[test]
    fn http_rejects_malformed_request_lines_and_headers() {
        for header in [
            "GET /state",
            "GET  /state HTTP/1.1",
            "GET /state HTTP/2.0",
            "GET /state HTTP/1.1 extra",
            "GET /state HTTP/1.1\r\nContent-Length 2",
            "GET /state HTTP/1.1\r\nContent-Length : 2",
            "GET /state HTTP/1.1\r\nX: a\nb",
        ] {
            let request = format!("{header}\r\n\r\n");
            assert!(
                parse_request_if_complete(request.as_bytes()).is_err(),
                "{header}"
            );
        }
        for version in ["HTTP/1.0", "HTTP/1.1"] {
            let request = format!("GET /healthz {version}\r\n\r\n");
            let request = parse_request_if_complete(request.as_bytes())
                .unwrap()
                .unwrap();
            assert_eq!(request.method, "GET");
            assert_eq!(request.path, "/healthz");
            assert!(request.body.is_empty());
        }
    }

    fn request_at_size(total: usize) -> Vec<u8> {
        let mut length = total;
        loop {
            let header = format!("POST /state HTTP/1.1\r\nContent-Length: {length}\r\n\r\n");
            let adjusted = total - header.len();
            if adjusted == length {
                let mut request = header.into_bytes();
                request.resize(total, b'a');
                return request;
            }
            length = adjusted;
        }
    }

    #[test]
    fn http_enforces_announced_total_before_body_arrives() {
        let request = request_at_size(MAX_REQUEST_BYTES);
        let body_start = request
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        assert!(parse_request_if_complete(&request[..body_start])
            .unwrap()
            .is_none());
        let parsed = parse_request_if_complete(&request).unwrap().unwrap();
        assert_eq!(parsed.body.len(), MAX_REQUEST_BYTES - body_start);
        let oversized = request_at_size(MAX_REQUEST_BYTES + 1);
        let body_start = oversized
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        assert_eq!(
            parse_request_if_complete(&oversized[..body_start]),
            Err(RequestError::TooLarge)
        );
        assert_eq!(
            parse_request_if_complete(&oversized),
            Err(RequestError::TooLarge)
        );
    }

    #[test]
    fn http_bounds_headers_including_terminator() {
        let mut header = b"GET /state HTTP/1.1\r\nX-Pad: ".to_vec();
        header.resize(MAX_HEADER_BYTES - 4, b'a');
        header.extend_from_slice(b"\r\n\r\n");
        assert!(parse_request_if_complete(&header).unwrap().is_some());
        header.insert(MAX_HEADER_BYTES - 4, b'a');
        assert_eq!(
            parse_request_if_complete(&header),
            Err(RequestError::TooLarge)
        );
        assert_eq!(
            parse_request_if_complete(&vec![b'a'; MAX_HEADER_BYTES]),
            Err(RequestError::TooLarge)
        );
        assert!(parse_request_if_complete(&vec![b'a'; MAX_HEADER_BYTES - 1])
            .unwrap()
            .is_none());
    }

    #[test]
    fn http_accepts_every_fragment_of_headers_and_binary_body() {
        let mut request = b"POST /state HTTP/1.1\r\nContent-Length: 7\r\n\r\n".to_vec();
        let body = [0xe2, 0x82, 0xac, 0xff, 0x00, 0x0d, 0x0a];
        request.extend_from_slice(&body);
        for split in 0..request.len() {
            assert!(
                parse_request_if_complete(&request[..split])
                    .unwrap()
                    .is_none(),
                "split {split}"
            );
        }
        assert_eq!(
            parse_request_if_complete(&request).unwrap().unwrap().body,
            body
        );
    }

    #[test]
    fn http_read_deadline_expires_despite_byte_drips() {
        let clock = std::cell::Cell::new(Instant::now());
        let mut remaining_budgets = Vec::new();
        let result = read_request_using(
            |chunk, remaining| {
                remaining_budgets.push(remaining);
                clock.set(clock.get() + Duration::from_secs(1));
                chunk[0] = b'G';
                Ok(1)
            },
            || clock.get(),
            REQUEST_TIMEOUT,
        );
        assert!(matches!(result, Err(ReadError::Deadline)));
        assert_eq!(
            remaining_budgets,
            (1..=5).rev().map(Duration::from_secs).collect::<Vec<_>>()
        );
    }

    #[test]
    fn http_read_deadline_rejects_late_complete_body() {
        let clock = std::cell::Cell::new(Instant::now());
        let mut fragments = [
            b"POST /state HTTP/1.1\r\nContent-Length: 2\r\n\r\na".as_slice(),
            b"b".as_slice(),
        ]
        .into_iter();
        let result = read_request_using(
            |chunk, _| {
                clock.set(clock.get() + Duration::from_secs(3));
                let fragment = fragments.next().unwrap();
                chunk[..fragment.len()].copy_from_slice(fragment);
                Ok(fragment.len())
            },
            || clock.get(),
            REQUEST_TIMEOUT,
        );
        assert!(matches!(result, Err(ReadError::Deadline)));
    }

    #[test]
    fn http_read_accepts_fragmented_binary_body() {
        let clock = std::cell::Cell::new(Instant::now());
        let mut bytes = b"POST /state HTTP/1.1\r\nContent-Length: 3\r\n\r\n\xe2\x82\xac".iter();
        let result = read_request_using(
            |chunk, _| {
                clock.set(clock.get() + Duration::from_millis(1));
                chunk[0] = *bytes.next().unwrap();
                Ok(1)
            },
            || clock.get(),
            REQUEST_TIMEOUT,
        )
        .unwrap();
        assert_eq!(result.body, [0xe2, 0x82, 0xac]);
    }

    #[test]
    fn http_read_rejects_announced_oversize_without_body() {
        let request = format!(
            "POST /state HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_REQUEST_BYTES
        );
        let mut reads = 0;
        let result = read_request_using(
            |chunk, _| {
                reads += 1;
                chunk[..request.len()].copy_from_slice(request.as_bytes());
                Ok(request.len())
            },
            Instant::now,
            REQUEST_TIMEOUT,
        );
        assert!(matches!(
            result,
            Err(ReadError::Parse(RequestError::TooLarge))
        ));
        assert_eq!(reads, 1);
    }

    #[test]
    fn http_socket_timeout_and_eof_are_errors() {
        assert!(matches!(
            read_request_using(
                |_, _| Err(io::ErrorKind::TimedOut.into()),
                Instant::now,
                REQUEST_TIMEOUT
            ),
            Err(ReadError::Deadline)
        ));
        assert!(matches!(
            read_request_using(|_, _| Ok(0), Instant::now, REQUEST_TIMEOUT),
            Err(ReadError::Closed)
        ));
        assert!(
            matches!(write_time_remaining_at(Instant::now(), Instant::now()), Err(error) if error.kind() == io::ErrorKind::TimedOut)
        );
    }
    #[test]
    fn http_response_write_deadline_expires_despite_partial_writes() {
        let started = Instant::now();
        let clock = std::cell::Cell::new(started);
        let mut remaining_budgets = Vec::new();
        let result = write_bytes_before(
            b"HTTP/1.1 400 Bad Request\r\n\r\n",
            |_, duration| {
                remaining_budgets.push(duration);
                clock.set(clock.get() + Duration::from_secs(1));
                Ok(1)
            },
            || clock.get(),
            started + RESPONSE_TIMEOUT,
        );
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::TimedOut));
        assert_eq!(
            remaining_budgets,
            (1..=5).rev().map(Duration::from_secs).collect::<Vec<_>>()
        );
    }
}
