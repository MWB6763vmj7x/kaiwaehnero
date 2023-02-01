//! A collection of traits abstracting over Listeners and Streams.
use std::any::Any;
use std::fmt;
use std::intrinsics::TypeId;
use std::io::{IoResult, IoError, ConnectionAborted, InvalidInput, OtherIoError,
              Stream, Listener, Acceptor};
use std::io::net::ip::{SocketAddr, ToSocketAddr, Port};
use std::io::net::tcp::{TcpStream, TcpListener, TcpAcceptor};
use std::mem;
use std::raw::{self, TraitObject};

use uany::UnsafeAnyExt;
use openssl::ssl::{Ssl, SslStream, SslContext, VerifyCallback};
use openssl::ssl::SslVerifyMode::SslVerifyPeer;
use openssl::ssl::SslMethod::Sslv23;
use openssl::ssl::error::{SslError, StreamError, OpenSslErrors, SslSessionClosed};

/// The write-status indicating headers have not been written.
#[allow(missing_copy_implementations)]
pub struct Fresh;

/// The write-status indicating headers have been written.
#[allow(missing_copy_implementations)]
pub struct Streaming;

/// An abstraction to listen for connections on a certain port.
pub trait NetworkListener {
    type Acceptor: NetworkAcceptor;
    /// Listens on a socket.
    fn listen<To: ToSocketAddr>(&mut self, addr: To) -> IoResult<Self::Acceptor>;
}

/// An abstraction to receive `NetworkStream`s.
pub trait NetworkAcceptor: Clone + Send {
    type Stream: NetworkStream + Send + Clone;

    /// Returns an iterator of streams.
    fn accept(&mut self) -> IoResult<Self::Stream>;

    /// Get the address this Listener ended up listening on.
    fn socket_name(&self) -> IoResult<SocketAddr>;

    /// Closes the Acceptor, so no more incoming connections will be handled.
    fn close(&mut self) -> IoResult<()>;

    /// Returns an iterator over incoming connections.
    fn incoming(&mut self) -> NetworkConnections<Self> {
        NetworkConnections(self)
    }
}

/// An iterator wrapper over a NetworkAcceptor.
pub struct NetworkConnections<'a, N: NetworkAcceptor>(&'a mut N);

impl<'a, N: NetworkAcceptor> Iterator for NetworkConnections<'a, N> {
    type Item = IoResult<N::Stream>;
    fn next(&mut self) -> Option<IoResult<N::Stream>> {
        Some(self.0.accept())
    }
}


/// An abstraction over streams that a Server can utilize.
pub trait NetworkStream: Stream + Any + StreamClone + Send {
    /// Get the remote address of the underlying connection.
    fn peer_name(&mut self) -> IoResult<SocketAddr>;
}


#[doc(hidden)]
pub trait StreamClone {
    fn clone_box(&self) -> Box<NetworkStream + Send>;
}

impl<T: NetworkStream + Send + Clone> StreamClone for T {
    #[inline]
    fn clone_box(&self) -> Box<NetworkStream + Send> {
        Box::new(self.clone())
    }
}

/// A connector creates a NetworkStream.
pub trait NetworkConnector {
    type Stream: NetworkStream + Send;
    /// Connect to a remote address.
    fn connect(&mut self, host: &str, port: Port, scheme: &str) -> IoResult<Self::Stream>;
}

impl fmt::Show for Box<NetworkStream + Send> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.pad("Box<NetworkStream>")
    }
}

impl Clone for Box<NetworkStream + Send> {
    #[inline]
    fn clone(&self) -> Box<NetworkStream + Send> { self.clone_box() }
}

impl Reader for Box<NetworkStream + Send> {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> { (**self).read(buf) }
}

impl Writer for Box<NetworkStream + Send> {
    #[inline]
    fn write(&mut self, msg: &[u8]) -> IoResult<()> { (**self).write(msg) }

    #[inline]
    fn flush(&mut self) -> IoResult<()> { (**self).flush() }
}

