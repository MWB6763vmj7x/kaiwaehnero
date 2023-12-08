//! Provides an Encoding enum.

use std::fmt;
use std::str;

pub use self::Encoding::{Chunked, Gzip, Deflate, Compress, Identity, EncodingExt};

/// A value to represent an encoding used in `Transfer-Encoding`
/// or `Accept-Encoding` header.
#[derive(Clone, PartialEq, Debug)]
pub enum Encoding {
    /// The `chunked` encoding.
    Chunked,
    /// The `gzip` encoding.
    Gzip,
    /// The `deflate` encoding.
    Deflate,
    /// The `compress` encoding.
    Compress,
    /// The `identity` encoding.
    Identity,
    /// Some other encoding that is less common, can be any String.
    EncodingExt(String)
}

impl fmt::Display for Encoding {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str(match *self {
            Chunked => "chunked",
            Gzip => "gzip",
            Deflate => "deflate",
            Compress => "compress",
            Identity => "identity",
            EncodingExt(ref s) => s.as_ref()
        })
    }
}

impl str::FromStr for Encoding {
    type Err = ();
    fn from_str(s: &str) -> Result<Encoding, ()> {
        match s {
            "chunked" => Ok(Chunked),
            "deflate" => Ok(Deflate),
            "gzip" => Ok(Gzip),
            "compress" => Ok(Compress),
            "identity" => Ok(Identity),
            _ => Ok(EncodingExt(s.to_string()))
        }
    }
}
