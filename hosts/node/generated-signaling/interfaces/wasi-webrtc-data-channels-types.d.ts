/** @module Interface wasi:webrtc-data-channels/types@0.1.0 **/
export type Error = ErrorClosed | ErrorTimedOut | ErrorInvalidSignaling | ErrorOther;
export interface ErrorClosed {
  tag: 'closed',
}
export interface ErrorTimedOut {
  tag: 'timed-out',
}
export interface ErrorInvalidSignaling {
  tag: 'invalid-signaling',
  val: string,
}
export interface ErrorOther {
  tag: 'other',
  val: string,
}
export interface DataChannelOptions {
  label: string,
  ordered: boolean,
  maxRetransmits?: number,
}
