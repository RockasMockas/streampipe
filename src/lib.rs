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
