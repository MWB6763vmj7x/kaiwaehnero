use std::error::Error as StdError;

use bytes::{Buf, Bytes};
use http::{Request, Response, StatusCode};
use tokio_io::{AsyncRead, AsyncWrite};

use crate::body::{Body, Payload};
use crate::body::internal::FullDataArg;
use crate::common::{Future, Never, Poll, Pin, Unpin, task};
use crate::proto::{BodyLength, DecodedLength, Conn, Dispatched, MessageHead, RequestHead, RequestLine, ResponseHead};
use super::Http1Transaction;
use crate::service::Service;

pub(crate) struct Dispatcher<D, Bs: Payload, I, T> {
    conn: Conn<I, Bs::Data, T>,
    dispatch: D,
    body_tx: Option<crate::body::Sender>,
    body_rx: Pin<Box<Option<Bs>>>,
    is_closing: bool,
}

pub(crate) trait Dispatch {
    type PollItem;
    type PollBody;
    type PollError;
    type RecvItem;
    fn poll_msg(&mut self, cx: &mut task::Context<'_>) -> Poll<Option<Result<(Self::PollItem, Self::PollBody), Self::PollError>>>;
    fn recv_msg(&mut self, msg: crate::Result<(Self::RecvItem, Body)>) -> crate::Result<()>;
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), ()>>;
    fn should_poll(&self) -> bool;
}

pub struct Server<S: Service> {
    in_flight: Pin<Box<Option<S::Future>>>,
    pub(crate) service: S,
}

pub struct Client<B> {
    callback: Option<crate::client::dispatch::Callback<Request<B>, Response<Body>>>,
    rx: ClientRx<B>,
}

type ClientRx<B> = crate::client::dispatch::Receiver<Request<B>, Response<Body>>;

