//! Containing a panic in the query engine to the connection that caused it.
//!
//! `tokio::spawn` already keeps a panicking task from touching the runtime,
//! so a server built on [`crate::Server`] survives one either way. What it
//! does not do is *tell the client*: the task unwinds, the socket is
//! dropped, and the peer sees a connection reset with no `ErrorResponse` and
//! no SQLSTATE. That is indistinguishable from a network failure, which is
//! the wrong thing for a driver to retry.
//!
//! [`guarded`] closes that gap. A panic inside the handler becomes a
//! [`ProtocolError::HandlerPanic`], which is deliberately *not* recoverable:
//! the session state on the other side of the trait — an open transaction,
//! above all — cannot be trusted after an unwind, so this connection is
//! answered and closed rather than reused. PostgreSQL terminates the backend
//! for the same reason. Every other connection is untouched.

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::error::ProtocolError;

/// Awaits `future`, turning a panic into an error instead of an unwind.
pub(crate) async fn guarded<F, T, E>(what: &str, future: F) -> Result<Result<T, E>, ProtocolError>
where
    F: Future<Output = Result<T, E>>,
{
    match (CatchUnwind {
        inner: Box::pin(future),
    })
    .await
    {
        Ok(result) => Ok(result),
        Err(payload) => {
            let message = describe(&payload);
            tracing::error!(target = what, panic = %message, "the query handler panicked");
            Err(ProtocolError::HandlerPanic(format!("{what}: {message}")))
        }
    }
}

/// The panic message, when the payload is one of the two shapes
/// `panic!`/`assert!` actually produce.
fn describe(payload: &Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_owned();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "panicked".to_owned()
}

/// `futures::FutureExt::catch_unwind` without the dependency.
///
/// The inner future is boxed rather than pinned in place, which makes this
/// type `Unpin` and the whole thing safe code. One allocation per statement
/// is not measurable next to planning and executing it.
struct CatchUnwind<F> {
    inner: Pin<Box<F>>,
}

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.get_mut().inner;
        // `AssertUnwindSafe` is the honest claim here: the future may well
        // be left in a torn state, which is exactly why the caller closes
        // the connection instead of polling it again.
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_common::FerriteError;

    #[tokio::test]
    async fn a_panicking_future_becomes_an_error() {
        let outcome = guarded::<_, (), FerriteError>("the test", async {
            panic!("deliberate");
        })
        .await;
        match outcome {
            Err(ProtocolError::HandlerPanic(message)) => assert!(message.contains("deliberate")),
            other => panic!("expected a HandlerPanic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_panic_after_an_await_point_is_caught_too() {
        let outcome = guarded::<_, (), FerriteError>("the test", async {
            tokio::task::yield_now().await;
            panic!("deliberate, later");
        })
        .await;
        assert!(matches!(outcome, Err(ProtocolError::HandlerPanic(_))));
    }

    #[tokio::test]
    async fn an_ordinary_result_passes_straight_through() {
        let ok = guarded::<_, i32, FerriteError>("the test", async { Ok(7) })
            .await
            .expect("no panic");
        assert_eq!(ok, Ok(7));

        let failed = guarded::<_, i32, FerriteError>("the test", async {
            Err(FerriteError::Exec("ordinary".into()))
        })
        .await
        .expect("no panic");
        assert!(failed.is_err());
    }
}
