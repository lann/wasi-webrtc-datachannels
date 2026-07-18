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
use futures::channel::mpsc::{self, UnboundedReceiver};
use futures::channel::oneshot;
use futures::lock::Mutex as AsyncMutex;
use futures::StreamExt;
use wasmtime::component::{
    Access, Accessor, Destination, Resource, Source, StreamConsumer, StreamProducer, StreamReader,
    StreamResult,
};
use wasmtime::{AsContextMut, Result, StoreContextMut};

use crate::bindings::webrtc_datachannels::connections::{
    self, HostDataChannel, HostDataChannelWithStore, HostPeerConnection,
    HostPeerConnectionWithStore,
};
use crate::bindings::webrtc_datachannels::types::{
    self, DataChannelOptions, Error, IceCandidate, Message, MessageKind, SendViaStreamError,
    SessionDescription, StreamMessage,
};
use crate::{
    DataChannel, InboundMessage, UnsupportedPeerConnection, WasiWebrtc, WasiWebrtcCtxView,
};

use webrtc::data_channel::RTCDataChannel;

// --- types -----------------------------------------------------------------

impl types::Host for WasiWebrtcCtxView<'_> {}

// --- connections -----------------------------------------------------------

impl connections::Host for WasiWebrtcCtxView<'_> {}

/// Send one message over a data channel, honoring its `binary`/`string` kind.
async fn send_channel_message(
    channel: &Arc<RTCDataChannel>,
    is_string: bool,
    data: Vec<u8>,
) -> std::result::Result<(), Error> {
    let result = if is_string {
        let text = match String::from_utf8(data) {
            Ok(text) => text,
            Err(err) => {
                return Err(Error::Other(format!(
                    "string message is not valid UTF-8: {err}"
                )))
            }
        };
        channel.send_text(text).await
    } else {
        channel.send(&Bytes::from(data)).await
    };
    result.map(|_| ()).map_err(|e| Error::Other(e.to_string()))
}

/// Await the next inbound message from a channel's shared receiver, reporting
/// `Error::Closed` once the channel has closed and no more messages will arrive.
async fn next_inbound(
    incoming: Arc<AsyncMutex<UnboundedReceiver<InboundMessage>>>,
) -> std::result::Result<InboundMessage, Error> {
    let mut receiver = incoming.lock().await;
    receiver.next().await.ok_or(Error::Closed)
}

/// Convert a host-side inbound message into the WIT `message` variant.
fn to_message(inbound: InboundMessage) -> std::result::Result<Message, Error> {
    if inbound.is_string {
        String::from_utf8(inbound.data)
            .map(Message::String)
            .map_err(|err| Error::Other(format!("string message is not valid UTF-8: {err}")))
    } else {
        Ok(Message::Binary(inbound.data))
    }
}

/// A [`StreamConsumer`] that drains every byte of a `stream<u8>` into a buffer,
/// handing the completed buffer back through `done_tx` when the stream ends.
struct ByteCollector {
    buf: Vec<u8>,
    done_tx: Option<oneshot::Sender<Vec<u8>>>,
}

impl ByteCollector {
    fn finish(&mut self) {
        if let Some(tx) = self.done_tx.take() {
            let _ = tx.send(std::mem::take(&mut self.buf));
        }
    }
}

impl<D: Send + 'static> StreamConsumer<D> for ByteCollector {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, u8>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: ByteCollector is Unpin

        let available = source.remaining(&mut store);
        if available > 0 {
            let mut chunk = Vec::with_capacity(available);
            source.read(&mut store, &mut chunk)?;
            this.buf.extend_from_slice(&chunk);
            return Poll::Ready(Ok(StreamResult::Completed));
        }

        // No bytes available. When `finish` is set the stream is ending, so hand
        // the collected buffer back; `Drop` covers a normal end-of-stream.
        if finish {
            this.finish();
            Poll::Ready(Ok(StreamResult::Cancelled))
        } else {
            Poll::Pending
        }
    }
}

impl Drop for ByteCollector {
    fn drop(&mut self) {
        self.finish();
    }
}

/// A [`StreamProducer`] that yields one `stream-message` per inbound WebRTC
/// message, wrapping each message's bytes in a fresh `stream<u8>`.
struct InboundStreamMessages {
    incoming: Arc<AsyncMutex<UnboundedReceiver<InboundMessage>>>,
    /// A future resolving to the next inbound message (or `None` at close),
    /// retained across polls so the shared receiver lock is only held while
    /// awaiting the next message.
    pending: Option<Pin<Box<dyn Future<Output = Option<InboundMessage>> + Send>>>,
}

