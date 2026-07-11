# jco-impl

Browser-first (Node) host implementation of `wasi:webrtc-data-channels`, using
jco to transpile the guest component and `@roamhq/wrtc` (or the browser's native
`RTCPeerConnection`) for the WebRTC data channel.
