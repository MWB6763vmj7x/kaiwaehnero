//! Pieces pertaining to the HTTP message protocol.
use std::cmp::min;
use std::fmt;
use std::io::{mod, Reader, IoResult};
use std::u16;

use url::Url;

use method;
use status;
use uri;
use version::{HttpVersion, Http09, Http10, Http11, Http20};
use {HttpResult, HttpMethodError, HttpVersionError, HttpIoError, HttpUriError};
use {HttpHeaderError, HttpStatusError};

/// Readers to handle different Transfer-Encodings.
///
/// If a message body does not include a Transfer-Encoding, it *should*
/// include a Content-Length header.
pub enum HttpReader<R> {
    /// A Reader used when a Content-Length header is passed with a positive integer.
    SizedReader(R, uint),
    /// A Reader used when Transfer-Encoding is `chunked`.
    ChunkedReader(R, Option<uint>),
    /// A Reader used for responses that don't indicate a length or chunked.
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
}

impl<R: Reader> Reader for HttpReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<uint> {
        match *self {
            SizedReader(ref mut body, ref mut remaining) => {
                debug!("Sized read, remaining={}", remaining);
                if *remaining == 0 {
                    Err(io::standard_error(io::EndOfFile))
                } else {
                    let num = try!(body.read(buf));
                    if num > *remaining {
                        *remaining = 0;
                    } else {
                        *remaining -= num;
                    }
                    Ok(num)
                }
            },
            ChunkedReader(ref mut body, ref mut opt_remaining) => {
                let mut rem = match *opt_remaining {
                    Some(ref rem) => *rem,
                    // None means we don't know the size of the next chunk
                    None => try!(read_chunk_size(body))
                };
                debug!("Chunked read, remaining={}", rem);

                if rem == 0 {
                    // chunk of size 0 signals the end of the chunked stream
                    // if the 0 digit was missing from the stream, it would
                    // be an InvalidInput error instead.
                    debug!("end of chunked");
                    return Err(io::standard_error(io::EndOfFile));
                }

                let to_read = min(rem, buf.len());
                let count = try!(body.read(buf.slice_to_mut(to_read)));

                rem -= count;
                *opt_remaining = if rem > 0 {
                    Some(rem)
                } else {
                    try!(eat(body, LINE_ENDING));
                    None
                };
                Ok(count)
            },
            EofReader(ref mut body) => {
                body.read(buf)
            }
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
fn read_chunk_size<R: Reader>(rdr: &mut R) -> IoResult<uint> {
    let mut size = 0u;
    let radix = 16;
    let mut in_ext = false;
    loop {
        match try!(rdr.read_byte()) {
            b@b'0'...b'9' if !in_ext => {
                size *= radix;
                size += (b - b'0') as uint;
            },
            b@b'a'...b'f' if !in_ext => {
                size *= radix;
                size += (b + 10 - b'a') as uint;
            },
            b@b'A'...b'F' if !in_ext => {
                size *= radix;
                size += (b + 10 - b'A') as uint;
            },
            CR => {
                match try!(rdr.read_byte()) {
                    LF => break,
                    _ => return Err(io::standard_error(io::InvalidInput))
                }
            },
            ext => {
                in_ext = true;
                todo!("chunk extension byte={}", ext);
            }
        }
    }
    debug!("chunk size={}", size);
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
    SizedWriter(W, uint),
}

impl<W: Writer> HttpWriter<W> {
    /// Unwraps the HttpWriter and returns the underlying Writer.
    #[inline]
    pub fn unwrap(self) -> W {
        match self {
            ThroughWriter(w) => w,
            ChunkedWriter(w) => w,
            SizedWriter(w, _) => w
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
                try!(write!(w, "{:X}{}{}", chunk_size, CR as char, LF as char));
                try!(w.write(msg));
                w.write(LINE_ENDING)
            },
            SizedWriter(ref mut w, ref mut remaining) => {
                let len = msg.len();
                if len > *remaining {
                    let len = *remaining;
                    *remaining = 0;
                    try!(w.write(msg.slice_to(len))); // msg[...len]
                    Err(io::standard_error(io::ShortWrite(len)))
                } else {
                    *remaining -= len;
                    w.write(msg)
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
        }
    }
}

pub const SP: u8 = b' ';
pub const CR: u8 = b'\r';
pub const LF: u8 = b'\n';
pub const STAR: u8 = b'*';
pub const LINE_ENDING: &'static [u8] = &[CR, LF];

/// A `Show`able struct to easily write line endings to a formatter.
pub struct LineEnding;

impl fmt::Show for LineEnding {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write(LINE_ENDING)
    }
}

impl AsSlice<u8> for LineEnding {
    fn as_slice(&self) -> &[u8] {
        LINE_ENDING
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

// omg
enum MethodState {
    MsStart,
    MsG,
    MsGE,
    MsGET,
    MsP,
    MsPO,
    MsPOS,
    MsPOST,
    MsPU,
    MsPUT,
    MsPA,
    MsPAT,
    MsPATC,
    MsPATCH,
    MsH,
    MsHE,
    MsHEA,
    MsHEAD,
    MsD,
    MsDE,
    MsDEL,
    MsDELE,
    MsDELET,
    MsDELETE,
    MsT,
    MsTR,
    MsTRA,
    MsTRAC,
    MsTRACE,
    MsO,
    MsOP,
    MsOPT,
    MsOPTI,
    MsOPTIO,
    MsOPTION,
    MsOPTIONS,
    MsC,
    MsCO,
    MsCON,
    MsCONN,
    MsCONNE,
    MsCONNEC,
    MsCONNECT,
    MsExt
}

// omg
impl MethodState {
    fn as_slice(&self) -> &str {
        match *self {
            MsG => "G",
            MsGE => "GE",
            MsGET => "GET",
            MsP => "P",
            MsPO => "PO",
            MsPOS => "POS",
            MsPOST => "POST",
            MsPU => "PU",
            MsPUT => "PUT",
            MsPA => "PA",
            MsPAT => "PAT",
            MsPATC => "PATC",
            MsPATCH => "PATCH",
            MsH => "H",
            MsHE => "HE",
            MsHEA => "HEA",
            MsHEAD => "HEAD",
            MsD => "D",
            MsDE => "DE",
            MsDEL => "DEL",
            MsDELE => "DELE",
            MsDELET => "DELET",
            MsDELETE => "DELETE",
            MsT => "T",
            MsTR => "TR",
            MsTRA => "TRA",
            MsTRAC => "TRAC",
            MsTRACE => "TRACE",
            MsO => "O",
            MsOP => "OP",
            MsOPT => "OPT",
            MsOPTI => "OPTI",
            MsOPTIO => "OPTIO",
            MsOPTION => "OPTION",
            MsOPTIONS => "OPTIONS",
            MsC => "C",
            MsCO => "CO",
            MsCON => "CON",
            MsCONN => "CONN",
            MsCONNE => "CONNE",
            MsCONNEC => "CONNEC",
            MsCONNECT => "CONNECT",
            MsStart | MsExt => unreachable!()
        }
    }
}

/// Read a `Method` from a raw stream, such as `GET`.
pub fn read_method<R: Reader>(stream: &mut R) -> HttpResult<method::Method> {
    let mut s = String::new();
    let mut state = MsStart;

    // omg
    loop {
        match (state, try_io!(stream.read_byte())) {
            (MsStart, b'G') => state = MsG,
            (MsStart, b'P') => state = MsP,
            (MsStart, b'H') => state = MsH,
            (MsStart, b'O') => state = MsO,
            (MsStart, b'T') => state = MsT,
            (MsStart, b'C') => state = MsC,
            (MsStart, b'D') => state = MsD,
            (MsStart, b@b'A'...b'Z') => {
                state = MsExt;
                s.push(b as char)
            },

            (MsG, b'E') => state = MsGE,
            (MsGE, b'T') => state = MsGET,

            (MsP, b'O') => state = MsPO,
            (MsPO, b'S') => state = MsPOS,
            (MsPOS, b'T') => state = MsPOST,

            (MsP, b'U') => state = MsPU,
            (MsPU, b'T') => state = MsPUT,

            (MsP, b'A') => state = MsPA,
            (MsPA, b'T') => state = MsPAT,
            (MsPAT, b'C') => state = MsPATC,
            (MsPATC, b'H') => state = MsPATCH,

            (MsH, b'E') => state = MsHE,
            (MsHE, b'A') => state = MsHEA,
            (MsHEA, b'D') => state = MsHEAD,

            (MsO, b'P') => state = MsOP,
            (MsOP, b'T') => state = MsOPT,
            (MsOPT, b'I') => state = MsOPTI,
            (MsOPTI, b'O') => state = MsOPTIO,
            (MsOPTIO, b'N') => state = MsOPTION,
            (MsOPTION, b'S') => state = MsOPTIONS,

            (MsT, b'R') => state = MsTR,
            (MsTR, b'A') => state = MsTRA,
            (MsTRA, b'C') => state = MsTRAC,
            (MsTRAC, b'E') => state = MsTRACE,

            (MsC, b'O') => state = MsCO,
            (MsCO, b'N') => state = MsCON,
            (MsCON, b'N') => state = MsCONN,
            (MsCONN, b'E') => state = MsCONNE,
            (MsCONNE, b'C') => state = MsCONNEC,
            (MsCONNEC, b'T') => state = MsCONNECT,

            (MsD, b'E') => state = MsDE,
            (MsDE, b'L') => state = MsDEL,
            (MsDEL, b'E') => state = MsDELE,
            (MsDELE, b'T') => state = MsDELET,
            (MsDELET, b'E') => state = MsDELETE,

            (MsExt, b@b'A'...b'Z') => s.push(b as char),

            (_, b@b'A'...b'Z') => {
                s = state.as_slice().to_string();
                s.push(b as char);
            },

            (MsGET, SP) => return Ok(method::Get),
            (MsPOST, SP) => return Ok(method::Post),
            (MsPUT, SP) => return Ok(method::Put),
            (MsPATCH, SP) => return Ok(method::Patch),
            (MsHEAD, SP) => return Ok(method::Head),
            (MsDELETE, SP) => return Ok(method::Delete),
            (MsTRACE, SP) => return Ok(method::Trace),
            (MsOPTIONS, SP) => return Ok(method::Options),
            (MsCONNECT, SP) => return Ok(method::Connect),
            (MsExt, SP) => return Ok(method::Extension(s)),

            (_, _) => return Err(HttpMethodError)
        }
    }
}

/// Read a `RequestUri` from a raw stream.
pub fn read_uri<R: Reader>(stream: &mut R) -> HttpResult<uri::RequestUri> {
    let mut b = try_io!(stream.read_byte());
    while b == SP {
        b = try_io!(stream.read_byte());
    }

    let mut s = String::new();
    if b == STAR {
        try!(expect(stream.read_byte(), SP));
        return Ok(uri::Star)
    } else {
        s.push(b as char);
        loop {
            match try_io!(stream.read_byte()) {
                SP => {
                    break;
                },
                CR | LF => {
                    return Err(HttpUriError)
                },
                b => s.push(b as char)
            }
        }
    }

    if s.as_slice().starts_with("/") {
        Ok(uri::AbsolutePath(s))
    } else if s.as_slice().contains("/") {
        match Url::parse(s.as_slice()) {
            Ok(u) => Ok(uri::AbsoluteUri(u)),
            Err(_e) => {
                debug!("URL err {}", _e);
                Err(HttpUriError)
            }
        }
    } else {
        let mut temp = "http://".to_string();
        temp.push_str(s.as_slice());
        match Url::parse(temp.as_slice()) {
            Ok(_u) => {
                todo!("compare vs u.authority()");
                Ok(uri::Authority(s))
            }
            Err(_e) => {
                debug!("URL err {}", _e);
                Err(HttpUriError)
            }
        }
    }


}


/// Read the `HttpVersion` from a raw stream, such as `HTTP/1.1`.
pub fn read_http_version<R: Reader>(stream: &mut R) -> HttpResult<HttpVersion> {
    try!(expect(stream.read_byte(), b'H'));
    try!(expect(stream.read_byte(), b'T'));
    try!(expect(stream.read_byte(), b'T'));
    try!(expect(stream.read_byte(), b'P'));
    try!(expect(stream.read_byte(), b'/'));

    match try_io!(stream.read_byte()) {
        b'0' => {
            try!(expect(stream.read_byte(), b'.'));
            try!(expect(stream.read_byte(), b'9'));
            Ok(Http09)
        },
        b'1' => {
            try!(expect(stream.read_byte(), b'.'));
            match try_io!(stream.read_byte()) {
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

/// The raw bytes when parsing a header line.
///
/// A String and Vec<u8>, divided by COLON (`:`). The String is guaranteed
/// to be all `token`s. See `is_token_char` source for all valid characters.
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
        match try_io!(stream.read_byte()) {
            CR if name.len() == 0 => {
                match try_io!(stream.read_byte()) {
                    LF => return Ok(None),
                    _ => return Err(HttpHeaderError)
                }
            },
            b':' => break,
            b if is_token(b) => name.push(b as char),
            _nontoken => return Err(HttpHeaderError)
        };
    }

    let mut ows = true; //optional whitespace

    todo!("handle obs-folding (gross!)");
    loop {
        match try_io!(stream.read_byte()) {
            CR => break,
            LF => return Err(HttpHeaderError),
            b' ' if ows => {},
            b => {
                ows = false;
                value.push(b)
            }
        };
    }

    match try_io!(stream.read_byte()) {
        LF => Ok(Some((name, value))),
        _ => Err(HttpHeaderError)
    }

}

/// `request-line   = method SP request-target SP HTTP-version CRLF`
pub type RequestLine = (method::Method, uri::RequestUri, HttpVersion);

/// Read the `RequestLine`, such as `GET / HTTP/1.1`.
pub fn read_request_line<R: Reader>(stream: &mut R) -> HttpResult<RequestLine> {
    let method = try!(read_method(stream));
    let uri = try!(read_uri(stream));
    let version = try!(read_http_version(stream));

    if try_io!(stream.read_byte()) != CR {
        return Err(HttpVersionError);
    }
    if try_io!(stream.read_byte()) != LF {
        return Err(HttpVersionError);
    }

    Ok((method, uri, version))
}

/// `status-line = HTTP-version SP status-code SP reason-phrase CRLF`
///
/// However, reason-phrase is absolutely useless, so its tossed.
pub type StatusLine = (HttpVersion, status::StatusCode);

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
    if try_io!(stream.read_byte()) != SP {
        return Err(HttpVersionError);
    }
    let code = try!(read_status(stream));

    Ok((version, code))
}

/// Read the StatusCode from a stream.
pub fn read_status<R: Reader>(stream: &mut R) -> HttpResult<status::StatusCode> {
    let code = [
        try_io!(stream.read_byte()),
        try_io!(stream.read_byte()),
        try_io!(stream.read_byte()),
    ];

    let code = match u16::parse_bytes(code.as_slice(), 10) {
        Some(num) => match FromPrimitive::from_u16(num) {
            Some(code) => code,
            None => return Err(HttpStatusError)
        },
        None => return Err(HttpStatusError)
    };

    // reason is purely for humans, so just consume it till we get to CRLF
    loop {
        match try_io!(stream.read_byte()) {
            CR => match try_io!(stream.read_byte()) {
                LF => break,
                _ => return Err(HttpStatusError)
            },
            _ => ()
        }
    }

    Ok(code)
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
    use std::io::{mod, MemReader, MemWriter};
    use test::Bencher;
    use uri::{RequestUri, Star, AbsoluteUri, AbsolutePath, Authority};
    use method;
    use status;
    use version::{HttpVersion, Http10, Http11, Http20};
    use {HttpResult, HttpVersionError};
    use url::Url;

    use super::{read_method, read_uri, read_http_version, read_header, RawHeaderLine, read_status};

    fn mem(s: &str) -> MemReader {
        MemReader::new(s.as_bytes().to_vec())
    }
    
    #[test]
    fn test_read_method() {
        fn read(s: &str, m: method::Method) {
            assert_eq!(read_method(&mut mem(s)), Ok(m));
        }

        read("GET /", method::Get);
        read("POST /", method::Post);
        read("PUT /", method::Put);
        read("HEAD /", method::Head);
        read("OPTIONS /", method::Options);
        read("CONNECT /", method::Connect);
        read("TRACE /", method::Trace);
        read("PATCH /", method::Patch);
        read("FOO /", method::Extension("FOO".to_string()));
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
        fn read(s: &str, result: HttpResult<status::StatusCode>) {
            assert_eq!(read_status(&mut mem(s)), result);
        }

        read("200 OK\r\n", Ok(status::Ok));
    }

    #[test]
    fn test_read_header() {
        fn read(s: &str, result: HttpResult<Option<RawHeaderLine>>) {
            assert_eq!(read_header(&mut mem(s)), result);
        }

        read("Host: rust-lang.org\r\n", Ok(Some(("Host".to_string(),
                                                "rust-lang.org".as_bytes().to_vec()))));
    }

    #[test]
    fn test_write_chunked() {
        use std::str::from_utf8;
        let mut w = super::ChunkedWriter(MemWriter::new());
        w.write(b"foo bar").unwrap();
        w.write(b"baz quux herp").unwrap();
        let buf = w.end().unwrap().unwrap();
        let s = from_utf8(buf.as_slice()).unwrap();
        assert_eq!(s, "7\r\nfoo bar\r\nD\r\nbaz quux herp\r\n0\r\n\r\n");
    }

    #[test]
    fn test_write_sized() {
        use std::str::from_utf8;
        let mut w = super::SizedWriter(MemWriter::new(), 8);
        w.write(b"foo bar").unwrap();
        assert_eq!(w.write(b"baz"), Err(io::standard_error(io::ShortWrite(1))));

        let buf = w.end().unwrap().unwrap();
        let s = from_utf8(buf.as_slice()).unwrap();
        assert_eq!(s, "foo barb");
    }

    #[bench]
    fn bench_read_method(b: &mut Bencher) {
        b.bytes = b"CONNECT ".len() as u64;
        b.iter(|| assert_eq!(read_method(&mut mem("CONNECT ")), Ok(method::Connect)));
    }

}
