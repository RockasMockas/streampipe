//! A minimal WHIP (WebRTC-HTTP ingestion protocol) server.
//!
//! OBS Studio 30 and newer has a built-in WHIP output: service "WHIP", server
//! URL and bearer token. This server answers the SDP offer OBS posts with the
//! SDP answer of a [`crate::stream`] session, which then receives the media
//! over ICE/DTLS-SRTP on the local network.
//!
//! Only what OBS actually uses is implemented:
//!
//! * `POST` (and `PUT`, for older OBS versions that negotiate that way) with
//!   `Content-Type: application/sdp` creates or renegotiates a session and
//!   answers `201` with `Content-Type: application/sdp`, a `Location` and an
//!   `ETag`.
//! * `PATCH` with `application/trickle-ice-sdpfrag` adds remote ICE
//!   candidates and answers `204`.
//! * `DELETE` closes the session and answers `200`.
//! * `OPTIONS` advertises the accepted content types.
//!
//! Anything else is `405`. Requests that are malformed or too large get a
//! plain text reason so that OBS shows something useful in its log.

use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, ALLOW, AUTHORIZATION, CONTENT_TYPE, ETAG, IF_MATCH, LOCATION};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use n0_error::{Result, StdResultExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

/// The content type of an SDP offer or answer.
const SDP: &str = "application/sdp";
/// The content type of a trickle ICE fragment.
const TRICKLE: &str = "application/trickle-ice-sdpfrag";
/// The largest offer we are willing to read.
const MAX_SDP: usize = 256 * 1024;

/// The answer of a negotiation, plus the entity tag of the resource.
#[derive(Debug)]
pub struct Answer {
    /// The SDP answer.
    pub sdp: String,
    /// The entity tag to advertise for subsequent `PUT`/`PATCH`/`DELETE`.
    pub etag: String,
}

/// A request from the HTTP server to the media session.
#[derive(Debug)]
pub enum WhipRequest {
    /// An offer to create or renegotiate a session.
    Offer {
        /// The SDP offer.
        sdp: String,
        /// Whether the offer arrived as `PUT` rather than `POST`.
        via_put: bool,
        /// Where to send the answer.
        reply: oneshot::Sender<Result<Answer, String>>,
    },
    /// A trickle ICE fragment.
    Trickle {
        /// The `application/trickle-ice-sdpfrag` body.
        sdp: String,
    },
    /// The publisher closed the session.
    Delete,
}

/// State shared by all connections of one WHIP server.
struct State {
    /// The bearer token to require, if any.
    token: Option<String>,
    /// The current entity tag, updated by the media session.
    etag: std::sync::Mutex<String>,
    /// Where to send requests.
    tx: mpsc::Sender<WhipRequest>,
}

impl State {
    fn authorized(&self, header: Option<&str>) -> bool {
        match &self.token {
            None => true,
            Some(token) => {
                let Some(header) = header else {
                    return false;
                };
                header
                    .strip_prefix("Bearer ")
                    .or_else(|| header.strip_prefix("bearer "))
                    .is_some_and(|given| given == token)
            }
        }
    }

    fn etag(&self) -> String {
        self.etag.lock().unwrap().clone()
    }

    fn set_etag(&self, etag: &str) {
        *self.etag.lock().unwrap() = etag.to_string();
    }

    /// Whether an `If-Match` header matches the current entity tag.
    ///
    /// A missing header is accepted, OBS does not always send one.
    fn matches(&self, header: Option<&str>) -> bool {
        match header {
            None => true,
            Some(value) => value.trim_matches('"') == self.etag().trim_matches('"'),
        }
    }
}

/// A [`JoinSet`] whose tasks die with the set: dropping it aborts every task
/// that has not finished.
///
/// `tokio::spawn` *detaches*: aborting the session task that drives
/// [`serve`] would drop the accept loop and its listener, but plainly spawned
/// connection tasks would keep running, holding the accepted sockets open and
/// answering requests forever. Keeping the tasks in this guarded set makes the
/// teardown complete — when the [`serve`] future is dropped (the GUI's "Stop
/// Broadcasting" aborts the session task), the listener is closed *and* every
/// connection task is aborted, so no socket, connection, or request handling
/// outlives the session.
pub struct AbortOnDrop<T: 'static>(pub JoinSet<T>);

impl<T: 'static> std::ops::Deref for AbortOnDrop<T> {
    type Target = JoinSet<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: 'static> std::ops::DerefMut for AbortOnDrop<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T: 'static> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        // `abort_all` is the synchronous counterpart of async `shutdown`: no
        // `.await` needed — tasks are cancelled at their next suspension point
        // and their sockets are dropped with them.
        self.0.abort_all();
    }
}

