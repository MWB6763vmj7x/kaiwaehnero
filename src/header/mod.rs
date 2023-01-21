//! Headers container, and common header fields.
//!
//! hyper has the opinion that Headers should be strongly-typed, because that's
//! why we're using Rust in the first place. To set or get any header, an object
//! must implement the `Header` trait from this module. Several common headers
//! are already provided, such as `Host`, `ContentType`, `UserAgent`, and others.
use std::any::Any;
use std::ascii::AsciiExt;
use std::borrow::Cow::{Borrowed, Owned};
use std::fmt::{self, Show};
use std::intrinsics::TypeId;
use std::raw::TraitObject;
use std::str::{FromStr, from_utf8};
use std::string::CowString;
use std::collections::HashMap;
use std::collections::hash_map::{Iter, Entry};
use std::iter::FromIterator;
use std::borrow::IntoCow;
use std::{hash, mem, raw};

use mucell::MuCell;
use uany::{UnsafeAnyExt};

use http::{self, LineEnding};
use {HttpResult};

pub use self::common::*;
pub use self::shared::*;

/// Common Headers
pub mod common;

pub mod shared;

/// A trait for any object that will represent a header field and value.
///
/// This trait represents the construction and identification of headers,
/// and contains trait-object unsafe methods.
pub trait Header: Clone + Any + Send + Sync {
    /// Returns the name of the header field this belongs to.
    ///
    /// The market `Option` is to hint to the type system which implementation
    /// to call. This can be done away with once UFCS arrives.
    fn header_name(marker: Option<Self>) -> &'static str;
    /// Parse a header from a raw stream of bytes.
    ///
    /// It's possible that a request can include a header field more than once,
    /// and in that case, the slice will have a length greater than 1. However,
    /// it's not necessarily the case that a Header is *allowed* to have more
    /// than one field value. If that's the case, you **should** return `None`
    /// if `raw.len() > 1`.
    fn parse_header(raw: &[Vec<u8>]) -> Option<Self>;

}

/// A trait for any object that will represent a header field and value.
///
/// This trait represents the formatting of a Header for output to a TcpStream.
pub trait HeaderFormat: HeaderClone + Any + Send + Sync {
    /// Format a header to be output into a TcpStream.
    ///
    /// This method is not allowed to introduce an Err not produced
    /// by the passed-in Formatter.
    fn fmt_header(&self, fmt: &mut fmt::Formatter) -> fmt::Result;

}

#[doc(hidden)]
pub trait HeaderClone {
    fn clone_box(&self) -> Box<HeaderFormat + Sync + Send>;
}

impl<T: HeaderFormat + Send + Sync + Clone> HeaderClone for T {
    #[inline]
    fn clone_box(&self) -> Box<HeaderFormat + Sync + Send> {
        box self.clone()
    }
}

impl HeaderFormat {
    #[inline]
    fn is<T: 'static>(&self) -> bool {
        self.get_type_id() == TypeId::of::<T>()
    }
}

impl UnsafeAnyExt for HeaderFormat {
    #[inline]
    unsafe fn downcast_ref_unchecked<T: 'static>(&self) -> &T {
        mem::transmute(mem::transmute::<&HeaderFormat, raw::TraitObject>(self).data)
    }

    #[inline]
    unsafe fn downcast_mut_unchecked<T: 'static>(&mut self) -> &mut T {
        mem::transmute(mem::transmute::<&mut HeaderFormat, raw::TraitObject>(self).data)
    }

    #[inline]
    unsafe fn downcast_unchecked<T: 'static>(self: Box<HeaderFormat>) -> Box<T> {
        mem::transmute(mem::transmute::<Box<HeaderFormat>, raw::TraitObject>(self).data)
    }
}

impl Clone for Box<HeaderFormat + Send + Sync> {
    #[inline]
    fn clone(&self) -> Box<HeaderFormat + Send + Sync> {
        self.clone_box()
    }
}

fn header_name<T: Header>() -> &'static str {
    let name = Header::header_name(None::<T>);
    name
}

/// A map of header fields on requests and responses.
#[derive(Clone)]
pub struct Headers {
    data: HashMap<CaseInsensitive, MuCell<Item>>
}

impl Headers {