impl<D, Bs, I, T> Dispatcher<D, Bs, I, T>
where
    D: Dispatch<PollItem=MessageHead<T::Outgoing>, PollBody=Bs, RecvItem=MessageHead<T::Incoming>> + Unpin,
    D::PollError: Into<Box<dyn StdError + Send + Sync>>,
    I: AsyncRead + AsyncWrite + Unpin,
    T: Http1Transaction + Unpin,
    Bs: Payload,
{
    pub fn new(dispatch: D, conn: Conn<I, Bs::Data, T>) -> Self {
        Dispatcher {
            conn: conn,
            dispatch: dispatch,
            body_tx: None,
            body_rx: Box::pin(None),
            is_closing: false,
        }
    }

    pub fn disable_keep_alive(&mut self) {
        self.conn.disable_keep_alive()
    }

    pub fn into_inner(self) -> (I, Bytes, D) {
        let (io, buf) = self.conn.into_inner();
        (io, buf, self.dispatch)
    }

    /// Run this dispatcher until HTTP says this connection is done,
    /// but don't call `AsyncWrite::shutdown` on the underlying IO.
    ///
    /// This is useful for old-style HTTP upgrades, but ignores
    /// newer-style upgrade API.
    pub(crate) fn poll_without_shutdown(&mut self, cx: &mut task::Context<'_>) -> Poll<crate::Result<()>>
    where
        Self: Unpin,
    {
        Pin::new(self).poll_catch(cx, false).map_ok(|ds| {
            if let Dispatched::Upgrade(pending) = ds {
                pending.manual();
            }
        })
    }

    fn poll_catch(&mut self, cx: &mut task::Context<'_>, should_shutdown: bool) -> Poll<crate::Result<Dispatched>> {
        Poll::Ready(ready!(self.poll_inner(cx, should_shutdown)).or_else(|e| {
            // An error means we're shutting down either way.
            // We just try to give the error to the user,
            // and close the connection with an Ok. If we
            // cannot give it to the user, then return the Err.
            self.dispatch.recv_msg(Err(e))?;
            Ok(Dispatched::Shutdown)
        }))
    }

    fn poll_inner(&mut self, cx: &mut task::Context<'_>, should_shutdown: bool) -> Poll<crate::Result<Dispatched>> {
        T::update_date();

        ready!(self.poll_loop(cx))?;
        loop {
            self.poll_read(cx)?;
            self.poll_write(cx)?;
            self.poll_flush(cx)?;

            // This could happen if reading paused before blocking on IO,
            // such as getting to the end of a framed message, but then
            // writing/flushing set the state back to Init. In that case,
            // if the read buffer still had bytes, we'd want to try poll_read
            // again, or else we wouldn't ever be woken up again.
            //
            // Using this instead of task::current() and notify() inside
            // the Conn is noticeably faster in pipelined benchmarks.
            if !self.conn.wants_read_again() {
                break;
            }
        }

        if self.is_done() {
            if let Some(pending) = self.conn.pending_upgrade() {
                self.conn.take_error()?;
                return Poll::Ready(Ok(Dispatched::Upgrade(pending)));
            } else if should_shutdown {
                ready!(self.conn.poll_shutdown(cx)).map_err(crate::Error::new_shutdown)?;
            }
            self.conn.take_error()?;
            Poll::Ready(Ok(Dispatched::Shutdown))
        } else {
            Poll::Pending
        }
    }

    fn poll_loop(&mut self, cx: &mut task::Context<'_>) -> Poll<crate::Result<()>> {
        // Limit the looping on this connection, in case it is ready far too
        // often, so that other futures don't starve.
        //
        // 16 was chosen arbitrarily, as that is number of pipelined requests
        // benchmarks often use. Perhaps it should be a config option instead.
        for _ in 0..16 {
            self.poll_read(cx)?;
            self.poll_write(cx)?;
            self.poll_flush(cx)?;

            // This could happen if reading paused before blocking on IO,
            // such as getting to the end of a framed message, but then
            // writing/flushing set the state back to Init. In that case,
            // if the read buffer still had bytes, we'd want to try poll_read
            // again, or else we wouldn't ever be woken up again.
            //
            // Using this instead of task::current() and notify() inside
            // the Conn is noticeably faster in pipelined benchmarks.
            if !self.conn.wants_read_again() {
                //break;
                return Poll::Ready(Ok(()));
            }
        }

        trace!("poll_loop yielding (self = {:p})", self);

        task::yield_now(cx).map(|never| match never {})
    }

    fn poll_read(&mut self, cx: &mut task::Context<'_>) -> Poll<crate::Result<()>> {
        loop {
            if self.is_closing {
                return Poll::Ready(Ok(()));
            } else if self.conn.can_read_head() {
                ready!(self.poll_read_head(cx))?;
            } else if let Some(mut body) = self.body_tx.take() {
                if self.conn.can_read_body() {
                    match body.poll_ready(cx) {
                        Poll::Ready(Ok(())) => (),
                        Poll::Pending => {
                            self.body_tx = Some(body);
                            return Poll::Pending;
                        },
                        Poll::Ready(Err(_canceled)) => {
                            // user doesn't care about the body
                            // so we should stop reading
                            trace!("body receiver dropped before eof, closing");
                            self.conn.close_read();
                            return Poll::Ready(Ok(()));
                        }
                    }
                    match self.conn.poll_read_body(cx) {
                        Poll::Ready(Some(Ok(chunk))) => {
                            match body.send_data(chunk) {
                                Ok(()) => {
                                    self.body_tx = Some(body);
                                },
                                Err(_canceled) => {
                                    if self.conn.can_read_body() {
                                        trace!("body receiver dropped before eof, closing");
                                        self.conn.close_read();
                                    }
                                }
                            }
                        },
                        Poll::Ready(None) => {
                            // just drop, the body will close automatically
                        },
                        Poll::Pending => {
                            self.body_tx = Some(body);
                            return Poll::Pending;
                        }
                        Poll::Ready(Some(Err(e))) => {
                            body.send_error(crate::Error::new_body(e));
                        }
                    }
                } else {
                    // just drop, the body will close automatically
                }
            } else {
                return self.conn.poll_read_keep_alive(cx);
            }
        }
    }

    fn poll_read_head(&mut self, cx: &mut task::Context<'_>) -> Poll<crate::Result<()>> {
        // can dispatch receive, or does it still care about, an incoming message?
        match ready!(self.dispatch.poll_ready(cx)) {
            Ok(()) => (),
            Err(()) => {
                trace!("dispatch no longer receiving messages");
                self.close();
                return Poll::Ready(Ok(()));
            }
        }
        // dispatch is ready for a message, try to read one
        match ready!(self.conn.poll_read_head(cx)) {
            Some(Ok((head, body_len, wants_upgrade))) => {
                let mut body = match body_len {
                    DecodedLength::ZERO => Body::empty(),
                    other => {
                        let (tx, rx) = Body::new_channel(other.into_opt());
                        self.body_tx = Some(tx);
                        rx
                    },
                };
                if wants_upgrade {
                    body.set_on_upgrade(self.conn.on_upgrade());
                }
                self.dispatch.recv_msg(Ok((head, body)))?;
                Poll::Ready(Ok(()))
            },
            Some(Err(err)) => {
                debug!("read_head error: {}", err);
                self.dispatch.recv_msg(Err(err))?;
                // if here, the dispatcher gave the user the error
                // somewhere else. we still need to shutdown, but
                // not as a second error.
                Poll::Ready(Ok(()))
            },
            None => {
                // read eof, conn will start to shutdown automatically
                Poll::Ready(Ok(()))
            }
        }
    }

    fn poll_write(&mut self, cx: &mut task::Context<'_>) -> Poll<crate::Result<()>> {
        loop {
            if self.is_closing {
                return Poll::Ready(Ok(()));
            } else if self.body_rx.is_none() && self.conn.can_write_head() && self.dispatch.should_poll() {
                if let Some(msg) = ready!(self.dispatch.poll_msg(cx)) {
                    let (head, mut body) = msg.map_err(crate::Error::new_user_service)?;

                    // Check if the body knows its full data immediately.
                    //
                    // If so, we can skip a bit of bookkeeping that streaming
                    // bodies need to do.
                    if let Some(full) = body.__hyper_full_data(FullDataArg(())).0 {
                        self.conn.write_full_msg(head, full);
                        return Poll::Ready(Ok(()));
                    }
                    let body_type = if body.is_end_stream() {
                        self.body_rx.set(None);
                        None
                    } else {
                        let btype = body.content_length()
                            .map(BodyLength::Known)
                            .or_else(|| Some(BodyLength::Unknown));
                        self.body_rx.set(Some(body));
                        btype
                    };
                    self.conn.write_head(head, body_type);
                } else {
                    self.close();
                    return Poll::Ready(Ok(()));
                }
            } else if !self.conn.can_buffer_body() {
                ready!(self.poll_flush(cx))?;
            } else {
                // A new scope is needed :(
                if let (Some(mut body), clear_body) = OptGuard::new(self.body_rx.as_mut()).guard_mut() {
                    debug_assert!(!*clear_body, "opt guard defaults to keeping body");
                    if !self.conn.can_write_body() {
                        trace!(
                            "no more write body allowed, user body is_end_stream = {}",
                            body.is_end_stream(),
                        );
                        *clear_body = true;
                        continue;
                    }

                    let item = ready!(body.as_mut().poll_data(cx));
                    if let Some(item) = item {
                        let chunk = item.map_err(|e| {
                            *clear_body = true;
                            crate::Error::new_user_body(e)
                        })?;
                        let eos = body.is_end_stream();
                        if eos {
                            *clear_body = true;
                            if chunk.remaining() == 0 {
                                trace!("discarding empty chunk");
                                self.conn.end_body();
                            } else {
                                self.conn.write_body_and_end(chunk);
                            }
                        } else {
                            if chunk.remaining() == 0 {
                                trace!("discarding empty chunk");
                                continue;
                            }
                            self.conn.write_body(chunk);
                        }
                    } else {
                        *clear_body = true;
                        self.conn.end_body();
                    }
                } else {
                    return Poll::Pending;
                }
            }
        }
    }

    fn poll_flush(&mut self, cx: &mut task::Context<'_>) -> Poll<crate::Result<()>> {
        self.conn.poll_flush(cx).map_err(|err| {
            debug!("error writing: {}", err);
            crate::Error::new_body_write(err)
        })
    }

    fn close(&mut self) {
        self.is_closing = true;
        self.conn.close_read();
        self.conn.close_write();
    }

    fn is_done(&self) -> bool {
        if self.is_closing {
            return true;
        }

        let read_done = self.conn.is_read_closed();

        if !T::should_read_first() && read_done {
            // a client that cannot read may was well be done.
            true
        } else {
            let write_done = self.conn.is_write_closed() ||
                (!self.dispatch.should_poll() && self.body_rx.is_none());
            read_done && write_done
        }
    }
}

impl<D, Bs, I, T> Future for Dispatcher<D, Bs, I, T>
where
    D: Dispatch<PollItem=MessageHead<T::Outgoing>, PollBody=Bs, RecvItem=MessageHead<T::Incoming>> + Unpin,
    D::PollError: Into<Box<dyn StdError + Send + Sync>>,
    I: AsyncRead + AsyncWrite + Unpin,
    T: Http1Transaction + Unpin,
    Bs: Payload,
{
    type Output = crate::Result<Dispatched>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        self.poll_catch(cx, true)
    }
}