impl<D: Send + 'static> StreamProducer<D> for InboundStreamMessages {
    type Item = StreamMessage;
    type Buffer = Option<StreamMessage>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'a, D>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: InboundStreamMessages is Unpin

        let incoming = this.incoming.clone();
        let fut = this.pending.get_or_insert_with(|| {
            Box::pin(async move {
                let mut receiver = incoming.lock().await;
                receiver.next().await
            })
        });

        match fut.as_mut().poll(cx) {
            Poll::Pending => {
                if finish {
                    Poll::Ready(Ok(StreamResult::Cancelled))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(None) => {
                this.pending = None;
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Poll::Ready(Some(inbound)) => {
                this.pending = None;
                let kind = if inbound.is_string {
                    MessageKind::String
                } else {
                    MessageKind::Binary
                };
                let length = inbound.data.len() as u32;
                let data = StreamReader::new(store, inbound.data)?;
                destination.set_buffer(Some(StreamMessage { kind, length, data }));
                Poll::Ready(Ok(StreamResult::Completed))
            }
        }
    }
}

/// One outbound message parsed from a `stream<stream-message>` element: its kind,
/// declared length, and a receiver for its fully-drained payload bytes.
struct PendingSend {
    is_string: bool,
    length: usize,
    done_rx: oneshot::Receiver<Vec<u8>>,
}

/// A [`StreamConsumer`] that reads each `stream-message` from a
/// `stream<stream-message>`, starts draining its `data` payload, and forwards
/// the resulting [`PendingSend`] to the `send-via-stream` driver.
struct OutboundStreamMessages {
    tx: mpsc::UnboundedSender<PendingSend>,
}

impl<D: Send + 'static> StreamConsumer<D> for OutboundStreamMessages {
    type Item = StreamMessage;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<D>,
        mut source: Source<'_, StreamMessage>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut(); // safe: OutboundStreamMessages is Unpin

        let available = source.remaining(&mut store);
        if available == 0 {
            // No items are ready. When `finish` is set the writer is done, so
            // report cancellation; otherwise wait to be re-polled once the
            // writer provides more items (returning `Completed` here without
            // taking an item would trap).
            return if finish {
                Poll::Ready(Ok(StreamResult::Cancelled))
            } else {
                Poll::Pending
            };
        }

        // Drain every message the writer has made available, so a writer that
        // queues several messages before closing does not have the trailing
        // ones silently discarded when it finishes. Each message's payload is
        // drained concurrently by a `ByteCollector`; the `send-via-stream`
        // driver then sends the fully-buffered messages one at a time, in order.
        let mut messages: Vec<StreamMessage> = Vec::with_capacity(available);
        source.read(&mut store, &mut messages)?;
        for message in messages {
            let is_string = matches!(message.kind, MessageKind::String);
            let length = message.length as usize;
            let (done_tx, done_rx) = oneshot::channel();
            message.data.pipe(
                store.as_context_mut(),
                ByteCollector {
                    buf: Vec::with_capacity(length),
                    done_tx: Some(done_tx),
                },
            )?;
            let _ = this.tx.unbounded_send(PendingSend {
                is_string,
                length,
                done_rx,
            });
        }
        Poll::Ready(Ok(StreamResult::Completed))
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
        message: Message,
    ) -> Result<std::result::Result<(), Error>> {
        let channel = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.channel())
        })?;

        let (is_string, data) = match message {
            Message::Binary(data) => (false, data),
            Message::String(text) => (true, text.into_bytes()),
        };
        Ok(send_channel_message(&channel, is_string, data).await)
    }

    async fn receive(
        accessor: &Accessor<T, Self>,
        self_: Resource<DataChannel>,
    ) -> Result<std::result::Result<Message, Error>> {
        let (incoming, stream_started, stream_receiving) = accessor.with(|mut access| {
            let channel = access.get().table.get(&self_)?;
            Ok::<_, wasmtime::Error>((
                channel.incoming(),
                channel.stream_started(),
                channel.is_stream_receiving(),
            ))
        })?;

        // `receive-via-stream` has already taken over the inbound messages.
        if stream_receiving {
            return Ok(Err(Error::ReceivingViaStream));
        }

        // Race the next inbound message against `receive-via-stream` being called:
        // whichever resolves first wins, so a pending receiver is woken and fails
        // with `receiving-via-stream` the moment the stream is claimed.
        let receive = std::pin::pin!(next_inbound(incoming));
        let started = std::pin::pin!(stream_started);
        Ok(match futures::future::select(receive, started).await {
            futures::future::Either::Left((result, _)) => match result {
                Ok(inbound) => to_message(inbound),
                Err(err) => Err(err),
            },
            // The stream-started signal fired; when it was actually sent (rather
            // than cancelled by the channel being dropped) report the takeover.
            futures::future::Either::Right((signal, _)) => {
                if signal.is_ok() {
                    Err(Error::ReceivingViaStream)
                } else {
                    Err(Error::Closed)
                }
            }
        })
    }

    async fn send_via_stream(
        accessor: &Accessor<T, Self>,
        self_: Resource<DataChannel>,
        messages: StreamReader<StreamMessage>,
    ) -> Result<std::result::Result<(), SendViaStreamError>> {
        let channel = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.channel())
        })?;

        // Drain each element's payload concurrently (via `OutboundStreamMessages`)
        // while this driver sends the fully-buffered messages one at a time, in
        // stream order.
        let (tx, mut rx) = mpsc::unbounded::<PendingSend>();
        accessor.with(|access| messages.pipe(access, OutboundStreamMessages { tx }))?;

        let mut sent: u64 = 0;
        while let Some(pending) = rx.next().await {
            let data = pending.done_rx.await.unwrap_or_default();
            if data.len() != pending.length {
                return Ok(Err(SendViaStreamError {
                    error: Error::Other(format!(
                        "stream-message payload was {} bytes but length declared {}",
                        data.len(),
                        pending.length
                    )),
                    sent,
                }));
            }
            if let Err(error) = send_channel_message(&channel, pending.is_string, data).await {
                return Ok(Err(SendViaStreamError { error, sent }));
            }
            sent += 1;
        }
        Ok(Ok(()))
    }

    fn receive_via_stream(
        mut access: wasmtime::component::Access<'_, T, Self>,
        self_: Resource<DataChannel>,
    ) -> Result<std::result::Result<StreamReader<StreamMessage>, Error>> {
        // Claim the inbound messages for this stream. A second call (or any
        // concurrent `receive`) observes the claim and fails.
        let (claimed, incoming) = {
            let channel = access.get().table.get(&self_)?;
            (channel.begin_stream_receiving(), channel.incoming())
        };
        if !claimed {
            return Ok(Err(Error::ReceivingViaStream));
        }
        let reader = StreamReader::new(
            access,
            InboundStreamMessages {
                incoming,
                pending: None,
            },
        )?;
        Ok(Ok(reader))
    }

    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<DataChannel>) -> Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}

