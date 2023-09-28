//! Server Requests
//!
//! These are requests that a `hyper::Server` receives, and include its method,
//! target URI, headers, and message body.
use std::io::{self, Read};
use std::net::SocketAddr;

use {HttpResult};
use version::{HttpVersion};
use method::Method::{self, Get, Head};
use header::{Headers, ContentLength, TransferEncoding};
use http::{read_request_line};
use http::HttpReader;
use http::HttpReader::{SizedReader, ChunkedReader, EmptyReader};
use uri::RequestUri;

/// A request bundles several parts of an incoming `NetworkStream`, given to a `Handler`.
pub struct Request<'a> {
    /// The IP address of the remote connection.
    pub remote_addr: SocketAddr,
    /// The `Method`, such as `Get`, `Post`, etc.
    pub method: Method,
    /// The headers of the incoming request.
    pub headers: Headers,
    /// The target request-uri for this request.
    pub uri: RequestUri,
    /// The version of HTTP for this request.
    pub version: HttpVersion,
    body: HttpReader<&'a mut (Read + 'a)>
}


impl<'a> Request<'a> {
    /// Create a new Request, reading the StartLine and Headers so they are
    /// immediately useful.
    pub fn new(mut stream: &'a mut (Read + 'a), addr: SocketAddr) -> HttpResult<Request<'a>> {
        let (method, uri, version) = try!(read_request_line(&mut stream));
        debug!("Request Line: {:?} {:?} {:?}", method, uri, version);
        let headers = try!(Headers::from_raw(&mut stream));
        debug!("{:?}", headers);

        let body = if method == Get || method == Head {
            EmptyReader(stream)
        } else if headers.has::<ContentLength>() {
            match headers.get::<ContentLength>() {
                Some(&ContentLength(len)) => SizedReader(stream, len),
                None => unreachable!()
            }
        } else if headers.has::<TransferEncoding>() {
            todo!("check for Transfer-Encoding: chunked");
            ChunkedReader(stream, None)
        } else {
            EmptyReader(stream)
        };

        Ok(Request {
            remote_addr: addr,
            method: method,
            uri: uri,
            headers: headers,
            version: version,
            body: body
        })
    }

    /// Deconstruct a Request into its constituent parts.
    pub fn deconstruct(self) -> (SocketAddr, Method, Headers,
                                 RequestUri, HttpVersion,
                                 HttpReader<&'a mut (Read + 'a)>,) {
        (self.remote_addr, self.method, self.headers,
         self.uri, self.version, self.body)
    }
}

impl<'a> Read for Request<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.body.read(buf)
    }
}

#[cfg(test)]
mod tests {
    use header::{Host, TransferEncoding, Encoding};
    use mock::MockStream;
    use super::Request;

    use std::io::{self, Read};
    use std::net::SocketAddr;

    fn sock(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn read_to_string(mut req: Request) -> io::Result<String> {
        let mut s = String::new();
        try!(req.read_to_string(&mut s));
        Ok(s)
    }

    #[test]
    fn test_get_empty_body() {
        let mut stream = MockStream::with_input(b"\
            GET / HTTP/1.1\r\n\
            Host: example.domain\r\n\
            \r\n\
            I'm a bad request.\r\n\
        ");

        let req = Request::new(&mut stream, sock("127.0.0.1:80")).unwrap();
        assert_eq!(read_to_string(req), Ok("".to_string()));
    }

    #[test]
    fn test_head_empty_body() {
        let mut stream = MockStream::with_input(b"\
            HEAD / HTTP/1.1\r\n\
            Host: example.domain\r\n\
            \r\n\
            I'm a bad request.\r\n\
        ");

        let req = Request::new(&mut stream, sock("127.0.0.1:80")).unwrap();
        assert_eq!(read_to_string(req), Ok("".to_string()));
    }

    #[test]
    fn test_post_empty_body() {
        let mut stream = MockStream::with_input(b"\
            POST / HTTP/1.1\r\n\
            Host: example.domain\r\n\
            \r\n\
            I'm a bad request.\r\n\
        ");

        let req = Request::new(&mut stream, sock("127.0.0.1:80")).unwrap();
        assert_eq!(read_to_string(req), Ok("".to_string()));
    }

    #[test]
    fn test_parse_chunked_request() {
        let mut stream = MockStream::with_input(b"\
            POST / HTTP/1.1\r\n\
            Host: example.domain\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            1\r\n\
            q\r\n\
            2\r\n\
            we\r\n\
            2\r\n\
            rt\r\n\
            0\r\n\
            \r\n"
        );

        let req = Request::new(&mut stream, sock("127.0.0.1:80")).unwrap();

        // The headers are correct?
        match req.headers.get::<Host>() {
            Some(host) => {
                assert_eq!("example.domain", host.hostname);
            },
            None => panic!("Host header expected!"),
        };
        match req.headers.get::<TransferEncoding>() {
            Some(encodings) => {
                assert_eq!(1, encodings.len());
                assert_eq!(Encoding::Chunked, encodings[0]);
            }
            None => panic!("Transfer-Encoding: chunked expected!"),
        };
        // The content is correctly read?
        assert_eq!(read_to_string(req), Ok("qwert".to_string()));
    }

    /// Tests that when a chunk size is not a valid radix-16 number, an error
    /// is returned.
    #[test]
    fn test_invalid_chunk_size_not_hex_digit() {
        let mut stream = MockStream::with_input(b"\
            POST / HTTP/1.1\r\n\
            Host: example.domain\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            X\r\n\
            1\r\n\
            0\r\n\
            \r\n"
        );

        let req = Request::new(&mut stream, sock("127.0.0.1:80")).unwrap();

        assert!(read_to_string(req).is_err());
    }

    /// Tests that when a chunk size contains an invalid extension, an error is
    /// returned.
    #[test]
    fn test_invalid_chunk_size_extension() {
        let mut stream = MockStream::with_input(b"\
            POST / HTTP/1.1\r\n\
            Host: example.domain\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            1 this is an invalid extension\r\n\
            1\r\n\
            0\r\n\
            \r\n"
        );

        let req = Request::new(&mut stream, sock("127.0.0.1:80")).unwrap();

        assert!(read_to_string(req).is_err());
    }

    /// Tests that when a valid extension that contains a digit is appended to
    /// the chunk size, the chunk is correctly read.
    #[test]
    fn test_chunk_size_with_extension() {
        let mut stream = MockStream::with_input(b"\
            POST / HTTP/1.1\r\n\
            Host: example.domain\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            1;this is an extension with a digit 1\r\n\
            1\r\n\
            0\r\n\
            \r\n"
        );

        let req = Request::new(&mut stream, sock("127.0.0.1:80")).unwrap();

        assert_eq!(read_to_string(req), Ok("1".to_string()));
    }

}
