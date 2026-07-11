//! Host trait implementations for the `lann:webrtc-datachannels` imports.
//!
//! Following the split the generated bindings produce (and mirroring
//! `wasmtime_wasi_http::p3`), the store-free traits are implemented for the
//! [`WasiWebrtcCtxView`] "data" type, while the traits whose methods need the
//! async `Accessor` are implemented for the [`WasiWebrtc`] `HasData` marker.

use wasmtime::component::{Accessor, Resource, StreamReader};
use wasmtime::Result;

use crate::bindings::webrtc_data_channels::data_channels::{
    self, HostDataChannel, HostDataChannelWithStore,
};
use crate::bindings::webrtc_data_channels::types::{self, Error};
use crate::pipe::{PipeConsumer, PipeProducer};
use crate::{data_channel, DataChannel, WasiWebrtc, WasiWebrtcCtxView};

use futures::channel::mpsc;
use futures::StreamExt;

// --- types -----------------------------------------------------------------

impl types::Host for WasiWebrtcCtxView<'_> {}

// --- data-channels ---------------------------------------------------------

impl data_channels::Host for WasiWebrtcCtxView<'_> {}

impl HostDataChannel for WasiWebrtcCtxView<'_> {
    fn label(&mut self, self_: Resource<DataChannel>) -> Result<String> {
        Ok(self.table.get(&self_)?.label())
    }
}

impl<T> HostDataChannelWithStore<T> for WasiWebrtc {
    async fn send(
        accessor: &Accessor<T, Self>,
        self_: Resource<DataChannel>,
        messages: StreamReader<Vec<u8>>,
    ) -> Result<std::result::Result<(), Error>> {
        let channel = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.channel()))?;

        // Drain the guest's outbound stream into an mpsc sink, then forward each
        // message to the WebRTC data channel, awaiting the transport.
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        accessor.with(move |access| messages.pipe(access, PipeConsumer::new(tx)))?;

        while let Some(message) = rx.next().await {
            if let Err(err) = data_channel::send_message(&channel, message).await {
                return Ok(Err(Error::Other(err.to_string())));
            }
        }
        Ok(Ok(()))
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
