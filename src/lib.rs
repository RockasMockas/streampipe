/// The ALPN for the dumb-pipe path, kept wire-compatible with upstream dumbpipe.
pub const ALPN: &[u8] = b"DUMBPIPEV0";

/// The handshake sent on an opened bidi stream.
pub const HANDSHAKE: [u8; 5] = *b"hello";

/// The ALPN for the WHIP/RTP subcommands (`listen-whip` / `connect-whip`),
pub const STREAM_ALPN: &[u8] = b"STREAMPIPE_WEBRTC_V0";

pub mod common;
pub mod gui;
pub mod rtp;
pub mod sdp;
pub mod stream;
pub mod udp;
pub mod whip;

pub use iroh_tickets::endpoint::EndpointTicket;

#[cfg(test)]
mod tests {
    use super::*;

    /// The pipe path must stay wire-compatible with upstream dumbpipe, so these
    /// are pinned and must never change.
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
}