/// Serve WHIP requests on `addr` until the returned future is dropped.
///
/// Offers, trickle fragments and deletes are forwarded to the sender, which is
/// expected to be owned by the media session loop.
///
/// Teardown is complete: dropping the future closes the listening socket *and*
/// aborts every connection task (they live in an [`AbortOnDrop`] set), so
/// nothing keeps the port's connections open or keeps processing requests
/// after the session is stopped.
pub async fn serve(
    addr: SocketAddr,
    token: Option<String>,
    tx: mpsc::Sender<WhipRequest>,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .std_context(format!("error binding whip http server to {addr}"))?;
    let bound = listener
        .local_addr()
        .std_context("error getting whip http server address")?;
    tracing::info!("whip input ready on http://{bound}/whip");

    let state = Arc::new(State {
        token,
        etag: std::sync::Mutex::new(String::new()),
        tx,
    });

    // Connection tasks live in a guarded set so they die with the server:
    // plainly spawned tasks are detached and would outlive `serve`, keeping
    // the accepted sockets open and answering requests forever.
    let mut conns = AbortOnDrop::<()>(JoinSet::new());
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => {
                accepted.std_context("error accepting tcp connection")?
            }
            // Reap finished connections so the set does not grow without
            // bound. The guard keeps the branch from completing instantly
            // while the set is empty, which would spin the loop.
            Some(joined) = conns.join_next(), if !conns.is_empty() => {
                if let Err(cause) = joined {
                    tracing::debug!("a whip http connection task was cancelled: {cause}");
                }
                continue;
            }
        };
        let io = TokioIo::new(stream);
        let state = state.clone();
        tracing::debug!("got whip http connection from {peer}");
        conns.spawn(async move {
            let service = service_fn(move |req| {
                let state = state.clone();
                async move { handle(req, state).await }
            });
            if let Err(cause) = http1::Builder::new().serve_connection(io, service).await {
                tracing::debug!("whip http connection from {peer} closed: {cause}");
            }
        });
    }
}

/// Answer one HTTP request.
async fn handle(
    req: Request<Incoming>,
    state: Arc<State>,
) -> std::result::Result<Response<Full<Bytes>>, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let auth = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());
    let if_match = req
        .headers()
        .get(IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());
    let content_type = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());

    let response = match method {
        Method::OPTIONS => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(HeaderName::from_static("accept-post"), SDP)
            .header(ALLOW, "POST, PUT, PATCH, DELETE, OPTIONS")
            .body(Full::new(Bytes::new()))
            .unwrap(),
        Method::POST | Method::PUT if !state.authorized(auth.as_deref()) => {
            tracing::warn!("rejecting whip offer with a bad or missing bearer token");
            text(
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token\n",
            )
        }
        Method::PUT if !state.matches(if_match.as_deref()) => text(
            StatusCode::PRECONDITION_FAILED,
            "if-match does not match the current session\n",
        ),
        Method::POST | Method::PUT => {
            offer(req, state, method == Method::PUT, content_type, path).await
        }
        Method::PATCH => {
            if !state.authorized(auth.as_deref()) {
                text(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid bearer token\n",
                )
            } else {
                trickle(req, state, content_type).await
            }
        }
        Method::DELETE => {
            if !state.authorized(auth.as_deref()) {
                text(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid bearer token\n",
                )
            } else {
                tracing::info!("whip session deleted by the publisher");
                let _ = state.tx.send(WhipRequest::Delete).await;
                ok(StatusCode::OK)
            }
        }
        _ => text(
            StatusCode::METHOD_NOT_ALLOWED,
            "use POST with an application/sdp offer\n",
        ),
    };
    Ok(response)
}

