use excalibur_providers::{
    ChatRequest, Error, KnownFailure, Message, Provider, ProviderClient, Role, Secret,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn mock(status: &str, response: &str) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut data = Vec::new();
        loop {
            let mut buf = [0; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            if let Some(pos) = data.windows(4).position(|x| x == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..pos]);
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|x| x.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if data.len() >= pos + 4 + length {
                    break;
                }
            }
        }
        stream.write_all(reply.as_bytes()).await.unwrap();
        String::from_utf8(data).unwrap()
    });
    (format!("http://{addr}/v1/"), task)
}
fn chat() -> ChatRequest {
    ChatRequest {
        model: "test-model".into(),
        messages: vec![Message {
            role: Role::User,
            content: "hello".into(),
        }],
        max_tokens: Some(12),
    }
}
fn client(provider: Provider, url: &str) -> ProviderClient {
    ProviderClient::with_base_url(provider, Secret::new("test-secret").unwrap(), url).unwrap()
}
#[tokio::test]
async fn compatible_chat_sends_auth_and_parses_usage() {
    let (url, task) = mock("200 OK", r#"{"choices":[{"message":{"content":"answer"}}],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#).await;
    let response = client(Provider::Groq, &url).chat(&chat()).await.unwrap();
    assert_eq!(response.text, "answer");
    assert_eq!(response.input_tokens, Some(3));
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/chat/completions "));
    assert!(sent.contains("authorization: bearer test-secret"));
    assert!(sent.contains("\"max_tokens\":12"));
}
#[tokio::test]
async fn anthropic_system_and_header_are_translated() {
    let (url, task) = mock(
        "200 OK",
        r#"{"content":[{"type":"text","text":"answer"}],"usage":{"input_tokens":4}}"#,
    )
    .await;
    let mut req = chat();
    req.messages.insert(
        0,
        Message {
            role: Role::System,
            content: "rules".into(),
        },
    );
    let out = client(Provider::Anthropic, &url).chat(&req).await.unwrap();
    assert_eq!(out.input_tokens, Some(4));
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.contains("x-api-key: test-secret"));
    assert!(sent.contains("anthropic-version: 2023-06-01"));
    assert!(sent.contains("\"system\":\"rules\""));
}
#[tokio::test]
async fn google_role_mapping_and_usage() {
    let (url, task) = mock("200 OK", r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]}}],"usageMetadata":{"candidatesTokenCount":8}}"#).await;
    let mut req = chat();
    req.messages.push(Message {
        role: Role::Assistant,
        content: "prior".into(),
    });
    let out = client(Provider::GoogleAi, &url).chat(&req).await.unwrap();
    assert_eq!(out.output_tokens, Some(8));
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/models/test-model:generatecontent "));
    assert!(sent.contains("x-goog-api-key: test-secret"));
    assert!(sent.contains("\"role\":\"model\""));
}
#[tokio::test]
async fn failures_do_not_echo_sensitive_response() {
    let (url, task) = mock("401 Unauthorized", r#"{"error":"test-secret"}"#).await;
    let err = client(Provider::OpenAi, &url)
        .chat(&chat())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Http(_)));
    assert!(!format!("{err:?} {err}").contains("test-secret"));
    task.await.unwrap();
}

