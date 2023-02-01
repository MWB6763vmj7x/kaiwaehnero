//! Pieces pertaining to the HTTP message protocol.
use std::borrow::Cow::{Borrowed, Owned};
use std::borrow::IntoCow;
use std::cmp::min;
use std::fmt;
use std::io::{self, Reader, IoResult, BufWriter};
use std::num::from_u16;
use std::str::{self, FromStr};
use std::string::CowString;

use url::Url;
use url::ParseError as UrlError;

use method;
use status::StatusCode;
use uri;
use uri::RequestUri::{AbsolutePath, AbsoluteUri, Authority, Star};
use version::HttpVersion;
use version::HttpVersion::{Http09, Http10, Http11, Http20};
use HttpError::{HttpHeaderError, HttpIoError, HttpMethodError, HttpStatusError,
                HttpUriError, HttpVersionError};
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

impl<R: Reader> HttpReader<R> {

    /// Unwraps this HttpReader and returns the underlying Reader.
    pub fn unwrap(self) -> R {
        match self {
            SizedReader(r, _) => r,
            ChunkedReader(r, _) => r,
            EofReader(r) => r,
            EmptyReader(r) => r,
        }
    }
}

impl<R: Reader> Reader for HttpReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        match *self {
            SizedReader(ref mut body, ref mut remaining) => {
                debug!("Sized read, remaining={:?}", remaining);
                if *remaining == 0 {
                    Err(io::standard_error(io::EndOfFile))
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
                    return Err(io::standard_error(io::EndOfFile));
                }

                let to_read = min(rem as usize, buf.len());
                let count = try!(body.read(buf.slice_to_mut(to_read))) as u64;

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
            EmptyReader(_) => Err(io::standard_error(io::EndOfFile))
        }
    }
}

fn eat<R: Reader>(rdr: &mut R, bytes: &[u8]) -> IoResult<()> {
    for &b in bytes.iter() {
        match try!(rdr.read_byte()) {
            byte if byte == b => (),
            _ => return Err(io::standard_error(io::InvalidInput))
        }
    }
    Ok(())
}

/// Chunked chunks start with 1*HEXDIGIT, indicating the size of the chunk.
fn read_chunk_size<R: Reader>(rdr: &mut R) -> IoResult<u64> {
    let mut size = 0u64;
    let radix = 16;
    let mut in_ext = false;
    let mut in_chunk_size = true;
    loop {
        match try!(rdr.read_byte()) {
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
                match try!(rdr.read_byte()) {
                    LF => break,
                    _ => return Err(io::standard_error(io::InvalidInput))
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
                return Err(io::standard_error(io::InvalidInput));
            }
        }
    }
    debug!("chunk size={:?}", size);
    Ok(size)
}

/// Writers to handle different Transfer-Encodings.
pub enum HttpWriter<W: Writer> {
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

impl<W: Writer> HttpWriter<W> {
    /// Unwraps the HttpWriter and returns the underlying Writer.
    #[inline]
    pub fn unwrap(self) -> W {
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
    /// A final `write()` is called with an empty message, and then flushed.
    /// The ChunkedWriter variant will use this to write the 0-sized last-chunk.
    #[inline]
    pub fn end(mut self) -> IoResult<W> {
        try!(self.write(&[]));
        try!(self.flush());
        Ok(self.unwrap())
    }
}

impl<W: Writer> Writer for HttpWriter<W> {
    #[inline]
    fn write(&mut self, msg: &[u8]) -> IoResult<()> {
        match *self {
            ThroughWriter(ref mut w) => w.write(msg),
            ChunkedWriter(ref mut w) => {
                let chunk_size = msg.len();
                debug!("chunked write, size = {:?}", chunk_size);
                try!(write!(w, "{:X}{}", chunk_size, LINE_ENDING));
                try!(w.write(msg));
                w.write_str(LINE_ENDING)
            },
            SizedWriter(ref mut w, ref mut remaining) => {
                let len = msg.len() as u64;
                if len > *remaining {
                    let len = *remaining;
                    *remaining = 0;
                    try!(w.write(&msg[..len as usize]));
                    Err(io::standard_error(io::ShortWrite(len as usize)))
                } else {
                    *remaining -= len;
                    w.write(msg)
                }
            },
            EmptyWriter(..) => {
                let bytes = msg.len();
                if bytes == 0 {
                    Ok(())
                } else {
                    Err(io::IoError {
                        kind: io::ShortWrite(bytes),
                        desc: "EmptyWriter cannot write any bytes",
                        detail: Some("Cannot include a body with this kind of message".to_string())
                    })
                }
            }
        }
    }

