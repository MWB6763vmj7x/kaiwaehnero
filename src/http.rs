//! Pieces pertaining to the HTTP message protocol.
use std::borrow::{Cow, IntoCow, ToOwned};
use std::cmp::min;
use std::io::{self, Read, Write, BufRead};

use httparse;

use header::Headers;
use method::Method;
use uri::RequestUri;
use version::HttpVersion::{self, Http10, Http11};
use HttpError:: HttpTooLargeError;
use HttpResult;

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
                                          "Invalid characters found",
                                           None))
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
                                                  "Invalid chunk size line",
                                                   None)),

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
                                                  "Invalid chunk size line",
                                                   None))

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
                                         "Invalid chunk size line",
                                         None))
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

/// Parses a request into an Incoming message head.
pub fn parse_request<T: BufRead>(buf: &mut T) -> HttpResult<Incoming<(Method, RequestUri)>> {
    let (inc, len) = {
        let slice = try!(buf.fill_buf());
        let mut headers = [httparse::Header { name: "", value: b"" }; 64];
        let mut req = httparse::Request::new(&mut headers);
        match try!(req.parse(slice)) {
            httparse::Status::Complete(len) => {
                (Incoming {
                    version: if req.version.unwrap() == 1 { Http11 } else { Http10 },
                    subject: (
                        try!(req.method.unwrap().parse()),
                        try!(req.path.unwrap().parse())
                    ),
                    headers: try!(Headers::from_raw(req.headers))
                }, len)
            },
            _ => {
                // request head is bigger than a BufRead's buffer? 400 that!
                return Err(HttpTooLargeError)
            }
        }
    };
    buf.consume(len);
    Ok(inc)
}

/// Parses a response into an Incoming message head.
pub fn parse_response<T: BufRead>(buf: &mut T) -> HttpResult<Incoming<RawStatus>> {
    let (inc, len) = {
        let mut headers = [httparse::Header { name: "", value: b"" }; 64];
        let mut res = httparse::Response::new(&mut headers);
        match try!(res.parse(try!(buf.fill_buf()))) {
            httparse::Status::Complete(len) => {
                (Incoming {
                    version: if res.version.unwrap() == 1 { Http11 } else { Http10 },
                    subject: RawStatus(
                        res.code.unwrap(), res.reason.unwrap().to_owned().into_cow()
                    ),
                    headers: try!(Headers::from_raw(res.headers))
                }, len)
            },
            _ => {
                // response head is bigger than a BufRead's buffer?
                return Err(HttpTooLargeError)
            }
        }
    };
    buf.consume(len);
    Ok(inc)
}

/// An Incoming Message head. Includes request/status line, and headers.
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
#[derive(PartialEq, Debug)]
pub struct RawStatus(pub u16, pub Cow<'static, str>);

impl Clone for RawStatus {
    fn clone(&self) -> RawStatus {
        RawStatus(self.0, self.1.clone().into_cow())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use super::{read_chunk_size};


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
        assert_eq!(w.write(b"baz"), Ok(1));

        let buf = w.end().unwrap();
        let s = from_utf8(buf.as_ref()).unwrap();
        assert_eq!(s, "foo barb");
    }

    #[test]
    fn test_read_chunk_size() {
        fn read(s: &str, result: io::Result<u64>) {
            assert_eq!(read_chunk_size(&mut s.as_bytes()), result);
        }

        fn read_err(s: &str) {
            assert_eq!(read_chunk_size(&mut s.as_bytes()).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }

        read("1\r\n", Ok(1));
        read("01\r\n", Ok(1));
        read("0\r\n", Ok(0));
        read("00\r\n", Ok(0));
        read("A\r\n", Ok(10));
        read("a\r\n", Ok(10));
        read("Ff\r\n", Ok(255));
        read("Ff   \r\n", Ok(255));
        // Missing LF or CRLF
        read_err("F\rF");
        read_err("F");
        // Invalid hex digit
        read_err("X\r\n");
        read_err("1X\r\n");
        read_err("-\r\n");
        read_err("-1\r\n");
        // Acceptable (if not fully valid) extensions do not influence the size
        read("1;extension\r\n", Ok(1));
        read("a;ext name=value\r\n", Ok(10));
        read("1;extension;extension2\r\n", Ok(1));
        read("1;;;  ;\r\n", Ok(1));
        read("2; extension...\r\n", Ok(2));
        read("3   ; extension=123\r\n", Ok(3));
        read("3   ;\r\n", Ok(3));
        read("3   ;   \r\n", Ok(3));
        // Invalid extensions cause an error
        read_err("1 invalid extension\r\n");
        read_err("1 A\r\n");
        read_err("1;no CRLF");
    }

    use test::Bencher;

    #[bench]
    fn bench_parse_incoming(b: &mut Bencher) {
        use std::io::BufReader;
        use mock::MockStream;

        use super::parse_request;
        b.iter(|| {
            let mut raw = MockStream::with_input(b"GET /echo HTTP/1.1\r\nHost: hyper.rs\r\n\r\n");
            let mut buf = BufReader::new(&mut raw);

            parse_request(&mut buf).unwrap();
        });
    }
}
