//! Host trait implementations for the `wasi:webrtc-data-channels` imports.
//!
//! Following the split the generated bindings produce (and mirroring
//! `wasmtime_wasi_http::p3`), the store-free traits are implemented for the
//! [`WasiWebrtcCtxView`] "data" type, while the traits whose methods need the
//! async `Accessor` are implemented for the [`WasiWebrtc`] `HasData` marker.

use wasmtime::component::{Accessor, Resource, StreamReader};
use wasmtime::Result;

use crate::p3::bindings::webrtc_data_channels::data_channels::{
    self, HostDataChannel, HostDataChannelWithStore,
};
use crate::p3::bindings::webrtc_data_channels::manual_signaling::{
    self, HostPeerConnection, HostPeerConnectionWithStore,
};
use crate::p3::bindings::webrtc_data_channels::types::{self, DataChannelOptions, Error};
use crate::p3::pipe::{PipeConsumer, PipeProducer};
use crate::p3::{data_channel, manual::ManualPeer, DataChannel, WasiWebrtc, WasiWebrtcCtxView};

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

// --- manual-signaling ------------------------------------------------------

impl manual_signaling::Host for WasiWebrtcCtxView<'_> {}

impl HostPeerConnection for WasiWebrtcCtxView<'_> {
    fn new(&mut self) -> Result<Resource<ManualPeer>> {
        Ok(self.table.push(ManualPeer::new())?)
    }

    fn close(&mut self, _self_: Resource<ManualPeer>) -> Result<()> {
        // The peer connection is torn down when the resource is dropped.
        Ok(())
    }
}

impl<T> HostPeerConnectionWithStore<T> for WasiWebrtc {
    async fn create_offer(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
        options: DataChannelOptions,
    ) -> Result<std::result::Result<String, Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        Ok(peer
            .create_offer(&options.label, options.ordered, options.max_retransmits)
            .await
            .map_err(|err| Error::Other(err.to_string())))
    }

    async fn accept_answer(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
        answer: String,
    ) -> Result<std::result::Result<(), Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        Ok(peer
            .accept_answer(answer)
            .await
            .map_err(|err| Error::Other(err.to_string())))
    }

    async fn create_answer(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
        offer: String,
    ) -> Result<std::result::Result<String, Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        Ok(peer
            .create_answer(offer)
            .await
            .map_err(|err| Error::Other(err.to_string())))
    }

    async fn connect(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
    ) -> Result<std::result::Result<Resource<DataChannel>, Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        match peer.connect().await {
            Ok(channel) => {
                let resource = accessor.with(|mut access| access.get().table.push(channel))?;
                Ok(Ok(resource))
            }
            Err(err) => Ok(Err(Error::Other(err.to_string()))),
        }
    }

    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<ManualPeer>) -> Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}

/// Clone the cheaply-`Arc`-backed [`ManualPeer`] out of the table so its async
/// methods can run without holding the store borrow across `.await`.
fn clone_peer(view: WasiWebrtcCtxView<'_>, self_: &Resource<ManualPeer>) -> Result<ManualPeer> {
    Ok(view.table.get(self_)?.clone())
}
