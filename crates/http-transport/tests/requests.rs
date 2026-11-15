use ekyc_http_transport::{decode_json, Transport};
use ekyc_ports::{ProviderError, ProviderErrorKind};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};
use std::time::Duration;

fn one_response(response: String, delay: Duration) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}/check", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(&socket);
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
            headers.push_str(&line);
        }
        let content_length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap_or(0);
        let mut request_body = vec![0; content_length];
        reader.read_exact(&mut request_body).unwrap();
        thread::sleep(delay);
        let _ = socket.write_all(response.as_bytes());
        headers
    });
    (url, server)
}

async fn post(
    response: String,
    timeout: Duration,
    delay: Duration,
) -> (Result<Value, ProviderError>, String) {
    let (url, server) = one_response(response, delay);
    let transport = Transport::new(timeout).unwrap();
    let result = transport
        .post_bytes(&url, "case-7-check-3", &json!({"ref": "demo"}))
        .await
        .and_then(|bytes| decode_json(&bytes));
    (result, server.join().unwrap())
}

#[tokio::test]
async fn returns_rate_limit_and_forwards_operation_key() {
    let (result, headers) = post(
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 0\r\n\r\n".into(),
        Duration::from_secs(3),
        Duration::ZERO,
    )
    .await;
    assert!(headers
        .to_ascii_lowercase()
        .contains("idempotency-key: case-7-check-3\r\n"));
    assert_eq!(
        result.unwrap_err(),
        ProviderError {
            kind: ProviderErrorKind::Throttled,
            retry_after_ms: Some(7000),
        }
    );
}

#[tokio::test]
async fn asynchronous_registration_requires_an_explicit_acknowledgement() {
    let transport = Transport::new(Duration::from_secs(3)).unwrap();
    for (status, accepted) in [("202 Accepted", true), ("200 OK", false)] {
        let (url, server) = one_response(
            format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n"),
            Duration::ZERO,
        );
        let result = transport
            .post_accepted(&url, "case/address", &json!({"case_id": "case"}))
            .await;
        assert_eq!(result.is_ok(), accepted);
        assert!(server
            .join()
            .unwrap()
            .to_lowercase()
            .contains("idempotency-key: case/address"));
    }
}

#[tokio::test]
async fn timeout_is_recoverable_and_corrupt_success_is_permanent() {
    let ok = "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\noops";
    let (slow, _) = post(
        ok.into(),
        Duration::from_millis(150),
        Duration::from_millis(350),
    )
    .await;
    assert_eq!(slow.unwrap_err().kind, ProviderErrorKind::Transient);
    let (corrupt, _) = post(ok.into(), Duration::from_secs(3), Duration::ZERO).await;
    assert_eq!(corrupt.unwrap_err().kind, ProviderErrorKind::Permanent);
}

#[tokio::test]
async fn limits_length_even_for_chunked_body() {
    // A JSON string just over the limit, without Content-Length.
    let body = format!("\"{}\"", "a".repeat(64 * 1024));
    let response = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
        body.len(),
        body
    );
    let (result, _) = post(response, Duration::from_secs(3), Duration::ZERO).await;
    assert_eq!(result.unwrap_err().kind, ProviderErrorKind::Permanent);
}