// ===== impl OptGuard =====

/// A drop guard to allow a mutable borrow of an Option while being able to
/// set whether the `Option` should be cleared on drop.
struct OptGuard<'a, T>(Pin<&'a mut Option<T>>, bool);

impl<'a, T> OptGuard<'a, T> {
    fn new(pin: Pin<&'a mut Option<T>>) -> Self {
        OptGuard(pin, false)
    }

    fn guard_mut(&mut self) -> (Option<Pin<&mut T>>, &mut bool) {
        (self.0.as_mut().as_pin_mut(), &mut self.1)
    }
}

impl<'a, T> Drop for OptGuard<'a, T> {
    fn drop(&mut self) {
        if self.1 {
            self.0.set(None);
        }
    }
}

// ===== impl Server =====

impl<S> Server<S>
where
    S: Service,
{
    pub fn new(service: S) -> Server<S> {
        Server {
            in_flight: Box::pin(None),
            service: service,
        }
    }

    pub fn into_service(self) -> S {
        self.service
    }
}

// Service is never pinned
impl<S: Service> Unpin for Server<S> {}

impl<S, Bs> Dispatch for Server<S>
where
    S: Service<ReqBody=Body, ResBody=Bs>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    Bs: Payload,
{
    type PollItem = MessageHead<StatusCode>;
    type PollBody = Bs;
    type PollError = S::Error;
    type RecvItem = RequestHead;

    fn poll_msg(&mut self, cx: &mut task::Context<'_>) -> Poll<Option<Result<(Self::PollItem, Self::PollBody), Self::PollError>>> {
        let ret = if let Some(ref mut fut) = self.in_flight.as_mut().as_pin_mut() {
            let resp = ready!(fut.as_mut().poll(cx)?);
            let (parts, body) = resp.into_parts();
            let head = MessageHead {
                version: parts.version,
                subject: parts.status,
                headers: parts.headers,
            };
            Poll::Ready(Some(Ok((head, body))))
        } else {
            unreachable!("poll_msg shouldn't be called if no inflight");
        };

        // Since in_flight finished, remove it
        self.in_flight.set(None);
        ret
    }

    fn recv_msg(&mut self, msg: crate::Result<(Self::RecvItem, Body)>) -> crate::Result<()> {
        let (msg, body) = msg?;
        let mut req = Request::new(body);
        *req.method_mut() = msg.subject.0;
        *req.uri_mut() = msg.subject.1;
        *req.headers_mut() = msg.headers;
        *req.version_mut() = msg.version;
        let fut = self.service.call(req);
        self.in_flight.set(Some(fut));
        Ok(())
    }

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), ()>> {
        if self.in_flight.is_some() {
            Poll::Pending
        } else {
            self.service.poll_ready(cx)
                .map_err(|_e| {
                    // FIXME: return error value.
                    trace!("service closed");
                })
        }
    }

    fn should_poll(&self) -> bool {
        self.in_flight.is_some()
    }
}

