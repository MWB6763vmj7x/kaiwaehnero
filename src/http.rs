//! Pieces pertaining to the HTTP message protocol.
use std::borrow::{Cow, ToOwned};
use std::cmp::min;
use std::io::{self, Read, Write, BufRead};
use std::fmt;

use httparse;

use buffer::BufReader;
use header::Headers;
use method::Method;
use status::StatusCode;
use uri::RequestUri;
use version::HttpVersion::{self, Http10, Http11};
use HttpError::{HttpIoError, HttpTooLargeError};
use {HttpError, HttpResult};

use self::HttpReader::{SizedReader, ChunkedReader, EofReader, EmptyReader};
use self::HttpWriter::{ThroughWriter, ChunkedWriter, SizedWriter, EmptyWriter};

/// Readers to handle different Transfer-Encodings.
///
/// If a message body does not include a Transfer-Encoding, it *should*
/// include a Content-Length header.
pub enum HttpReader<R> {
    /// A Reader used when a Content-Length header is passed with a positive integer.
    SizedReader(R, u64),
    /// A Reader used when Transfer-Encoding is `chunked`.
    ChunkedReader(R, Option<u64>),
    /// A Reader used for responses that don't indicate a length or chunked.
    ///
    /// Note: This should only used for `Response`s. It is illegal for a
    /// `Request` to be made with both `Content-Length` and
    /// `Transfer-Encoding: chunked` missing, as explained from the spec:
    ///
    /// > If a Transfer-Encoding header field is present in a response and
    /// > the chunked transfer coding is not the final encoding, the
    /// > message body length is determined by reading the connection until
    /// > it is closed by the server.  If a Transfer-Encoding header field
    /// > is present in a request and the chunked transfer coding is not
    /// > the final encoding, the message body length cannot be determined
    /// > reliably; the server MUST respond with the 400 (Bad Request)
    /// > status code and then close the connection.
    EofReader(R),
    /// A Reader used for messages that should never have a body.
    ///
    /// See https://tools.ietf.org/html/rfc7230#section-3.3.3
    EmptyReader(R),
}

impl<R: Read> HttpReader<R> {

    /// Unwraps this HttpReader and returns the underlying Reader.
    pub fn into_inner(self) -> R {
        match self {
            SizedReader(r, _) => r,
            ChunkedReader(r, _) => r,
            EofReader(r) => r,
            EmptyReader(r) => r,
        }
    }
}

impl<R> fmt::Debug for HttpReader<R> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            SizedReader(_,rem) => write!(fmt, "SizedReader(remaining={:?})", rem),
            ChunkedReader(_, None) => write!(fmt, "ChunkedReader(chunk_remaining=unknown)"),
            ChunkedReader(_, Some(rem)) => write!(fmt, "ChunkedReader(chunk_remaining={:?})", rem),
            EofReader(_) => write!(fmt, "EofReader"),
            EmptyReader(_) => write!(fmt, "EmptyReader"),
        }
    }
}

impl<R: Read> Read for HttpReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            SizedReader(ref mut body, ref mut remaining) => {
                debug!("Sized read, remaining={:?}", remaining);
                if *remaining == 0 {
                    Ok(0)
                } else {
                    let num = try!(body.read(buf)) as u64;
                    if num > *remaining {
                        *remaining = 0;
                    } else {
                        *remaining -= num;
                    }
                    Ok(num as usize)
                }
            },
            ChunkedReader(ref mut body, ref mut opt_remaining) => {
                let mut rem = match *opt_remaining {
                    Some(ref rem) => *rem,
                    // None means we don't know the size of the next chunk
                    None => try!(read_chunk_size(body))
                };
                debug!("Chunked read, remaining={:?}", rem);

                if rem == 0 {
                    *opt_remaining = Some(0);

                    // chunk of size 0 signals the end of the chunked stream
                    // if the 0 digit was missing from the stream, it would
                    // be an InvalidInput error instead.
                    debug!("end of chunked");
                    return Ok(0)
                }

                let to_read = min(rem as usize, buf.len());
                let count = try!(body.read(&mut buf[..to_read])) as u64;

                rem -= count;
                *opt_remaining = if rem > 0 {
                    Some(rem)
                } else {
                    try!(eat(body, LINE_ENDING.as_bytes()));
                    None
                };
                Ok(count as usize)
            },
            EofReader(ref mut body) => {
                body.read(buf)
            },
            EmptyReader(_) => Ok(0)
        }
    }
}

fn eat<R: Read>(rdr: &mut R, bytes: &[u8]) -> io::Result<()> {
    let mut buf = [0];
    for &b in bytes.iter() {
        match try!(rdr.read(&mut buf)) {
            1 if buf[0] == b => (),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                          "Invalid characters found")),
        }
    }
    Ok(())
}

