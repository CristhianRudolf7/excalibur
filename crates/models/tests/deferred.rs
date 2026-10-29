use excalibur_models::{
    GatewayConfig, ModelError, ModelGateway, ModelRole, OpenAiGateway, StreamDelta,
};
use excalibur_providers::{Provider, ProviderClient, Secret};
use excalibur_types::runtime::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
async fn mock(replies: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = vec![];
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buffer[..end]);
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if buffer.len() >= end + 4 + length {
                        requests.push(
                            serde_json::from_slice(&buffer[end + 4..end + 4 + length]).unwrap(),
                        );
                        break;
                    }
                }
            }
            let mime = if reply.starts_with("event:") {
                "text/event-stream"
            } else {
                "application/json"
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    (format!("http://{address}/v1/"), task)
}
fn def(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: format!("Run {name}"),
        input_schema: json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false}),
    }
}
fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![
            Message::text("system", "[L0]\ncore"),
            Message::text("user", "Export an invoice"),
        ],
        tools: vec![def("core_status")],
        deferred_tools: vec![def("invoice_export")],
        tools_enabled: true,
        ..Default::default()
    }
}
fn gateway(url: &str, role: ModelRole) -> OpenAiGateway {
    OpenAiGateway::new(
        ProviderClient::with_base_url(Provider::Anthropic, Secret::new("synthetic").unwrap(), url)
            .unwrap(),
        GatewayConfig::new("claude-sonnet-4-6", role),
    )
    .unwrap()
}
fn blocks() -> Value {
    json!([{"type":"server_tool_use","id":"srvtoolu_search","name":"tool_search_tool_bm25","input":{"query":"export invoice","limit":5}},{"type":"tool_search_tool_result","tool_use_id":"srvtoolu_search","content":{"type":"tool_search_tool_search_result","tool_references":[{"type":"tool_reference","tool_name":"invoice_export"}]}},{"type":"tool_use","id":"client_call","name":"invoice_export","input":{"value":7}}])
}
fn reply(content: Value, stop: &str) -> String {
    json!({"content":content,"stop_reason":stop,"usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":4}}).to_string()
}
#[tokio::test]
async fn native_deferred_wire_keeps_core_and_replays_server_references_without_client_effects() {
    let original = blocks();
    let (url, server) = mock(vec![
        reply(original.clone(), "tool_use"),
        reply(json!([{"type":"text","text":"done"}]), "end_turn"),
    ])
    .await;
    let gateway = gateway(&url, ModelRole::Worker);
    assert!(gateway.native_deferred_tools());
    let mut req = request();
    let observed = gateway.complete_observed(req.clone()).await.unwrap();
    assert_eq!(observed.response.message.tool_calls.len(), 1);
    assert_eq!(observed.response.message.tool_calls[0].id, "client_call");
    assert_eq!(observed.metrics.usage.input_tokens, 10);
    let restored: Message =
        serde_json::from_slice(&serde_json::to_vec(&observed.response.message).unwrap()).unwrap();
    req.messages.push(restored);
    let mut result = Message::text("tool", "{\"exported\":true}");
    result.tool_call_id = Some("client_call".into());
    req.messages.push(result);
    gateway.complete(req).await.unwrap();
    let sent = server.await.unwrap();
    for body in &sent {
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0]["name"], "core_status");
        assert!(tools[0].get("defer_loading").is_none());
        assert_eq!(tools[1]["type"], "tool_search_tool_bm25_20251119");
        assert_eq!(tools[2]["name"], "invoice_export");
        assert_eq!(tools[2]["defer_loading"], true);
        assert!(!body["system"].to_string().contains("invoice_export"));
    }
    let assistant = sent[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["content"], original);
    let result = sent[1]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(result["content"][0]["tool_use_id"], "client_call");
}
fn sse(kind: &str, data: Value) -> String {
    format!("event: {kind}\ndata: {data}\n\n")
}
#[tokio::test]
async fn native_deferred_sse_assembles_search_args_preserves_results_and_emits_only_client_calls() {
    let mut payload = sse(
        "message_start",
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}),
    );
    payload += &sse(
        "content_block_start",
        json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_search","name":"tool_search_tool_bm25","input":{}}}),
    );
    for part in ["{\"query\":\"export ", "invoice\",\"limit\":5}"] {
        payload += &sse(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":part}}),
        );
    }
    payload += &sse(
        "content_block_start",
        json!({"type":"content_block_start","index":1,"content_block":blocks()[1]}),
    );
    payload += &sse(
        "content_block_start",
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"client_call","name":"invoice_export","input":{}}}),
    );
    payload += &sse(
        "content_block_delta",
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"value\":7}"}}),
    );
    payload += &sse(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),
    );
    payload += &sse("message_stop", json!({"type":"message_stop"}));
    let (url, server) = mock(vec![payload]).await;
    let gateway = gateway(&url, ModelRole::Master);
    let deltas = Arc::new(Mutex::new(vec![]));
    let captured = deltas.clone();
    let observed = gateway
        .complete_stream_observed(
            request(),
            Arc::new(move |d| captured.lock().unwrap().push(d)),
        )
        .await
        .unwrap();
    assert_eq!(
        observed.response.message.provider_state.as_ref().unwrap()["blocks"],
        blocks()
    );
    assert_eq!(observed.response.message.tool_calls.len(), 1);
    for delta in deltas.lock().unwrap().iter() {
        if let StreamDelta::Tool { id, name, .. } = delta {
            assert!(!id.starts_with("srvtoolu"));
            assert_ne!(name, "tool_search_tool_bm25");
        }
    }
    server.await.unwrap();
}
#[tokio::test]
async fn native_catalog_is_retained_when_tools_are_disabled_and_unsupported_adapters_reject() {
    let (url, server) = mock(vec![reply(
        json!([{"type":"text","text":"done"}]),
        "end_turn",
    )])
    .await;
    let gateway = gateway(&url, ModelRole::Worker);
    let mut req = request();
    req.tools_enabled = false;
    gateway.complete(req.clone()).await.unwrap();
    let sent = server.await.unwrap();
    assert_eq!(sent[0]["tool_choice"]["type"], "none");
    assert_eq!(sent[0]["tools"][2]["defer_loading"], true);
    for (provider, model) in [
        (Provider::OpenAi, "synthetic"),
        (Provider::Anthropic, "claude-opus-4-1"),
    ] {
        let gateway = OpenAiGateway::new(
            ProviderClient::with_base_url(
                provider,
                Secret::new("synthetic").unwrap(),
                "http://127.0.0.1:9/v1/",
            )
            .unwrap(),
            GatewayConfig::new(model, ModelRole::Worker),
        )
        .unwrap();
        assert!(!gateway.native_deferred_tools());
        let error = gateway.complete_observed(req.clone()).await.unwrap_err();
        assert!(matches!(error.error, ModelError::Configuration(_)));
        assert_eq!(error.metrics.provider_calls, 0);
    }
}