#[tokio::test]
async fn openai_known_http_failures_retain_only_a_closed_class_and_status() {
    for (status, field, value, expected) in [
        (
            "429 Too Many Requests",
            "code",
            "organization_spend_limit_exceeded",
            KnownFailure::Quota,
        ),
        (
            "429 Too Many Requests",
            "code",
            "project_spend_limit_exceeded",
            KnownFailure::Quota,
        ),
        (
            "429 Too Many Requests",
            "code",
            "organization_usage_limit_exceeded",
            KnownFailure::Quota,
        ),
        (
            "429 Too Many Requests",
            "code",
            "credit_balance_exhausted",
            KnownFailure::Quota,
        ),
        (
            "429 Too Many Requests",
            "code",
            "insufficient_quota",
            KnownFailure::Quota,
        ),
        (
            "429 Too Many Requests",
            "type",
            "insufficient_quota",
            KnownFailure::Quota,
        ),
        (
            "400 Bad Request",
            "code",
            "context_length_exceeded",
            KnownFailure::ContextWindow,
        ),
        (
            "503 Service Unavailable",
            "code",
            "server_is_overloaded",
            KnownFailure::Overloaded,
        ),
    ] {
        let body =
            serde_json::json!({"error":{field:value,"message":"synthetic-private-response-value"}})
                .to_string();
        let (url, task) = mock(status, &body).await;
        let error = client(Provider::OpenAi, &url)
            .chat(&chat())
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::ClassifiedHttp { status: actual, kind } if actual.as_u16() == status[..3].parse::<u16>().unwrap() && kind == expected)
        );
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains("synthetic-private"));
        assert!(!diagnostic.contains(value));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn unknown_oversized_conflicting_and_other_provider_errors_preserve_http_fallback() {
    let known = r#"{"error":{"code":"credit_balance_exhausted","message":"synthetic-private-response-value"}}"#;
    for (provider, status, body) in [
        (Provider::OpenAi, "429 Too Many Requests", r#"{"error":{"code":"synthetic-private-unknown-code","message":"credit_balance_exhausted"}}"#.to_string()),
        (Provider::OpenAi, "429 Too Many Requests", "invalid JSON synthetic-private-response-value".to_string()),
        (Provider::OpenAi, "401 Unauthorized", known.to_string()),
        (Provider::OpenAi, "400 Bad Request", known.to_string()),
        (Provider::Groq, "429 Too Many Requests", known.to_string()),
        (Provider::OpenAi, "429 Too Many Requests", format!("{known}{}", " ".repeat(16 * 1024))),
    ] {
        let (url, task) = mock(status, &body).await;
        let error = client(provider, &url).chat(&chat()).await.unwrap_err();
        assert!(matches!(error, Error::Http(actual) if actual.as_u16() == status[..3].parse::<u16>().unwrap()));
        assert!(!format!("{error:?} {error}").contains("synthetic-private"));
        task.await.unwrap();
    }
    // Unknown-length responses must obey the same cap, rather than trusting headers.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        let body = format!("{}{}", " ".repeat(16 * 1024), known);
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });
    assert!(matches!(
        client(Provider::OpenAi, &format!("http://{address}/v1/"))
            .chat(&chat())
            .await
            .unwrap_err(),
        Error::Http(_)
    ));
    task.await.unwrap();
}
#[tokio::test]
async fn image_and_droplet_have_distinct_contracts() {
    let (url, task) = mock("200 OK", r#"{"data":[{"b64_json":"image"}]}"#).await;
    assert!(
        client(Provider::OpenAi, &url)
            .generate_image("image-model", "prompt")
            .await
            .is_ok()
    );
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/images/generations ")
    );
    let (url, task) = mock("202 Accepted", r#"{"droplet":{"id":42}}"#).await;
    assert!(
        client(Provider::DigitalOcean, &url)
            .create_droplet("gpu", "region", "gpu-size", "image")
            .await
            .is_ok()
    );
    assert!(task.await.unwrap().starts_with("POST /v1/droplets "));
}
#[tokio::test]
async fn xai_video_submit_and_poll_are_separate() {
    let (url, task) = mock("200 OK", r#"{"request_id":"job-42"}"#).await;
    let id = client(Provider::Xai, &url)
        .start_video("video-model", "a scene")
        .await
        .unwrap();
    assert_eq!(id, "job-42");
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/videos/generations ")
    );
    let (url, task) = mock("200 OK", r#"{"status":"pending"}"#).await;
    assert_eq!(
        client(Provider::Xai, &url).video_status(&id).await.unwrap()["status"],
        "pending"
    );
    assert!(task.await.unwrap().starts_with("GET /v1/videos/job-42 "));
}
#[test]
fn credentials_are_redacted_and_insecure_hosts_rejected() {
    let secret = Secret::new("test-secret").unwrap();
    assert!(!format!("{secret:?}").contains("test-secret"));
    assert!(
        ProviderClient::with_base_url(Provider::OpenAi, secret, "http://example.com/").is_err()
    );
}

#[tokio::test]
async fn lambda_instance_lifecycle_requests() {
    let (url, task) = mock("200 OK", r#"{"data":{"instance_ids":["instance-1"]}}"#).await;
    let result = client(Provider::LambdaCloud, &url)
        .lambda_launch("region", "gpu-type", "ssh-name")
        .await
        .unwrap();
    assert_eq!(result["data"]["instance_ids"][0], "instance-1");
    let sent = task.await.unwrap();
    assert!(sent.starts_with("POST /v1/instance-operations/launch "));
    assert!(sent.contains("\"ssh_key_names\":[\"ssh-name\"]"));
    let (url, task) = mock("200 OK", r#"{"data":[{"id":"instance-1"}]}"#).await;
    assert!(
        client(Provider::LambdaCloud, &url)
            .lambda_instances()
            .await
            .is_ok()
    );
    assert!(task.await.unwrap().starts_with("GET /v1/instances "));
    let (url, task) = mock(
        "200 OK",
        r#"{"data":{"terminated_instances":[{"id":"instance-1"}]}}"#,
    )
    .await;
    assert!(
        client(Provider::LambdaCloud, &url)
            .lambda_terminate("instance-1")
            .await
            .is_ok()
    );
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/instance-operations/terminate ")
    );
}

#[tokio::test]
async fn cohere_chat_parses_native_content_and_tokens() {
    let (url, task) = mock("200 OK", r#"{"message":{"content":[{"type":"text","text":"answer"}]},"usage":{"tokens":{"input_tokens":5,"output_tokens":2}}}"#).await;
    let out = client(Provider::Cohere, &url).chat(&chat()).await.unwrap();
    assert_eq!(out.text, "answer");
    assert_eq!(out.input_tokens, Some(5));
    assert!(task.await.unwrap().starts_with("POST /v1/chat "));
}
#[tokio::test]
async fn replicate_submission_and_poll_use_model_specific_input() {
    let (url, task) = mock("201 Created", r#"{"id":"pred-1","status":"starting"}"#).await;
    let result = client(Provider::Replicate, &url)
        .replicate_predict("owner/model:version", serde_json::json!({"prompt":"draw"}))
        .await
        .unwrap();
    assert_eq!(result["id"], "pred-1");
    assert!(task.await.unwrap().starts_with("POST /v1/predictions "));
    let (url, task) = mock("200 OK", r#"{"status":"succeeded","output":"result"}"#).await;
    let result = client(Provider::Replicate, &url)
        .replicate_status("pred-1")
        .await
        .unwrap();
    assert_eq!(result["output"], "result");
    assert!(
        task.await
            .unwrap()
            .starts_with("GET /v1/predictions/pred-1 ")
    );
}

#[tokio::test]
async fn native_tool_calls_are_preserved_and_replayed() {
    let (url, task) = mock("200 OK", r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]}}]}"#).await;
    let request = serde_json::json!({"messages":[{"role":"user","content":"read a"}],"tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]});
    let result = client(Provider::OpenAi, &url)
        .native_chat("model", request)
        .await
        .unwrap();
    assert_eq!(
        result["choices"][0]["message"]["tool_calls"][0]["id"],
        "call-1"
    );
    let sent = task.await.unwrap();
    assert!(sent.contains("\"tools\""));
    assert!(sent.contains("\"stream\":false"));
}
#[tokio::test]
async fn streaming_retains_tool_deltas_and_terminal_marker() {
    let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\"}}]}}]}\r\n\r\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"}\"}}]}}]}\r\n\r\ndata: [DONE]\r\n\r\n";
    let (url, task) = mock("200 OK", body).await;
    let mut stream = client(Provider::OpenAi, &url)
        .stream_native_chat("model", serde_json::json!({"messages":[]}))
        .await
        .unwrap();
    let first = stream.next_event().await.unwrap().unwrap();
    assert_eq!(
        first.data["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{"
    );
    let second = stream.next_event().await.unwrap().unwrap();
    assert_eq!(
        second.data["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "}"
    );
    assert!(stream.next_event().await.unwrap().is_none());
    assert!(task.await.unwrap().contains("\"stream\":true"));
}
#[tokio::test]
async fn streaming_detects_truncated_connection() {
    let (url, task) = mock("200 OK", "data: {\"choices\":[]}\n\n").await;
    let mut stream = client(Provider::OpenAi, &url)
        .stream_native_chat("model", serde_json::json!({}))
        .await
        .unwrap();
    assert!(stream.next_event().await.unwrap().is_some());
    assert!(matches!(
        stream.next_event().await,
        Err(Error::InvalidResponse)
    ));
    task.await.unwrap();
}
#[tokio::test]
async fn fal_and_runway_jobs_use_provider_specific_auth_and_paths() {
    let (url, task) = mock("200 OK", r#"{"request_id":"job-1"}"#).await;
    let c = client(Provider::Fal, &url);
    assert!(
        c.fal_submit("fal-ai/flux/schnell", serde_json::json!({"prompt":"x"}))
            .await
            .is_ok()
    );
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/fal-ai/flux/schnell "));
    assert!(sent.contains("authorization: key test-secret"));
    assert!(
        c.fal_submit("../escape", serde_json::json!({}))
            .await
            .is_err()
    );
    let (url, task) = mock("200 OK", r#"{"id":"task-1"}"#).await;
    assert!(
        client(Provider::Runway, &url)
            .runway_submit(
                excalibur_providers::RunwayOperation::TextToVideo,
                serde_json::json!({"model":"test","promptText":"x"})
            )
            .await
            .is_ok()
    );
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/text_to_video "));
    assert!(sent.contains("x-runway-version: 2024-11-06"));
}

#[tokio::test]
async fn google_video_and_ideogram_authentication() {
    let (url, task) = mock("200 OK", r#"{"name":"models/veo/operations/abc"}"#).await;
    let c = client(Provider::GoogleAi, &url);
    c.google_video_submit("veo", serde_json::json!({"instances":[{"prompt":"x"}]}))
        .await
        .unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/models/veo:predictlongrunning "));
    assert!(sent.contains("x-goog-api-key: test-secret"));
    assert!(
        c.google_video_status("models/../../operations/id")
            .await
            .is_err()
    );
    let (url, task) = mock(
        "200 OK",
        r#"{"data":[{"url":"https://example.test/image"}]}"#,
    )
    .await;
    client(Provider::Ideogram, &url)
        .ideogram_image("draw")
        .await
        .unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/image/generate/ideogram-4-5 "));
    assert!(sent.contains("api-key: test-secret"));
}
#[tokio::test]
async fn vertex_and_foundry_use_distinct_auth_and_native_shapes() {
    let (url, task) = mock(
        "200 OK",
        r#"{"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#,
    )
    .await;
    client(Provider::Vertex, &url).chat(&chat()).await.unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/models/test-model:generatecontent "));
    assert!(sent.contains("authorization: bearer test-secret"));
    let (url, task) = mock("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#).await;
    client(Provider::Foundry, &url).chat(&chat()).await.unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.contains("api-key: test-secret"));
}

#[tokio::test]
async fn runpod_gpu_and_serverless_have_different_hosts_and_contracts() {
    let (url, task) = mock("201 Created", r#"{"id":"pod-1"}"#).await;
    let c = client(Provider::Runpod, &url);
    assert!(
        c.runpod_create_pod(serde_json::json!({"computeType":"GPU","imageName":"image"}))
            .await
            .is_ok()
    );
    assert!(task.await.unwrap().starts_with("POST /v1/pods "));
    let (url, task) = mock("200 OK", r#"{"id":"job-1","status":"IN_QUEUE"}"#).await;
    let c = client(Provider::RunpodServerless, &url);
    c.runpod_job("endpoint", serde_json::json!({"prompt":"x"}))
        .await
        .unwrap();
    let sent = task.await.unwrap();
    assert!(sent.starts_with("POST /v1/endpoint/run "));
    assert!(sent.contains("\"input\""));
}
#[tokio::test]
async fn alibaba_media_uses_async_header_and_preserves_task_id() {
    let (url, task) = mock(
        "200 OK",
        r#"{"output":{"task_id":"job-1","task_status":"PENDING"}}"#,
    )
    .await;
    let c = client(Provider::AlibabaMedia, &url);
    c.alibaba_media_submit(
        excalibur_providers::AlibabaMediaOperation::TextToImage,
        serde_json::json!({"model":"test","input":{"prompt":"x"}}),
    )
    .await
    .unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.contains("x-dashscope-async: enable"));
    assert!(sent.starts_with("post /v1/services/aigc/text2image/image-synthesis "));
}
#[tokio::test]
async fn google_stream_requires_terminal_reason() {
    let (url, task) = mock(
        "200 OK",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n",
    )
    .await;
    let mut s = client(Provider::GoogleAi, &url)
        .stream_native_chat("model", serde_json::json!({"contents":[]}))
        .await
        .unwrap();
    assert!(s.next_event().await.unwrap().is_some());
    assert!(matches!(s.next_event().await, Err(Error::InvalidResponse)));
    task.await.unwrap();
}

#[tokio::test]
async fn image_media_contracts_and_safe_polling() {
    let (url, task) = mock(
        "200 OK",
        r#"{"data":[{"url":"https://example.test/image"}]}"#,
    )
    .await;
    client(Provider::Recraft, &url)
        .recraft_image("draw")
        .await
        .unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.contains("content-type: multipart/form-data"));
    assert!(sent.contains("name=\"prompt\""));
    let (url, task) = mock(
        "200 OK",
        r#"{"id":"bfl-1","polling_url":"https://api.bfl.ai/v1/get_result?id=bfl-1"}"#,
    )
    .await;
    let c = client(Provider::BlackForest, &url);
    c.bfl_submit("flux-2-pro", serde_json::json!({"prompt":"draw"}))
        .await
        .unwrap();
    assert!(
        task.await
            .unwrap()
            .to_lowercase()
            .contains("x-key: test-secret")
    );
    assert!(
        c.bfl_status("https://bfl.ai.attacker.test/v1/get_result")
            .await
            .is_err()
    );
    let (url, task) = mock("200 OK", r#"{"id":"luma-1","state":"queued"}"#).await;
    client(Provider::Luma, &url)
        .luma_submit(serde_json::json!({"type":"video","model":"x","prompt":"draw"}))
        .await
        .unwrap();
    assert!(task.await.unwrap().starts_with("POST /v1/generations "));
}

#[tokio::test]
async fn minimax_media_distinguishes_http_success_from_api_error() {
    let (url, task) = mock(
        "200 OK",
        r#"{"base_resp":{"status_code":1004,"status_msg":"error"}}"#,
    )
    .await;
    let c = client(Provider::MiniMaxMedia, &url);
    assert!(matches!(
        c.minimax_image(serde_json::json!({"prompt":"x"})).await,
        Err(Error::InvalidResponse)
    ));
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/v1/image_generation ")
    );
    let (url, task) = mock("200 OK", r#"{"task_id":"123"}"#).await;
    client(Provider::MiniMaxMedia, &url)
        .minimax_video_submit(serde_json::json!({"model":"x","content":[]}))
        .await
        .unwrap();
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/v2/video_generation ")
    );
}

#[tokio::test]
async fn adobe_and_byteplus_use_documented_requests() {
    let (url, task) = mock("202 Accepted", r#"{"jobId":"urn:ff:jobs:test:123"}"#).await;
    let c = ProviderClient::adobe_with_base_url(
        &url,
        Secret::new("test-token").unwrap(),
        Secret::new("test-client").unwrap(),
    )
    .unwrap();
    c.adobe_image_submit(serde_json::json!({"prompt":"draw"}))
        .await
        .unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.contains("authorization: bearer test-token"));
    assert!(sent.contains("x-api-key: test-client"));
    let (url, task) = mock("200 OK", r#"{"id":"task-1"}"#).await;
    client(Provider::BytePlus, &url)
        .byteplus_video_submit(serde_json::json!({"model":"test","content":[]}))
        .await
        .unwrap();
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/contents/generations/tasks ")
    );
}
#[tokio::test]
async fn ai21_and_perplexity_use_correct_chat_paths() {
    for (provider, path) in [
        (Provider::Ai21, "/v1/chat/completions"),
        (Provider::Perplexity, "/v1/sonar"),
    ] {
        let (url, task) = mock("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#).await;
        client(provider, &url).chat(&chat()).await.unwrap();
        assert!(task.await.unwrap().starts_with(&format!("POST {path} ")));
    }
}

#[tokio::test]
async fn streaming_handles_split_utf8_and_sse_boundaries() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        for chunk in [
            &b"data: {\"text\":\""[..],
            &[0xc3][..],
            &[0xa9][..],
            &b"\"}\r\n"[..],
            &b"\r\ndata: [DONE]\n\n"[..],
        ] {
            stream
                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                .await
                .unwrap();
            stream.write_all(chunk).await.unwrap();
            stream.write_all(b"\r\n").await.unwrap();
            tokio::task::yield_now().await;
        }
        stream.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let mut s = client(Provider::OpenAi, &format!("http://{addr}/v1/"))
        .stream_native_chat("m", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(s.next_event().await.unwrap().unwrap().data["text"], "é");
    assert!(s.next_event().await.unwrap().is_none());
    task.await.unwrap();
}

#[tokio::test]
async fn deletion_accepts_empty_204_response() {
    let (url, task) = mock("204 No Content", "").await;
    client(Provider::DigitalOcean, &url)
        .digitalocean_delete_droplet(42)
        .await
        .unwrap();
    assert!(task.await.unwrap().starts_with("DELETE /v1/droplets/42 "));
    let (url, task) = mock("204 No Content", "").await;
    client(Provider::Runpod, &url)
        .runpod_delete_pod("pod-1")
        .await
        .unwrap();
    assert!(task.await.unwrap().starts_with("DELETE /v1/pods/pod-1 "));
}
#[tokio::test]
async fn vertex_video_uses_fetch_operation_contract() {
    let (url, task) = mock(
        "200 OK",
        r#"{"name":"projects/p/locations/r/publishers/google/models/veo-3.1/operations/123"}"#,
    )
    .await;
    client(Provider::Vertex, &url)
        .vertex_video_submit("veo-3.1", serde_json::json!({"instances":[{"prompt":"x"}]}))
        .await
        .unwrap();
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/models/veo-3.1:predictLongRunning ")
    );
    let (url, task) = mock("200 OK", r#"{"done":true,"response":{}}"#).await;
    client(Provider::Vertex, &url)
        .vertex_video_status(
            "veo-3.1",
            "projects/p/locations/r/publishers/google/models/veo-3.1/operations/123",
        )
        .await
        .unwrap();
    let sent = task.await.unwrap();
    assert!(sent.starts_with("POST /v1/models/veo-3.1:fetchPredictOperation "));
    assert!(sent.contains("\"operationName\""));
}

#[tokio::test]
async fn vidu_and_modal_authenticate_with_distinct_schemes() {
    let (url, task) = mock("200 OK", r#"{"task_id":"job-1","state":"created"}"#).await;
    client(Provider::Vidu, &url)
        .vidu_submit(serde_json::json!({"model":"test","prompt":"x"}))
        .await
        .unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/text2video "));
    assert!(sent.contains("authorization: token test-secret"));
    let (url, task) = mock("200 OK", r#"{"result":"ok"}"#).await;
    let c = ProviderClient::modal(
        &format!("{url}function"),
        Secret::new("proxy-key").unwrap(),
        Secret::new("proxy-secret").unwrap(),
    )
    .unwrap();
    c.modal_invoke(serde_json::json!({"input":"x"}))
        .await
        .unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/function "));
    assert!(sent.contains("modal-key: proxy-key"));
    assert!(sent.contains("modal-secret: proxy-secret"));
    assert!(!format!("{c:?}").contains("proxy-secret"));
}

#[tokio::test]
async fn pika_submission_keeps_failed_job_status_and_key_header() {
    let (url, task) = mock(
        "200 OK",
        r#"{"id":"media-1","status":"failed","error":{"code":"insufficient_balance"}}"#,
    )
    .await;
    let c = client(Provider::Pika, &url);
    let result = c
        .pika_submit(
            "pika/pika-2.5/text-to-video",
            serde_json::json!({"prompt":"x"}),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "failed");
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.contains("x-api-key: test-secret"));
    assert!(
        c.pika_submit("../../escape", serde_json::json!({}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn bedrock_together_and_stability_contracts() {
    let (url, task) = mock("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#).await;
    client(Provider::Bedrock, &url).chat(&chat()).await.unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.contains("authorization: bearer test-secret"));
    let (url, task) = mock("200 OK", r#"{"id":"video-1","status":"queued"}"#).await;
    client(Provider::TogetherVideo, &url)
        .together_video_submit(serde_json::json!({"model":"test","prompt":"x"}))
        .await
        .unwrap();
    assert!(task.await.unwrap().starts_with("POST /v1/videos "));
    let (url, task) = mock("200 OK", r#"{"image":"base64","finish_reason":"SUCCESS"}"#).await;
    client(Provider::Stability, &url)
        .stability_image("x")
        .await
        .unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.contains("accept: application/json"));
    assert!(sent.contains("content-type: multipart/form-data"));
}

#[tokio::test]
async fn anthropic_stream_emits_message_stop_and_preserves_tool_events() {
    let body = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"read\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let (url, task) = mock("200 OK", body).await;
    let mut s = client(Provider::Anthropic, &url)
        .stream_native_chat(
            "model",
            serde_json::json!({"messages":[],"max_tokens":10,"tools":[]}),
        )
        .await
        .unwrap();
    assert_eq!(
        s.next_event().await.unwrap().unwrap().data["content_block"]["id"],
        "t1"
    );
    assert_eq!(
        s.next_event().await.unwrap().unwrap().event.as_deref(),
        Some("message_stop")
    );
    assert!(s.next_event().await.unwrap().is_none());
    task.await.unwrap();
}
#[tokio::test]
async fn baseten_activation_uses_management_authentication() {
    let (url, task) = mock("200 OK", r#"{"success":true}"#).await;
    client(Provider::BasetenManage, &url)
        .baseten_activate("model-1", "deployment-2")
        .await
        .unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/models/model-1/deployments/deployment-2/activate "));
    assert!(sent.contains("authorization: bearer test-secret"));
}

#[tokio::test]
async fn baseten_query_and_deactivate_deployment() {
    let (url, task) = mock("200 OK", r#"{"id":"d1","status":"ACTIVE"}"#).await;
    let result = client(Provider::BasetenManage, &url)
        .baseten_deployment("m1", "d1")
        .await
        .unwrap();
    assert_eq!(result["status"], "ACTIVE");
    assert!(
        task.await
            .unwrap()
            .starts_with("GET /v1/models/m1/deployments/d1 ")
    );
    let (url, task) = mock("200 OK", r#"{"success":true}"#).await;
    client(Provider::BasetenManage, &url)
        .baseten_deactivate("m1", "d1")
        .await
        .unwrap();
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/models/m1/deployments/d1/deactivate ")
    );
}

#[tokio::test]
async fn google_stream_preserves_tool_call_and_finishes() {
    let (url, task) = mock("200 OK", "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"lookup\",\"args\":{}}}]},\"finishReason\":\"STOP\"}]}\n\n").await;
    let mut stream = client(Provider::GoogleAi, &url).stream_native_chat("model", serde_json::json!({"contents":[],"tools":[{"functionDeclarations":[{"name":"lookup"}]}]})).await.unwrap();
    let event = stream.next_event().await.unwrap().unwrap();
    assert_eq!(
        event.data["candidates"][0]["content"]["parts"][0]["functionCall"]["name"],
        "lookup"
    );
    assert!(stream.next_event().await.unwrap().is_none());
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/models/model:streamgeneratecontent?alt=sse "));
    assert!(sent.contains("x-goog-api-key: test-secret"));
    assert!(sent.contains("functiondeclarations"));
}

#[tokio::test]
async fn meta_text_and_image_use_shared_compatible_contracts() {
    let (url, task) = mock("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#).await;
    client(Provider::Meta, &url).chat(&chat()).await.unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/chat/completions "));
    assert!(sent.contains("authorization: bearer test-secret"));
    let (url, task) = mock("200 OK", r#"{"data":[{"b64_json":"abc"}]}"#).await;
    client(Provider::Meta, &url)
        .generate_image("muse-image-1.0", "x")
        .await
        .unwrap();
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/images/generations ")
    );
}

#[tokio::test]
async fn kling_checks_application_code_and_distinct_task_paths() {
    let (url, task) = mock(
        "200 OK",
        r#"{"code":0,"data":{"task_id":"id-1","task_status":"submitted"}}"#,
    )
    .await;
    client(Provider::Kling, &url)
        .kling_submit(
            excalibur_providers::KlingOperation::Image,
            serde_json::json!({"model_name":"m","prompt":"x"}),
        )
        .await
        .unwrap();
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/images/generations "));
    assert!(sent.contains("authorization: bearer test-secret"));
    let (url, task) = mock("200 OK", r#"{"code":123,"message":"error"}"#).await;
    assert!(matches!(
        client(Provider::Kling, &url)
            .kling_submit(
                excalibur_providers::KlingOperation::TextToVideo,
                serde_json::json!({})
            )
            .await,
        Err(Error::InvalidResponse)
    ));
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/videos/text2video ")
    );
}

#[tokio::test]
async fn cohere_stream_retains_tool_events_and_stops_on_message_end() {
    let body = "event: tool-call-delta\ndata: {\"type\":\"tool-call-delta\",\"delta\":{\"message\":{\"tool_calls\":{\"function\":{\"arguments\":\"{}\"}}}}}\n\nevent: message-end\ndata: {\"type\":\"message-end\"}\n\n";
    let (url, task) = mock("200 OK", body).await;
    let mut s = client(Provider::Cohere, &url)
        .stream_native_chat("m", serde_json::json!({"messages":[],"tools":[]}))
        .await
        .unwrap();
    assert_eq!(
        s.next_event().await.unwrap().unwrap().event.as_deref(),
        Some("tool-call-delta")
    );
    assert!(s.next_event().await.unwrap().is_some());
    assert!(s.next_event().await.unwrap().is_none());
    assert!(task.await.unwrap().starts_with("POST /v1/chat "));
}

#[tokio::test]
async fn perplexity_agent_tools_stream_and_background_contract() {
    let (url, task) = mock("200 OK", r#"{"id":"resp_1","status":"completed","output":[{"type":"function_call","name":"lookup"}]}"#).await;
    let response = client(Provider::PerplexityAgent, &url)
        .perplexity_agent(
            serde_json::json!({"input":"hi","tools":[{"type":"function","name":"lookup"}]}),
        )
        .await
        .unwrap();
    assert_eq!(response["output"][0]["name"], "lookup");
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/agent "));
    assert!(sent.contains("authorization: bearer test-secret"));
    assert!(sent.contains("\"stream\":false"));
    let (url, task) = mock("200 OK", "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n").await;
    let mut stream = client(Provider::PerplexityAgent, &url)
        .perplexity_agent_stream(serde_json::json!({"input":"hi"}))
        .await
        .unwrap();
    assert_eq!(
        stream.next_event().await.unwrap().unwrap().data["delta"],
        "hi"
    );
    assert_eq!(
        stream.next_event().await.unwrap().unwrap().data["type"],
        "response.completed"
    );
    assert!(stream.next_event().await.unwrap().is_none());
    assert!(task.await.unwrap().contains("\"stream\":true"));
    let (url, task) = mock("200 OK", r#"{"id":"resp_1","status":"cancelling"}"#).await;
    let response = client(Provider::PerplexityAgent, &url)
        .perplexity_agent_status("resp_1", true)
        .await
        .unwrap();
    assert_eq!(response["status"], "cancelling");
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/agent/resp_1/cancel ")
    );
}

#[tokio::test]
async fn replicate_official_deployment_and_cancel_paths() {
    for (deployment, prefix) in [(false, "models"), (true, "deployments")] {
        let (url, task) = mock("201 Created", r#"{"id":"p1","status":"starting"}"#).await;
        client(Provider::Replicate, &url)
            .replicate_named_prediction(
                deployment,
                "acme",
                "video-model",
                serde_json::json!({"prompt":"test"}),
            )
            .await
            .unwrap();
        let sent = task.await.unwrap();
        assert!(sent.starts_with(&format!("POST /v1/{prefix}/acme/video-model/predictions ")));
        assert!(sent.contains("\"input\""));
    }
    let (url, task) = mock("200 OK", r#"{"id":"p1","status":"canceled"}"#).await;
    let result = client(Provider::Replicate, &url)
        .replicate_cancel("p1")
        .await
        .unwrap();
    assert_eq!(result["status"], "canceled");
    assert!(
        task.await
            .unwrap()
            .starts_with("POST /v1/predictions/p1/cancel ")
    );
}

#[tokio::test]
async fn lambda_restart_requires_restart_result() {
    let (url, task) = mock(
        "200 OK",
        r#"{"data":{"restarted_instances":[{"id":"i1"}]}}"#,
    )
    .await;
    client(Provider::LambdaCloud, &url)
        .lambda_restart("i1")
        .await
        .unwrap();
    let sent = task.await.unwrap();
    assert!(sent.starts_with("POST /v1/instance-operations/restart "));
    assert!(sent.contains("\"instance_ids\":[\"i1\"]"));
}

#[tokio::test]
async fn bedrock_converse_preserves_tools_and_uses_bearer() {
    let (url, task) = mock("200 OK", r#"{"output":{"message":{"content":[{"toolUse":{"name":"lookup","toolUseId":"t1","input":{}}}]}}}"#).await;
    let response = client(Provider::BedrockNative, &url).bedrock_converse("us.anthropic.model-v1:0", serde_json::json!({"messages":[],"toolConfig":{"tools":[{"toolSpec":{"name":"lookup"}}]}})).await.unwrap();
    assert_eq!(
        response["output"]["message"]["content"][0]["toolUse"]["name"],
        "lookup"
    );
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/model/us.anthropic.model-v1:0/converse "));
    assert!(sent.contains("authorization: bearer test-secret"));
    assert!(sent.contains("toolconfig"));
}

#[tokio::test]
async fn digitalocean_gpu_list_is_filtered_and_paginated() {
    let (url, task) = mock(
        "200 OK",
        r#"{"droplets":[],"links":{"pages":{"next":"example"}}}"#,
    )
    .await;
    let c = client(Provider::DigitalOcean, &url);
    let result = c.digitalocean_gpu_droplets(2, 50).await.unwrap();
    assert!(result.pointer("/links/pages/next").is_some());
    assert!(
        task.await
            .unwrap()
            .starts_with("GET /v1/droplets?type=gpus&page=2&per_page=50 ")
    );
    assert!(matches!(
        c.digitalocean_gpu_droplets(0, 50).await,
        Err(Error::Configuration(_))
    ));
}

#[tokio::test]
async fn baseten_listing_deploy_has_native_source_and_team() {
    let (url, task) = mock(
        "200 OK",
        r#"{"model":{"id":"m1"},"deployment":{"id":"d1","status":"BUILDING"}}"#,
    )
    .await;
    let c = client(Provider::BasetenManage, &url);
    let response = c
        .baseten_deploy_listing(
            Some("team-1"),
            "Example lab",
            "example-id",
            Some("my-model"),
        )
        .await
        .unwrap();
    assert_eq!(response["deployment"]["status"], "BUILDING");
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/teams/team-1/models "));
    assert!(sent.contains("authorization: bearer test-secret"));
    assert!(sent.contains("\"kind\":\"library_listing\""));
    assert!(sent.contains("\"user_defined_listing_id\":\"example-id\""));
}

#[tokio::test]
async fn runpod_pod_lifecycle_uses_explicit_paths() {
    use excalibur_providers::RunpodPodAction;
    for (action, path) in [
        (RunpodPodAction::Start, "start"),
        (RunpodPodAction::Stop, "stop"),
        (RunpodPodAction::Restart, "restart"),
    ] {
        let (url, task) = mock("200 OK", r#"{"id":"pod-1","desiredStatus":"RUNNING"}"#).await;
        let c = client(Provider::Runpod, &url);
        c.runpod_pod_action("pod-1", action).await.unwrap();
        assert!(
            task.await
                .unwrap()
                .starts_with(&format!("POST /v1/pods/pod-1/{path} "))
        );
    }
}

fn aws_frame(event: &str, payload: &str) -> Vec<u8> {
    fn header(dst: &mut Vec<u8>, name: &str, value: &str) {
        dst.push(name.len() as u8);
        dst.extend(name.as_bytes());
        dst.push(7);
        dst.extend((value.len() as u16).to_be_bytes());
        dst.extend(value.as_bytes());
    }
    let mut headers = Vec::new();
    header(&mut headers, ":message-type", "event");
    header(&mut headers, ":event-type", event);
    let mut frame = Vec::new();
    frame.extend(((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    frame.extend((headers.len() as u32).to_be_bytes());
    frame.extend(crc32fast::hash(&frame).to_be_bytes());
    frame.extend(headers);
    frame.extend(payload.as_bytes());
    frame.extend(crc32fast::hash(&frame).to_be_bytes());
    frame
}
async fn binary_mock(data: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0; 8192];
        let n = stream.read(&mut buf).await.unwrap();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            data.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        for chunk in data.chunks(7) {
            stream.write_all(chunk).await.unwrap();
        }
        String::from_utf8_lossy(&buf[..n]).into_owned()
    });
    (format!("http://{addr}/v1/"), task)
}
#[tokio::test]
async fn bedrock_binary_stream_validates_crc_and_preserves_tool_events() {
    let mut frames = aws_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{}"}}}"#,
    );
    frames.extend(aws_frame("messageStop", r#"{"stopReason":"tool_use"}"#));
    frames.extend(aws_frame("metadata", r#"{"usage":{"outputTokens":3}}"#));
    let (url, task) = binary_mock(frames).await;
    let mut stream = client(Provider::BedrockNative, &url)
        .bedrock_converse_stream("model-v1:0", serde_json::json!({"messages":[]}))
        .await
        .unwrap();
    let delta = stream.next_event().await.unwrap().unwrap();
    assert_eq!(delta.data["delta"]["toolUse"]["input"], "{}");
    assert_eq!(
        stream.next_event().await.unwrap().unwrap().event.as_deref(),
        Some("messageStop")
    );
    assert_eq!(
        stream.next_event().await.unwrap().unwrap().data["usage"]["outputTokens"],
        3
    );
    assert!(stream.next_event().await.unwrap().is_none());
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.starts_with("post /v1/model/model-v1:0/converse-stream "));
    assert!(sent.contains("authorization: bearer test-secret"));
    let mut bad = aws_frame("messageStop", r#"{"stopReason":"end_turn"}"#);
    let index = bad.len() - 5;
    bad[index] ^= 1;
    let (url, task) = binary_mock(bad).await;
    let mut stream = client(Provider::BedrockNative, &url)
        .bedrock_converse_stream("model", serde_json::json!({}))
        .await
        .unwrap();
    assert!(matches!(
        stream.next_event().await,
        Err(Error::InvalidResponse)
    ));
    task.await.unwrap();
    let (url, task) = binary_mock(aws_frame("messageStart", r#"{"role":"assistant"}"#)).await;
    let mut stream = client(Provider::BedrockNative, &url)
        .bedrock_converse_stream("model", serde_json::json!({}))
        .await
        .unwrap();
    stream.next_event().await.unwrap();
    assert!(matches!(
        stream.next_event().await,
        Err(Error::InvalidResponse)
    ));
    task.await.unwrap();
}

#[tokio::test]
async fn replicate_stream_handles_plain_text_and_rejects_untrusted_hosts() {
    let (url, task) = mock(
        "200 OK",
        "event: output\ndata: hola\n\nevent: done\ndata: {}\n\n",
    )
    .await;
    let c = client(Provider::Replicate, &url);
    assert!(matches!(
        c.replicate_stream("https://evil.example/stream").await,
        Err(Error::Configuration(_))
    ));
    let mut stream = c.replicate_stream(&format!("{url}stream")).await.unwrap();
    assert_eq!(stream.next_event().await.unwrap().unwrap().data, "hola");
    assert_eq!(
        stream.next_event().await.unwrap().unwrap().event.as_deref(),
        Some("done")
    );
    assert!(stream.next_event().await.unwrap().is_none());
    let sent = task.await.unwrap().to_lowercase();
    assert!(sent.contains("authorization: bearer test-secret"));
    assert!(sent.contains("accept: text/event-stream"));
}

#[tokio::test]
async fn meta_responses_preserves_reasoning_history_and_streams() {
    let (url, task) = mock(
        "200 OK",
        r#"{"id":"resp1","output":[{"type":"reasoning","encrypted_content":"opaque"}]}"#,
    )
    .await;
    let c = client(Provider::Meta, &url);
    let result = c
        .meta_response(
            serde_json::json!({"model":"muse","input":"hello","previous_response_id":"resp0"}),
        )
        .await
        .unwrap();
    assert_eq!(result["output"][0]["encrypted_content"], "opaque");
    let sent = task.await.unwrap();
    assert!(sent.starts_with("POST /v1/responses "));
    assert!(sent.contains("\"previous_response_id\":\"resp0\""));
    let (url, task) = mock("200 OK", "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n").await;
    let mut stream = client(Provider::Meta, &url)
        .meta_response_stream(serde_json::json!({"model":"muse","input":"hello"}))
        .await
        .unwrap();
    assert_eq!(
        stream.next_event().await.unwrap().unwrap().data["delta"],
        "x"
    );
    assert_eq!(
        stream.next_event().await.unwrap().unwrap().event.as_deref(),
        Some("response.completed")
    );
    assert!(stream.next_event().await.unwrap().is_none());
    task.await.unwrap();
}

#[tokio::test]
async fn runpod_serverless_actions_keep_native_job_results() {
    use excalibur_providers::RunpodJobAction;
    for (action, method, path) in [
        (RunpodJobAction::Cancel, "POST", "cancel"),
        (RunpodJobAction::Retry, "POST", "retry"),
        (RunpodJobAction::Stream, "GET", "stream"),
    ] {
        let (url, task) = mock(
            "200 OK",
            r#"{"id":"job1","status":"IN_PROGRESS","stream":[{"output":"part"}]}"#,
        )
        .await;
        let result = client(Provider::RunpodServerless, &url)
            .runpod_job_action("endpoint1", "job1", action)
            .await
            .unwrap();
        assert_eq!(result["stream"][0]["output"], "part");
        assert!(
            task.await
                .unwrap()
                .starts_with(&format!("{method} /v1/endpoint1/{path}/job1 "))
        );
    }
}

#[tokio::test]
async fn openai_embeddings_send_dimensions_and_restore_input_order() {
    let (url, task) = mock(
        "200 OK",
        r#"{"data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}],"model":"text-embedding-3-small"}"#,
    )
    .await;
    let vectors = client(Provider::OpenAi, &url)
        .embeddings(
            "text-embedding-3-small",
            &["first".to_string(), "second".to_string()],
            Some(2),
        )
        .await
        .unwrap();
    assert_eq!(vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    let sent = task.await.unwrap();
    assert!(sent.starts_with("POST /v1/embeddings "));
    assert!(sent.contains("\"dimensions\":2"));
    assert!(sent.contains("\"model\":\"text-embedding-3-small\""));
    assert!(
        sent.to_lowercase()
            .contains("authorization: bearer test-secret")
    );
}

#[tokio::test]
async fn embeddings_reject_mismatched_responses_and_unsupported_providers() {
    let (url, task) = mock("200 OK", r#"{"data":[{"index":0,"embedding":[1.0]}]}"#).await;
    let error = client(Provider::OpenAi, &url)
        .embeddings("m", &["a".to_string(), "b".to_string()], None)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidResponse));
    task.await.unwrap();
    let unsupported = client(Provider::Anthropic, "http://127.0.0.1:9/v1/")
        .embeddings("m", &["a".to_string()], None)
        .await
        .unwrap_err();
    assert!(matches!(unsupported, Error::Unsupported));
}
