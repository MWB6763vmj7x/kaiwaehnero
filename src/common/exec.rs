use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_executor::{SpawnError, TypedExecutor};

use crate::body::{Payload, Body};
use crate::proto::h2::server::H2Stream;
use crate::server::conn::spawn_all::{NewSvcTask, Watcher};
use crate::service::Service;

pub trait H2Exec<F, B: Payload>: Clone {
    fn execute_h2stream(&mut self, fut: H2Stream<F, B>) -> crate::Result<()>;
}

pub trait NewSvcExec<I, N, S: Service<Body>, E, W: Watcher<I, S, E>>: Clone {
    fn execute_new_svc(&mut self, fut: NewSvcTask<I, N, S, E, W>) -> crate::Result<()>;
}

type BoxFuture = Pin<Box<dyn Future<Output=()> + Send>>;

pub trait SharedExecutor {
    fn shared_spawn(&self, future: BoxFuture) -> Result<(), SpawnError>;
}

impl<E> SharedExecutor for E
where
    for<'a> &'a E: tokio_executor::Executor,
{
    fn shared_spawn(mut self: &Self, future: BoxFuture) -> Result<(), SpawnError> {
        tokio_executor::Executor::spawn(&mut self, future)
    }
}

// Either the user provides an executor for background tasks, or we use
// `tokio::spawn`.
#[derive(Clone)]
pub enum Exec {
    Default,
    Executor(Arc<dyn SharedExecutor + Send + Sync>),
}

// ===== impl Exec =====

impl Exec {
    pub(crate) fn execute<F>(&self, fut: F) -> crate::Result<()>
    where
        F: Future<Output=()> + Send + 'static,
    {
        match *self {
            Exec::Default => {
                #[cfg(feature = "runtime")]
                {
                    use std::error::Error as StdError;

                    struct TokioSpawnError;

                    impl fmt::Debug for TokioSpawnError {
                        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                            fmt::Debug::fmt("tokio::spawn failed (is a tokio runtime running this future?)", f)
                        }
                    }

                    impl fmt::Display for TokioSpawnError {
                        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                            fmt::Display::fmt("tokio::spawn failed (is a tokio runtime running this future?)", f)
                        }
                    }

                    impl StdError for TokioSpawnError {
                        fn description(&self) -> &str {
                            "tokio::spawn failed"
                        }
                    }

                    ::tokio_executor::DefaultExecutor::current()
                        .spawn(Box::pin(fut))
                        .map_err(|err| {
                            warn!("executor error: {:?}", err);
                            crate::Error::new_execute(TokioSpawnError)
                        })
                }
                #[cfg(not(feature = "runtime"))]
                {
                    // If no runtime, we need an executor!
                    panic!("executor must be set")
                }
            },
            Exec::Executor(ref e) => {
                e.shared_spawn(Box::pin(fut))
                    .map_err(|err| {
                        warn!("executor error: {:?}", err);
                        crate::Error::new_execute("custom executor failed")
                    })
            },
        }
    }
}

impl fmt::Debug for Exec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Exec")
            .finish()
    }
}


impl<F, B> H2Exec<F, B> for Exec
where
    H2Stream<F, B>: Future<Output = ()> + Send + 'static,
    B: Payload,
{
    fn execute_h2stream(&mut self, fut: H2Stream<F, B>) -> crate::Result<()> {
        self.execute(fut)
    }
}

impl<I, N, S, E, W> NewSvcExec<I, N, S, E, W> for Exec
where
    NewSvcTask<I, N, S, E, W>: Future<Output=()> + Send + 'static,
    S: Service<Body>,
    W: Watcher<I, S, E>,
{
    fn execute_new_svc(&mut self, fut: NewSvcTask<I, N, S, E, W>) -> crate::Result<()> {
        self.execute(fut)
    }
}

// ==== impl Executor =====

impl<E, F, B> H2Exec<F, B> for E
where
    E: TypedExecutor<H2Stream<F, B>> + Clone,
    H2Stream<F, B>: Future<Output=()>,
    B: Payload,
{
    fn execute_h2stream(&mut self, fut: H2Stream<F, B>) -> crate::Result<()> {
        self.spawn(fut)
            .map_err(|err| {
                warn!("executor error: {:?}", err);
                crate::Error::new_execute("custom executor failed")
            })
    }
}

impl<I, N, S, E, W> NewSvcExec<I, N, S, E, W> for E
where
    E: TypedExecutor<NewSvcTask<I, N, S, E, W>> + Clone,
    NewSvcTask<I, N, S, E, W>: Future<Output=()>,
    S: Service<Body>,
    W: Watcher<I, S, E>,
{
    fn execute_new_svc(&mut self, fut: NewSvcTask<I, N, S, E, W>) -> crate::Result<()> {
        self.spawn(fut)
            .map_err(|err| {
                warn!("executor error: {:?}", err);
                crate::Error::new_execute("custom executor failed")
            })
    }
}

// ===== StdError impls =====

