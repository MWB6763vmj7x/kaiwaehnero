//! HTTP Client

use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures::{Async, Future, Poll};
use futures::future::{self, Executor};
use http::{Method, Request, Response, Uri, Version};
use http::header::{Entry, HeaderValue, HOST};
use tokio::reactor::Handle;
pub use tokio_service::Service;

use proto::body::{Body, Entity};
use proto;
use self::pool::Pool;

pub use self::connect::{HttpConnector, Connect};

use self::background::{bg, Background};

pub mod conn;
mod connect;
//TODO(easy): move cancel and dispatch into common instead
pub(crate) mod dispatch;
mod dns;
mod pool;
mod signal;
#[cfg(test)]
mod tests;

/// A Client to make outgoing HTTP requests.
pub struct Client<C, B = proto::Body> {
    connector: Rc<C>,
    executor: Exec,
    h1_writev: bool,
    pool: Pool<PoolClient<B>>,
    retry_canceled_requests: bool,
    set_host: bool,
}

impl Client<HttpConnector, proto::Body> {
    /// Create a new Client with the default config.
    #[inline]
    pub fn new(handle: &Handle) -> Client<HttpConnector, proto::Body> {
        Config::default().build(handle)
    }
}

impl Client<HttpConnector, proto::Body> {
    /// Configure a Client.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # extern crate hyper;
    /// # extern crate tokio_core;
    ///
    /// # fn main() {
    /// # let core = tokio_core::reactor::Core::new().unwrap();
    /// # let handle = core.handle();
    /// let client = hyper::Client::configure()
    ///     .keep_alive(true)
    ///     .build(&handle);
    /// # drop(client);
    /// # }
    /// ```
    #[inline]
    pub fn configure() -> Config<UseDefaultConnector, proto::Body> {
        Config::default()
    }
}

impl<C, B> Client<C, B> {
    // Eventually, a Client won't really care about a tokio Handle, and only
    // the executor used to spawn background tasks. Removing this method is
    // a breaking change, so for now, it's just deprecated.
    #[doc(hidden)]
    #[deprecated]
    pub fn handle(&self) -> &Handle {
        match self.executor {
            Exec::Handle(ref h) => h,
            Exec::Executor(..) => panic!("Client not built with a Handle"),
        }
    }

    #[inline]
    fn configured(config: Config<C, B>, exec: Exec) -> Client<C, B> {
        Client {
            connector: Rc::new(config.connector),
            executor: exec,
            h1_writev: config.h1_writev,
            pool: Pool::new(config.keep_alive, config.keep_alive_timeout),
            retry_canceled_requests: config.retry_canceled_requests,
            set_host: config.set_host,
        }
    }
}

