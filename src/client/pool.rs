use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use futures::{Future, Async, Poll, Stream};
use futures::sync::oneshot;
use futures_timer::Interval;

use common::{Exec, Never};
use super::Ver;

pub(super) struct Pool<T> {
    inner: Arc<Mutex<PoolInner<T>>>,
}

// Before using a pooled connection, make sure the sender is not dead.
//
// This is a trait to allow the `client::pool::tests` to work for `i32`.
//
// See https://github.com/hyperium/hyper/issues/1429
pub(super) trait Poolable: Sized {
    fn is_closed(&self) -> bool;
    /// Reserve this connection.
    ///
    /// Allows for HTTP/2 to return a shared reservation.
    fn reserve(self) -> Reservation<Self>;
}

/// When checking out a pooled connection, it might be that the connection
/// only supports a single reservation, or it might be usable for many.
///
/// Specifically, HTTP/1 requires a unique reservation, but HTTP/2 can be
/// used for multiple requests.
pub(super) enum Reservation<T> {
    /// This connection could be used multiple times, the first one will be
    /// reinserted into the `idle` pool, and the second will be given to
    /// the `Checkout`.
    #[allow(unused)]
    Shared(T, T),
    /// This connection requires unique access. It will be returned after
    /// use is complete.
    Unique(T),
}

/// Simple type alias in case the key type needs to be adjusted.
type Key = (Arc<String>, Ver);

struct PoolInner<T> {
    // A flag that a connection is being estabilished, and the connection
    // should be shared. This prevents making multiple HTTP/2 connections
    // to the same host.
    connecting: HashSet<Key>,
    enabled: bool,
    // These are internal Conns sitting in the event loop in the KeepAlive
    // state, waiting to receive a new Request to send on the socket.
    idle: HashMap<Key, Vec<Idle<T>>>,
    // These are outstanding Checkouts that are waiting for a socket to be
    // able to send a Request one. This is used when "racing" for a new
    // connection.
    //
    // The Client starts 2 tasks, 1 to connect a new socket, and 1 to wait
    // for the Pool to receive an idle Conn. When a Conn becomes idle,
    // this list is checked for any parked Checkouts, and tries to notify
    // them that the Conn could be used instead of waiting for a brand new
    // connection.
    parked: HashMap<Key, VecDeque<oneshot::Sender<T>>>,
    timeout: Option<Duration>,
    // A oneshot channel is used to allow the interval to be notified when
    // the Pool completely drops. That way, the interval can cancel immediately.
    idle_interval_ref: Option<oneshot::Sender<Never>>,
}

impl<T> Pool<T> {
    pub fn new(enabled: bool, timeout: Option<Duration>) -> Pool<T> {
        Pool {
            inner: Arc::new(Mutex::new(PoolInner {
                connecting: HashSet::new(),
                enabled: enabled,
                idle: HashMap::new(),
                idle_interval_ref: None,
                parked: HashMap::new(),
                timeout: timeout,
            })),
        }
    }
}

impl<T: Poolable> Pool<T> {
    /// Returns a `Checkout` which is a future that resolves if an idle
    /// connection becomes available.
    pub fn checkout(&self, key: Key) -> Checkout<T> {
        Checkout {
            key,
            pool: self.clone(),
            parked: None,
        }
    }

    /// Ensure that there is only ever 1 connecting task for HTTP/2
    /// connections. This does nothing for HTTP/1.
    pub(super) fn connecting(&self, key: &Key) -> Option<Connecting<T>> {
        if key.1 == Ver::Http2 {
            let mut inner = self.inner.lock().unwrap();
            if inner.connecting.insert(key.clone()) {
                let connecting = Connecting {
                    key: key.clone(),
                    pool: Arc::downgrade(&self.inner),
                };
                Some(connecting)
            } else {
                trace!("HTTP/2 connecting already in progress for {:?}", key.0);
                None
            }
        } else {
            Some(Connecting {
                key: key.clone(),
                // in HTTP/1's case, there is never a lock, so we don't
                // need to do anything in Drop.
                pool: Weak::new(),
            })
        }
    }

