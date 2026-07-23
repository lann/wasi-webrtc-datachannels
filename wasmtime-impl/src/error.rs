//! The crate-level host error type.
//!
//! Every fallible host path produces a [`WebrtcError`], the crate's single
//! classification point (mirroring `wasmtime-wasi-http`'s crate-level error).
//! Unlike the WIT `error` variant — which can only carry strings — it retains
//! the full `anyhow`/`webrtc-rs` source chain, which is flattened (with
//! causes) only at the binding boundary via the [`From`] conversion into the
//! generated variant.

use std::fmt;
use std::sync::Arc;

use crate::bindings::webrtc_datachannels::types::Error as WitError;

/// A classified host-side WebRTC failure.
///
/// The unit variants mirror the WIT `error` cases directly; the payload
/// variants keep the underlying error (behind an [`Arc`], so the type stays
/// [`Clone`]-able for the shared wiring/build futures) until the binding
/// boundary flattens it.
#[derive(Clone, Debug)]
pub enum WebrtcError {
    /// The connection or channel closed before the operation completed.
    Closed,
    /// The signaling or connection attempt timed out.
    TimedOut,
    /// A supplied session description or ICE candidate was malformed.
    InvalidSignaling(Arc<anyhow::Error>),
    /// `receive-via-stream` has claimed the channel's inbound messages.
    ReceivingViaStream,
    /// The channel's bounded inbound buffer overflowed.
    ReceiveBufferOverflow,
    /// An implementation-specific failure, retaining its source chain.
    Other(Arc<anyhow::Error>),
}

/// The result type of the crate's fallible host paths.
pub type WebrtcResult<T> = std::result::Result<T, WebrtcError>;

impl WebrtcError {
    /// An implementation-specific failure wrapping `err` (source chain kept).
    pub fn other(err: impl Into<anyhow::Error>) -> Self {
        Self::Other(Arc::new(err.into()))
    }

    /// An implementation-specific failure from a plain message.
    pub fn msg(msg: impl fmt::Display) -> Self {
        Self::Other(Arc::new(anyhow::anyhow!(msg.to_string())))
    }

    /// A malformed-signaling failure wrapping `err` (source chain kept).
    pub fn invalid_signaling(err: impl Into<anyhow::Error>) -> Self {
        Self::InvalidSignaling(Arc::new(err.into()))
    }
}

impl fmt::Display for WebrtcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "connection or channel closed"),
            Self::TimedOut => write!(f, "timed out"),
            Self::InvalidSignaling(err) => write!(f, "invalid signaling: {err}"),
            Self::ReceivingViaStream => {
                write!(f, "inbound messages are claimed by receive-via-stream")
            }
            Self::ReceiveBufferOverflow => write!(f, "inbound buffer overflowed"),
            Self::Other(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for WebrtcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidSignaling(err) | Self::Other(err) => Some(err.as_ref().as_ref()),
            _ => None,
        }
    }
}

/// Flatten a [`WebrtcError`] into the generated WIT `error` variant at the
/// binding boundary. The payload variants render their whole source chain
/// (`anyhow`'s `{:#}` format) into the variant's string.
impl From<WebrtcError> for WitError {
    fn from(err: WebrtcError) -> Self {
        match err {
            WebrtcError::Closed => WitError::Closed,
            WebrtcError::TimedOut => WitError::TimedOut,
            WebrtcError::InvalidSignaling(err) => WitError::InvalidSignaling(format!("{err:#}")),
            WebrtcError::ReceivingViaStream => WitError::ReceivingViaStream,
            WebrtcError::ReceiveBufferOverflow => WitError::ReceiveBufferOverflow,
            WebrtcError::Other(err) => WitError::Other(format!("{err:#}")),
        }
    }
}
