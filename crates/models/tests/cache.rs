use excalibur_models::{
    CacheIdentity, CacheObservation, FanoutRelease, GatewayConfig, ModelError, ModelGateway,
    ModelRole, OpenAiGateway, StreamDelta, StreamSink, complete_fanout,
};
use excalibur_providers::{Provider, ProviderClient, Secret};
use excalibur_types::runtime::{
    CacheBreakpoint, CacheTtl, Message, ModelRequest, PromptCache, ToolDefinition,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, mpsc},
};
async fn read_request(socket: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]);
            let n: usize = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + n {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + n]).unwrap();
            }
        }
    }
}
async fn send(socket: &mut TcpStream, body: &str) {
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
}
async fn send_sse(socket: &mut TcpStream, body: &str) {
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
}
async fn mock(replies: Vec<Value>) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = vec![];
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            send(&mut socket, &reply.to_string()).await;
        }
        requests
    });
    (format!("http://{address}/v1/"), task)
}
fn gateway(provider: Provider, url: &str) -> OpenAiGateway {
    let mut config = GatewayConfig::new("synthetic-model", ModelRole::Worker);
    if provider == Provider::Anthropic {
        config.native_system_updates = Some(true);
    }
    OpenAiGateway::new(
        ProviderClient::with_base_url(provider, Secret::new("synthetic-key").unwrap(), url)
            .unwrap(),
        config,
    )
    .unwrap()
}

