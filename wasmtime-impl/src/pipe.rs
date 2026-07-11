//! `StreamProducer` / `StreamConsumer` adapters bridging Wasmtime's
//! component-model host streams to `futures` `Stream`/`Sink`s.
//!
//! These mirror the helpers in Wasmtime's own `component-async-tests` and let
//! the host feed a `StreamReader` from an mpsc receiver ([`PipeProducer`]) or
//! drain a `StreamReader` into an mpsc sender ([`PipeConsumer`]).

use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::{Sink, Stream};
use wasmtime::component::{Destination, Source, StreamConsumer, StreamProducer, StreamResult};
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

/// Consume component-stream items into a [`futures::Sink`].
pub struct PipeConsumer<T, S>(S, PhantomData<fn() -> T>);

impl<T, S> PipeConsumer<T, S> {
    /// Wrap a `futures::Sink` as a component-model stream consumer.
    pub fn new(sink: S) -> Self {
        Self(sink, PhantomData)
    }
}

impl<D, T, S> StreamConsumer<D> for PipeConsumer<T, S>
where
    T: wasmtime::component::Lift + 'static,
    S: Sink<T, Error: std::error::Error + Send + Sync> + Send + 'static,
{
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<D>,
        mut source: Source<Self::Item>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        // SAFETY: standard pin projection; we never move out of `self`.
        let mut sink = unsafe { self.map_unchecked_mut(|v| &mut v.0) };

        let on_pending = || {
            if finish {
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                Poll::Pending
            }
        };

        match sink.as_mut().poll_flush(cx) {
            Poll::Pending => on_pending(),
            Poll::Ready(result) => {
                result?;
                match sink.as_mut().poll_ready(cx) {
                    Poll::Pending => on_pending(),
                    Poll::Ready(result) => {
                        result?;
                        let item = &mut None;
                        source.read(store, item)?;
                        sink.start_send(item.take().unwrap())?;
                        Poll::Ready(Ok(StreamResult::Completed))
                    }
                }
            }
        }
    }
}