impl<C, B> Client<C, B>
where C: Connect,
      B: Entity<Error=::Error> + 'static,
{

    /// Send a `GET` request to the supplied `Uri`.
    ///
    /// # Note
    ///
    /// This requires that the `Entity` type have a `Default` implementation.
    /// It *should* return an "empty" version of itself, such that
    /// `Entity::is_end_stream` is `true`.
    pub fn get(&self, uri: Uri) -> FutureResponse
    where
        B: Default,
    {
        let body = B::default();
        if !body.is_end_stream() {
            warn!("default Entity used for get() does not return true for is_end_stream");
        }

        let mut req = Request::new(body);
        *req.uri_mut() = uri;
        self.request(req)
    }

    /// Send a constructed Request using this Client.
    pub fn request(&self, mut req: Request<B>) -> FutureResponse {
        // TODO(0.12): do this at construction time.
        //
        // It cannot be done in the constructor because the Client::configured
        // does not have `B: 'static` bounds, which are required to spawn
        // the interval. In 0.12, add a static bounds to the constructor,
        // and move this.
        self.schedule_pool_timer();

        match req.version() {
            Version::HTTP_10 |
            Version::HTTP_11 => (),
            other => {
                error!("Request has unsupported version \"{:?}\"", other);
                return FutureResponse(Box::new(future::err(::Error::Version)));
            }
        }

        if req.method() == &Method::CONNECT {
            debug!("Client does not support CONNECT requests");
            return FutureResponse(Box::new(future::err(::Error::Method)));
        }

        let uri = req.uri().clone();
        let domain = match (uri.scheme_part(), uri.authority_part()) {
            (Some(scheme), Some(auth)) => {
                format!("{}://{}", scheme, auth)
            }
            _ => {
                return FutureResponse(Box::new(future::err(::Error::Io(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid URI for Client Request"
                    )
                ))));
            }
        };

        if self.set_host {
            if let Entry::Vacant(entry) = req.headers_mut().entry(HOST).expect("HOST is always valid header name") {
                let hostname = uri.host().expect("authority implies host");
                let host = if let Some(port) = uri.port() {
                    let s = format!("{}:{}", hostname, port);
                    HeaderValue::from_str(&s)
                } else {
                    HeaderValue::from_str(hostname)
                }.expect("uri host is valid header value");
                entry.insert(host);
            }
        }


        let client = self.clone();
        //TODO: let is_proxy = req.is_proxy();
        let uri = req.uri().clone();
        let fut = RetryableSendRequest {
            client: client,
            future: self.send_request(req, &domain),
            domain: domain,
            //is_proxy: is_proxy,
            uri: uri,
        };
        FutureResponse(Box::new(fut))
    }

    //TODO: replace with `impl Future` when stable
    fn send_request(&self, mut req: Request<B>, domain: &str) -> Box<Future<Item=Response<Body>, Error=ClientError<B>>> {
        let url = req.uri().clone();

        let path = match url.path_and_query() {
            Some(path) => {
                let mut parts = ::http::uri::Parts::default();
                parts.path_and_query = Some(path.clone());
                Uri::from_parts(parts).expect("path is valid uri")
            },
            None => {
                "/".parse().expect("/ is valid path")
            }
        };
        *req.uri_mut() = path;

        let checkout = self.pool.checkout(domain);
        let connect = {
            let executor = self.executor.clone();
            let pool = self.pool.clone();
            let pool_key = Arc::new(domain.to_string());
            let h1_writev = self.h1_writev;
            let connector = self.connector.clone();
            future::lazy(move || {
                connector.connect(url)
                    .from_err()
                    .and_then(move |io| {
                        conn::Builder::new()
                            .h1_writev(h1_writev)
                            .handshake_no_upgrades(io)
                    }).and_then(move |(tx, conn)| {
                        executor.execute(conn.map_err(|e| debug!("client connection error: {}", e)))?;
                        Ok(pool.pooled(pool_key, PoolClient {
                            tx: tx,
                        }))
                    })
            })
        };

        let race = checkout.select(connect)
            .map(|(pooled, _work)| pooled)
            .map_err(|(e, _checkout)| {
                // the Pool Checkout cannot error, so the only error
                // is from the Connector
                // XXX: should wait on the Checkout? Problem is
                // that if the connector is failing, it may be that we
                // never had a pooled stream at all
                ClientError::Normal(e)
            });

        let executor = self.executor.clone();
        let resp = race.and_then(move |mut pooled| {
            let conn_reused = pooled.is_reused();
            let fut = pooled.tx.send_request_retryable(req)
                .map_err(move |(err, orig_req)| {
                    if let Some(req) = orig_req {
                        ClientError::Canceled {
                            connection_reused: conn_reused,
                            reason: err,
                            req: req,
                        }
                    } else {
                        ClientError::Normal(err)
                    }
                })
                .map(move |res| {
                    // when pooled is dropped, it will try to insert back into the
                    // pool. To delay that, spawn a future that completes once the
                    // sender is ready again.
                    //
                    // This *should* only be once the related `Connection` has polled
                    // for a new request to start.
                    //
                    // It won't be ready if there is a body to stream.
                    if let Ok(Async::NotReady) = pooled.tx.poll_ready() {
                        // If the executor doesn't have room, oh well. Things will likely
                        // be blowing up soon, but this specific task isn't required.
                        let _ = executor.execute(future::poll_fn(move || {
                            pooled.tx.poll_ready().map_err(|_| ())
                        }));
                    }

                    res
                });


            fut
        });

        Box::new(resp)
    }

    fn schedule_pool_timer(&self) {
        if let Exec::Handle(ref h) = self.executor {
            self.pool.spawn_expired_interval(h);
        }
    }
}

impl<C, B> Service for Client<C, B>
where C: Connect,
      B: Entity<Error=::Error> + 'static,
{
    type Request = Request<B>;
    type Response = Response<Body>;
    type Error = ::Error;
    type Future = FutureResponse;

    fn call(&self, req: Self::Request) -> Self::Future {
        self.request(req)
    }
}

impl<C, B> Clone for Client<C, B> {
    fn clone(&self) -> Client<C, B> {
        Client {
            connector: self.connector.clone(),
            executor: self.executor.clone(),
            h1_writev: self.h1_writev,
            pool: self.pool.clone(),
            retry_canceled_requests: self.retry_canceled_requests,
            set_host: self.set_host,
        }
    }
}