    fn take(&self, key: &Key) -> Option<Pooled<T>> {
        let entry = {
            let mut inner = self.inner.lock().unwrap();
            let expiration = Expiration::new(inner.timeout);
            let maybe_entry = inner.idle.get_mut(key)
                .and_then(|list| {
                    trace!("take? {:?}: expiration = {:?}", key, expiration.0);
                    // A block to end the mutable borrow on list,
                    // so the map below can check is_empty()
                    {
                        let popper = IdlePopper {
                            key,
                            list,
                        };
                        popper.pop(&expiration)
                    }
                        .map(|e| (e, list.is_empty()))
                });

            let (entry, empty) = if let Some((e, empty)) = maybe_entry {
                (Some(e), empty)
            } else {
                // No entry found means nuke the list for sure.
                (None, true)
            };
            if empty {
                //TODO: This could be done with the HashMap::entry API instead.
                inner.idle.remove(key);
            }
            entry
        };

        entry.map(|e| self.reuse(key, e.value))
    }

    pub(super) fn pooled(&self, mut connecting: Connecting<T>, value: T) -> Pooled<T> {
        let value = match value.reserve() {
            Reservation::Shared(to_insert, to_return) => {
                debug_assert_eq!(
                    connecting.key.1,
                    Ver::Http2,
                    "shared reservation without Http2"
                );
                let mut inner = self.inner.lock().unwrap();
                inner.put(connecting.key.clone(), to_insert);
                // Do this here instead of Drop for Connecting because we
                // already have a lock, no need to lock the mutex twice.
                inner.connected(&connecting.key);
                // prevent the Drop of Connecting from repeating inner.connected()
                connecting.pool = Weak::new();

                to_return
            },
            Reservation::Unique(value) => value,
        };
        Pooled {
            is_reused: false,
            key: connecting.key.clone(),
            pool: Arc::downgrade(&self.inner),
            value: Some(value)
        }
    }

    fn reuse(&self, key: &Key, value: T) -> Pooled<T> {
        debug!("reuse idle connection for {:?}", key);
        Pooled {
            is_reused: true,
            key: key.clone(),
            pool: Arc::downgrade(&self.inner),
            value: Some(value),
        }
    }

    fn park(&mut self, key: Key, tx: oneshot::Sender<T>) {
        trace!("checkout waiting for idle connection: {:?}", key);
        self.inner.lock().unwrap()
            .parked.entry(key)
            .or_insert(VecDeque::new())
            .push_back(tx);
    }
}

/// Pop off this list, looking for a usable connection that hasn't expired.
struct IdlePopper<'a, T: 'a> {
    key: &'a Key,
    list: &'a mut Vec<Idle<T>>,
}

impl<'a, T: Poolable + 'a> IdlePopper<'a, T> {
    fn pop(self, expiration: &Expiration) -> Option<Idle<T>> {
        while let Some(entry) = self.list.pop() {
            // If the connection has been closed, or is older than our idle
            // timeout, simply drop it and keep looking...
            //
            // TODO: Actually, since the `idle` list is pushed to the end always,
            // that would imply that if *this* entry is expired, then anything
            // "earlier" in the list would *have* to be expired also... Right?
            //
            // In that case, we could just break out of the loop and drop the
            // whole list...
            if entry.value.is_closed() || expiration.expires(entry.idle_at) {
                trace!("remove unacceptable pooled connection for {:?}", self.key);
                continue;
            }

            let value = match entry.value.reserve() {
                Reservation::Shared(to_reinsert, to_checkout) => {
                    self.list.push(Idle {
                        idle_at: Instant::now(),
                        value: to_reinsert,
                    });
                    to_checkout
                },
                Reservation::Unique(unique) => {
                    unique
                }
            };

            return Some(Idle {
                idle_at: entry.idle_at,
                value,
            });
        }

        None
    }
}