fn req() -> ModelRequest {
    ModelRequest {
        messages: vec![
            Message::text("system", "L0 core"),
            Message::text("system", "L1 identity"),
            Message::text("system", "L2 scope"),
            Message::text("system", "L3 task"),
            Message::text("user", "L4 activity"),
            Message::text("system", "L5 now=synthetic"),
        ],
        tools: vec![ToolDefinition {
            name: "read_file".into(),
            description: "Read synthetic text".into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        }],
        tools_enabled: true,
        cache: Some(PromptCache {
            namespace_key: CacheIdentity::new("principal", "worker", "scope")
                .unwrap()
                .namespace_key,
            ttl: CacheTtl::FiveMinutes,
            breakpoints: vec![0, 1, 2, 4]
                .into_iter()
                .map(|message_index| CacheBreakpoint { message_index })
                .collect(),
            stable_prefix_hash: "a".repeat(64),
        }),
        ..Default::default()
    }
}
fn oa_reply(cached: Option<u64>) -> Value {
    let mut raw = json!({"choices":[{"finish_reason":"stop","message":{"content":"ok"}}],"usage":{"prompt_tokens":100,"completion_tokens":5}});
    if let Some(cached) = cached {
        raw["usage"]["prompt_tokens_details"]["cached_tokens"] = json!(cached);
    }
    raw
}
fn an_reply(read: u64, write: u64) -> Value {
    json!({"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"cache_read_input_tokens":read,"cache_creation_input_tokens":write,"cache_creation":{"ephemeral_5m_input_tokens":write,"ephemeral_1h_input_tokens":0},"output_tokens":5}})
}
fn marks(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(map.contains_key("cache_control")) + map.values().map(marks).sum::<usize>()
        }
        Value::Array(a) => a.iter().map(marks).sum(),
        _ => 0,
    }
}
#[test]
fn namespace_is_private_unambiguous_and_isolates_each_dimension() {
    let a = CacheIdentity::new("ab", "c", "d").unwrap();
    assert_eq!(a, CacheIdentity::new("ab", "c", "d").unwrap());
    for b in [
        CacheIdentity::new("a", "bc", "d").unwrap(),
        CacheIdentity::new("ab", "x", "d").unwrap(),
        CacheIdentity::new("ab", "c", "e").unwrap(),
    ] {
        assert_ne!(a, b);
    }
    assert_eq!(a.namespace_key.len(), 64);
    assert!(!a.namespace_key.contains("principal"));
    assert!(CacheIdentity::new("", "x", "y").is_err());
}
#[tokio::test]
async fn anthropic_marks_are_real_four_ttl_blocks_with_volatile_tail_and_inclusive_accounting() {
    let (url, server) = mock(vec![an_reply(0, 90), an_reply(90, 0)]).await;
    let gateway = gateway(Provider::Anthropic, &url);
    let request = req();
    let first = gateway.complete_observed(request.clone()).await.unwrap();
    let mut second = request.clone();
    second.cache.as_mut().unwrap().ttl = CacheTtl::OneHour;
    second.messages.last_mut().unwrap().content = "L5 now=next".into();
    let next = gateway.complete_observed(second).await.unwrap();
    assert_eq!(first.metrics.usage.input_tokens, 100);
    assert_eq!(first.metrics.usage.cache_write_tokens, Some(90));
    assert_eq!(first.metrics.usage.cache_write_by_ttl["5m"], 90);
    assert_eq!(next.metrics.usage.cache_read_tokens, Some(90));
    assert!(!next.metrics.usage.estimated);
    let sent = server.await.unwrap();
    for (index, body) in sent.iter().enumerate() {
        assert_eq!(marks(body), 4);
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(
            body["system"][0]["cache_control"]["ttl"],
            if index == 0 { "5m" } else { "1h" }
        );
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(body["messages"][1]["role"], "system");
        assert!(body["messages"][1]["content"][0]["cache_control"].is_null());
        assert!(body.get("prompt_cache_key").is_none());
    }
    for (a, b) in sent[0]["system"]
        .as_array()
        .unwrap()
        .iter()
        .zip(sent[1]["system"].as_array().unwrap())
        .take(3)
    {
        assert_eq!(a["text"], b["text"]);
    }
}
#[tokio::test]
async fn openai_stable_key_and_catalogue_change_only_tool_choice_when_disabled() {
    let (url, server) = mock(vec![oa_reply(Some(0)), oa_reply(Some(90)), oa_reply(None)]).await;
    let gateway = gateway(Provider::OpenAi, &url);
    let request = req();
    let a = gateway.complete_observed(request.clone()).await.unwrap();
    let mut closing = request.clone();
    closing.tools_enabled = false;
    let b = gateway.complete_observed(closing.clone()).await.unwrap();
    let comparison = CacheObservation::compare(Some((&request, &a)), &closing, &b);
    assert!(comparison.unchanged_prefix);
    assert_eq!(comparison.cache_read_tokens, Some(90));
    let no_hit = CacheObservation::compare(Some((&request, &a)), &request, &a);
    assert!(no_hit.unchanged_prefix);
    assert_eq!(no_hit.cache_read_tokens, Some(0));
    let c = gateway.complete_observed(request).await.unwrap();
    assert_eq!(a.metrics.usage.cache_read_tokens, Some(0));
    assert_eq!(b.metrics.usage.cache_read_tokens, Some(90));
    assert_eq!(c.metrics.usage.cache_read_tokens, None);
    assert_eq!(c.metrics.usage.cache_write_tokens, None);
    let sent = server.await.unwrap();
    assert_eq!(sent[0]["prompt_cache_key"], sent[1]["prompt_cache_key"]);
    assert_eq!(sent[0]["tools"], sent[1]["tools"]);
    assert_eq!(sent[0]["tool_choice"], "auto");
    assert_eq!(sent[1]["tool_choice"], "none");
    assert_eq!(marks(&sent[0]), 0);
    assert!(sent[0].get("prompt_cache_retention").is_none());
}
#[tokio::test]
async fn prefix_providers_send_no_speculative_controls_and_deepseek_miss_is_not_write() {
    for provider in [
        Provider::DeepSeek,
        Provider::Alibaba,
        Provider::Zai,
        Provider::Moonshot,
        Provider::Ollama,
    ] {
        let mut reply = oa_reply(None);
        if provider == Provider::DeepSeek {
            reply["usage"]["prompt_cache_hit_tokens"] = json!(80);
            reply["usage"]["prompt_cache_miss_tokens"] = json!(20);
        } else if provider != Provider::Ollama {
            reply["usage"]["prompt_tokens_details"]["cached_tokens"] = json!(80);
        }
        if provider == Provider::Moonshot {
            reply["usage"]["prompt_tokens_details"]["cache_write_tokens"] = json!(10);
        }
        let (url, server) = mock(vec![reply]).await;
        let observed = gateway(provider, &url)
            .complete_observed(req())
            .await
            .unwrap();
        assert_eq!(
            observed.metrics.usage.cache_read_tokens,
            if provider == Provider::Ollama {
                None
            } else {
                Some(80)
            }
        );
        assert_eq!(
            observed.metrics.usage.cache_write_tokens,
            if provider == Provider::Moonshot {
                Some(10)
            } else {
                None
            }
        );
        assert_eq!(observed.metrics.usage.input_tokens, 100);
        let sent = server.await.unwrap();
        assert_eq!(marks(&sent[0]), 0);
        assert!(sent[0].get("prompt_cache_key").is_none());
    }
}
#[tokio::test]
async fn gemini_native_signature_survives_durable_serialization_and_tool_history() {
    let parts = json!([{"thought":true,"text":"opaque reasoning summary","thoughtSignature":"c3ludGhldGlj"},{"functionCall":{"id":"native-id","name":"read_file","args":{"path":"sample"}},"thoughtSignature":"c2ln"}]);
    let reply = json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"toolUsePromptTokenCount":3,"cachedContentTokenCount":80,"candidatesTokenCount":5,"thoughtsTokenCount":3}});
    let done = json!({"candidates":[{"content":{"parts":[{"text":"done","thoughtSignature":"eA=="}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":110,"candidatesTokenCount":2,"thoughtsTokenCount":0}});
    let (url, server) = mock(vec![reply, done]).await;
    let gateway = gateway(Provider::GoogleAi, &url);
    let request = req();
    let observed = gateway.complete_observed(request.clone()).await.unwrap();
    assert_eq!(observed.response.message.tool_calls[0].id, "native-id");
    assert_eq!(observed.metrics.usage.cache_read_tokens, Some(80));
    assert_eq!(observed.metrics.usage.input_tokens, 103);
    assert_eq!(observed.metrics.usage.output_tokens, 8);
    assert_eq!(observed.metrics.usage.reasoning_tokens, Some(3));
    assert!(observed.response.message.content.is_empty());
    let restored: Message =
        serde_json::from_slice(&serde_json::to_vec(&observed.response.message).unwrap()).unwrap();
    let mut next = request;
    next.messages.push(restored);
    let mut result = Message::text("tool", r#"{"synthetic":true}"#);
    result.tool_call_id = Some("native-id".into());
    next.messages.push(result);
    gateway.complete(next).await.unwrap();
    let sent = server.await.unwrap();
    assert_eq!(sent[1]["contents"][2]["parts"], parts);
    assert_eq!(
        sent[1]["contents"][3]["parts"][0]["functionResponse"]["id"],
        "native-id"
    );
    assert_eq!(
        sent[0]["tools"][0]["functionDeclarations"][0]["name"],
        "read_file"
    );
    assert_eq!(
        sent[0]["toolConfig"]["functionCallingConfig"]["mode"],
        "AUTO"
    );
    assert_eq!(sent[0]["contents"][1]["role"], "user");
    assert!(
        sent[0]["contents"][1]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("runtime_instruction")
    );
    assert!(sent[0].get("cachedContent").is_none());
    assert_eq!(marks(&sent[0]), 0);
}
#[tokio::test]
async fn malformed_cache_boundaries_and_forged_native_replay_fail_before_network() {
    let gateway = gateway(Provider::Anthropic, "http://127.0.0.1:9/v1/");
    let mut request = req();
    request
        .cache
        .as_mut()
        .unwrap()
        .breakpoints
        .push(CacheBreakpoint { message_index: 5 });
    assert!(matches!(
        gateway.complete(request).await,
        Err(ModelError::Configuration(_))
    ));
    let gateway = crate::gateway(Provider::GoogleAi, "http://127.0.0.1:9/v1/");
    let mut request = req();
    let mut message = Message::text("assistant", "modified");
    message.provider_state =
        Some(json!({"provider":"gemini","parts":[{"text":"original","thoughtSignature":"c2ln"}]}));
    request.messages.push(message);
    assert!(matches!(
        gateway.complete(request).await,
        Err(ModelError::Configuration(_))
    ));
}
fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}
fn an_start() -> String {
    sse(
        "message_start",
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":90}}}),
    ) + &sse(
        "content_block_start",
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
    )
}
fn an_finish() -> String {
    sse(
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    ) + &sse(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
    ) + &sse("message_stop", json!({"type":"message_stop"}))
}
#[tokio::test]
async fn actual_anthropic_worker_warming_releases_fanout_after_first_delta_before_leader_finishes()
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let first_delta = Arc::new(Notify::new());
    let send_delta = first_delta.clone();
    let (tx, mut received) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (mut leader, _) = listener.accept().await.unwrap();
        let first = read_request(&mut leader).await;
        assert_eq!(first["stream"], true);
        tx.send("leader").unwrap();
        leader
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        leader.write_all(an_start().as_bytes()).await.unwrap();
        send_delta.notified().await;
        leader.write_all(sse("content_block_delta",json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}})).as_bytes()).await.unwrap();
        let (mut follower, _) = listener.accept().await.unwrap();
        let second = read_request(&mut follower).await;
        assert_eq!(second["stream"], false);
        tx.send("follower").unwrap();
        send(&mut follower, &an_reply(90, 0).to_string()).await;
        leader.write_all(an_finish().as_bytes()).await.unwrap();
    });
    let gateway = Arc::new(gateway(
        Provider::Anthropic,
        &format!("http://{address}/v1/"),
    ));
    assert_eq!(gateway.metadata().role, Some(ModelRole::Worker));
    let batch = tokio::spawn(complete_fanout(gateway, vec![req(), req()], 2));
    assert_eq!(received.recv().await, Some("leader"));
    assert!(received.try_recv().is_err());
    first_delta.notify_one();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(10), received.recv())
            .await
            .unwrap(),
        Some("follower")
    );
    let result = batch.await.unwrap().unwrap();
    assert_eq!(result.release, FanoutRelease::FirstDelta);
    let first = result.results[0].as_ref().unwrap().as_ref().unwrap();
    let second = result.results[1].as_ref().unwrap().as_ref().unwrap();
    assert_eq!(first.response.message.content, "ok");
    assert_eq!(first.metrics.usage.cache_write_tokens, Some(90));
    assert_eq!(second.metrics.usage.cache_read_tokens, Some(90));
    server.await.unwrap();
}
#[tokio::test]
async fn anthropic_native_tool_sse_and_opaque_signatures_replay_exactly() {
    let content = sse(
        "message_start",
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}),
    ) + &sse(
        "content_block_start",
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
    ) + &sse(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"synthetic thought"}}),
    ) + &sse(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signature"}}),
    ) + &sse(
        "content_block_start",
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call1","name":"read_file","input":{}}}),
    ) + &sse(
        "content_block_delta",
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"sample\"}"}}),
    ) + &sse(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5,"output_tokens_details":{"thinking_tokens":2}}}),
    ) + &sse("message_stop", json!({"type":"message_stop"}));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        send_sse(&mut socket, &content).await;
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        send(&mut socket, &an_reply(0, 0).to_string()).await;
        request
    });
    let mut config = GatewayConfig::new("synthetic-model", ModelRole::Master);
    config.native_system_updates = Some(true);
    let gateway = OpenAiGateway::new(
        ProviderClient::with_base_url(
            Provider::Anthropic,
            Secret::new("synthetic").unwrap(),
            &format!("http://{address}/v1/"),
        )
        .unwrap(),
        config,
    )
    .unwrap();
    let deltas = Arc::new(Mutex::new(vec![]));
    let capture = deltas.clone();
    let sink: StreamSink = Arc::new(move |d| capture.lock().unwrap().push(d));
    let response = gateway.complete_stream(req(), sink).await.unwrap();
    assert_eq!(
        response.message.tool_calls[0].arguments,
        r#"{"path":"sample"}"#
    );
    assert!(
        deltas
            .lock()
            .unwrap()
            .iter()
            .any(|d| matches!(d,StreamDelta::Tool{arguments,..}if arguments.contains("sample")))
    );
    let mut request = req();
    request.messages.push(response.message);
    let mut result = Message::text("tool", "synthetic");
    result.tool_call_id = Some("call1".into());
    request.messages.push(result);
    gateway.complete(request).await.unwrap();
    let sent = server.await.unwrap();
    assert_eq!(sent["messages"][2]["content"][0]["signature"], "signature");
    assert_eq!(
        sent["messages"][2]["content"][0]["thinking"],
        "synthetic thought"
    );
    assert_eq!(sent["messages"][3]["content"][0]["tool_use_id"], "call1");
}
#[tokio::test]
async fn failed_warm_leader_never_dispatches_followers() {
    let (url, server) = mock(vec![json!({"error":"synthetic failure"})]).await;
    let result = complete_fanout(
        Arc::new(gateway(Provider::Anthropic, &url)),
        vec![req(), req(), req()],
        3,
    )
    .await
    .unwrap();
    assert_eq!(result.release, FanoutRelease::LeaderFailed);
    assert!(result.results[0].as_ref().unwrap().is_err());
    assert!(result.results[1..].iter().all(Option::is_none));
    assert_eq!(server.await.unwrap().len(), 1);
}
#[tokio::test]
async fn cancelling_warm_fanout_closes_the_active_http_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert_eq!(request["stream"], true);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        socket.write_all(an_start().as_bytes()).await.unwrap();
        tx.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
    });
    let batch = tokio::spawn(complete_fanout(
        Arc::new(gateway(
            Provider::Anthropic,
            &format!("http://{address}/v1/"),
        )),
        vec![req(), req()],
        2,
    ));
    rx.await.unwrap();
    batch.abort();
    assert!(batch.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn warming_rejects_distinct_principals_or_prefixes_before_dispatch() {
    let gateway = Arc::new(gateway(Provider::Anthropic, "http://127.0.0.1:9/v1/"));
    let mut other = req();
    other.cache.as_mut().unwrap().namespace_key = CacheIdentity::new("other", "worker", "scope")
        .unwrap()
        .namespace_key;
    assert!(matches!(
        complete_fanout(gateway.clone(), vec![req(), other], 2).await,
        Err(ModelError::Configuration(_))
    ));
    let mut other = req();
    other.cache.as_mut().unwrap().stable_prefix_hash = "b".repeat(64);
    assert!(
        complete_fanout(gateway, vec![req(), other], 2)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn inconsistent_reported_cache_subsets_fail_without_a_price_table() {
    let (url, server) = mock(vec![oa_reply(Some(101))]).await;
    let failure = gateway(Provider::OpenAi, &url)
        .complete_observed(req())
        .await
        .unwrap_err();
    assert!(matches!(failure.error, ModelError::Accounting(_)));
    server.await.unwrap();
}
#[tokio::test]
async fn successful_empty_warm_output_reports_completion_instead_of_a_first_delta() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        send_sse(&mut socket, &(an_start() + &an_finish())).await;
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        send(&mut socket, &an_reply(0, 0).to_string()).await;
    });
    let result = complete_fanout(
        Arc::new(gateway(
            Provider::Anthropic,
            &format!("http://{address}/v1/"),
        )),
        vec![req(), req()],
        2,
    )
    .await
    .unwrap();
    assert_eq!(result.release, FanoutRelease::CompletedWithoutDelta);
    assert!(result.results.iter().all(|r| r.as_ref().unwrap().is_ok()));
    server.await.unwrap();
}
#[tokio::test]
async fn gemini_stream_preserves_signature_parts_and_accounts_final_usage() {
    let parts = json!([{"text":"ok","thoughtSignature":"c2ln"}]);
    let payload = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"candidates":[{"content":{"parts":parts}}]}),
        json!({"candidates":[{"finishReason":"STOP","content":{"parts":[]}}],"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":80,"candidatesTokenCount":2,"thoughtsTokenCount":3}})
    );
    let (url, server) = mock_sse(payload).await;
    let gateway = OpenAiGateway::new(
        ProviderClient::with_base_url(Provider::GoogleAi, Secret::new("synthetic").unwrap(), &url)
            .unwrap(),
        GatewayConfig::new("synthetic-model", ModelRole::Master),
    )
    .unwrap();
    let deltas = Arc::new(Mutex::new(vec![]));
    let capture = deltas.clone();
    let observed = gateway
        .complete_stream_observed(req(), Arc::new(move |d| capture.lock().unwrap().push(d)))
        .await
        .unwrap();
    assert_eq!(observed.response.message.content, "ok");
    assert_eq!(
        observed.response.message.provider_state.as_ref().unwrap()["parts"],
        parts
    );
    assert_eq!(observed.metrics.usage.cache_read_tokens, Some(80));
    assert_eq!(observed.metrics.usage.output_tokens, 5);
    assert!(
        deltas
            .lock()
            .unwrap()
            .iter()
            .any(|d| matches!(d,StreamDelta::Text{delta,..}if delta=="ok"))
    );
    let sent = server.await.unwrap();
    assert!(sent.get("stream_options").is_none());
}
async fn mock_sse(payload: String) -> (String, tokio::task::JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        send_sse(&mut socket, &payload).await;
        request
    });
    (format!("http://{address}/v1/"), task)
}
#[tokio::test]
async fn native_validation_retry_preserves_gemini_signature_and_accounts_both_calls() {
    let parts = json!([{"text":"{\"ok\":\"wrong\"}","thoughtSignature":"c2ln"}]);
    let first = json!({"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":5}});
    let second = json!({"candidates":[{"content":{"parts":[{"text":"{\"ok\":true}"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":110,"candidatesTokenCount":4}});
    let (url, server) = mock(vec![first, second]).await;
    let gateway = gateway(Provider::GoogleAi, &url);
    let mut request = req();
    request.tools.clear();
    request.output_schema =
        Some(json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}));
    let observed = gateway.complete_observed(request).await.unwrap();
    assert_eq!(observed.response.message.content, r#"{"ok":true}"#);
    assert_eq!(observed.metrics.provider_calls, 2);
    assert_eq!(observed.metrics.usage.input_tokens, 210);
    let sent = server.await.unwrap();
    assert_eq!(sent[1]["contents"][2]["parts"], parts);
    assert_eq!(sent[1]["contents"][3]["role"], "user");
    assert!(
        sent[1]["contents"][3]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Output validation failed")
    );
}
#[tokio::test]
async fn incomplete_anthropic_input_components_are_estimated_and_never_invented_zeroes() {
    let reply = json!({"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":2}});
    let (url, server) = mock(vec![reply]).await;
    let observed = gateway(Provider::Anthropic, &url)
        .complete_observed(req())
        .await
        .unwrap();
    assert!(observed.metrics.usage.estimated);
    assert!(!observed.metrics.usage_known);
    assert_eq!(observed.metrics.usage.cache_read_tokens, None);
    assert_eq!(observed.metrics.usage.cache_write_tokens, None);
    assert!(observed.metrics.usage.input_tokens > 10);
    server.await.unwrap();
}
#[tokio::test]
async fn cache_namespace_and_boundaries_are_validated_without_disclosing_values() {
    let gateway = gateway(Provider::Anthropic, "http://127.0.0.1:9/v1/");
    for indexes in [vec![0, 0], vec![2, 1], vec![100]] {
        let mut request = req();
        request.cache.as_mut().unwrap().breakpoints = indexes
            .into_iter()
            .map(|message_index| CacheBreakpoint { message_index })
            .collect();
        let failure = gateway.complete_observed(request).await.unwrap_err();
        assert!(matches!(failure.error, ModelError::Configuration(_)));
        assert_eq!(failure.metrics.provider_calls, 0);
    }
    let mut request = req();
    request.cache.as_mut().unwrap().namespace_key = "private-synthetic-principal".into();
    let failure = gateway.complete_observed(request).await.unwrap_err();
    assert!(matches!(failure.error, ModelError::Configuration(_)));
    assert!(!failure.error.to_string().contains("private-synthetic"));
    assert_eq!(failure.metrics.provider_calls, 0);
}
#[tokio::test]
async fn anthropic_system_updates_respect_model_availability_and_placement() {
    for (model, override_mode, expected_role) in [
        ("claude-sonnet-4-6", None, "user"),
        ("claude-opus-5-5", None, "system"),
        ("unknown-host-model", Some(true), "system"),
    ] {
        let (url, server) = mock(vec![an_reply(0, 0)]).await;
        let mut config = GatewayConfig::new(model, ModelRole::Worker);
        config.native_system_updates = override_mode;
        let gateway = OpenAiGateway::new(
            ProviderClient::with_base_url(
                Provider::Anthropic,
                Secret::new("synthetic").unwrap(),
                &url,
            )
            .unwrap(),
            config,
        )
        .unwrap();
        let mut request = req();
        let literal = "retain \"quotes\"\nand newlines exactly";
        request.messages.last_mut().unwrap().content = literal.into();
        gateway.complete(request).await.unwrap();
        let sent = server.await.unwrap();
        assert_eq!(sent[0]["messages"][1]["role"], expected_role);
        assert!(
            sent[0]["messages"][1]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(literal)
        );
    }
    let (url, server) = mock(vec![an_reply(0, 0)]).await;
    let gateway = gateway(Provider::Anthropic, &url);
    let mut request = req();
    request
        .messages
        .insert(5, Message::text("assistant", "previous reply"));
    gateway.complete(request).await.unwrap();
    let sent = server.await.unwrap();
    assert_eq!(sent[0]["messages"][2]["role"], "user");
}
#[tokio::test]
async fn textual_protocol_preserves_native_signature_without_exposing_it_as_prompt_text() {
    let envelope=json!({"content":"","tool_calls":[{"id":"text-call","name":"read_file","arguments":{"path":"sample"}}]}).to_string();
    let blocks = json!([{"type":"thinking","thinking":"opaque private thought","signature":"synthetic-signature"},{"type":"text","text":envelope}]);
    let first = json!({"content":blocks,"stop_reason":"end_turn","usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":5}});
    let done = json!({"content":[{"type":"text","text":json!({"content":"done","tool_calls":[]}).to_string()}],"stop_reason":"end_turn","usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":2}});
    let (url, server) = mock(vec![first, done]).await;
    let mut config = GatewayConfig::new("synthetic", ModelRole::Worker);
    config.native_tools = false;
    config.native_system_updates = Some(true);
    let gateway = OpenAiGateway::new(
        ProviderClient::with_base_url(Provider::Anthropic, Secret::new("synthetic").unwrap(), &url)
            .unwrap(),
        config,
    )
    .unwrap();
    let mut request = req();
    let response = gateway.complete(request.clone()).await.unwrap();
    request.messages.push(response.message);
    let mut result = Message::text("tool", "result");
    result.tool_call_id = Some("text-call".into());
    request.messages.push(result);
    gateway.complete(request).await.unwrap();
    let sent = server.await.unwrap();
    assert_eq!(sent[1]["messages"][2]["content"], blocks);
}
#[tokio::test]
async fn compatible_text_protocol_never_promotes_opaque_provider_state_to_instruction_text() {
    let reply = oa_reply(None);
    let mut reply = reply;
    reply["choices"][0]["message"]["content"] =
        json!(json!({"content":"done","tool_calls":[]}).to_string());
    let (url, server) = mock(vec![reply]).await;
    let mut config = GatewayConfig::new("synthetic", ModelRole::Worker);
    config.native_tools = false;
    let gateway = OpenAiGateway::new(
        ProviderClient::with_base_url(Provider::OpenAi, Secret::new("synthetic").unwrap(), &url)
            .unwrap(),
        config,
    )
    .unwrap();
    let mut request = req();
    let mut assistant = Message::text("assistant", "");
    assistant
        .tool_calls
        .push(excalibur_types::runtime::ToolCall {
            id: "call1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"sample"}"#.into(),
        });
    assistant.provider_state = Some(
        json!({"provider":"gemini","parts":[{"thought":true,"text":"private-opaque-thought","thoughtSignature":"signature-not-prompt"}]}),
    );
    request.messages.push(assistant);
    gateway.complete(request).await.unwrap();
    let sent = server.await.unwrap();
    assert!(!sent[0].to_string().contains("private-opaque-thought"));
    assert!(!sent[0].to_string().contains("signature-not-prompt"));
    assert!(!sent[0].to_string().contains("provider_state"));
}
#[tokio::test]
async fn native_compiled_empty_history_and_later_deltas_keep_identical_system_prefix() {
    use excalibur_context::{ContextInput, compile};
    for (provider, native_tools) in [Provider::Anthropic, Provider::GoogleAi]
        .into_iter()
        .flat_map(|p| [true, false].into_iter().map(move |native| (p, native)))
    {
        let mut input = ContextInput::new("stable core", "stable original task", "clock-initial");
        input.layers[1] = "stable identity".into();
        input.layers[2] = "stable scope".into();
        input.tools = req().tools;
        let mut first = compile(&input).unwrap().request;
        assert_eq!(first.messages.len(), 5);
        assert!(first.messages.last().unwrap().content.starts_with("[L5]\n"));
        input.append_delta(1, "new requirement literal");
        input.append_message(Message::text("user", "confirmed activity"));
        input.volatile.now_utc = "clock-next".into();
        let mut next = compile(&input).unwrap().request;
        let mut cache = req().cache.unwrap();
        cache.breakpoints = vec![0, 1, 2]
            .into_iter()
            .map(|message_index| CacheBreakpoint { message_index })
            .collect();
        first.cache = Some(cache.clone());
        cache.breakpoints.push(CacheBreakpoint {
            message_index: next.messages.len() - 2,
        });
        next.cache = Some(cache);
        let mut replies: Vec<Value> = if provider == Provider::Anthropic {
            vec![an_reply(0, 90), an_reply(90, 0)]
        } else {
            [0,90].into_iter().map(|cached|json!({"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":cached,"candidatesTokenCount":2}})).collect()
        };
        if !native_tools {
            for reply in &mut replies {
                let text = json!({"content":"ok","tool_calls":[]}).to_string();
                if provider == Provider::Anthropic {
                    reply["content"][0]["text"] = json!(text);
                } else {
                    reply["candidates"][0]["content"]["parts"][0]["text"] = json!(text);
                }
            }
        }
        let (url, server) = mock(replies).await;
        let mut config = GatewayConfig::new("synthetic", ModelRole::Worker);
        config.native_tools = native_tools;
        config.native_system_updates = Some(true);
        let gateway = OpenAiGateway::new(
            ProviderClient::with_base_url(provider, Secret::new("synthetic").unwrap(), &url)
                .unwrap(),
            config,
        )
        .unwrap();
        gateway.complete(first).await.unwrap();
        gateway.complete(next).await.unwrap();
        let sent = server.await.unwrap();
        let system_key = if provider == Provider::Anthropic {
            "system"
        } else {
            "systemInstruction"
        };
        assert_eq!(sent[0][system_key], sent[1][system_key]);
        let stable = sent[0][system_key].to_string();
        assert!(!stable.contains("[L5]"));
        assert!(!stable.contains("Context update v"));
        let activity_key = if provider == Provider::Anthropic {
            "messages"
        } else {
            "contents"
        };
        for (body, clock) in sent.iter().zip(["clock-initial", "clock-next"]) {
            let activity = body[activity_key].as_array().unwrap();
            assert!(!activity.is_empty());
            let last = activity.last().unwrap();
            let text = if provider == Provider::Anthropic {
                &last["content"][0]["text"]
            } else {
                &last["parts"][0]["text"]
            };
            assert!(text.as_str().unwrap().contains("[L5]\n"));
            assert!(text.as_str().unwrap().contains(clock));
        }
        assert_eq!(sent[0][activity_key][0]["role"], "user");
    }
}
#[tokio::test]
async fn anthropic_worker_public_streaming_remains_buffered() {
    let (url, server) = mock(vec![an_reply(0, 0), an_reply(0, 0)]).await;
    let gateway = gateway(Provider::Anthropic, &url);
    assert_eq!(gateway.metadata().role, Some(ModelRole::Worker));
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let seen = deltas.clone();
    let sink: StreamSink = Arc::new(move |delta| seen.lock().unwrap().push(delta));
    let observed = gateway
        .complete_stream_observed(req(), sink.clone())
        .await
        .unwrap();
    assert_eq!(observed.response.message.content, "ok");
    let response = gateway.complete_stream(req(), sink).await.unwrap();
    assert_eq!(response.message.content, "ok");
    let sent = server.await.unwrap();
    assert!(sent.iter().all(|r| r["stream"] == false));
    let captured = deltas.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert!(
        captured
            .iter()
            .all(|d| matches!(d, StreamDelta::Complete { .. }))
    );
}
