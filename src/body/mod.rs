//! Streaming bodies for Requests and Responses
//!
//! For both [Clients](::client) and [Servers](::server), requests and
//! responses use streaming bodies, instead of complete buffering. This
//! allows applications to not use memory they don't need, and allows exerting
//! back-pressure on connections by only reading when asked.
//!
//! There are two pieces to this in hyper:
//!
//! - The [`Payload`](body::Payload) trait the describes all possible bodies. hyper
//!   allows any body type that implements `Payload`, allowing applications to
//!   have fine-grained control over their streaming.
//! - The [`Body`](Body) concrete type, which is an implementation of `Payload`,
//!  and returned by hyper as a "receive stream" (so, for server requests and
//!  client responses). It is also a decent default implementation if you don't
//!  have very custom needs of your send streams.

#[doc(hidden)]
pub use http_body::Body as HttpBody;

pub use self::body::{Body, Sender};
pub use self::chunk::Chunk;
pub use self::payload::Payload;

mod body;
mod chunk;
mod payload;

/// An optimization to try to take a full body if immediately available.
///
/// This is currently limited to *only* `hyper::Body`s.
pub(crate) fn take_full_data<T: Payload + 'static>(body: &mut T) -> Option<T::Data> {
    use std::any::{Any, TypeId};

    // This static type check can be optimized at compile-time.
    if TypeId::of::<T>() == TypeId::of::<Body>() {
        let mut full = (body as &mut dyn Any)
            .downcast_mut::<Body>()
            .expect("must be Body")
            .take_full_data();
        // This second cast is required to make the type system happy.
        // Without it, the compiler cannot reason that the type is actually
        // `T::Data`. Oh wells.
        //
        // It's still a measurable win!
        (&mut full as &mut dyn Any)
            .downcast_mut::<Option<T::Data>>()
            .expect("must be T::Data")
            .take()
    } else {
        None
    }
}

fn _assert_send_sync() {
    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}

    _assert_send::<Body>();
    _assert_send::<Chunk>();
    _assert_sync::<Chunk>();
}