/// Chunked chunks start with 1*HEXDIGIT, indicating the size of the chunk.
fn read_chunk_size<R: Read>(rdr: &mut R) -> io::Result<u64> {
    macro_rules! byte (
        ($rdr:ident) => ({
            let mut buf = [0];
            match try!($rdr.read(&mut buf)) {
                1 => buf[0],
                _ => return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                                  "Invalid chunk size line")),

            }
        })
    );
    let mut size = 0u64;
    let radix = 16;
    let mut in_ext = false;
    let mut in_chunk_size = true;
    loop {
        match byte!(rdr) {
            b@b'0'...b'9' if in_chunk_size => {
                size *= radix;
                size += (b - b'0') as u64;
            },
            b@b'a'...b'f' if in_chunk_size => {
                size *= radix;
                size += (b + 10 - b'a') as u64;
            },
            b@b'A'...b'F' if in_chunk_size => {
                size *= radix;
                size += (b + 10 - b'A') as u64;
            },
            CR => {
                match byte!(rdr) {
                    LF => break,
                    _ => return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                                  "Invalid chunk size line"))

                }
            },
            // If we weren't in the extension yet, the ";" signals its start
            b';' if !in_ext => {
                in_ext = true;
                in_chunk_size = false;
            },
            // "Linear white space" is ignored between the chunk size and the
            // extension separator token (";") due to the "implied *LWS rule".
            b'\t' | b' ' if !in_ext & !in_chunk_size => {},
            // LWS can follow the chunk size, but no more digits can come
            b'\t' | b' ' if in_chunk_size => in_chunk_size = false,
            // We allow any arbitrary octet once we are in the extension, since
            // they all get ignored anyway. According to the HTTP spec, valid
            // extensions would have a more strict syntax:
            //     (token ["=" (token | quoted-string)])
            // but we gain nothing by rejecting an otherwise valid chunk size.
            ext if in_ext => {
                todo!("chunk extension byte={}", ext);
            },
            // Finally, if we aren't in the extension and we're reading any
            // other octet, the chunk size line is invalid!
            _ => {
                return Err(io::Error::new(io::ErrorKind::InvalidInput,
                                         "Invalid chunk size line"));
            }
        }
    }
    debug!("chunk size={:?}", size);
    Ok(size)
}

/// Writers to handle different Transfer-Encodings.
pub enum HttpWriter<W: Write> {
    /// A no-op Writer, used initially before Transfer-Encoding is determined.
    ThroughWriter(W),
    /// A Writer for when Transfer-Encoding includes `chunked`.
    ChunkedWriter(W),
    /// A Writer for when Content-Length is set.
    ///
    /// Enforces that the body is not longer than the Content-Length header.
    SizedWriter(W, u64),
    /// A writer that should not write any body.
    EmptyWriter(W),
}

impl<W: Write> HttpWriter<W> {
    /// Unwraps the HttpWriter and returns the underlying Writer.
    #[inline]
    pub fn into_inner(self) -> W {
        match self {
            ThroughWriter(w) => w,
            ChunkedWriter(w) => w,
            SizedWriter(w, _) => w,
            EmptyWriter(w) => w,
        }
    }

    /// Access the inner Writer.
    #[inline]
    pub fn get_ref<'a>(&'a self) -> &'a W {
        match *self {
            ThroughWriter(ref w) => w,
            ChunkedWriter(ref w) => w,
            SizedWriter(ref w, _) => w,
            EmptyWriter(ref w) => w,
        }
    }

    /// Access the inner Writer mutably.
    ///
    /// Warning: You should not write to this directly, as you can corrupt
    /// the state.
    #[inline]
    pub fn get_mut<'a>(&'a mut self) -> &'a mut W {
        match *self {
            ThroughWriter(ref mut w) => w,
            ChunkedWriter(ref mut w) => w,
            SizedWriter(ref mut w, _) => w,
            EmptyWriter(ref mut w) => w,
        }
    }

    /// Ends the HttpWriter, and returns the underlying Writer.
    ///
    /// A final `write_all()` is called with an empty message, and then flushed.
    /// The ChunkedWriter variant will use this to write the 0-sized last-chunk.
    #[inline]
    pub fn end(mut self) -> io::Result<W> {
        try!(self.write(&[]));
        try!(self.flush());
        Ok(self.into_inner())
    }
}

