use githearth_server::{MIGRATOR, config::Config, connect, router};
use std::net::SocketAddr;
use std::time::Duration;
use uuid::Uuid;

fn test_config(storage_root: std::path::PathBuf, trusted_proxies: Vec<String>) -> Config {
    Config {
        database_url: std::env::var("GITHEARTH_TEST_DATABASE_URL").unwrap(),
        database_max_connections: 2,
        storage_root,
        trusted_proxies,
        ..Config::default()
    }
}

fn temp_storage() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("githearth-clientip-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn serve_with_config(config: Config) -> (tokio::sync::oneshot::Sender<()>, u16) {
    let pool = connect(&config).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    let state = githearth_server::AppState::new(pool, config).await.unwrap();
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (shutdown_tx, port)
}

/// Server log lines, captured once for this test binary.
fn logs() -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
    static LOGS: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<Vec<u8>>>> =
        std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = buf.clone();
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter("githearth_server=info")
            .with_writer(move || Sink(sink.clone()))
            .init();
        buf
    })
    .clone()
}

/// The client_ip that the server's "http request" log line records for one
/// request. Requests run one at a time so the line belongs to this request.
async fn logged_client_ip(port: u16, forwarded: Option<&str>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = ONE.lock().await;
    let logs = logs();
    logs.lock().unwrap().clear();

    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let mut req = "GET /health/live HTTP/1.0\r\nHost: 127.0.0.1\r\n".to_string();
    if let Some(fwd) = forwarded {
        req.push_str(&format!("X-Forwarded-For: {fwd}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    assert!(
        String::from_utf8_lossy(&buf).starts_with("HTTP/1.0 200"),
        "expected 200"
    );

    let text = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    let line = text
        .lines()
        .rfind(|l| l.contains("http request") && l.contains("/health/live"))
        .unwrap_or_else(|| panic!("no request log line in: {text}"))
        .to_owned();
    line.split("client_ip=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no client_ip in: {line}"))
        .trim_matches('"')
        .to_owned()
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn peer_ip_when_no_trusted_proxies() {
    let storage = temp_storage();
    let (shutdown, port) = serve_with_config(test_config(storage, vec![])).await;
    let ip = logged_client_ip(port, None).await;
    assert_eq!(ip, "127.0.0.1", "no trusted proxies → peer IP (got {ip})");
    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn header_ip_when_peer_is_trusted() {
    let storage = temp_storage();
    let (shutdown, port) = serve_with_config(test_config(storage, vec!["127.0.0.1".into()])).await;
    let ip = logged_client_ip(port, Some("203.0.113.42")).await;
    assert_eq!(
        ip, "203.0.113.42",
        "trusted peer reads X-Forwarded-For (got {ip})"
    );
    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn fallback_to_peer_on_malformed_header() {
    let storage = temp_storage();
    let (shutdown, port) = serve_with_config(test_config(storage, vec!["127.0.0.1".into()])).await;
    let ip = logged_client_ip(port, Some("not-an-ip")).await;
    assert_eq!(ip, "127.0.0.1", "malformed header → peer (got {ip})");
    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn untrusted_peer_ignores_header() {
    let storage = temp_storage();
    let (shutdown, port) = serve_with_config(test_config(storage, vec![])).await;
    let ip = logged_client_ip(port, Some("203.0.113.42")).await;
    assert_eq!(
        ip, "127.0.0.1",
        "untrusted peer ignores X-Forwarded-For (got {ip})"
    );
    let _ = shutdown.send(());
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via GITHEARTH_TEST_DATABASE_URL; see docs/foundation.md"]
async fn cidr_trusted_proxy_works() {
    let storage = temp_storage();
    let (shutdown, port) =
        serve_with_config(test_config(storage, vec!["127.0.0.0/8".into()])).await;
    let ip = logged_client_ip(port, Some("2001:db8::1")).await;
    assert_eq!(
        ip, "2001:db8::1",
        "CIDR-trusted peer reads header (got {ip})"
    );
    let _ = shutdown.send(());
}