    /// Creates a new, empty headers map.
    pub fn new() -> Headers {
        Headers {
            data: HashMap::new()
        }
    }

    #[doc(hidden)]
    pub fn from_raw<R: Reader>(rdr: &mut R) -> HttpResult<Headers> {
        let mut headers = Headers::new();
        loop {
            match try!(http::read_header(rdr)) {
                Some((name, value)) => {
                    debug!("raw header: {:?}={:?}", name, &value[]);
                    let name = CaseInsensitive(Owned(name));
                    let mut item = match headers.data.entry(name) {
                        Entry::Vacant(entry) => entry.insert(MuCell::new(Item::raw(vec![]))),
                        Entry::Occupied(entry) => entry.into_mut()
                    };

                    match &mut item.borrow_mut().raw {
                        &mut Some(ref mut raw) => raw.push(value),
                        // Unreachable
                        _ => {}
                    };
                },
                None => break,
            }
        }
        Ok(headers)
    }

    /// Set a header field to the corresponding value.
    ///
    /// The field is determined by the type of the value being set.
    pub fn set<H: Header + HeaderFormat>(&mut self, value: H) {
        self.data.insert(CaseInsensitive(Borrowed(header_name::<H>())),
                         MuCell::new(Item::typed(box value as Box<HeaderFormat + Send + Sync>)));
    }

    /// Access the raw value of a header.
    ///
    /// Prefer to use the typed getters instead.
    ///
    /// Example:
    ///
    /// ```
    /// # use hyper::header::Headers;
    /// # let mut headers = Headers::new();
    /// let raw_content_type = headers.get_raw("content-type");
    /// ```
    pub fn get_raw(&self, name: &str) -> Option<&[Vec<u8>]> {
        self.data
            // FIXME(reem): Find a better way to do this lookup without find_equiv.
            .get(&CaseInsensitive(Borrowed(unsafe { mem::transmute::<&str, &str>(name) })))
            .and_then(|item| {
                if let Some(ref raw) = item.borrow().raw {
                    return unsafe { mem::transmute(Some(&raw[])) };
                }

                let worked = item.try_mutate(|item| {
                    let raw = vec![item.typed.as_ref().unwrap().to_string().into_bytes()];
                    item.raw = Some(raw);
                });
                debug_assert!(worked, "item.try_mutate should return true");

                let item = item.borrow();
                let raw = item.raw.as_ref().unwrap();
                unsafe { mem::transmute(Some(&raw[])) }
            })
    }

    /// Set the raw value of a header, bypassing any typed headers.
    ///
    /// Example:
    ///
    /// ```
    /// # use hyper::header::Headers;
    /// # let mut headers = Headers::new();
    /// headers.set_raw("content-length", vec![b"5".to_vec()]);
    /// ```
    pub fn set_raw<K: IntoCow<'static, String, str>>(&mut self, name: K, value: Vec<Vec<u8>>) {
        self.data.insert(CaseInsensitive(name.into_cow()), MuCell::new(Item::raw(value)));
    }

    /// Get a reference to the header field's value, if it exists.
    pub fn get<H: Header + HeaderFormat>(&self) -> Option<&H> {
        self.get_or_parse::<H>().map(|item| {
            unsafe {
                mem::transmute::<&H, &H>(downcast(&*item.borrow()))
            }
        })
    }

    /// Get a mutable reference to the header field's value, if it exists.
    pub fn get_mut<H: Header + HeaderFormat>(&mut self) -> Option<&mut H> {
        self.get_or_parse_mut::<H>().map(|item| {
            unsafe { downcast_mut(item.borrow_mut()) }
        })
    }

    fn get_or_parse<H: Header + HeaderFormat>(&self) -> Option<&MuCell<Item>> {
        self.data.get(&CaseInsensitive(Borrowed(header_name::<H>()))).and_then(get_or_parse::<H>)
    }

    fn get_or_parse_mut<H: Header + HeaderFormat>(&mut self) -> Option<&mut MuCell<Item>> {
        self.data.get_mut(&CaseInsensitive(Borrowed(header_name::<H>()))).and_then(get_or_parse_mut::<H>)
    }

    /// Returns a boolean of whether a certain header is in the map.
    ///
    /// Example:
    ///
    /// ```
    /// # use hyper::header::Headers;
    /// # use hyper::header::common::ContentType;
    /// # let mut headers = Headers::new();
    /// let has_type = headers.has::<ContentType>();
    /// ```
    pub fn has<H: Header + HeaderFormat>(&self) -> bool {
        self.data.contains_key(&CaseInsensitive(Borrowed(header_name::<H>())))
    }

    /// Removes a header from the map, if one existed.
    /// Returns true if a header has been removed.
    pub fn remove<H: Header + HeaderFormat>(&mut self) -> bool {
        self.data.remove(&CaseInsensitive(Borrowed(Header::header_name(None::<H>)))).is_some()
    }

    /// Returns an iterator over the header fields.
    pub fn iter<'a>(&'a self) -> HeadersItems<'a> {
        HeadersItems {
            inner: self.data.iter()
        }
    }

    /// Returns the number of headers in the map.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Remove all headers from the map.
    pub fn clear(&mut self) {
        self.data.clear()
    }
}

