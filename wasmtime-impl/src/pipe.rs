//! `StreamProducer` adapter bridging Wasmtime's component-model host streams
//! to a `futures` `Stream`, letting the host feed a `StreamReader` from an
//! mpsc receiver ([`PipeProducer`]).

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use wasmtime::component::{Destination, StreamProducer, StreamResult};
use wasmtime::{Result, StoreContextMut};

/// Produce component-stream items from a [`futures::Stream`].
pub struct PipeProducer<S>(S);

impl<S> PipeProducer<S> {
    /// Wrap a `futures::Stream` as a component-model stream producer.
    pub fn new(stream: S) -> Self {
        Self(stream)
    }
}

impl<D, T, S> StreamProducer<D> for PipeProducer<S>
where
    T: Send + Sync + wasmtime::component::Lower + 'static,
    S: Stream<Item = T> + Send + 'static,
{
    type Item = T;
    type Buffer = Option<T>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _: StoreContextMut<D>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        // SAFETY: standard pin projection; we never move out of `self`.
        let stream = unsafe { self.map_unchecked_mut(|v| &mut v.0) };
        match stream.poll_next(cx) {
            Poll::Pending => {
                if finish {
                    Poll::Ready(Ok(StreamResult::Cancelled))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(Some(item)) => {
                destination.set_buffer(Some(item));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(None) => Poll::Ready(Ok(StreamResult::Dropped)),
        }
    }
}
