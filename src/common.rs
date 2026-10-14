//! CLI plumbing shared by the binary and the GUI.
//!
//! The GUI starts endpoints, picks ALPNs and reads verbosity the same way the CLI
//! subcommands do, so that plumbing lives in the lib rather than in `main.rs`.

use std::{
    net::{SocketAddrV4, SocketAddrV6},
    sync::Arc,
    time::Duration,
};

use clap::Parser;
use iroh::{
    endpoint::{presets, QuicTransportConfig, QuicTransportConfigBuilder},
    Endpoint, SecretKey,
};
use n0_error::{Result, StdResultExt};
use tracing::Level;

use crate::{pace::MediaCongestion, udp};

/// How long to wait for the endpoint to come online before giving up.
pub const ONLINE_TIMEOUT: Duration = Duration::from_secs(5);

/// The highest verbosity level supported by `--verbose` / `--verbose-max`.
pub const MAX_VERBOSE: u8 = 3;

/// QUIC datagram buffers for the media path.
pub const DATAGRAM_BUFFER: usize = 8 * 1024 * 1024;

/// Options shared by every subcommand.
#[derive(Parser, Debug)]
pub struct CommonArgs {
    /// The IPv4 address the endpoint listens on. Default: a random free port,
    /// useful to pin when a firewall rule needs a fixed one.
    #[clap(long, default_value = None)]
    pub ipv4_addr: Option<SocketAddrV4>,

    /// The IPv6 address the endpoint listens on. Default: a random free port.
    #[clap(long, default_value = None)]
    pub ipv6_addr: Option<SocketAddrV6>,

    /// A custom ALPN, for driving an existing iroh protocol.
    ///
    /// Both sides must pass the same value; the listen side then expects no
    /// handshake. Byte strings: prefix with `utf8:` for text, otherwise hex.
    #[clap(long)]
    pub custom_alpn: Option<String>,

    /// Verbosity; repeat to increase. Each level includes the ones below it.
    ///
    /// 0: errors only. 1 (`-v`): connection lifecycle. 2 (`-vv`): debug, every
    /// stream/datagram event and periodic stats. 3 (`-vvv`): trace, per-chunk
    /// detail plus iroh's own logging. Capped by `--verbose-max`; `RUST_LOG`
    /// overrides everything.
    #[clap(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// The highest level `--verbose` can reach: effective verbosity is
    /// `min(verbose, verbose_max)`, so flags set globally (alias, wrapper
    /// script) don't force noisy logging on every invocation.
    #[clap(long, default_value_t = MAX_VERBOSE)]
    pub verbose_max: u8,
}

impl CommonArgs {
    pub fn alpn(&self) -> Result<Vec<u8>> {
        Ok(match &self.custom_alpn {
            Some(alpn) => parse_alpn(alpn)?,
            None => crate::ALPN.to_vec(),
        })
    }

    pub fn is_custom_alpn(&self) -> bool {
        self.custom_alpn.is_some()
    }

    /// The effective verbosity: the `-v` count, clamped to `0..=MAX_VERBOSE`.
    pub fn verbosity(&self) -> u8 {
        self.verbose.min(self.verbose_max).min(MAX_VERBOSE)
    }
}

/// The log level for a verbosity level.
pub fn log_level(verbosity: u8) -> Level {
    match verbosity {
        0 => Level::ERROR,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    }
}

/// Whether per-tunnel periodic stats should be logged while running.
pub fn stats_enabled(verbosity: u8) -> bool {
    verbosity >= 2
}

pub fn parse_alpn(alpn: &str) -> Result<Vec<u8>> {
    Ok(if let Some(text) = alpn.strip_prefix("utf8:") {
        text.as_bytes().to_vec()
    } else {
        hex::decode(alpn).anyerr()?
    })
}

/// The ALPN for a WHIP/RTP media tunnel
pub fn stream_alpn(common: &CommonArgs) -> Result<Vec<u8>> {
    Ok(match &common.custom_alpn {
        Some(alpn) => parse_alpn(alpn)?,
        None => crate::STREAM_ALPN.to_vec(),
    })
}

/// The ALPN for a udp tunnel.
pub fn udp_alpn(common: &CommonArgs) -> Result<Vec<u8>> {
    Ok(match &common.custom_alpn {
        Some(alpn) => parse_alpn(alpn)?,
        None => udp::ALPN.to_vec(),
    })
}

/// The QUIC transport config for a tunnel: default state machine, widened
/// datagram buffers. See [`DATAGRAM_BUFFER`].
pub fn tunnel_transport_config() -> QuicTransportConfig {
    tunnel_transport().build()
}

/// The QUIC transport config for a WHIP/RTP media tunnel: the tunnel config
/// with [`MediaCongestion`] in place of the default loss-based controller, which
/// throttles a lossy path below the bitrate of a live stream that cannot slow
/// down. The host's [`crate::pace::Pacer`] shapes the media instead.
pub fn media_transport_config() -> QuicTransportConfig {
    tunnel_transport()
        .congestion_controller_factory(Arc::new(MediaCongestion))
        .build()
}

fn tunnel_transport() -> QuicTransportConfigBuilder {
    QuicTransportConfig::builder()
        .datagram_receive_buffer_size(Some(DATAGRAM_BUFFER))
        .datagram_send_buffer_size(DATAGRAM_BUFFER)
}

/// Bind an endpoint for a tunnel (tcp, udp, unix); see [`tunnel_transport_config`].
pub async fn create_endpoint(
    secret_key: SecretKey,
    common: &CommonArgs,
    alpns: Vec<Vec<u8>>,
) -> Result<Endpoint> {
    bind_endpoint(secret_key, common, alpns, tunnel_transport_config()).await
}

/// Bind an endpoint for the WHIP/RTP media path; see [`media_transport_config`].
pub async fn create_media_endpoint(
    secret_key: SecretKey,
    common: &CommonArgs,
    alpns: Vec<Vec<u8>>,
) -> Result<Endpoint> {
    bind_endpoint(secret_key, common, alpns, media_transport_config()).await
}

/// Bind an endpoint with the given ALPNs, transport and the addresses from `common`.
async fn bind_endpoint(
    secret_key: SecretKey,
    common: &CommonArgs,
    alpns: Vec<Vec<u8>>,
    transport: QuicTransportConfig,
) -> Result<Endpoint> {
    for alpn in &alpns {
        tracing::debug!(
            "endpoint alpn: {} (hex {})",
            String::from_utf8_lossy(alpn),
            hex::encode(alpn)
        );
    }
    if alpns.is_empty() {
        tracing::debug!("endpoint has no alpns (connect-only)");
    }
    let mut builder = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .alpns(alpns)
        .transport_config(transport);
    if let Some(addr) = common.ipv4_addr {
        tracing::debug!("binding ipv4 addr {addr}");
        builder = builder.bind_addr(addr)?;
    }
    if let Some(addr) = common.ipv6_addr {
        tracing::debug!("binding ipv6 addr {addr}");
        builder = builder.bind_addr(addr)?;
    }
    let endpoint = builder.bind().await.anyerr()?;
    tracing::debug!("endpoint bound: {:?}", endpoint.addr());
    Ok(endpoint)
}
