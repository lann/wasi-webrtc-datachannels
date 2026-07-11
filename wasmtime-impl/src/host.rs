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
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::oneshot;
use wasmtime::component::{
    Accessor, HasData, Resource, Source, StreamConsumer, StreamReader, StreamResult,
};
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

/// Build the inbound-message [`StreamReader`] for a data channel from its
/// `webrtc-rs` message receiver.
///
/// The inbound stream is produced once, at channel construction, and handed
/// back to the guest alongside the [`DataChannel`] resource (rather than being
/// fetched from a callable-once method on the resource). Host implementations of
/// the channel-constructing functions (for example the demo `connect.open-echo`
/// or `manual-signaling.connect`) call this with the receiver they wired to the
/// channel's `on_message` handler.
pub fn inbound_stream<T, D: HasData>(
    accessor: &Accessor<T, D>,
    incoming: UnboundedReceiver<Vec<u8>>,
) -> Result<StreamReader<Vec<u8>>> {
    accessor.with(|access| StreamReader::new(access, PipeProducer::new(incoming)))
}

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

        // Read the next item.  When finish=true this may be a stream-end signal
        // with no trailing item (all prior items were already consumed).
        let item = &mut None;
        source.read(store, item)?;
        let Some(msg) = item.take() else {
            // No item available.  When finish=true this is a normal end-of-stream
            // signal with no trailing item; acknowledge with Cancelled (which is
            // only valid when finish=true) and report success.
            if let Some(tx) = this.done_tx.take() {
                let _ = tx.send(Ok(()));
            }
            return Poll::Ready(Ok(if finish {
                StreamResult::Cancelled
            } else {
                // finish=false with no item should not occur per the
                // StreamConsumer contract; Completed is the safest fallback.
                StreamResult::Completed
            }));
        };

        let channel = this.channel.clone();
        let mut fut = Box::pin(async move {
            channel
                .send(&Bytes::from(msg))
                .await
                .map(|_| ())
                .map_err(Into::into)
        });

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
        let channel = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.channel())
        })?;

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

    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<DataChannel>) -> Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}