    #[inline]
    fn flush(&mut self) -> IoResult<()> {
        match *self {
            ThroughWriter(ref mut w) => w.flush(),
            ChunkedWriter(ref mut w) => w.flush(),
            SizedWriter(ref mut w, _) => w.flush(),
            EmptyWriter(ref mut w) => w.flush(),
        }
    }
}

pub const SP: u8 = b' ';
pub const CR: u8 = b'\r';
pub const LF: u8 = b'\n';
pub const STAR: u8 = b'*';
pub const LINE_ENDING: &'static str = "\r\n";

/// A `Show`able struct to easily write line endings to a formatter.
pub struct LineEnding;

impl Copy for LineEnding {}

impl fmt::String for LineEnding {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str(LINE_ENDING)
    }
}

impl fmt::Show for LineEnding {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        self.to_string().fmt(fmt)
    }
}

/// Determines if byte is a token char.
///
/// > ```notrust
/// > token          = 1*tchar
/// >
/// > tchar          = "!" / "#" / "$" / "%" / "&" / "'" / "*"
/// >                / "+" / "-" / "." / "^" / "_" / "`" / "|" / "~"
/// >                / DIGIT / ALPHA
/// >                ; any VCHAR, except delimiters
/// > ```
#[inline]
pub fn is_token(b: u8) -> bool {
    match b {
        b'a'...b'z' |
        b'A'...b'Z' |
        b'0'...b'9' |
        b'!' |
        b'#' |
        b'$' |
        b'%' |
        b'&' |
        b'\''|
        b'*' |
        b'+' |
        b'-' |
        b'.' |
        b'^' |
        b'_' |
        b'`' |
        b'|' |
        b'~' => true,
        _ => false
    }
}

/// Read token bytes from `stream` into `buf` until a space is encountered.
/// Returns `Ok(true)` if we read until a space,
/// `Ok(false)` if we got to the end of `buf` without encountering a space,
/// otherwise returns any error encountered reading the stream.
///
/// The remaining contents of `buf` are left untouched.
fn read_token_until_space<R: Reader>(stream: &mut R, buf: &mut [u8]) -> HttpResult<bool> {
    use std::io::BufWriter;
    let mut bufwrt = BufWriter::new(buf);

    loop {
        let byte = try!(stream.read_byte());

        if byte == SP {
            break;
        } else if !is_token(byte) {
            return Err(HttpMethodError);
        // Read to end but there's still more
        } else if bufwrt.write_u8(byte).is_err() {
            return Ok(false);
        }
    }

    if bufwrt.tell().unwrap() == 0 {
        return Err(HttpMethodError);
    }

    Ok(true)
}

/// Read a `Method` from a raw stream, such as `GET`.
/// ### Note:
/// Extension methods are only parsed to 16 characters.
pub fn read_method<R: Reader>(stream: &mut R) -> HttpResult<method::Method> {
    let mut buf = [SP; 16];

    if !try!(read_token_until_space(stream, &mut buf)) {
        return Err(HttpMethodError);
    }

    let maybe_method = match &buf[0..7] {
        b"GET    " => Some(method::Method::Get),
        b"PUT    " => Some(method::Method::Put),
        b"POST   " => Some(method::Method::Post),
        b"HEAD   " => Some(method::Method::Head),
        b"PATCH  " => Some(method::Method::Patch),
        b"TRACE  " => Some(method::Method::Trace),
        b"DELETE " => Some(method::Method::Delete),
        b"CONNECT" => Some(method::Method::Connect),
        b"OPTIONS" => Some(method::Method::Options),
        _ => None,
    };

    debug!("maybe_method = {:?}", maybe_method);

    match (maybe_method, &buf[]) {
        (Some(method), _) => Ok(method),
        (None, ext) => {
            // We already checked that the buffer is ASCII
            Ok(method::Method::Extension(unsafe { str::from_utf8_unchecked(ext) }.trim().to_string()))
        },
    }
}