// ===== impl Client =====


impl<B> Client<B> {
    pub fn new(rx: ClientRx<B>) -> Client<B> {
        Client {
            callback: None,
            rx: rx,
        }
    }
}

impl<B> Dispatch for Client<B>
where
    B: Payload,
{
    type PollItem = RequestHead;
    type PollBody = B;
    type PollError = Never;
    type RecvItem = ResponseHead;

    fn poll_msg(&mut self, cx: &mut task::Context<'_>) -> Poll<Option<Result<(Self::PollItem, Self::PollBody), Never>>> {
        match self.rx.poll_next(cx) {
            Poll::Ready(Some((req, mut cb))) => {
                // check that future hasn't been canceled already
                match cb.poll_cancel(cx) {
                    Poll::Ready(()) => {
                        trace!("request canceled");
                        Poll::Ready(None)
                    },
                    Poll::Pending => {
                        let (parts, body) = req.into_parts();
                        let head = RequestHead {
                            version: parts.version,
                            subject: RequestLine(parts.method, parts.uri),
                            headers: parts.headers,
                        };
                        self.callback = Some(cb);
                        Poll::Ready(Some(Ok((head, body))))
                    }
                }
            },
            Poll::Ready(None) => {
                trace!("client tx closed");
                // user has dropped sender handle
                Poll::Ready(None)
            },
            Poll::Pending => Poll::Pending,
        }
    }

    fn recv_msg(&mut self, msg: crate::Result<(Self::RecvItem, Body)>) -> crate::Result<()> {
        match msg {
            Ok((msg, body)) => {
                if let Some(cb) = self.callback.take() {
                    let mut res = Response::new(body);
                    *res.status_mut() = msg.subject;
                    *res.headers_mut() = msg.headers;
                    *res.version_mut() = msg.version;
                    let _ = cb.send(Ok(res));
                    Ok(())
                } else {
                    // Getting here is likely a bug! An error should have happened
                    // in Conn::require_empty_read() before ever parsing a
                    // full message!
                    Err(crate::Error::new_unexpected_message())
                }
            },
            Err(err) => {
                if let Some(cb) = self.callback.take() {
                    let _ = cb.send(Err((err, None)));
                    Ok(())
                } else {
                    self.rx.close();
                    if let Some((req, cb)) = self.rx.try_recv() {
                        trace!("canceling queued request with connection error: {}", err);
                        // in this case, the message was never even started, so it's safe to tell
                        // the user that the request was completely canceled
                        let _ = cb.send(Err((crate::Error::new_canceled().with(err), Some(req))));
                        Ok(())
                    } else {
                        Err(err)
                    }
                }
            }
        }
    }

    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), ()>> {
        match self.callback {
            Some(ref mut cb) => match cb.poll_cancel(cx) {
                Poll::Ready(()) => {
                    trace!("callback receiver has dropped");
                    Poll::Ready(Err(()))
                },
                Poll::Pending => Poll::Ready(Ok(())),
            },
            None => Poll::Ready(Err(())),
        }
    }

    fn should_poll(&self) -> bool {
        self.callback.is_none()
    }
}

