//! A Collection of Header implementations for common HTTP Headers.
//!
//! ## Mime
//!
//! Several header fields use MIME values for their contents. Keeping with the
//! strongly-typed theme, the [mime](http://seanmonstar.github.io/mime.rs) crate
//! is used, such as `ContentType(pub Mime)`.

pub use self::access_control::*;
pub use self::accept::Accept;
pub use self::accept_charset::AcceptCharset;
pub use self::accept_encoding::AcceptEncoding;
pub use self::accept_language::AcceptLanguage;
pub use self::allow::Allow;
pub use self::authorization::{Authorization, Scheme, Basic};
pub use self::cache_control::{CacheControl, CacheDirective};
pub use self::connection::{Connection, ConnectionOption};
pub use self::content_length::ContentLength;
pub use self::content_encoding::ContentEncoding;
pub use self::content_type::ContentType;
pub use self::cookie::Cookie;
pub use self::date::Date;
pub use self::etag::Etag;
pub use self::expect::Expect;
pub use self::expires::Expires;
pub use self::host::Host;
pub use self::if_match::IfMatch;
pub use self::if_modified_since::IfModifiedSince;
pub use self::if_none_match::IfNoneMatch;
pub use self::if_unmodified_since::IfUnmodifiedSince;
pub use self::last_modified::LastModified;
pub use self::location::Location;
pub use self::pragma::Pragma;
pub use self::referer::Referer;
pub use self::server::Server;
pub use self::set_cookie::SetCookie;
pub use self::transfer_encoding::TransferEncoding;
pub use self::upgrade::{Upgrade, Protocol};
pub use self::user_agent::UserAgent;
pub use self::vary::Vary;

#[macro_export]
macro_rules! bench_header(
    ($name:ident, $ty:ty, $value:expr) => {
        #[cfg(test)]
        mod $name {
            use test::Bencher;
            use super::*;

            use header::{Header, HeaderFormatter};

            #[bench]
            fn bench_parse(b: &mut Bencher) {
                let val = $value;
                b.iter(|| {
                    let _: $ty = Header::parse_header(&val[..]).unwrap();
                });
            }

            #[bench]
            fn bench_format(b: &mut Bencher) {
                let val: $ty = Header::parse_header(&$value[..]).unwrap();
                let fmt = HeaderFormatter(&val);
                b.iter(|| {
                    format!("{}", fmt);
                });
            }
        }
    }
);

#[macro_export]
macro_rules! deref(
    ($from:ty => $to:ty) => {
        impl ::std::ops::Deref for $from {
            type Target = $to;

            fn deref<'a>(&'a self) -> &'a $to {
                &self.0
            }
        }

        impl ::std::ops::DerefMut for $from {
            fn deref_mut<'a>(&'a mut self) -> &'a mut $to {
                &mut self.0
            }
        }
    }
);

#[macro_export]
macro_rules! header {
    // $a:meta: Attributes associated with the header item (usually docs)
    // $id:ident: Identifier of the header
    // $n:expr: Lowercase name of the header
    // $nn:expr: Nice name of the header

    // List header, zero or more items
    ($(#[$a:meta])*($id:ident, $n:expr) => ($item:ty)*) => {
        $(#[$a])*
        #[derive(Clone, Debug, PartialEq)]
        pub struct $id(pub Vec<$item>);
        deref!($id => Vec<$item>);
        impl $crate::header::Header for $id {
            fn header_name() -> &'static str {
                $n
            }
            fn parse_header(raw: &[Vec<u8>]) -> Option<Self> {
                $crate::header::parsing::from_comma_delimited(raw).map($id)
            }
        }
        impl $crate::header::HeaderFormat for $id {
            fn fmt_header(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                $crate::header::parsing::fmt_comma_delimited(f, &self.0[..])
            }
        }
        impl ::std::fmt::Display for $id {
            fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                use $crate::header::HeaderFormat;
                self.fmt_header(f)
            }
        }

    };
    // List header, one or more items
    ($(#[$a:meta])*($id:ident, $n:expr) => ($item:ty)+) => {
        $(#[$a])*
        #[derive(Clone, Debug, PartialEq)]
        pub struct $id(pub Vec<$item>);
        deref!($id => Vec<$item>);
        impl $crate::header::Header for $id {
            fn header_name() -> &'static str {
                $n
            }
            fn parse_header(raw: &[Vec<u8>]) -> Option<Self> {
                $crate::header::parsing::from_comma_delimited(raw).map($id)
            }
        }
        impl $crate::header::HeaderFormat for $id {
            fn fmt_header(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                $crate::header::parsing::fmt_comma_delimited(f, &self.0[..])
            }
        }
        impl ::std::fmt::Display for $id {
            fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                use $crate::header::HeaderFormat;
                self.fmt_header(f)
            }
        }
    };
    // Single value header
    ($(#[$a:meta])*($id:ident, $n:expr) => [$value:ty]) => {
        $(#[$a])*
        #[derive(Clone, Debug, PartialEq)]
        pub struct $id(pub $value);
        deref!($id => $value);
        impl $crate::header::Header for $id {
            fn header_name() -> &'static str {
                $n
            }
            fn parse_header(raw: &[Vec<u8>]) -> Option<Self> {
                $crate::header::parsing::from_one_raw_str(raw).map($id)
            }
        }
        impl $crate::header::HeaderFormat for $id {
            fn fmt_header(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                ::std::fmt::Display::fmt(&**self, f)
            }
        }
        impl ::std::fmt::Display for $id {
            fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                ::std::fmt::Display::fmt(&**self, f)
            }
        }
    };
    // List header, one or more items with "*" option
    ($(#[$a:meta])*($id:ident, $n:expr) => {Any / ($item:ty)+}) => {
        $(#[$a])*
        #[derive(Clone, Debug, PartialEq)]
        pub enum $id {
            /// Any value is a match
            Any,
            /// Only the listed items are a match
            Items(Vec<$item>),
        }
        impl $crate::header::Header for $id {
            fn header_name() -> &'static str {
                $n
            }
            fn parse_header(raw: &[Vec<u8>]) -> Option<Self> {
                // FIXME: Return None if no item is in $id::Only
                if raw.len() == 1 {
                    if raw[0] == b"*" {
                        return Some($id::Any)
                    } else if raw[0] == b"" {
                        return None
                    }
                }
                $crate::header::parsing::from_comma_delimited(raw).map(|vec| $id::Items(vec))
            }
        }
        impl $crate::header::HeaderFormat for $id {
            fn fmt_header(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                match *self {
                    $id::Any => write!(f, "*"),
                    $id::Items(ref fields) => $crate::header::parsing::fmt_comma_delimited(f, &fields[..])
                }
            }
        }
        impl ::std::fmt::Display for $id {
            fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                use $crate::header::HeaderFormat;
                self.fmt_header(f)
            }
        }
    };
}

mod access_control;
mod accept;
mod accept_charset;
mod accept_encoding;
mod accept_language;
mod allow;
mod authorization;
mod cache_control;
mod cookie;
mod connection;
mod content_encoding;
mod content_length;
mod content_type;
mod date;
mod etag;
mod expect;
mod expires;
mod host;
mod if_match;
mod last_modified;
mod if_modified_since;
mod if_none_match;
mod if_unmodified_since;
mod location;
mod pragma;
mod referer;
mod server;
mod set_cookie;
mod transfer_encoding;
mod upgrade;
mod user_agent;
mod vary;