/// Read a `RequestUri` from a raw stream.
pub fn read_uri<R: Reader>(stream: &mut R) -> HttpResult<uri::RequestUri> {
    let mut b = try!(stream.read_byte());
    while b == SP {
        b = try!(stream.read_byte());
    }

    let mut s = String::new();
    if b == STAR {
        try!(expect(stream.read_byte(), SP));
        return Ok(Star)
    } else {
        s.push(b as char);
        loop {
            match try!(stream.read_byte()) {
                SP => {
                    break;
                },
                CR | LF => {
                    return Err(HttpUriError(UrlError::InvalidCharacter))
                },
                b => s.push(b as char)
            }
        }
    }

    debug!("uri buf = {:?}", s);

    if s.as_slice().starts_with("/") {
        Ok(AbsolutePath(s))
    } else if s.as_slice().contains("/") {
        Ok(AbsoluteUri(try!(Url::parse(s.as_slice()))))
    } else {
        let mut temp = "http://".to_string();
        temp.push_str(s.as_slice());
        try!(Url::parse(temp.as_slice()));
        todo!("compare vs u.authority()");
        Ok(Authority(s))
    }


}


/// Read the `HttpVersion` from a raw stream, such as `HTTP/1.1`.
pub fn read_http_version<R: Reader>(stream: &mut R) -> HttpResult<HttpVersion> {
    try!(expect(stream.read_byte(), b'H'));
    try!(expect(stream.read_byte(), b'T'));
    try!(expect(stream.read_byte(), b'T'));
    try!(expect(stream.read_byte(), b'P'));
    try!(expect(stream.read_byte(), b'/'));

    match try!(stream.read_byte()) {
        b'0' => {
            try!(expect(stream.read_byte(), b'.'));
            try!(expect(stream.read_byte(), b'9'));
            Ok(Http09)
        },
        b'1' => {
            try!(expect(stream.read_byte(), b'.'));
            match try!(stream.read_byte()) {
                b'0' => Ok(Http10),
                b'1' => Ok(Http11),
                _ => Err(HttpVersionError)
            }
        },
        b'2' => {
            try!(expect(stream.read_byte(), b'.'));
            try!(expect(stream.read_byte(), b'0'));
            Ok(Http20)
        },
        _ => Err(HttpVersionError)
    }
}

const MAX_HEADER_NAME_LENGTH: usize = 100;
const MAX_HEADER_FIELD_LENGTH: usize = 1000;

/// The raw bytes when parsing a header line.
///
/// A String and Vec<u8>, divided by COLON (`:`). The String is guaranteed
/// to be all `token`s. See `is_token` source for all valid characters.
pub type RawHeaderLine = (String, Vec<u8>);

