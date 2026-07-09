/** @module Interface demo:webrtc-echo/signaling-demo@0.1.0 **/
export function run(config: SignalingConfig): Promise<SignalingStats>;
export type Error = import('./wasi-webrtc-data-channels-types.js').Error;
export type Role = import('./demo-webrtc-echo-rendezvous.js').Role;
export interface SignalingConfig {
  room: string,
  asRole: Role,
  messageCount: number,
  messageSize: number,
}
export interface SignalingStats {
  connected: boolean,
  messagesSent: number,
  messagesReceived: number,
  bytesEchoed: bigint,
}