impl<W: Write> Write for HttpWriter<W> {
    #[inline]
    fn write(&mut self, msg: &[u8]) -> io::Result<usize> {
        match *self {
            ThroughWriter(ref mut w) => w.write(msg),
            ChunkedWriter(ref mut w) => {
                let chunk_size = msg.len();
                debug!("chunked write, size = {:?}", chunk_size);
                try!(write!(w, "{:X}{}", chunk_size, LINE_ENDING));
                try!(w.write_all(msg));
                try!(w.write_all(LINE_ENDING.as_bytes()));
                Ok(msg.len())
            },
            SizedWriter(ref mut w, ref mut remaining) => {
                let len = msg.len() as u64;
                if len > *remaining {
                    let len = *remaining;
                    *remaining = 0;
                    try!(w.write_all(&msg[..len as usize]));
                    Ok(len as usize)
                } else {
                    *remaining -= len;
                    try!(w.write_all(msg));
                    Ok(len as usize)
                }
            },
            EmptyWriter(..) => {
                if msg.len() != 0 {
                    error!("Cannot include a body with this kind of message");
                }
                Ok(0)
            }
        }
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        match *self {
            ThroughWriter(ref mut w) => w.flush(),
            ChunkedWriter(ref mut w) => w.flush(),
            SizedWriter(ref mut w, _) => w.flush(),
            EmptyWriter(ref mut w) => w.flush(),
        }
    }
}

impl<W: Write> fmt::Debug for HttpWriter<W> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ThroughWriter(_) => write!(fmt, "ThroughWriter"),
            ChunkedWriter(_) => write!(fmt, "ChunkedWriter"),
            SizedWriter(_, rem) => write!(fmt, "SizedWriter(remaining={:?})", rem),
            EmptyWriter(_) => write!(fmt, "EmptyWriter"),
        }
    }
}

const MAX_HEADERS: usize = 100;

/// Parses a request into an Incoming message head.
#[inline]
pub fn parse_request<R: Read>(buf: &mut BufReader<R>) -> HttpResult<Incoming<(Method, RequestUri)>> {
    parse::<R, httparse::Request, (Method, RequestUri)>(buf)
}

/// Parses a response into an Incoming message head.
#[inline]
pub fn parse_response<R: Read>(buf: &mut BufReader<R>) -> HttpResult<Incoming<RawStatus>> {
    parse::<R, httparse::Response, RawStatus>(buf)
}

fn parse<R: Read, T: TryParse<Subject=I>, I>(rdr: &mut BufReader<R>) -> HttpResult<Incoming<I>> {
    loop {
        match try!(try_parse::<R, T, I>(rdr)) {
            httparse::Status::Complete((inc, len)) => {
                rdr.consume(len);
                return Ok(inc);
            },
            _partial => ()
        }
        match try!(rdr.read_into_buf()) {
            0 if rdr.get_buf().len() == 0 => {
                return Err(HttpIoError(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Connection closed"
                )))
            },
            0 => return Err(HttpTooLargeError),
            _ => ()
        }
    }
}

fn try_parse<R: Read, T: TryParse<Subject=I>, I>(rdr: &mut BufReader<R>) -> TryParseResult<I> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    <T as TryParse>::try_parse(&mut headers, rdr.get_buf())
}

#[doc(hidden)]
trait TryParse {
    type Subject;
    fn try_parse<'a>(headers: &'a mut [httparse::Header<'a>], buf: &'a [u8]) -> TryParseResult<Self::Subject>;
}

type TryParseResult<T> = Result<httparse::Status<(Incoming<T>, usize)>, HttpError>;

impl<'a> TryParse for httparse::Request<'a> {
    type Subject = (Method, RequestUri);

    fn try_parse<'b>(headers: &'b mut [httparse::Header<'b>], buf: &'b [u8]) -> TryParseResult<(Method, RequestUri)> {
        let mut req = httparse::Request::new(headers);
        Ok(match try!(req.parse(buf)) {
            httparse::Status::Complete(len) => {
                httparse::Status::Complete((Incoming {
                    version: if req.version.unwrap() == 1 { Http11 } else { Http10 },
                    subject: (
                        try!(req.method.unwrap().parse()),
                        try!(req.path.unwrap().parse())
                    ),
                    headers: try!(Headers::from_raw(req.headers))
                }, len))
            },
            httparse::Status::Partial => httparse::Status::Partial
        })
    }
}

impl<'a> TryParse for httparse::Response<'a> {
    type Subject = RawStatus;

    fn try_parse<'b>(headers: &'b mut [httparse::Header<'b>], buf: &'b [u8]) -> TryParseResult<RawStatus> {
        let mut res = httparse::Response::new(headers);
        Ok(match try!(res.parse(buf)) {
            httparse::Status::Complete(len) => {
                let code = res.code.unwrap();
                let reason = match StatusCode::from_u16(code).canonical_reason() {
                    Some(reason) => Cow::Borrowed(reason),
                    None => Cow::Owned(res.reason.unwrap().to_owned())
                };
                httparse::Status::Complete((Incoming {
                    version: if res.version.unwrap() == 1 { Http11 } else { Http10 },
                    subject: RawStatus(code, reason),
                    headers: try!(Headers::from_raw(res.headers))
                }, len))
            },
            httparse::Status::Partial => httparse::Status::Partial
        })
    }
}

