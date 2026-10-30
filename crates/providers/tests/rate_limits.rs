use excalibur_providers::{Provider, ProviderClient, Secret};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
#[tokio::test]
async fn response_numeric_rate_ceilings_are_observed_without_retaining_raw_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        for headers in [
            "x-ratelimit-limit-requests: 60\r\nx-ratelimit-limit-tokens: 150000\r\n",
            "x-ratelimit-limit-requests: secret-invalid\r\nx-ratelimit-limit-tokens: 0\r\n",
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(head) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let length = String::from_utf8_lossy(&bytes[..head])
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= head + 4 + length {
                        break;
                    }
                }
            }
            assert!(bytes.starts_with(b"POST /v1/chat/completions "));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{headers}Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let client = ProviderClient::with_base_url(
        Provider::OpenAi,
        Secret::new("synthetic-key").unwrap(),
        &format!("http://{address}/v1/"),
    )
    .unwrap();
    assert_eq!(client.rate_limits(), None);
    client
        .native_chat(
            "synthetic",
            serde_json::json!({"model":"synthetic","input":"test"}),
        )
        .await
        .unwrap();
    assert_eq!(client.rate_limits(), Some((60, 150000)));
    client
        .native_chat(
            "synthetic",
            serde_json::json!({"model":"synthetic","input":"test"}),
        )
        .await
        .unwrap();
    assert_eq!(client.rate_limits(), Some((60, 150000)));
    fixture.await.unwrap();
}
