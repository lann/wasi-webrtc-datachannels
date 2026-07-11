//! Host trait implementations for the `lann:webrtc-datachannels` imports.
//!
//! Following the split the generated bindings produce (and mirroring
//! `wasmtime_wasi_http::p3`), the store-free traits are implemented for the
//! [`WasiWebrtcCtxView`] "data" type, while the traits whose methods need the
//! async `Accessor` are implemented for the [`WasiWebrtc`] `HasData` marker.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::channel::oneshot;
use wasmtime::component::{Accessor, Resource, Source, StreamConsumer, StreamReader, StreamResult};
use wasmtime::{Result, StoreContextMut};

use crate::bindings::webrtc_datachannels::data_channels::{
    self, HostDataChannel, HostDataChannelWithStore,
};
use crate::bindings::webrtc_datachannels::types::{self, Error};
use crate::pipe::PipeProducer;
use crate::{DataChannel, WasiWebrtc, WasiWebrtcCtxView};

use webrtc::data_channel::RTCDataChannel;

// --- types -----------------------------------------------------------------

impl types::Host for WasiWebrtcCtxView<'_> {}

// --- data-channels ---------------------------------------------------------

impl data_channels::Host for WasiWebrtcCtxView<'_> {}

/// A [`StreamConsumer`] that forwards each item directly to a WebRTC data
/// channel by polling a `send` future inside `poll_consume`.  Completion or
/// any send error is reported back to the `send` host function via `done_tx`.
struct SendConsumer {
    channel: Arc<RTCDataChannel>,
    /// A send future for the most-recently-read item, if still in flight.
    pending: Option<Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>>,
    /// Oneshot used to pass the final result back to the `send` host function.
    done_tx: Option<oneshot::Sender<std::result::Result<(), Error>>>,
}

impl<D: Send + 'static> StreamConsumer<D> for SendConsumer {
    type Item = Vec<u8>;

    fn poll_consume(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<D>,
        mut source: Source<'_, Vec<u8>>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: SendConsumer is Unpin

        // Complete any in-flight send before reading the next item.  The docs
        // say we must not "put back" an item once taken from `source`, so we
        // never read a new item while a prior send is still in progress.
        if let Some(fut) = this.pending.as_mut() {
            match fut.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(())) => this.pending = None,
                Poll::Ready(Err(e)) => {
                    if let Some(tx) = this.done_tx.take() {
                        let _ = tx.send(Err(Error::Other(e.to_string())));
                    }
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
            }
        }

        // Read the next item.  poll_consume is only called when source has an
        // item (or when finish=true, which per the docs still provides an item).
        let item = &mut None;
        source.read(store, item)?;
        let msg = item.take().expect("source.read did not populate item");

        let channel = this.channel.clone();
        let mut fut = Box::pin(async move { channel.send(&Bytes::from(msg)).await.map(|_| ()).map_err(Into::into) });

        match fut.as_mut().poll(cx) {
            Poll::Pending => {
                this.pending = Some(fut);
                Poll::Pending
            }
            Poll::Ready(Ok(())) => {
                if finish {
                    if let Some(tx) = this.done_tx.take() {
                        let _ = tx.send(Ok(()));
                    }
                    Poll::Ready(Ok(StreamResult::Dropped))
                } else {
                    Poll::Ready(Ok(StreamResult::Completed))
                }
            }
            Poll::Ready(Err(e)) => {
                if let Some(tx) = this.done_tx.take() {
                    let _ = tx.send(Err(Error::Other(e.to_string())));
                }
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

impl Drop for SendConsumer {
    fn drop(&mut self) {
        // If the stream ended without finish=true (consumer dropped normally),
        // signal success.
        if let Some(tx) = self.done_tx.take() {
            let _ = tx.send(Ok(()));
        }
    }
}

impl HostDataChannel for WasiWebrtcCtxView<'_> {
    fn label(&mut self, self_: Resource<DataChannel>) -> Result<String> {
        Ok(self.table.get(&self_)?.label())
    }
}

impl<T: Send> HostDataChannelWithStore<T> for WasiWebrtc {
    async fn send(
        accessor: &Accessor<T, Self>,
        self_: Resource<DataChannel>,
        messages: StreamReader<Vec<u8>>,
    ) -> Result<std::result::Result<(), Error>> {
        let channel = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.channel()))?;

        let (done_tx, done_rx) = oneshot::channel();
        accessor.with(move |access| {
            messages.pipe(
                access,
                SendConsumer {
                    channel,
                    pending: None,
                    done_tx: Some(done_tx),
                },
            )
        })?;

        Ok(done_rx.await.unwrap_or(Ok(())))
    }

    async fn receive(
        accessor: &Accessor<T, Self>,
        self_: Resource<DataChannel>,
    ) -> Result<StreamReader<Vec<u8>>> {
        let incoming = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.take_incoming())
        })?;
        let incoming = incoming
            .ok_or_else(|| wasmtime::Error::msg("receive() may only be called once per channel"))?;
        accessor.with(|access| StreamReader::new(access, PipeProducer::new(incoming)))
    }

    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<DataChannel>) -> Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}