impl<C, B> fmt::Debug for Client<C, B> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Client")
            .finish()
    }
}

/// A `Future` that will resolve to an HTTP Response.
#[must_use = "futures do nothing unless polled"]
pub struct FutureResponse(Box<Future<Item=Response<Body>, Error=::Error> + 'static>);

impl fmt::Debug for FutureResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad("Future<Response>")
    }
}

impl Future for FutureResponse {
    type Item = Response<Body>;
    type Error = ::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.0.poll()
    }
}

struct RetryableSendRequest<C, B> {
    client: Client<C, B>,
    domain: String,
    future: Box<Future<Item=Response<Body>, Error=ClientError<B>>>,
    //is_proxy: bool,
    uri: Uri,
}

impl<C, B> Future for RetryableSendRequest<C, B>
where
    C: Connect,
    B: Entity<Error=::Error> + 'static,
{
    type Item = Response<Body>;
    type Error = ::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.future.poll() {
                Ok(Async::Ready(resp)) => return Ok(Async::Ready(resp)),
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Err(ClientError::Normal(err)) => return Err(err),
                Err(ClientError::Canceled {
                    connection_reused,
                    mut req,
                    reason,
                }) => {
                    if !self.client.retry_canceled_requests || !connection_reused {
                        // if client disabled, don't retry
                        // a fresh connection means we definitely can't retry
                        return Err(reason);
                    }

                    trace!("unstarted request canceled, trying again (reason={:?})", reason);
                    *req.uri_mut() = self.uri.clone();
                    self.future = self.client.send_request(req, &self.domain);
                }
            }
        }
    }
}

struct PoolClient<B> {
    tx: conn::SendRequest<B>,
}

