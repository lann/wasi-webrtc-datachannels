/** @module Interface wasi:webrtc-data-channels/data-channels@0.1.0 **/
export type Error = import('./wasi-webrtc-data-channels-types.js').Error;

export class DataChannel {
  /**
   * This type does not have a public constructor.
   */
  private constructor();
  send(messages: ReadableStream<Uint8Array>): Promise<void>;
  receive(): Promise<ReadableStream<Uint8Array>>;
}
