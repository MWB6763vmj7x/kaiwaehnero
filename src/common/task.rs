pub(crate) use std::task::{Context, Poll};
use super::Never;

/// A function to help "yield" a future, such that it is re-scheduled immediately.
///
/// Useful for spin counts, so a future doesn't hog too much time.
pub(crate) fn yield_now(cx: &mut Context) -> Poll<Never> {
    cx.waker().wake_by_ref();
    Poll::Pending
}
