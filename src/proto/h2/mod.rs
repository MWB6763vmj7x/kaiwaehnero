use bytes::Buf;
use futures::{Async, Future, Poll};
use h2::{Reason, SendStream};
use http::HeaderMap;
use http::header::{CONNECTION, TRANSFER_ENCODING};

use ::body::Payload;

mod client;
mod server;

pub(crate) use self::client::Client;
pub(crate) use self::server::Server;

fn strip_connection_headers(headers: &mut HeaderMap) {
    if headers.remove(TRANSFER_ENCODING).is_some() {
        trace!("removed illegal Transfer-Encoding header");
    }
    if headers.contains_key(CONNECTION) {
        warn!("Connection header illegal in HTTP/2");
        //TODO: actually remove it, after checking the value
        //and removing all related headers
    }
}

// body adapters used by both Client and Server

struct PipeToSendStream<S>
where
    S: Payload,
{
    body_tx: SendStream<SendBuf<S::Data>>,
    data_done: bool,
    stream: S,
}

impl<S> PipeToSendStream<S>
where
    S: Payload,
{
    fn new(stream: S, tx: SendStream<SendBuf<S::Data>>) -> PipeToSendStream<S> {
        PipeToSendStream {
            body_tx: tx,
            data_done: false,
            stream: stream,
        }
    }

    fn on_err(&mut self, err: S::Error) -> ::Error {
        let err = ::Error::new_user_body(err);
        trace!("send body user stream error: {}", err);
        self.body_tx.send_reset(Reason::INTERNAL_ERROR);
        err
    }

    fn send_eos_frame(&mut self) -> ::Result<()> {
        trace!("send body eos");
        self.body_tx.send_data(SendBuf(None), true)
            .map_err(::Error::new_body_write)
    }
}

impl<S> Future for PipeToSendStream<S>
where
    S: Payload,
{
    type Item = ();
    type Error = ::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            if !self.data_done {
                // TODO: make use of flow control on SendStream
                // If you're looking at this and thinking of trying to fix this TODO,
                // you may want to look at:
                // https://docs.rs/h2/0.1.*/h2/struct.SendStream.html
                //
                // With that doc open, we'd want to do these things:
                // - check self.body_tx.capacity() to see if we can send *any* data
                // - if > 0:
                // -   poll self.stream
                // -   reserve chunk.len() more capacity (because its about to be used)?
                // -   send the chunk
                // - else:
                // -   try reserve a smallish amount of capacity
                // -   call self.body_tx.poll_capacity(), return if NotReady
                match try_ready!(self.stream.poll_data().map_err(|e| self.on_err(e))) {
                    Some(chunk) => {
                        let is_eos = self.stream.is_end_stream();
                        trace!(
                            "send body chunk: {} bytes, eos={}",
                            chunk.remaining(),
                            is_eos,
                        );

                        let buf = SendBuf(Some(chunk));
                        self.body_tx.send_data(buf, is_eos)
                            .map_err(::Error::new_body_write)?;

                        if is_eos {
                            return Ok(Async::Ready(()))
                        }
                    },
                    None => {
                        let is_eos = self.stream.is_end_stream();
                        if is_eos {
                            return self.send_eos_frame().map(Async::Ready);
                        } else {
                            self.data_done = true;
                            // loop again to poll_trailers
                        }
                    },
                }
            } else {
                match try_ready!(self.stream.poll_trailers().map_err(|e| self.on_err(e))) {
                    Some(trailers) => {
                        self.body_tx.send_trailers(trailers)
                            .map_err(::Error::new_body_write)?;
                        return Ok(Async::Ready(()));
                    },
                    None => {
                        // There were no trailers, so send an empty DATA frame...
                        return self.send_eos_frame().map(Async::Ready);
                    },
                }
            }
        }
    }
}

struct SendBuf<B>(Option<B>);

impl<B: Buf> Buf for SendBuf<B> {
    #[inline]
    fn remaining(&self) -> usize {
        self.0
            .as_ref()
            .map(|b| b.remaining())
            .unwrap_or(0)
    }

    #[inline]
    fn bytes(&self) -> &[u8] {
        self.0
            .as_ref()
            .map(|b| b.bytes())
            .unwrap_or(&[])
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        self.0
            .as_mut()
            .map(|b| b.advance(cnt));
    }
}