impl<T: Poolable> PoolInner<T> {
    fn put(&mut self, key: Key, value: T) {
        if !self.enabled {
            return;
        }
        if key.1 == Ver::Http2 && self.idle.contains_key(&key) {
            trace!("Pool::put; existing idle HTTP/2 connection for {:?}", key);
            return;
        }
        trace!("Pool::put {:?}", key);
        let mut remove_parked = false;
        let mut value = Some(value);
        if let Some(parked) = self.parked.get_mut(&key) {
            while let Some(tx) = parked.pop_front() {
                if !tx.is_canceled() {
                    let reserved = value.take().expect("value already sent");
                    let reserved = match reserved.reserve() {
                        Reservation::Shared(to_keep, to_send) => {
                            value = Some(to_keep);
                            to_send
                        },
                        Reservation::Unique(uniq) => uniq,
                    };
                    match tx.send(reserved) {
                        Ok(()) => {
                            if value.is_none() {
                                break;
                            } else {
                                continue;
                            }
                        },
                        Err(e) => {
                            value = Some(e);
                        }
                    }
                }

                trace!("Pool::put removing canceled parked {:?}", key);
            }
            remove_parked = parked.is_empty();
        }
        if remove_parked {
            self.parked.remove(&key);
        }

        match value {
            Some(value) => {
                debug!("pooling idle connection for {:?}", key);
                self.idle.entry(key)
                     .or_insert(Vec::new())
                     .push(Idle {
                         value: value,
                         idle_at: Instant::now(),
                     });
            }
            None => trace!("Pool::put found parked {:?}", key),
        }
    }

    /// A `Connecting` task is complete. Not necessarily successfully,
    /// but the lock is going away, so clean up.
    fn connected(&mut self, key: &Key) {
        let existed = self.connecting.remove(key);
        debug_assert!(
            existed,
            "Connecting dropped, key not in pool.connecting"
        );
        // cancel any waiters. if there are any, it's because
        // this Connecting task didn't complete successfully.
        // those waiters would never receive a connection.
        self.parked.remove(key);
    }
}

impl<T> PoolInner<T> {
    /// Any `FutureResponse`s that were created will have made a `Checkout`,
    /// and possibly inserted into the pool that it is waiting for an idle
    /// connection. If a user ever dropped that future, we need to clean out
    /// those parked senders.
    fn clean_parked(&mut self, key: &Key) {
        let mut remove_parked = false;
        if let Some(parked) = self.parked.get_mut(key) {
            parked.retain(|tx| {
                !tx.is_canceled()
            });
            remove_parked = parked.is_empty();
        }
        if remove_parked {
            self.parked.remove(key);
        }
    }
}

impl<T: Poolable> PoolInner<T> {
    fn clear_expired(&mut self) {
        let dur = if let Some(dur) = self.timeout {
            dur
        } else {
            return
        };

        let now = Instant::now();
        //self.last_idle_check_at = now;

        self.idle.retain(|_key, values| {

            values.retain(|entry| {
                if entry.value.is_closed() {
                    return false;
                }
                now - entry.idle_at < dur
            });

            // returning false evicts this key/val
            !values.is_empty()
        });
    }
}


impl<T: Poolable + Send + 'static> Pool<T> {
    pub(super) fn spawn_expired_interval(&self, exec: &Exec) {
        let (dur, rx) = {
            let mut inner = self.inner.lock().unwrap();

            if !inner.enabled {
                return;
            }

            if inner.idle_interval_ref.is_some() {
                return;
            }

            if let Some(dur) = inner.timeout {
                let (tx, rx) = oneshot::channel();
                inner.idle_interval_ref = Some(tx);
                (dur, rx)
            } else {
                return
            }
        };

        let interval = Interval::new(dur);
        exec.execute(IdleInterval {
            interval: interval,
            pool: Arc::downgrade(&self.inner),
            pool_drop_notifier: rx,
        });
    }
}

