//! Client Responses
use std::io::{self, Read};

use header;
use net::NetworkStream;
use http::{self, RawStatus, ResponseHead, HttpMessage};
use status;
use version;
use http::h1::Http11Message;

/// A response for a client request to a remote server.
#[derive(Debug)]
pub struct Response {
    /// The status from the server.
    pub status: status::StatusCode,
    /// The headers from the server.
    pub headers: header::Headers,
    /// The HTTP version of this response from the server.
    pub version: version::HttpVersion,
    status_raw: RawStatus,
    message: Box<HttpMessage>,
}

impl Response {

    /// Creates a new response from a server.
    pub fn new(stream: Box<NetworkStream + Send>) -> ::Result<Response> {
        trace!("Response::new");
        Response::with_message(Box::new(Http11Message::with_stream(stream)))
    }

    /// Creates a new response received from the server on the given `HttpMessage`.
    pub fn with_message(mut message: Box<HttpMessage>) -> ::Result<Response> {
        trace!("Response::with_message");
        let ResponseHead { headers, raw_status, version } = try!(message.get_incoming());
        let status = status::StatusCode::from_u16(raw_status.0);
        debug!("version={:?}, status={:?}", version, status);
        debug!("headers={:?}", headers);

        Ok(Response {
            status: status,
            version: version,
            headers: headers,
            message: message,
            status_raw: raw_status,
        })
    }

    /// Get the raw status code and reason.
    pub fn status_raw(&self) -> &RawStatus {
        &self.status_raw
    }
}

impl Read for Response {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = try!(self.message.read(buf));

        if count == 0 {
            if !http::should_keep_alive(self.version, &self.headers) {
                try!(self.message.close_connection()
                                 .map_err(|_| io::Error::new(io::ErrorKind::Other,
                                                             "Error closing connection")));
            }
        }

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow::Borrowed;
    use std::io::{self, Read};

    use header::Headers;
    use header::TransferEncoding;
    use header::Encoding;
    use http::RawStatus;
    use mock::MockStream;
    use status;
    use version;
    use http::h1::Http11Message;

    use super::Response;

    fn read_to_string(mut r: Response) -> io::Result<String> {
        let mut s = String::new();
        try!(r.read_to_string(&mut s));
        Ok(s)
    }


    #[test]
    fn test_into_inner() {
        let res = Response {
            status: status::StatusCode::Ok,
            headers: Headers::new(),
            version: version::HttpVersion::Http11,
            message: Box::new(Http11Message::with_stream(Box::new(MockStream::new()))),
            status_raw: RawStatus(200, Borrowed("OK")),
        };

        let message = res.message.downcast::<Http11Message>().ok().unwrap();
        let b = message.into_inner().downcast::<MockStream>().ok().unwrap();
        assert_eq!(b, Box::new(MockStream::new()));

    }

    #[test]
    fn test_parse_chunked_response() {
        let stream = MockStream::with_input(b"\
            HTTP/1.1 200 OK\r\n\
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

        let res = Response::new(Box::new(stream)).unwrap();

        // The status line is correct?
        assert_eq!(res.status, status::StatusCode::Ok);
        assert_eq!(res.version, version::HttpVersion::Http11);
        // The header is correct?
        match res.headers.get::<TransferEncoding>() {
            Some(encodings) => {
                assert_eq!(1, encodings.len());
                assert_eq!(Encoding::Chunked, encodings[0]);
            },
            None => panic!("Transfer-Encoding: chunked expected!"),
        };
        // The body is correct?
        assert_eq!(read_to_string(res).unwrap(), "qwert".to_owned());
    }

    /// Tests that when a chunk size is not a valid radix-16 number, an error
    /// is returned.
    #[test]
    fn test_invalid_chunk_size_not_hex_digit() {
        let stream = MockStream::with_input(b"\
            HTTP/1.1 200 OK\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            X\r\n\
            1\r\n\
            0\r\n\
            \r\n"
        );

        let res = Response::new(Box::new(stream)).unwrap();

        assert!(read_to_string(res).is_err());
    }

    /// Tests that when a chunk size contains an invalid extension, an error is
    /// returned.
    #[test]
    fn test_invalid_chunk_size_extension() {
        let stream = MockStream::with_input(b"\
            HTTP/1.1 200 OK\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            1 this is an invalid extension\r\n\
            1\r\n\
            0\r\n\
            \r\n"
        );

        let res = Response::new(Box::new(stream)).unwrap();

        assert!(read_to_string(res).is_err());
    }

    /// Tests that when a valid extension that contains a digit is appended to
    /// the chunk size, the chunk is correctly read.
    #[test]
    fn test_chunk_size_with_extension() {
        let stream = MockStream::with_input(b"\
            HTTP/1.1 200 OK\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            1;this is an extension with a digit 1\r\n\
            1\r\n\
            0\r\n\
            \r\n"
        );

        let res = Response::new(Box::new(stream)).unwrap();

        assert_eq!(read_to_string(res).unwrap(), "1".to_owned());
    }
}