impl fmt::String for Headers {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        for header in self.iter() {
            try!(write!(fmt, "{}{}", header, LineEnding));
        }
        Ok(())
    }
}

impl fmt::Show for Headers {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        self.to_string().fmt(fmt)
    }
}

/// An `Iterator` over the fields in a `Headers` map.
pub struct HeadersItems<'a> {
    inner: Iter<'a, CaseInsensitive, MuCell<Item>>
}

impl<'a> Iterator for HeadersItems<'a> {
    type Item = HeaderView<'a>;

    fn next(&mut self) -> Option<HeaderView<'a>> {
        match self.inner.next() {
            Some((k, v)) => Some(HeaderView(k, v)),
            None => None
        }
    }
}

/// Returned with the `HeadersItems` iterator.
pub struct HeaderView<'a>(&'a CaseInsensitive, &'a MuCell<Item>);

impl<'a> HeaderView<'a> {
    /// Check if a HeaderView is a certain Header.
    #[inline]
    pub fn is<H: Header>(&self) -> bool {
        CaseInsensitive(header_name::<H>().into_cow()) == *self.0
    }

    /// Get the Header name as a slice.
    #[inline]
    pub fn name(&self) -> &'a str {
        self.0.as_slice()
    }

    /// Cast the value to a certain Header type.
    #[inline]
    pub fn value<H: Header + HeaderFormat>(&self) -> Option<&'a H> {
        get_or_parse::<H>(self.1).map(|item| {
            unsafe {
                mem::transmute::<&H, &H>(downcast(&*item.borrow()))
            }
        })
    }

    /// Get just the header value as a String.
    #[inline]
    pub fn value_string(&self) -> String {
        (*self.1.borrow()).to_string()
    }
}

impl<'a> fmt::String for HeaderView<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.0, *self.1.borrow())
    }
}

impl<'a> fmt::Show for HeaderView<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        self.to_string().fmt(fmt)
    }
}

impl<'a> Extend<HeaderView<'a>> for Headers {
    fn extend<I: Iterator<Item=HeaderView<'a>>>(&mut self, mut iter: I) {
        for header in iter {
            self.data.insert((*header.0).clone(), (*header.1).clone());
        }
    }
}

impl<'a> FromIterator<HeaderView<'a>> for Headers {
    fn from_iter<I: Iterator<Item=HeaderView<'a>>>(iter: I) -> Headers {
        let mut headers = Headers::new();
        headers.extend(iter);
        headers
    }
}

#[derive(Clone)]
struct Item {
    raw: Option<Vec<Vec<u8>>>,
    typed: Option<Box<HeaderFormat + Send + Sync>>
}

impl Item {
    fn raw(data: Vec<Vec<u8>>) -> Item {
        Item {
            raw: Some(data),
            typed: None,
        }
    }

    fn typed(ty: Box<HeaderFormat + Send + Sync>) -> Item {
        Item {
            raw: None,
            typed: Some(ty),
        }
    }

}