impl<'a> Reader for &'a mut NetworkStream {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> { (**self).read(buf) }
}

impl<'a> Writer for &'a mut NetworkStream {
    #[inline]
    fn write(&mut self, msg: &[u8]) -> IoResult<()> { (**self).write(msg) }

    #[inline]
    fn flush(&mut self) -> IoResult<()> { (**self).flush() }
}

impl UnsafeAnyExt for NetworkStream {
    unsafe fn downcast_ref_unchecked<T: 'static>(&self) -> &T {
        mem::transmute(mem::transmute::<&NetworkStream,
                                        raw::TraitObject>(self).data)
    }

    unsafe fn downcast_mut_unchecked<T: 'static>(&mut self) -> &mut T {
        mem::transmute(mem::transmute::<&mut NetworkStream,
                                        raw::TraitObject>(self).data)
    }

    unsafe fn downcast_unchecked<T: 'static>(self: Box<NetworkStream>) -> Box<T>  {
        mem::transmute(mem::transmute::<Box<NetworkStream>,
                                        raw::TraitObject>(self).data)
    }
}

impl NetworkStream {
    /// Is the underlying type in this trait object a T?
    #[inline]
    pub fn is<T: 'static>(&self) -> bool {
        self.get_type_id() == TypeId::of::<T>()
    }

    /// If the underlying type is T, get a reference to the contained data.
    #[inline]
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        if self.is::<T>() {
            Some(unsafe { self.downcast_ref_unchecked() })
        } else {
            None
        }
    }

    /// If the underlying type is T, get a mutable reference to the contained
    /// data.
    #[inline]
    pub fn downcast_mut<T: 'static>(&mut self) -> Option<&mut T> {
        if self.is::<T>() {
            Some(unsafe { self.downcast_mut_unchecked() })
        } else {
            None
        }
    }

    /// If the underlying type is T, extract it.
    pub fn downcast<T: 'static>(self: Box<NetworkStream>)
            -> Result<Box<T>, Box<NetworkStream>> {
        if self.is::<T>() {
            Ok(unsafe { self.downcast_unchecked() })
        } else {
            Err(self)
        }
    }
}

/// A `NetworkListener` for `HttpStream`s.
#[allow(missing_copy_implementations)]
pub enum HttpListener {
    /// Http variant.
    Http,
    /// Https variant.
    Https,
}

impl NetworkListener for HttpListener {
    type Acceptor = HttpAcceptor;

    #[inline]
    fn listen<To: ToSocketAddr>(&mut self, addr: To) -> IoResult<HttpAcceptor> {
        let mut tcp = try!(TcpListener::bind(addr));
        let addr = try!(tcp.socket_name());
        Ok(match *self {
            HttpListener::Http => HttpAcceptor::Http(try!(tcp.listen()), addr),
            HttpListener::Https => unimplemented!(),
        })
    }
}

/// A `NetworkAcceptor` for `HttpStream`s.
#[derive(Clone)]
pub enum HttpAcceptor {
    /// Http variant.
    Http(TcpAcceptor, SocketAddr),
    /// Https variant.
    Https(TcpAcceptor, SocketAddr),
}

impl NetworkAcceptor for HttpAcceptor {
    type Stream = HttpStream;

    #[inline]
    fn accept(&mut self) -> IoResult<HttpStream> {
        Ok(match *self {
            HttpAcceptor::Http(ref mut tcp, _) => HttpStream::Http(try!(tcp.accept())),
            HttpAcceptor::Https(ref mut _tcp, _) => unimplemented!(),
        })
    }

    #[inline]
    fn close(&mut self) -> IoResult<()> {
        match *self {
            HttpAcceptor::Http(ref mut tcp, _) => tcp.close_accept(),
            HttpAcceptor::Https(ref mut tcp, _) => tcp.close_accept(),
        }
    }

    #[inline]
    fn socket_name(&self) -> IoResult<SocketAddr> {
        match *self {
            HttpAcceptor::Http(_, addr) => Ok(addr),
            HttpAcceptor::Https(_, addr) => Ok(addr),
        }
    }
}

