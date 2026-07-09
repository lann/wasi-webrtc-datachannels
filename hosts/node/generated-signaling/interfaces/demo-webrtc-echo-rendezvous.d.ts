/** @module Interface demo:webrtc-echo/rendezvous@0.1.0 **/
export type Error = import('./wasi-webrtc-data-channels-types.js').Error;
/**
 * # Variants
 * 
 * ## `"offerer"`
 * 
 * ## `"answerer"`
 */
export type Role = 'offerer' | 'answerer';

export class Session {
  /**
   * This type does not have a public constructor.
   */
  private constructor();
  static open(room: string, asRole: Role): Promise<Session>;
  send(blob: Uint8Array): Promise<void>;
  recv(): Promise<Uint8Array | undefined>;
  close(): void;
}