fn get_or_parse<H: Header + HeaderFormat>(item: &MuCell<Item>) -> Option<&MuCell<Item>> {
    match item.borrow().typed {
        Some(ref typed) if typed.is::<H>() => return Some(item),
        Some(ref typed) => {
            warn!("attempted to access {:?} as wrong type", typed);
            return None;
        }
        _ => ()
    }

    let worked = item.try_mutate(parse::<H>);
    debug_assert!(worked, "item.try_mutate should return true");
    if item.borrow().typed.is_some() {
        Some(item)
    } else {
        None
    }
}

fn get_or_parse_mut<H: Header + HeaderFormat>(item: &mut MuCell<Item>) -> Option<&mut MuCell<Item>> {
    let is_correct_type = match item.borrow().typed {
        Some(ref typed) if typed.is::<H>() => Some(true),
        Some(ref typed) => {
            warn!("attempted to access {:?} as wrong type", typed);
            Some(false)
        }
        _ => None
    };

    match is_correct_type {
        Some(true) => return Some(item),
        Some(false) => return None,
        None => ()
    }

    parse::<H>(item.borrow_mut());
    if item.borrow().typed.is_some() {
        Some(item)
    } else {
        None
    }
}

fn parse<H: Header + HeaderFormat>(item: &mut Item) {
    item.typed = match item.raw {
        Some(ref raw) => match Header::parse_header(&raw[]) {
            Some::<H>(h) => Some(box h as Box<HeaderFormat + Send + Sync>),
            None => None
        },
        None => unreachable!()
    };
}

unsafe fn downcast<H: Header + HeaderFormat>(item: &Item) -> &H {
    item.typed.as_ref().expect("item.typed must be set").downcast_ref_unchecked()
}

unsafe fn downcast_mut<H: Header + HeaderFormat>(item: &mut Item) -> &mut H {
    item.typed.as_mut().expect("item.typed must be set").downcast_mut_unchecked()
}

impl fmt::String for Item {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self.typed {
            Some(ref h) => h.fmt_header(fmt),
            None => match self.raw {
                Some(ref raw) => {
                    for part in raw.iter() {
                        match from_utf8(&part[]) {
                            Ok(s) => try!(fmt.write_str(s)),
                            Err(e) => {
                                error!("raw header value is not utf8. header={:?}, error={:?}", &part[], e);
                                return Err(fmt::Error);
                            }
                        }
                    }
                    Ok(())
                },
                None => unreachable!()
            }
        }
    }
}

impl<'a> fmt::Show for Item {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        self.to_string().fmt(fmt)
    }
}

impl fmt::String for Box<HeaderFormat + Send + Sync> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        (**self).fmt_header(fmt)
    }
}

impl fmt::Show for Box<HeaderFormat + Send + Sync> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        self.to_string().fmt(fmt)
    }
}

/// Case-insensitive string.
pub struct CaseInsensitive(CowString<'static>);

impl FromStr for CaseInsensitive {
    fn from_str(s: &str) -> Option<CaseInsensitive> {
        Some(CaseInsensitive(Owned(s.to_string())))
    }
}

impl Clone for CaseInsensitive {
    fn clone(&self) -> CaseInsensitive {
        CaseInsensitive(self.0.clone().into_cow())
    }
}

impl Str for CaseInsensitive {
    fn as_slice(&self) -> &str {
        let CaseInsensitive(ref s) = *self;
        s.as_slice()
    }

}

impl fmt::String for CaseInsensitive {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "{}", self.as_slice())
    }
}

impl fmt::Show for CaseInsensitive {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        self.to_string().fmt(fmt)
    }
}

impl PartialEq for CaseInsensitive {
    fn eq(&self, other: &CaseInsensitive) -> bool {
        self.as_slice().eq_ignore_ascii_case(other.as_slice())
    }
}

impl Eq for CaseInsensitive {}

impl<H: hash::Writer + hash::Hasher> hash::Hash<H> for CaseInsensitive {
    #[inline]
    fn hash(&self, hasher: &mut H) {
        for b in self.as_slice().bytes() {
            hasher.write(&[b.to_ascii_lowercase()])
        }
    }
}

/// A wrapper around any Header with a Show impl that calls fmt_header.
///
/// This can be used like so: `format!("{}", HeaderFormatter(&header))` to
/// get the representation of a Header which will be written to an
/// outgoing TcpStream.
pub struct HeaderFormatter<'a, H: HeaderFormat>(pub &'a H);