/// A wrapper around a TcpStream.
#[derive(Clone)]
pub enum HttpStream {
    /// A stream over the HTTP protocol.
    Http(TcpStream),
    /// A stream over the HTTP protocol, protected by SSL.
    Https(SslStream<TcpStream>),
}

impl Reader for HttpStream {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        match *self {
            HttpStream::Http(ref mut inner) => inner.read(buf),
            HttpStream::Https(ref mut inner) => inner.read(buf)
        }
    }
}

impl Writer for HttpStream {
    #[inline]
    fn write(&mut self, msg: &[u8]) -> IoResult<()> {
        match *self {
            HttpStream::Http(ref mut inner) => inner.write(msg),
            HttpStream::Https(ref mut inner) => inner.write(msg)
        }
    }
    #[inline]
    fn flush(&mut self) -> IoResult<()> {
        match *self {
            HttpStream::Http(ref mut inner) => inner.flush(),
            HttpStream::Https(ref mut inner) => inner.flush(),
        }
    }
}

impl NetworkStream for HttpStream {
    fn peer_name(&mut self) -> IoResult<SocketAddr> {
        match *self {
            HttpStream::Http(ref mut inner) => inner.peer_name(),
            HttpStream::Https(ref mut inner) => inner.get_mut().peer_name()
        }
    }
}

/// A connector that will produce HttpStreams.
#[allow(missing_copy_implementations)]
pub struct HttpConnector(pub Option<VerifyCallback>);

impl NetworkConnector for HttpConnector {
    type Stream = HttpStream;

    fn connect(&mut self, host: &str, port: Port, scheme: &str) -> IoResult<HttpStream> {
        let addr = (host, port);
        match scheme {
            "http" => {
                debug!("http scheme");
                Ok(HttpStream::Http(try!(TcpStream::connect(addr))))
            },
            "https" => {
                debug!("https scheme");
                let stream = try!(TcpStream::connect(addr));
                let mut context = try!(SslContext::new(Sslv23).map_err(lift_ssl_error));
                self.0.as_ref().map(|cb| context.set_verify(SslVerifyPeer, Some(*cb)));
                let ssl = try!(Ssl::new(&context).map_err(lift_ssl_error));
                try!(ssl.set_hostname(host).map_err(lift_ssl_error));
                let stream = try!(SslStream::new(&context, stream).map_err(lift_ssl_error));
                Ok(HttpStream::Https(stream))
            },
            _ => {
                Err(IoError {
                    kind: InvalidInput,
                    desc: "Invalid scheme for Http",
                    detail: None
                })
            }
        }
    }
}

fn lift_ssl_error(ssl: SslError) -> IoError {
    debug!("lift_ssl_error: {:?}", ssl);
    match ssl {
        StreamError(err) => err,
        SslSessionClosed => IoError {
            kind: ConnectionAborted,
            desc: "SSL Connection Closed",
            detail: None
        },
        // Unfortunately throw this away. No way to support this
        // detail without a better Error abstraction.
        OpenSslErrors(errs) => IoError {
            kind: OtherIoError,
            desc: "Error in OpenSSL",
            detail: Some(format!("{:?}", errs))
        }
    }
}

#[cfg(test)]
mod tests {
    use uany::UnsafeAnyExt;

    use mock::MockStream;
    use super::NetworkStream;

    #[test]
    fn test_downcast_box_stream() {
        let stream = box MockStream::new() as Box<NetworkStream + Send>;

        let mock = stream.downcast::<MockStream>().ok().unwrap();
        assert_eq!(mock, box MockStream::new());

    }

    #[test]
    fn test_downcast_unchecked_box_stream() {
        let stream = box MockStream::new() as Box<NetworkStream + Send>;

        let mock = unsafe { stream.downcast_unchecked::<MockStream>() };
        assert_eq!(mock, box MockStream::new());

    }

}
