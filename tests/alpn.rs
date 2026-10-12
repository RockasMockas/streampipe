//! ALPN wire-compatibility pins.
//!
//! The pipe path must stay wire-compatible with upstream dumbpipe forever, so these
//! values are pinned and must never change. The stream and udp paths use their own
//! dedicated ALPNs so a viewer can never be mistaken for a pipe connector.

use streampipe::{udp, ALPN, HANDSHAKE, STREAM_ALPN};

#[test]
fn classic_pipe_wire_compat_is_pinned() {
    assert_eq!(ALPN, b"DUMBPIPEV0");
    assert_eq!(HANDSHAKE, *b"hello");
}

#[test]
fn new_features_use_streampipe_alpn() {
    assert_eq!(STREAM_ALPN, b"STREAMPIPE_WEBRTC_V0");
    assert_eq!(udp::ALPN, b"STREAMPIPE_UDP_V0");
}