impl<'a, H: HeaderFormat> fmt::String for HeaderFormatter<'a, H> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt_header(f)
    }
}

impl<'a, H: HeaderFormat> Show for HeaderFormatter<'a, H> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt_header(f)
    }
}

#[cfg(test)]
mod tests {
    use std::io::MemReader;
    use std::fmt;
    use std::borrow::Cow::Borrowed;
    use std::hash::{SipHasher, hash};
    use mime::Mime;
    use mime::TopLevel::Text;
    use mime::SubLevel::Plain;
    use super::CaseInsensitive;
    use super::{Headers, Header, HeaderFormat};
    use super::common::{ContentLength, ContentType, Accept, Host};
    use super::shared::{QualityItem};

    use test::Bencher;

    fn mem(s: &str) -> MemReader {
        MemReader::new(s.as_bytes().to_vec())
    }

    #[test]
    fn test_case_insensitive() {
        let a = CaseInsensitive(Borrowed("foobar"));
        let b = CaseInsensitive(Borrowed("FOOBAR"));

        assert_eq!(a, b);
        assert_eq!(hash::<_, SipHasher>(&a), hash::<_, SipHasher>(&b));
    }

    #[test]
    fn test_from_raw() {
        let headers = Headers::from_raw(&mut mem("Content-Length: 10\r\n\r\n")).unwrap();
        assert_eq!(headers.get(), Some(&ContentLength(10)));
    }

    #[test]
    fn test_content_type() {
        let content_type = Header::parse_header([b"text/plain".to_vec()].as_slice());
        assert_eq!(content_type, Some(ContentType(Mime(Text, Plain, vec![]))));
    }

    #[test]
    fn test_accept() {
        let text_plain = QualityItem{item: Mime(Text, Plain, vec![]), quality: 1f32};
        let application_vendor = "application/vnd.github.v3.full+json; q=0.5".parse().unwrap();

        let accept = Header::parse_header([b"text/plain".to_vec()].as_slice());
        assert_eq!(accept, Some(Accept(vec![text_plain.clone()])));

        let accept = Header::parse_header([b"application/vnd.github.v3.full+json; q=0.5, text/plain".to_vec()].as_slice());
        assert_eq!(accept, Some(Accept(vec![application_vendor, text_plain])));
    }

    #[derive(Clone, Show)]
    struct CrazyLength(Option<bool>, usize);

