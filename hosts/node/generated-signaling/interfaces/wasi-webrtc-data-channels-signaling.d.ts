/** @module Interface wasi:webrtc-data-channels/signaling@0.1.0 **/
/**
 * # Variants
 * 
 * ## `"offer"`
 * 
 * ## `"answer"`
 * 
 * ## `"pranswer"`
 * 
 * ## `"rollback"`
 */
export type SdpType = 'offer' | 'answer' | 'pranswer' | 'rollback';
export interface SessionDescription {
  kind: SdpType,
  sdp: string,
}
export type Error = import('./wasi-webrtc-data-channels-types.js').Error;
export type DataChannelOptions = import('./wasi-webrtc-data-channels-types.js').DataChannelOptions;
export type DataChannel = import('./wasi-webrtc-data-channels-data-channels.js').DataChannel;
export interface IceCandidate {
  candidate: string,
  sdpMid?: string,
  sdpMlineIndex?: number,
}

export class PeerConnection {
  constructor()
  createDataChannel(options: DataChannelOptions): DataChannel;
  incomingDataChannels(): ReadableStream<DataChannel>;
  createOffer(): Promise<SessionDescription>;
  createAnswer(): Promise<SessionDescription>;
  setLocalDescription(description: SessionDescription): Promise<void>;
  setRemoteDescription(description: SessionDescription): Promise<void>;
  localIceCandidates(): ReadableStream<IceCandidate>;
  addIceCandidate(candidate: IceCandidate): Promise<void>;
  waitConnected(): Promise<void>;
}