/// Read a RawHeaderLine from a Reader.
///
/// From [spec](https://tools.ietf.org/html/http#section-3.2):
///
/// > Each header field consists of a case-insensitive field name followed
/// > by a colon (":"), optional leading whitespace, the field value, and
/// > optional trailing whitespace.
/// >
/// > ```notrust
/// > header-field   = field-name ":" OWS field-value OWS
/// >
/// > field-name     = token
/// > field-value    = *( field-content / obs-fold )
/// > field-content  = field-vchar [ 1*( SP / HTAB ) field-vchar ]
/// > field-vchar    = VCHAR / obs-text
/// >
/// > obs-fold       = CRLF 1*( SP / HTAB )
/// >                ; obsolete line folding
/// >                ; see Section 3.2.4
/// > ```
pub fn read_header<R: Reader>(stream: &mut R) -> HttpResult<Option<RawHeaderLine>> {
    let mut name = String::new();
    let mut value = vec![];

    loop {
        match try!(stream.read_byte()) {
            CR if name.len() == 0 => {
                match try!(stream.read_byte()) {
                    LF => return Ok(None),
                    _ => return Err(HttpHeaderError)
                }
            },
            b':' => break,
            b if is_token(b) => {
                if name.len() > MAX_HEADER_NAME_LENGTH { return Err(HttpHeaderError); }
                name.push(b as char)
            },
            _nontoken => return Err(HttpHeaderError)
        };
    }

    debug!("header name = {:?}", name);

    let mut ows = true; //optional whitespace

    todo!("handle obs-folding (gross!)");
    loop {
        match try!(stream.read_byte()) {
            CR => break,
            LF => return Err(HttpHeaderError),
            b' ' if ows => {},
            b => {
                ows = false;
                if value.len() > MAX_HEADER_FIELD_LENGTH { return Err(HttpHeaderError); }
                value.push(b)
            }
        };
    }
    // Remove optional trailing whitespace
    let real_len = value.len() - value.iter().rev().take_while(|&&x| b' ' == x).count();
    value.truncate(real_len);

    match try!(stream.read_byte()) {
        LF => Ok(Some((name, value))),
        _ => Err(HttpHeaderError)
    }

}

/// `request-line   = method SP request-target SP HTTP-version CRLF`
pub type RequestLine = (method::Method, uri::RequestUri, HttpVersion);

/// Read the `RequestLine`, such as `GET / HTTP/1.1`.
pub fn read_request_line<R: Reader>(stream: &mut R) -> HttpResult<RequestLine> {
    debug!("read request line");
    let method = try!(read_method(stream));
    debug!("method = {:?}", method);
    let uri = try!(read_uri(stream));
    debug!("uri = {:?}", uri);
    let version = try!(read_http_version(stream));
    debug!("version = {:?}", version);

    if try!(stream.read_byte()) != CR {
        return Err(HttpVersionError);
    }
    if try!(stream.read_byte()) != LF {
        return Err(HttpVersionError);
    }

    Ok((method, uri, version))
}

/// `status-line = HTTP-version SP status-code SP reason-phrase CRLF`
///
/// However, reason-phrase is absolutely useless, so its tossed.
pub type StatusLine = (HttpVersion, RawStatus);

/// The raw status code and reason-phrase.
#[derive(PartialEq, Show)]
pub struct RawStatus(pub u16, pub CowString<'static>);

impl Clone for RawStatus {
    fn clone(&self) -> RawStatus {
        RawStatus(self.0, self.1.clone().into_cow())
    }
}

/// Read the StatusLine, such as `HTTP/1.1 200 OK`.
///
/// > The first line of a response message is the status-line, consisting
/// > of the protocol version, a space (SP), the status code, another
/// > space, a possibly empty textual phrase describing the status code,
/// > and ending with CRLF.
/// >
/// >```notrust
/// > status-line = HTTP-version SP status-code SP reason-phrase CRLF
/// > status-code    = 3DIGIT
/// > reason-phrase  = *( HTAB / SP / VCHAR / obs-text )
/// >```
pub fn read_status_line<R: Reader>(stream: &mut R) -> HttpResult<StatusLine> {
    let version = try!(read_http_version(stream));
    if try!(stream.read_byte()) != SP {
        return Err(HttpVersionError);
    }
    let code = try!(read_status(stream));

    Ok((version, code))
}

/// Read the StatusCode from a stream.
pub fn read_status<R: Reader>(stream: &mut R) -> HttpResult<RawStatus> {
    let code = [
        try!(stream.read_byte()),
        try!(stream.read_byte()),
        try!(stream.read_byte()),
    ];

    let code = match str::from_utf8(code.as_slice()).ok().and_then(FromStr::from_str) {
        Some(num) => num,
        None => return Err(HttpStatusError)
    };

    match try!(stream.read_byte()) {
        b' ' => (),
        _ => return Err(HttpStatusError)
    }

    let mut buf = [b' '; 32];

    {
        let mut bufwrt = BufWriter::new(&mut buf);
        'read: loop {
            match try!(stream.read_byte()) {
                CR => match try!(stream.read_byte()) {
                    LF => break,
                    _ => return Err(HttpStatusError)
                },
                b => match bufwrt.write_u8(b) {
                    Ok(_) => (),
                    Err(_) => {
                        for _ in 0u8..128 {
                            match try!(stream.read_byte()) {
                                CR => match try!(stream.read_byte()) {
                                    LF => break 'read,
                                    _ => return Err(HttpStatusError)
                                },
                                _ => { /* ignore */ }
                            }
                        }
                        return Err(HttpStatusError)
                    }
                }
            }
        }
    }

    let reason = match str::from_utf8(&buf[]) {
        Ok(s) => s.trim(),
        Err(_) => return Err(HttpStatusError)
    };

    let reason = match from_u16::<StatusCode>(code) {
        Some(status) => match status.canonical_reason() {
            Some(phrase) => {
                if phrase == reason {
                    Borrowed(phrase)
                } else {
                    Owned(reason.to_string())
                }
            }
            _ => Owned(reason.to_string())
        },
        None => return Err(HttpStatusError)
    };

    Ok(RawStatus(code, reason))
}