    impl Header for CrazyLength {
        fn header_name(_: Option<CrazyLength>) -> &'static str {
            "content-length"
        }
        fn parse_header(raw: &[Vec<u8>]) -> Option<CrazyLength> {
            use std::str::from_utf8;
            use std::str::FromStr;

            if raw.len() != 1 {
                return None;
            }
            // we JUST checked that raw.len() == 1, so raw[0] WILL exist.
            match from_utf8(unsafe { &raw[].get_unchecked(0)[] }) {
                Ok(s) => FromStr::from_str(s),
                Err(_) => None
            }.map(|u| CrazyLength(Some(false), u))
        }
    }

    impl HeaderFormat for CrazyLength {
        fn fmt_header(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
            let CrazyLength(ref opt, ref value) = *self;
            write!(fmt, "{:?}, {:?}", opt, value)
        }
    }

    #[test]
    fn test_different_structs_for_same_header() {
        let headers = Headers::from_raw(&mut mem("Content-Length: 10\r\n\r\n")).unwrap();
        let ContentLength(_) = *headers.get::<ContentLength>().unwrap();
        assert!(headers.get::<CrazyLength>().is_none());
    }

    #[test]
    fn test_trailing_whitespace() {
        let headers = Headers::from_raw(&mut mem("Content-Length: 10   \r\n\r\n")).unwrap();
        let ContentLength(_) = *headers.get::<ContentLength>().unwrap();
        assert!(headers.get::<CrazyLength>().is_none());
    }

    #[test]
    fn test_multiple_reads() {
        let headers = Headers::from_raw(&mut mem("Content-Length: 10\r\n\r\n")).unwrap();
        let ContentLength(one) = *headers.get::<ContentLength>().unwrap();
        let ContentLength(two) = *headers.get::<ContentLength>().unwrap();
        assert_eq!(one, two);
    }

    #[test]
    fn test_different_reads() {
        let headers = Headers::from_raw(&mut mem("Content-Length: 10\r\nContent-Type: text/plain\r\n\r\n")).unwrap();
        let ContentLength(_) = *headers.get::<ContentLength>().unwrap();
        let ContentType(_) = *headers.get::<ContentType>().unwrap();
    }

    #[test]
    fn test_get_mutable() {
        let mut headers = Headers::from_raw(&mut mem("Content-Length: 10\r\nContent-Type: text/plain\r\n\r\n")).unwrap();
        *headers.get_mut::<ContentLength>().unwrap() = ContentLength(20);
        assert_eq!(*headers.get::<ContentLength>().unwrap(), ContentLength(20));
    }

    #[test]
    fn test_headers_show() {
        let mut headers = Headers::new();
        headers.set(ContentLength(15));
        headers.set(Host { hostname: "foo.bar".to_string(), port: None });

        let s = headers.to_string();
        // hashmap's iterators have arbitrary order, so we must sort first
        let mut pieces = s[].split_str("\r\n").collect::<Vec<&str>>();
        pieces.sort();
        let s = pieces.into_iter().rev().collect::<Vec<&str>>().connect("\r\n");
        assert_eq!(&s[], "Host: foo.bar\r\nContent-Length: 15\r\n");
    }

    #[test]
    fn test_headers_show_raw() {
        let headers = Headers::from_raw(&mut mem("Content-Length: 10\r\n\r\n")).unwrap();
        let s = headers.to_string();
        assert_eq!(s, "Content-Length: 10\r\n");
    }

    #[test]
    fn test_set_raw() {
        let mut headers = Headers::new();
        headers.set(ContentLength(10));
        headers.set_raw("content-LENGTH", vec![b"20".to_vec()]);
        assert_eq!(headers.get_raw("Content-length").unwrap(), &[b"20".to_vec()][]);
        assert_eq!(headers.get(), Some(&ContentLength(20)));
    }

    #[test]
    fn test_len() {
        let mut headers = Headers::new();
        headers.set(ContentLength(10));
        assert_eq!(headers.len(), 1);
        headers.set(ContentType(Mime(Text, Plain, vec![])));
        assert_eq!(headers.len(), 2);
        // Redundant, should not increase count.
        headers.set(ContentLength(20));
        assert_eq!(headers.len(), 2);
    }

    #[test]
    fn test_clear() {
        let mut headers = Headers::new();
        headers.set(ContentLength(10));
        headers.set(ContentType(Mime(Text, Plain, vec![])));
        assert_eq!(headers.len(), 2);
        headers.clear();
        assert_eq!(headers.len(), 0);
    }

    #[test]
    fn test_iter() {
        let mut headers = Headers::new();
        headers.set(ContentLength(11));
        for header in headers.iter() {
            assert!(header.is::<ContentLength>());
            assert_eq!(header.name(), Header::header_name(None::<ContentLength>));
            assert_eq!(header.value(), Some(&ContentLength(11)));
            assert_eq!(header.value_string(), "11".to_string());
        }
    }

    #[bench]
    fn bench_header_get(b: &mut Bencher) {
        let mut headers = Headers::new();
        headers.set(ContentLength(11));
        b.iter(|| assert_eq!(headers.get::<ContentLength>(), Some(&ContentLength(11))))
    }

    #[bench]
    fn bench_header_get_miss(b: &mut Bencher) {
        let headers = Headers::new();
        b.iter(|| assert!(headers.get::<ContentLength>().is_none()))
    }

    #[bench]
    fn bench_header_set(b: &mut Bencher) {
        let mut headers = Headers::new();
        b.iter(|| headers.set(ContentLength(12)))
    }

    #[bench]
    fn bench_header_has(b: &mut Bencher) {
        let mut headers = Headers::new();
        headers.set(ContentLength(11));
        b.iter(|| assert!(headers.has::<ContentLength>()))
    }

    #[bench]
    fn bench_header_view_is(b: &mut Bencher) {
        let mut headers = Headers::new();
        headers.set(ContentLength(11));
        let mut iter = headers.iter();
        let view = iter.next().unwrap();
        b.iter(|| assert!(view.is::<ContentLength>()))
    }
}