/// Read an offer body and forward it to the media session.
async fn offer(
    req: Request<Incoming>,
    state: Arc<State>,
    via_put: bool,
    content_type: Option<String>,
    path: String,
) -> Response<Full<Bytes>> {
    let content_type = content_type.unwrap_or_default();
    if !content_type.starts_with(SDP) {
        return text(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            &format!("expected content-type {SDP}, got {content_type}\n"),
        );
    }
    let sdp = match read_body(req).await {
        Ok(sdp) => sdp,
        Err(response) => return response,
    };
    if !sdp.starts_with("v=0") {
        return text(StatusCode::BAD_REQUEST, "not an SDP: missing v=0\n");
    }

    let (reply, answered) = oneshot::channel();
    if state
        .tx
        .send(WhipRequest::Offer {
            sdp,
            via_put,
            reply,
        })
        .await
        .is_err()
    {
        return text(
            StatusCode::SERVICE_UNAVAILABLE,
            "the media session is not accepting offers\n",
        );
    }
    let answer = match answered.await {
        Ok(Ok(answer)) => answer,
        Ok(Err(reason)) => return text(StatusCode::BAD_REQUEST, &format!("{reason}\n")),
        Err(_) => {
            return text(
                StatusCode::SERVICE_UNAVAILABLE,
                "the media session is not accepting offers\n",
            )
        }
    };
    state.set_etag(&answer.etag);
    tracing::info!(
        "answered a whip {} for {path}",
        if via_put { "PUT" } else { "POST" }
    );
    Response::builder()
        .status(StatusCode::CREATED)
        .header(CONTENT_TYPE, SDP)
        .header(LOCATION, format!("{path}/{}", answer.etag))
        .header(ETAG, format!("\"{}\"", answer.etag))
        .body(Full::new(Bytes::from(answer.sdp)))
        .unwrap()
}

/// Read a trickle ICE fragment and forward it to the media session.
async fn trickle(
    req: Request<Incoming>,
    state: Arc<State>,
    content_type: Option<String>,
) -> Response<Full<Bytes>> {
    let content_type = content_type.unwrap_or_default();
    if !content_type.starts_with(TRICKLE) && !content_type.starts_with(SDP) {
        return text(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            &format!("expected content-type {TRICKLE}, got {content_type}\n"),
        );
    }
    let sdp = match read_body(req).await {
        Ok(sdp) => sdp,
        Err(response) => return response,
    };
    if state.tx.send(WhipRequest::Trickle { sdp }).await.is_err() {
        return text(
            StatusCode::SERVICE_UNAVAILABLE,
            "the media session is not accepting candidates\n",
        );
    }
    ok(StatusCode::NO_CONTENT)
}

/// Read a body, refusing anything larger than [`MAX_SDP`].
async fn read_body(req: Request<Incoming>) -> std::result::Result<String, Response<Full<Bytes>>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut body = req.into_body();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| too_large())?;
        let Ok(data) = frame.into_data() else {
            return Err(too_large());
        };
        if buf.len() + data.len() > MAX_SDP {
            return Err(too_large());
        }
        buf.extend_from_slice(&data);
    }
    String::from_utf8(buf)
        .map_err(|_| text(StatusCode::BAD_REQUEST, "the offer is not valid utf8\n"))
}

/// A `413` telling the publisher to lower the offer size.
fn too_large() -> Response<Full<Bytes>> {
    text(
        StatusCode::PAYLOAD_TOO_LARGE,
        "the offer is larger than 256 KiB\n",
    )
}

/// A response with a body and no content type.
fn ok(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

/// A plain text response.
fn text(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;
    use tokio::time::{timeout, Duration};

    /// Stopping the WHIP server must tear down *everything*: the listening
    /// socket is freed and every accepted connection is closed. Regression
    /// test for the "Stop Broadcasting" bug: connection tasks were spawned
    /// with plain `tokio::spawn`, so they were detached, survived the
    /// session abort, and kept the port's established connection open and
    /// answering requests forever.
    #[tokio::test]
    async fn dropping_serve_frees_the_port_and_closes_connections() {
        // A free port: bind a probe, take its port, drop the probe.
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let (tx, _rx) = mpsc::channel::<WhipRequest>(16);
        let handle = tokio::spawn(serve(addr, None, tx));

        // Open a connection; the server accepts it and keeps it alive.
        let mut client = loop {
            match TcpStream::connect(addr).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };

        // Stop: abort the task running `serve`.
        handle.abort();
        let _ = handle.await;

        // The accepted connection must close — as a clean EOF or a reset
        // (dropping hyper's `serve_connection` closes the socket), not be
        // kept alive by a detached task.
        let mut buf = [0u8; 1];
        let read = timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("the connection must close");
        let closed = match read {
            Ok(0) => true, // clean EOF
            Err(ref e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof
            ),
            Ok(_) => false, // a byte read: the connection is still alive
        };
        assert!(closed, "expected the connection to close, got {read:?}");

        // The listening socket must be gone: connecting is refused.
        let refused = timeout(Duration::from_secs(2), async {
            while TcpStream::connect(addr).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        assert!(refused, "the listening socket must be freed");
    }
}