/// An Incoming Message head. Includes request/status line, and headers.
#[derive(Debug)]
pub struct Incoming<S> {
    /// HTTP version of the message.
    pub version: HttpVersion,
    /// Subject (request line or status line) of Incoming message.
    pub subject: S,
    /// Headers of the Incoming message.
    pub headers: Headers
}

pub const SP: u8 = b' ';
pub const CR: u8 = b'\r';
pub const LF: u8 = b'\n';
pub const STAR: u8 = b'*';
pub const LINE_ENDING: &'static str = "\r\n";

/// The raw status code and reason-phrase.
#[derive(Clone, PartialEq, Debug)]
pub struct RawStatus(pub u16, pub Cow<'static, str>);

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use buffer::BufReader;
    use mock::MockStream;

    use super::{read_chunk_size, parse_request};

    #[test]
    fn test_write_chunked() {
        use std::str::from_utf8;
        let mut w = super::HttpWriter::ChunkedWriter(Vec::new());
        w.write_all(b"foo bar").unwrap();
        w.write_all(b"baz quux herp").unwrap();
        let buf = w.end().unwrap();
        let s = from_utf8(buf.as_ref()).unwrap();
        assert_eq!(s, "7\r\nfoo bar\r\nD\r\nbaz quux herp\r\n0\r\n\r\n");
    }

    #[test]
    fn test_write_sized() {
        use std::str::from_utf8;
        let mut w = super::HttpWriter::SizedWriter(Vec::new(), 8);
        w.write_all(b"foo bar").unwrap();
        assert_eq!(w.write(b"baz").unwrap(), 1);

        let buf = w.end().unwrap();
        let s = from_utf8(buf.as_ref()).unwrap();
        assert_eq!(s, "foo barb");
    }

    #[test]
    fn test_read_chunk_size() {
        fn read(s: &str, result: u64) {
            assert_eq!(read_chunk_size(&mut s.as_bytes()).unwrap(), result);
        }

        fn read_err(s: &str) {
            assert_eq!(read_chunk_size(&mut s.as_bytes()).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }

        read("1\r\n", 1);
        read("01\r\n", 1);
        read("0\r\n", 0);
        read("00\r\n", 0);
        read("A\r\n", 10);
        read("a\r\n", 10);
        read("Ff\r\n", 255);
        read("Ff   \r\n", 255);
        // Missing LF or CRLF
        read_err("F\rF");
        read_err("F");
        // Invalid hex digit
        read_err("X\r\n");
        read_err("1X\r\n");
        read_err("-\r\n");
        read_err("-1\r\n");
        // Acceptable (if not fully valid) extensions do not influence the size
        read("1;extension\r\n", 1);
        read("a;ext name=value\r\n", 10);
        read("1;extension;extension2\r\n", 1);
        read("1;;;  ;\r\n", 1);
        read("2; extension...\r\n", 2);
        read("3   ; extension=123\r\n", 3);
        read("3   ;\r\n", 3);
        read("3   ;   \r\n", 3);
        // Invalid extensions cause an error
        read_err("1 invalid extension\r\n");
        read_err("1 A\r\n");
        read_err("1;no CRLF");
    }

    #[test]
    fn test_parse_incoming() {
        let mut raw = MockStream::with_input(b"GET /echo HTTP/1.1\r\nHost: hyper.rs\r\n\r\n");
        let mut buf = BufReader::new(&mut raw);
        parse_request(&mut buf).unwrap();
    }

    #[test]
    fn test_parse_tcp_closed() {
        use std::io::ErrorKind;
        use error::HttpError::HttpIoError;

        let mut empty = MockStream::new();
        let mut buf = BufReader::new(&mut empty);
        match parse_request(&mut buf) {
            Err(HttpIoError(ref e)) if e.kind() == ErrorKind::ConnectionAborted => (),
            other => panic!("unexpected result: {:?}", other)
        }
    }

    #[cfg(feature = "nightly")]
    use test::Bencher;

    #[cfg(feature = "nightly")]
    #[bench]
    fn bench_parse_incoming(b: &mut Bencher) {
        let mut raw = MockStream::with_input(b"GET /echo HTTP/1.1\r\nHost: hyper.rs\r\n\r\n");
        let mut buf = BufReader::new(&mut raw);
        b.iter(|| {
            parse_request(&mut buf).unwrap();
            buf.get_mut().read.set_position(0);
        });
    }
}
