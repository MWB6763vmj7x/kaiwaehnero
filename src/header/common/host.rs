use header::{Header, HeaderFormat};
use Port;
use std::fmt;
use header::parsing::from_one_raw_str;

/// The `Host` header.
///
/// HTTP/1.1 requires that all requests include a `Host` header, and so hyper
/// client requests add one automatically.
///
/// Currently is just a String, but it should probably become a better type,
/// like url::Host or something.
#[derive(Clone, PartialEq, Show)]
pub struct Host {
    /// The hostname, such a example.domain.
    pub hostname: String,
    /// An optional port number.
    pub port: Option<Port>
}

impl Header for Host {
    fn header_name(_: Option<Host>) -> &'static str {
        "Host"
    }

    fn parse_header(raw: &[Vec<u8>]) -> Option<Host> {
        from_one_raw_str(raw).and_then(|mut s: String| {
            // FIXME: use rust-url to parse this
            // https://github.com/servo/rust-url/issues/42
            let idx = {
                let slice = &s[];
                if slice.char_at(1) == '[' {
                    match slice.rfind(']') {
                        Some(idx) => {
                            if slice.len() > idx + 2 {
                                Some(idx + 1)
                            } else {
                                None
                            }
                        }
                        None => return None // this is a bad ipv6 address...
                    }
                } else {
                    slice.rfind(':')
                }
            };

            let port = match idx {
                Some(idx) => s[].slice_from(idx + 1).parse(),
                None => None
            };

            match idx {
                Some(idx) => s.truncate(idx),
                None => ()
            }

            Some(Host {
                hostname: s,
                port: port
            })
        })
    }
}

impl HeaderFormat for Host {
    fn fmt_header(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self.port {
            None | Some(80) | Some(443) => write!(fmt, "{}", self.hostname),
            Some(port) => write!(fmt, "{}:{}", self.hostname, port)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Host;
    use header::Header;


    #[test]
    fn test_host() {
        let host = Header::parse_header([b"foo.com".to_vec()].as_slice());
        assert_eq!(host, Some(Host {
            hostname: "foo.com".to_string(),
            port: None
        }));


        let host = Header::parse_header([b"foo.com:8080".to_vec()].as_slice());
        assert_eq!(host, Some(Host {
            hostname: "foo.com".to_string(),
            port: Some(8080)
        }));
    }
}

bench_header!(bench, Host, { vec![b"foo.com:3000".to_vec()] });

