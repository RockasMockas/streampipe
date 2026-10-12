//! The WHIP HTTP server's lifecycle.
//!
//! Stopping the server must tear down *everything*: the listening socket is freed
//! and every accepted connection is closed. Regression test for the "Stop
//! Broadcasting" bug: connection tasks were spawned with plain `tokio::spawn`, so
//! they were detached, survived the session abort, and kept the port's established
//! connection open and answering requests forever.

use std::net::SocketAddr;

use streampipe::whip::{serve, WhipRequest};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

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