impl<B> self::pool::Closed for PoolClient<B>
where
    B: 'static,
{
    fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

pub(crate) enum ClientError<B> {
    Normal(::Error),
    Canceled {
        connection_reused: bool,
        req: Request<B>,
        reason: ::Error,
    }
}

/// Configuration for a Client
pub struct Config<C, B> {
    _body_type: PhantomData<B>,
    //connect_timeout: Duration,
    connector: C,
    keep_alive: bool,
    keep_alive_timeout: Option<Duration>,
    h1_writev: bool,
    //TODO: make use of max_idle config
    max_idle: usize,
    retry_canceled_requests: bool,
    set_host: bool,
}

/// Phantom type used to signal that `Config` should create a `HttpConnector`.
#[derive(Debug, Clone, Copy)]
pub struct UseDefaultConnector(());

impl Default for Config<UseDefaultConnector, proto::Body> {
    fn default() -> Config<UseDefaultConnector, proto::Body> {
        Config {
            _body_type: PhantomData::<proto::Body>,
            connector: UseDefaultConnector(()),
            keep_alive: true,
            keep_alive_timeout: Some(Duration::from_secs(90)),
            h1_writev: true,
            max_idle: 5,
            retry_canceled_requests: true,
            set_host: true,
        }
    }
}

impl<C, B> Config<C, B> {
    /// Set the body stream to be used by the `Client`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use hyper::client::Config;
    /// let cfg = Config::default()
    ///     .body::<hyper::Body>();
    /// # drop(cfg);
    #[inline]
    pub fn body<BB>(self) -> Config<C, BB> {
        Config {
            _body_type: PhantomData::<BB>,
            connector: self.connector,
            keep_alive: self.keep_alive,
            keep_alive_timeout: self.keep_alive_timeout,
            h1_writev: self.h1_writev,
            max_idle: self.max_idle,
            retry_canceled_requests: self.retry_canceled_requests,
            set_host: self.set_host,
        }
    }

    /// Set the `Connect` type to be used.
    #[inline]
    pub fn connector<CC>(self, val: CC) -> Config<CC, B> {
        Config {
            _body_type: self._body_type,
            connector: val,
            keep_alive: self.keep_alive,
            keep_alive_timeout: self.keep_alive_timeout,
            h1_writev: self.h1_writev,
            max_idle: self.max_idle,
            retry_canceled_requests: self.retry_canceled_requests,
            set_host: self.set_host,
        }
    }

    /// Enable or disable keep-alive mechanics.
    ///
    /// Default is enabled.
    #[inline]
    pub fn keep_alive(mut self, val: bool) -> Config<C, B> {
        self.keep_alive = val;
        self
    }

    /// Set an optional timeout for idle sockets being kept-alive.
    ///
    /// Pass `None` to disable timeout.
    ///
    /// Default is 90 seconds.
    #[inline]
    pub fn keep_alive_timeout(mut self, val: Option<Duration>) -> Config<C, B> {
        self.keep_alive_timeout = val;
        self
    }

    /// Set whether HTTP/1 connections should try to use vectored writes,
    /// or always flatten into a single buffer.
    ///
    /// Note that setting this to false may mean more copies of body data,
    /// but may also improve performance when an IO transport doesn't
    /// support vectored writes well, such as most TLS implementations.
    ///
    /// Default is `true`.
    #[inline]
    pub fn http1_writev(mut self, val: bool) -> Config<C, B> {
        self.h1_writev = val;
        self
    }

    /// Set whether to retry requests that get disrupted before ever starting
    /// to write.
    ///
    /// This means a request that is queued, and gets given an idle, reused
    /// connection, and then encounters an error immediately as the idle
    /// connection was found to be unusable.
    ///
    /// When this is set to `false`, the related `FutureResponse` would instead
    /// resolve to an `Error::Cancel`.
    ///
    /// Default is `true`.
    #[inline]
    pub fn retry_canceled_requests(mut self, val: bool) -> Config<C, B> {
        self.retry_canceled_requests = val;
        self
    }

    /// Set whether to automatically add the `Host` header to requests.
    ///
    /// If true, and a request does not include a `Host` header, one will be
    /// added automatically, derived from the authority of the `Uri`.
    ///
    /// Default is `true`.
    #[inline]
    pub fn set_host(mut self, val: bool) -> Config<C, B> {
        self.set_host = val;
        self
    }
}

impl<C, B> Config<C, B>
where C: Connect,
      B: Entity<Error=::Error>,
{
    /// Construct the Client with this configuration.
    #[inline]
    pub fn build(self, handle: &Handle) -> Client<C, B> {
        Client::configured(self, Exec::Handle(handle.clone()))
    }

    /// Construct a Client with this configuration and an executor.
    ///
    /// The executor will be used to spawn "background" connection tasks
    /// to drive requests and responses.
    pub fn executor<E>(self, executor: E) -> Client<C, B>
    where
        E: Executor<Background> + 'static,
    {
        Client::configured(self, Exec::Executor(Rc::new(executor)))
    }
}

impl<B> Config<UseDefaultConnector, B>
where B: Entity<Error=::Error>,
{
    /// Construct the Client with this configuration.
    #[inline]
    pub fn build(self, handle: &Handle) -> Client<HttpConnector, B> {
        let mut connector = HttpConnector::new(4, handle);
        if self.keep_alive {
            connector.set_keepalive(self.keep_alive_timeout);
        }
        self.connector(connector).build(handle)
    }
}

impl<C, B> fmt::Debug for Config<C, B> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Config")
            .field("keep_alive", &self.keep_alive)
            .field("keep_alive_timeout", &self.keep_alive_timeout)
            .field("http1_writev", &self.h1_writev)
            .field("max_idle", &self.max_idle)
            .field("set_host", &self.set_host)
            .finish()
    }
}

impl<C: Clone, B> Clone for Config<C, B> {
    fn clone(&self) -> Config<C, B> {
        Config {
            connector: self.connector.clone(),
            .. *self
        }
    }
}


// ===== impl Exec =====

#[derive(Clone)]
enum Exec {
    Handle(Handle),
    Executor(Rc<Executor<Background>>),
}


impl Exec {
    fn execute<F>(&self, fut: F) -> io::Result<()>
    where
        F: Future<Item=(), Error=()> + 'static,
    {
        match *self {
            Exec::Handle(ref h) => h.spawn(fut),
            Exec::Executor(ref e) => {
                e.execute(bg(Box::new(fut)))
                    .map_err(|err| {
                        debug!("executor error: {:?}", err.kind());
                        io::Error::new(
                            io::ErrorKind::Other,
                            "executor error",
                        )
                    })?
            },
        }
        Ok(())
    }
}

// ===== impl Background =====

// The types inside this module are not exported out of the crate,
// so they are in essence un-nameable.
mod background {
    use futures::{Future, Poll};

    // This is basically `impl Future`, since the type is un-nameable,
    // and only implementeds `Future`.
    #[allow(missing_debug_implementations)]
    pub struct Background {
        inner: Box<Future<Item=(), Error=()>>,
    }

    pub fn bg(fut: Box<Future<Item=(), Error=()>>) -> Background {
        Background {
            inner: fut,
        }
    }

    impl Future for Background {
        type Item = ();
        type Error = ();

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            self.inner.poll()
        }
    }
}

