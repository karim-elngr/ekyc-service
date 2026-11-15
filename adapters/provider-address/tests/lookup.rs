use ekyc_domain::Observation;
use ekyc_http_transport::Transport;
use ekyc_ports::{Provider, ScreeningLookup};
use ekyc_provider_address::AddressProvider;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    time::Duration,
};

#[tokio::test]
async fn result_only_lookup_sends_only_the_operation_id_and_recovers_the_vendor_timestamp() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let vendor = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(&mut conn);
        let mut header = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                length = n.trim().parse().unwrap();
            }
            header.push_str(&line);
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        assert!(header.starts_with("POST /addresses/result HTTP/1.1"));
        assert!(header
            .to_ascii_lowercase()
            .contains("idempotency-key: original-address"));
        assert_eq!(body, br#"{"operation_id":"original-address"}"#);
        let reply = r#"{"status":"complete","result":{"evaluated_at":"2026-11-11T10:00:00Z","status":"PASS","confidence":97,"proof_date":"2026-11-01"}}"#;
        write!(
            conn,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        )
        .unwrap();
    });
    let provider = AddressProvider::new(Transport::new(Duration::from_secs(2)).unwrap(), base);
    let ScreeningLookup::Complete(facts) =
        provider.screening_result("original-address").await.unwrap()
    else {
        panic!("missing vendor result")
    };
    assert_eq!(facts.evaluated_at.to_rfc3339(), "2026-11-11T10:00:00+00:00");
    assert!(matches!(
        facts.result,
        Observation::Address {
            passed: true,
            confidence: 97.0,
            ..
        }
    ));
    vendor.join().unwrap();
}
