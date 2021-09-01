use header::Header;
use std::fmt::{mod, Show};
use super::from_one_raw_str;
use mime::Mime;

/// The `Content-Type` header.
///
/// Used to describe the MIME type of message body. Can be used with both
/// requests and responses.
#[deriving(Clone, PartialEq, Show)]
pub struct ContentType(pub Mime);

impl Header for ContentType {
    fn header_name(_: Option<ContentType>) -> &'static str {
        "Content-Type"
    }

    fn parse_header(raw: &[Vec<u8>]) -> Option<ContentType> {
        from_one_raw_str(raw).map(|mime| ContentType(mime))
    }

    fn fmt_header(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let ContentType(ref value) = *self;
        value.fmt(fmt)
    }
}

