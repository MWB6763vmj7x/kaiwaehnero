use std::any::Any;
use std::any::TypeId;
use std::fmt;
use std::str::from_utf8;

use super::cell::{OptCell, PtrMapCell};
use header::{Header, Raw};


#[derive(Clone)]
pub struct Item {
    raw: OptCell<Raw>,
    typed: PtrMapCell<Header + Send + Sync>
}

impl Item {
    #[inline]
    pub fn new_raw(data: Raw) -> Item {
        Item {
            raw: OptCell::new(Some(data)),
            typed: PtrMapCell::new(),
        }
    }

    #[inline]
    pub fn new_typed(ty: Box<Header + Send + Sync>) -> Item {
        let map = PtrMapCell::new();
        unsafe { map.insert((*ty).get_type(), ty); }
        Item {
            raw: OptCell::new(None),
            typed: map,
        }
    }

    #[inline]
    pub fn mut_raw(&mut self) -> &mut Raw {
        self.typed = PtrMapCell::new();
        unsafe {
            self.raw.get_mut()
        }
    }

    pub fn raw(&self) -> &Raw {
        if let Some(ref raw) = *self.raw {
            return raw;
        }

        let raw = unsafe { self.typed.one() }.to_string().into_bytes().into();
        self.raw.set(raw);

        self.raw.as_ref().unwrap()
    }

    pub fn typed<H: Header + Any>(&self) -> Option<&H> {
        let tid = TypeId::of::<H>();
        match self.typed.get(tid) {
            Some(val) => Some(val),
            None => {
                match parse::<H>(self.raw.as_ref().expect("item.raw must exist")) {
                    Ok(typed) => {
                        unsafe { self.typed.insert(tid, typed); }
                        self.typed.get(tid)
                    },
                    Err(_) => None
                }
            }
        }.map(|typed| unsafe { typed.downcast_ref_unchecked() })
    }

    pub fn typed_mut<H: Header>(&mut self) -> Option<&mut H> {
        let tid = TypeId::of::<H>();
        if self.typed.get_mut(tid).is_none() {
            match parse::<H>(self.raw.as_ref().expect("item.raw must exist")) {
                Ok(typed) => {
                    unsafe { self.typed.insert(tid, typed); }
                },
                Err(_) => ()
            }
        }
        if self.raw.is_some() && self.typed.get_mut(tid).is_some() {
            self.raw = OptCell::new(None);
        }
        self.typed.get_mut(tid).map(|typed| unsafe { typed.downcast_mut_unchecked() })
    }
}

#[inline]
fn parse<H: Header>(raw: &Raw) -> ::Result<Box<Header + Send + Sync>> {
    H::parse_header(raw).map(|h| {
        let h: Box<Header + Send + Sync> = Box::new(h);
        h
    })
}

impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self.raw {
            Some(ref raw) => {
                for part in raw.iter() {
                    match from_utf8(&part[..]) {
                        Ok(s) => try!(f.write_str(s)),
                        Err(e) => {
                            error!("raw header value is not utf8. header={:?}, error={:?}",
                                part, e);
                            return Err(fmt::Error);
                        }
                    }
                }
                Ok(())
            },
            None => fmt::Display::fmt(&unsafe { self.typed.one() }, f)
        }
    }
}