// --- peer-connection (unimplemented) ---------------------------------------

/// The error surfaced by every `peer-connection` host function.
///
/// The `peer-connection` guest-driven connection design target is not
/// implemented by this crate; these functions exist only because
/// `peer-connection` shares the `connections` interface with the `data-channel`
/// resource this crate does implement.
fn peer_connection_unsupported() -> wasmtime::Error {
    wasmtime::Error::msg(
        "lann:webrtc-datachannels/connections.peer-connection (the guest-driven connection \
         design target) is not implemented by this host",
    )
}

impl HostPeerConnection for WasiWebrtcCtxView<'_> {
    fn new(&mut self) -> Result<Resource<UnsupportedPeerConnection>> {
        Err(peer_connection_unsupported())
    }

    fn create_data_channel(
        &mut self,
        _self_: Resource<UnsupportedPeerConnection>,
        _options: DataChannelOptions,
    ) -> Result<std::result::Result<Resource<DataChannel>, Error>> {
        Err(peer_connection_unsupported())
    }

    fn close(&mut self, _self_: Resource<UnsupportedPeerConnection>) -> Result<()> {
        Err(peer_connection_unsupported())
    }
}

impl<T: Send> HostPeerConnectionWithStore<T> for WasiWebrtc {
    fn incoming_data_channels(
        _access: Access<'_, T, Self>,
        _self_: Resource<UnsupportedPeerConnection>,
    ) -> Result<StreamReader<Resource<DataChannel>>> {
        Err(peer_connection_unsupported())
    }

    fn local_ice_candidates(
        _access: Access<'_, T, Self>,
        _self_: Resource<UnsupportedPeerConnection>,
    ) -> Result<StreamReader<IceCandidate>> {
        Err(peer_connection_unsupported())
    }

    async fn create_offer(
        _accessor: &Accessor<T, Self>,
        _self_: Resource<UnsupportedPeerConnection>,
    ) -> Result<std::result::Result<SessionDescription, Error>> {
        Err(peer_connection_unsupported())
    }

    async fn create_answer(
        _accessor: &Accessor<T, Self>,
        _self_: Resource<UnsupportedPeerConnection>,
    ) -> Result<std::result::Result<SessionDescription, Error>> {
        Err(peer_connection_unsupported())
    }

    async fn set_local_description(
        _accessor: &Accessor<T, Self>,
        _self_: Resource<UnsupportedPeerConnection>,
        _description: SessionDescription,
    ) -> Result<std::result::Result<(), Error>> {
        Err(peer_connection_unsupported())
    }

    async fn set_remote_description(
        _accessor: &Accessor<T, Self>,
        _self_: Resource<UnsupportedPeerConnection>,
        _description: SessionDescription,
    ) -> Result<std::result::Result<(), Error>> {
        Err(peer_connection_unsupported())
    }

    async fn add_ice_candidate(
        _accessor: &Accessor<T, Self>,
        _self_: Resource<UnsupportedPeerConnection>,
        _candidate: IceCandidate,
    ) -> Result<std::result::Result<(), Error>> {
        Err(peer_connection_unsupported())
    }

    async fn wait_connected(
        _accessor: &Accessor<T, Self>,
        _self_: Resource<UnsupportedPeerConnection>,
    ) -> Result<std::result::Result<(), Error>> {
        Err(peer_connection_unsupported())
    }

    async fn drop(
        _accessor: &Accessor<T, Self>,
        _rep: Resource<UnsupportedPeerConnection>,
    ) -> Result<()> {
        Err(peer_connection_unsupported())
    }
}
