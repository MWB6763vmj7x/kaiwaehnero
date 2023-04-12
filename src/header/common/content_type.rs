use header::{Header, HeaderFormat};
use std::fmt;
use header::parsing::from_one_raw_str;
use mime::Mime;

/// The `Content-Type` header.
///
/// Used to describe the MIME type of message body. Can be used with both
/// requests and responses.
#[derive(Clone, PartialEq, Show)]
pub struct ContentType(pub Mime);

deref!(ContentType => Mime);

impl Header for ContentType {
    fn header_name() -> &'static str {
        "Content-Type"
    }

    fn parse_header(raw: &[Vec<u8>]) -> Option<ContentType> {
        from_one_raw_str(raw).map(|mime| ContentType(mime))
    }
}

impl HeaderFormat for ContentType {
    fn fmt_header(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::String::fmt(&self.0, fmt)
    }
}

bench_header!(bench, ContentType, { vec![b"application/json; charset=utf-8".to_vec()] });

