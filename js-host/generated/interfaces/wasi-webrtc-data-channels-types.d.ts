/** @module Interface wasi:webrtc-data-channels/types@0.0.1 **/
export type Error = string;

export class DataChannel {
  /**
   * This type does not have a public constructor.
   */
  private constructor();
  send(data: Uint8Array): Promise<void>;
  receive(): Promise<Uint8Array>;
}

export class PeerConnection {
  constructor()
  createDataChannel(label: string): DataChannel;
}