#[cfg(test)]
mod tests {
    // FIXME: re-implement tests with `async/await`, this import should
    // trigger a warning to remind us
    use crate::Error;
    /*
    extern crate pretty_env_logger;

    use super::*;
    use crate::mock::AsyncIo;
    use crate::proto::h1::ClientTransaction;

    #[test]
    fn client_read_bytes_before_writing_request() {
        let _ = pretty_env_logger::try_init();
        ::futures::lazy(|| {
            // Block at 0 for now, but we will release this response before
            // the request is ready to write later...
            let io = AsyncIo::new_buf(b"HTTP/1.1 200 OK\r\n\r\n".to_vec(), 0);
            let (mut tx, rx) = crate::client::dispatch::channel();
            let conn = Conn::<_, crate::Chunk, ClientTransaction>::new(io);
            let mut dispatcher = Dispatcher::new(Client::new(rx), conn);

            // First poll is needed to allow tx to send...
            assert!(dispatcher.poll().expect("nothing is ready").is_not_ready());
            // Unblock our IO, which has a response before we've sent request!
            dispatcher.conn.io_mut().block_in(100);

            let res_rx = tx.try_send(crate::Request::new(crate::Body::empty())).unwrap();

            let a1 = dispatcher.poll().expect("error should be sent on channel");
            assert!(a1.is_ready(), "dispatcher should be closed");
            let err = res_rx.wait()
                .expect("callback poll")
                .expect_err("callback response");

            match (err.0.kind(), err.1) {
                (&crate::error::Kind::Canceled, Some(_)) => (),
                other => panic!("expected Canceled, got {:?}", other),
            }
            Ok::<(), ()>(())
        }).wait().unwrap();
    }

    #[test]
    fn body_empty_chunks_ignored() {
        let _ = pretty_env_logger::try_init();
        ::futures::lazy(|| {
            let io = AsyncIo::new_buf(vec![], 0);
            let (mut tx, rx) = crate::client::dispatch::channel();
            let conn = Conn::<_, crate::Chunk, ClientTransaction>::new(io);
            let mut dispatcher = Dispatcher::new(Client::new(rx), conn);

            // First poll is needed to allow tx to send...
            assert!(dispatcher.poll().expect("nothing is ready").is_not_ready());

            let body = crate::Body::wrap_stream(::futures::stream::once(Ok::<_, crate::Error>("")));

            let _res_rx = tx.try_send(crate::Request::new(body)).unwrap();

            dispatcher.poll().expect("empty body shouldn't panic");
            Ok::<(), ()>(())
        }).wait().unwrap();
    }
    */
}