#[inline]
fn expect(r: IoResult<u8>, expected: u8) -> HttpResult<()> {
    match r {
        Ok(b) if b == expected => Ok(()),
        Ok(_) => Err(HttpVersionError),
        Err(e) => Err(HttpIoError(e))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, MemReader, MemWriter, IoResult};
    use std::borrow::Cow::{Borrowed, Owned};
    use test::Bencher;
    use uri::RequestUri;
    use uri::RequestUri::{Star, AbsoluteUri, AbsolutePath, Authority};
    use method;
    use version::HttpVersion;
    use version::HttpVersion::{Http10, Http11, Http20};
    use HttpError::{HttpVersionError, HttpMethodError};
    use HttpResult;
    use url::Url;

    use super::{read_method, read_uri, read_http_version, read_header,
                RawHeaderLine, read_status, RawStatus, read_chunk_size};

    fn mem(s: &str) -> MemReader {
        MemReader::new(s.as_bytes().to_vec())
    }

    #[test]
    fn test_read_method() {
        fn read(s: &str, result: HttpResult<method::Method>) {
            assert_eq!(read_method(&mut mem(s)), result);
        }

        read("GET /", Ok(method::Method::Get));
        read("POST /", Ok(method::Method::Post));
        read("PUT /", Ok(method::Method::Put));
        read("HEAD /", Ok(method::Method::Head));
        read("OPTIONS /", Ok(method::Method::Options));
        read("CONNECT /", Ok(method::Method::Connect));
        read("TRACE /", Ok(method::Method::Trace));
        read("PATCH /", Ok(method::Method::Patch));
        read("FOO /", Ok(method::Method::Extension("FOO".to_string())));
        read("akemi!~#HOMURA /", Ok(method::Method::Extension("akemi!~#HOMURA".to_string())));
        read(" ", Err(HttpMethodError));
    }

    #[test]
    fn test_read_uri() {
        fn read(s: &str, result: HttpResult<RequestUri>) {
            assert_eq!(read_uri(&mut mem(s)), result);
        }

        read("* ", Ok(Star));
        read("http://hyper.rs/ ", Ok(AbsoluteUri(Url::parse("http://hyper.rs/").unwrap())));
        read("hyper.rs ", Ok(Authority("hyper.rs".to_string())));
        read("/ ", Ok(AbsolutePath("/".to_string())));
    }

    #[test]
    fn test_read_http_version() {
        fn read(s: &str, result: HttpResult<HttpVersion>) {
            assert_eq!(read_http_version(&mut mem(s)), result);
        }

        read("HTTP/1.0", Ok(Http10));
        read("HTTP/1.1", Ok(Http11));
        read("HTTP/2.0", Ok(Http20));
        read("HTP/2.0", Err(HttpVersionError));
        read("HTTP.2.0", Err(HttpVersionError));
        read("HTTP 2.0", Err(HttpVersionError));
        read("TTP 2.0", Err(HttpVersionError));
    }

    #[test]
    fn test_read_status() {
        fn read(s: &str, result: HttpResult<RawStatus>) {
            assert_eq!(read_status(&mut mem(s)), result);
        }

        fn read_ignore_string(s: &str, result: HttpResult<RawStatus>) {
            match (read_status(&mut mem(s)), result) {
                (Ok(RawStatus(ref c1, _)), Ok(RawStatus(ref c2, _))) => {
                    assert_eq!(c1, c2);
                },
                (r1, r2) => assert_eq!(r1, r2)
            }
        }

        read("200 OK\r\n", Ok(RawStatus(200, Borrowed("OK"))));
        read("404 Not Found\r\n", Ok(RawStatus(404, Borrowed("Not Found"))));
        read("200 crazy pants\r\n", Ok(RawStatus(200, Owned("crazy pants".to_string()))));
        read("301 Moved Permanently\r\n", Ok(RawStatus(301, Owned("Moved Permanently".to_string()))));
        read_ignore_string("301 Unreasonably long header that should not happen, \
                           but some men just want to watch the world burn\r\n",
             Ok(RawStatus(301, Owned("Ignored".to_string()))));
    }

    #[test]
    fn test_read_header() {
        fn read(s: &str, result: HttpResult<Option<RawHeaderLine>>) {
            assert_eq!(read_header(&mut mem(s)), result);
        }

        read("Host: rust-lang.org\r\n", Ok(Some(("Host".to_string(),
                                                 b"rust-lang.org".to_vec()))));
    }

    #[test]
    fn test_write_chunked() {
        use std::str::from_utf8;
        let mut w = super::HttpWriter::ChunkedWriter(MemWriter::new());
        w.write(b"foo bar").unwrap();
        w.write(b"baz quux herp").unwrap();
        let buf = w.end().unwrap().into_inner();
        let s = from_utf8(buf.as_slice()).unwrap();
        assert_eq!(s, "7\r\nfoo bar\r\nD\r\nbaz quux herp\r\n0\r\n\r\n");
    }

    #[test]
    fn test_write_sized() {
        use std::str::from_utf8;
        let mut w = super::HttpWriter::SizedWriter(MemWriter::new(), 8);
        w.write(b"foo bar").unwrap();
        assert_eq!(w.write(b"baz"), Err(io::standard_error(io::ShortWrite(1))));

        let buf = w.end().unwrap().into_inner();
        let s = from_utf8(buf.as_slice()).unwrap();
        assert_eq!(s, "foo barb");
    }

    #[test]
    fn test_read_chunk_size() {
        fn read(s: &str, result: IoResult<u64>) {
            assert_eq!(read_chunk_size(&mut mem(s)), result);
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
        read("F\rF", Err(io::standard_error(io::InvalidInput)));
        read("F", Err(io::standard_error(io::EndOfFile)));
        // Invalid hex digit
        read("X\r\n", Err(io::standard_error(io::InvalidInput)));
        read("1X\r\n", Err(io::standard_error(io::InvalidInput)));
        read("-\r\n", Err(io::standard_error(io::InvalidInput)));
        read("-1\r\n", Err(io::standard_error(io::InvalidInput)));
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
        read("1 invalid extension\r\n", Err(io::standard_error(io::InvalidInput)));
        read("1 A\r\n", Err(io::standard_error(io::InvalidInput)));
        read("1;no CRLF", Err(io::standard_error(io::EndOfFile)));
    }

    #[bench]
    fn bench_read_method(b: &mut Bencher) {
        b.bytes = b"CONNECT ".len() as u64;
        b.iter(|| assert_eq!(read_method(&mut mem("CONNECT ")), Ok(method::Method::Connect)));
    }

    #[bench]
    fn bench_read_status(b: &mut Bencher) {
        b.bytes = b"404 Not Found\r\n".len() as u64;
        b.iter(|| assert_eq!(read_status(&mut mem("404 Not Found\r\n")), Ok(RawStatus(404, Borrowed("Not Found")))));
    }

}