impl<T> Clone for Pool<T> {
    fn clone(&self) -> Pool<T> {
        Pool {
            inner: self.inner.clone(),
        }
    }
}

/// A wrapped poolable value that tries to reinsert to the Pool on Drop.
// Note: The bounds `T: Poolable` is needed for the Drop impl.
pub(super) struct Pooled<T: Poolable> {
    value: Option<T>,
    is_reused: bool,
    key: Key,
    pool: Weak<Mutex<PoolInner<T>>>,
}

impl<T: Poolable> Pooled<T> {
    pub fn is_reused(&self) -> bool {
        self.is_reused
    }

    fn as_ref(&self) -> &T {
        self.value.as_ref().expect("not dropped")
    }

    fn as_mut(&mut self) -> &mut T {
        self.value.as_mut().expect("not dropped")
    }
}

impl<T: Poolable> Deref for Pooled<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.as_ref()
    }
}

impl<T: Poolable> DerefMut for Pooled<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.as_mut()
    }
}

impl<T: Poolable> Drop for Pooled<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            if value.is_closed() {
                // If we *already* know the connection is done here,
                // it shouldn't be re-inserted back into the pool.
                return;
            }

            if let Some(inner) = self.pool.upgrade() {
                if let Ok(mut inner) = inner.lock() {
                    inner.put(self.key.clone(), value);
                }
            } else {
                trace!("pool dropped, dropping pooled ({:?})", self.key);
            }
        }
    }
}

impl<T: Poolable> fmt::Debug for Pooled<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Pooled")
            .field("key", &self.key)
            .finish()
    }
}

struct Idle<T> {
    idle_at: Instant,
    value: T,
}

pub(super) struct Checkout<T> {
    key: Key,
    pool: Pool<T>,
    parked: Option<oneshot::Receiver<T>>,
}

impl<T: Poolable> Checkout<T> {
    fn poll_parked(&mut self) -> Poll<Option<Pooled<T>>, ::Error> {
        static CANCELED: &str = "pool checkout failed";
        if let Some(ref mut rx) = self.parked {
            match rx.poll() {
                Ok(Async::Ready(value)) => {
                    if !value.is_closed() {
                        Ok(Async::Ready(Some(self.pool.reuse(&self.key, value))))
                    } else {
                        Err(::Error::new_canceled(Some(CANCELED)))
                    }
                },
                Ok(Async::NotReady) => Ok(Async::NotReady),
                Err(_canceled) => Err(::Error::new_canceled(Some(CANCELED))),
            }
        } else {
            Ok(Async::Ready(None))
        }
    }

    fn park(&mut self) {
        if self.parked.is_none() {
            let (tx, mut rx) = oneshot::channel();
            let _ = rx.poll(); // park this task
            self.pool.park(self.key.clone(), tx);
            self.parked = Some(rx);
        }
    }
}

impl<T: Poolable> Future for Checkout<T> {
    type Item = Pooled<T>;
    type Error = ::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Some(pooled) = try_ready!(self.poll_parked()) {
            return Ok(Async::Ready(pooled));
        }

        let entry = self.pool.take(&self.key);

        if let Some(pooled) = entry {
            Ok(Async::Ready(pooled))
        } else {
            self.park();
            Ok(Async::NotReady)
        }
    }
}

impl<T> Drop for Checkout<T> {
    fn drop(&mut self) {
        self.parked.take();
        if let Ok(mut inner) = self.pool.inner.lock() {
            inner.clean_parked(&self.key);
        }
    }
}

pub(super) struct Connecting<T: Poolable> {
    key: Key,
    pool: Weak<Mutex<PoolInner<T>>>,
}

impl<T: Poolable> Drop for Connecting<T> {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            // No need to panic on drop, that could abort!
            if let Ok(mut inner) = pool.lock() {
                debug_assert_eq!(
                    self.key.1,
                    Ver::Http2,
                    "Connecting constructed without Http2"
                );
                inner.connected(&self.key);
            }
        }
    }
}

struct Expiration(Option<Duration>);

impl Expiration {
    fn new(dur: Option<Duration>) -> Expiration {
        Expiration(dur)
    }

    fn expires(&self, instant: Instant) -> bool {
        match self.0 {
            Some(timeout) => instant.elapsed() > timeout,
            None => false,
        }
    }
}

struct IdleInterval<T> {
    interval: Interval,
    pool: Weak<Mutex<PoolInner<T>>>,
    // This allows the IdleInterval to be notified as soon as the entire
    // Pool is fully dropped, and shutdown. This channel is never sent on,
    // but Err(Canceled) will be received when the Pool is dropped.
    pool_drop_notifier: oneshot::Receiver<Never>,
}

impl<T: Poolable + 'static> Future for IdleInterval<T> {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match self.pool_drop_notifier.poll() {
                Ok(Async::Ready(n)) => match n {},
                Ok(Async::NotReady) => (),
                Err(_canceled) => {
                    trace!("pool closed, canceling idle interval");
                    return Ok(Async::Ready(()));
                }
            }

            try_ready!(self.interval.poll().map_err(|_| unreachable!("interval cannot error")));

            if let Some(inner) = self.pool.upgrade() {
                if let Ok(mut inner) = inner.lock() {
                    inner.clear_expired();
                    continue;
                }
            }
            return Ok(Async::Ready(()));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Weak};
    use std::time::Duration;
    use futures::{Async, Future};
    use futures::future;
    use super::{Connecting, Key, Poolable, Pool, Reservation, Exec, Ver};

    /// Test unique reservations.
    #[derive(Debug, PartialEq, Eq)]
    struct Uniq<T>(T);

    impl<T> Poolable for Uniq<T> {
        fn is_closed(&self) -> bool {
            false
        }

        fn reserve(self) -> Reservation<Self> {
            Reservation::Unique(self)
        }
    }

    /*
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    struct Share<T>(T);

    impl<T> Poolable for Share<T> {
        fn is_closed(&self) -> bool {
            false
        }

        fn reserve(self) -> Reservation<Self> {
            Reservation::Shared(self.clone(), self)
        }
    }
    */

    fn c<T: Poolable>(key: Key) -> Connecting<T> {
        Connecting {
            key,
            pool: Weak::new(),
        }
    }

    #[test]
    fn test_pool_checkout_smoke() {
        let pool = Pool::new(true, Some(Duration::from_secs(5)));
        let key = (Arc::new("foo".to_string()), Ver::Http1);
        let pooled = pool.pooled(c(key.clone()), Uniq(41));

        drop(pooled);

        match pool.checkout(key).poll().unwrap() {
            Async::Ready(pooled) => assert_eq!(*pooled, Uniq(41)),
            _ => panic!("not ready"),
        }
    }

    #[test]
    fn test_pool_checkout_returns_none_if_expired() {
        future::lazy(|| {
            let pool = Pool::new(true, Some(Duration::from_millis(100)));
            let key = (Arc::new("foo".to_string()), Ver::Http1);
            let pooled = pool.pooled(c(key.clone()), Uniq(41));
            drop(pooled);
            ::std::thread::sleep(pool.inner.lock().unwrap().timeout.unwrap());
            assert!(pool.checkout(key).poll().unwrap().is_not_ready());
            ::futures::future::ok::<(), ()>(())
        }).wait().unwrap();
    }

    #[test]
    fn test_pool_checkout_removes_expired() {
        future::lazy(|| {
            let pool = Pool::new(true, Some(Duration::from_millis(100)));
            let key = (Arc::new("foo".to_string()), Ver::Http1);

            pool.pooled(c(key.clone()), Uniq(41));
            pool.pooled(c(key.clone()), Uniq(5));
            pool.pooled(c(key.clone()), Uniq(99));

            assert_eq!(pool.inner.lock().unwrap().idle.get(&key).map(|entries| entries.len()), Some(3));
            ::std::thread::sleep(pool.inner.lock().unwrap().timeout.unwrap());

            // checkout.poll() should clean out the expired
            pool.checkout(key.clone()).poll().unwrap();
            assert!(pool.inner.lock().unwrap().idle.get(&key).is_none());

            Ok::<(), ()>(())
        }).wait().unwrap();
    }

    #[test]
    fn test_pool_timer_removes_expired() {
        use std::sync::Arc;
        let runtime = ::tokio::runtime::Runtime::new().unwrap();
        let pool = Pool::new(true, Some(Duration::from_millis(100)));

        let executor = runtime.executor();
        pool.spawn_expired_interval(&Exec::Executor(Arc::new(executor)));
        let key = (Arc::new("foo".to_string()), Ver::Http1);

        pool.pooled(c(key.clone()), Uniq(41));
        pool.pooled(c(key.clone()), Uniq(5));
        pool.pooled(c(key.clone()), Uniq(99));

        assert_eq!(pool.inner.lock().unwrap().idle.get(&key).map(|entries| entries.len()), Some(3));

        ::futures_timer::Delay::new(
            Duration::from_millis(400) // allow for too-good resolution
        ).wait().unwrap();

        assert!(pool.inner.lock().unwrap().idle.get(&key).is_none());
    }

    #[test]
    fn test_pool_checkout_task_unparked() {
        let pool = Pool::new(true, Some(Duration::from_secs(10)));
        let key = (Arc::new("foo".to_string()), Ver::Http1);
        let pooled = pool.pooled(c(key.clone()), Uniq(41));

        let checkout = pool.checkout(key).join(future::lazy(move || {
            // the checkout future will park first,
            // and then this lazy future will be polled, which will insert
            // the pooled back into the pool
            //
            // this test makes sure that doing so will unpark the checkout
            drop(pooled);
            Ok(())
        })).map(|(entry, _)| entry);
        assert_eq!(*checkout.wait().unwrap(), Uniq(41));
    }

    #[test]
    fn test_pool_checkout_drop_cleans_up_parked() {
        future::lazy(|| {
            let pool = Pool::<Uniq<i32>>::new(true, Some(Duration::from_secs(10)));
            let key = (Arc::new("localhost:12345".to_string()), Ver::Http1);

            let mut checkout1 = pool.checkout(key.clone());
            let mut checkout2 = pool.checkout(key.clone());

            // first poll needed to get into Pool's parked
            checkout1.poll().unwrap();
            assert_eq!(pool.inner.lock().unwrap().parked.get(&key).unwrap().len(), 1);
            checkout2.poll().unwrap();
            assert_eq!(pool.inner.lock().unwrap().parked.get(&key).unwrap().len(), 2);

            // on drop, clean up Pool
            drop(checkout1);
            assert_eq!(pool.inner.lock().unwrap().parked.get(&key).unwrap().len(), 1);

            drop(checkout2);
            assert!(pool.inner.lock().unwrap().parked.get(&key).is_none());

            ::futures::future::ok::<(), ()>(())
        }).wait().unwrap();
    }

    #[derive(Debug)]
    struct CanClose {
        val: i32,
        closed: bool,
    }

    impl Poolable for CanClose {
        fn is_closed(&self) -> bool {
            self.closed
        }

        fn reserve(self) -> Reservation<Self> {
            Reservation::Unique(self)
        }
    }

    #[test]
    fn pooled_drop_if_closed_doesnt_reinsert() {
        let pool = Pool::new(true, Some(Duration::from_secs(10)));
        let key = (Arc::new("localhost:12345".to_string()), Ver::Http1);
        pool.pooled(c(key.clone()), CanClose {
            val: 57,
            closed: true,
        });

        assert!(!pool.inner.lock().unwrap().idle.contains_key(&key));
    }
}
