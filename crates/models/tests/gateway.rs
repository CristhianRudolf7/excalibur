use excalibur_models::{
    GatewayConfig, ModelError, ModelGateway, ModelRole, OpenAiGateway, StreamDelta, StreamSink,
};
use excalibur_providers::{Provider, ProviderClient, Secret};
use excalibur_types::runtime::{Message, ModelRequest, ToolCall, ToolDefinition};
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
        let mut requests = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            let body;
            loop {
                let mut chunk = [0u8; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buffer[..index]);
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|n| n.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if buffer.len() >= index + 4 + length {
                        body =
                            serde_json::from_slice(&buffer[index + 4..index + 4 + length]).unwrap();
                        break;
                    }
                }
            }
            requests.push(body);
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            for part in reply.as_bytes().chunks(9) {
                socket.write_all(part).await.unwrap();
            }
        }
        requests
    });
    (format!("http://{address}/v1/"), task)
}
fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![
            Message::text("system", "core"),
            Message::text("user", "task"),
        ],
        tools: vec![ToolDefinition {
            name: "read_file".into(),
            description: "Read synthetic file".into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
        }],
        tools_enabled: true,
        reasoning_effort: None,
        output_schema: None,
        cache: None,
    }
}
fn gateway(url: &str, native_tools: bool, role: ModelRole) -> OpenAiGateway {
    let client =
        ProviderClient::with_base_url(Provider::OpenAi, Secret::new("synthetic-key").unwrap(), url)
            .unwrap();
    let mut config = GatewayConfig::new("synthetic-model", role);
    config.native_tools = native_tools;
    OpenAiGateway::new(client, config).unwrap()
}
fn completion(content: &str) -> String {
    json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":content}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}).to_string()
}
#[tokio::test]
async fn native_tool_history_and_results_translate_without_losing_call_identity() {
    let reply=json!({"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_next","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"next\"}"}}]}}],"usage":{"prompt_tokens":10,"completion_tokens":4}}).to_string();
    let (url, task) = mock(vec![reply]).await;
    let gateway = gateway(&url, true, ModelRole::Worker);
    let mut req = request();
    req.messages.push(Message {
        role: "assistant".into(),
        content: String::new(),
        tool_calls: vec![ToolCall {
            id: "call_old".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"old\"}".into(),
        }],
        tool_call_id: None,
        provider_state: None,
    });
    req.messages.push(Message {
        role: "tool".into(),
        content: "synthetic result".into(),
        tool_calls: vec![],
        tool_call_id: Some("call_old".into()),
        provider_state: None,
    });
    let result = gateway.complete(req).await.unwrap();
    assert_eq!(result.message.tool_calls[0].id, "call_next");
    assert_eq!(result.usage.input_tokens, 10);
    assert!(!result.usage.estimated);
    let sent = task.await.unwrap();
    assert_eq!(sent[0]["messages"][2]["tool_calls"][0]["id"], "call_old");
    assert_eq!(sent[0]["messages"][3]["tool_call_id"], "call_old");
    assert_eq!(sent[0]["tools"][0]["function"]["name"], "read_file");
}
#[tokio::test]
async fn textual_tools_get_bounded_minor_repairs_and_schema_validation() {
    let text = "```json\n{\"content\":\"\",\"tool_calls\":[{\"id\":\"call1\",\"name\":\"read_file\",\"arguments\":{\"path\":\"synthetic\",},}],}\n```";
    let (url, task) = mock(vec![completion(text)]).await;
    let result = gateway(&url, false, ModelRole::Worker)
        .complete(request())
        .await
        .unwrap();
    assert_eq!(
        result.message.tool_calls[0].arguments,
        "{\"path\":\"synthetic\"}"
    );
    let sent = task.await.unwrap();
    assert!(sent[0].get("tools").is_none());
    assert!(
        sent[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("JSON object")
    );
}
#[tokio::test]
async fn invalid_structured_output_retries_once_with_concrete_error_then_succeeds() {
    let (url, task) = mock(vec![
        completion("{\"ok\":\"wrong\"}"),
        completion("{\"ok\":true}"),
    ])
    .await;
    let mut req = request();
    req.output_schema = Some(
        json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}),
    );
    let result = gateway(&url, true, ModelRole::Worker)
        .complete(req)
        .await
        .unwrap();
    assert_eq!(result.message.content, "{\"ok\":true}");
    assert_eq!(result.usage.input_tokens, 10);
    assert!(!result.usage.estimated);
    assert_eq!(result.usage.output_tokens, 4);
    let sent = task.await.unwrap();
    assert_eq!(sent.len(), 2);
    assert!(
        sent[1]["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("/ok")
    );
    assert_eq!(sent[0]["response_format"]["json_schema"]["strict"], true);
}
#[tokio::test]
async fn persistent_invalid_output_stops_after_single_retry_and_disabled_tools_cannot_execute() {
    let (url, task) = mock(vec![completion("{\"ok\":2}"), completion("{\"ok\":3}")]).await;
    let mut req = request();
    req.output_schema =
        Some(json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}));
    assert!(matches!(
        gateway(&url, true, ModelRole::Worker).complete(req).await,
        Err(ModelError::Validation(_))
    ));
    assert_eq!(task.await.unwrap().len(), 2);
    let (url, task) = mock(vec![completion("done")]).await;
    let mut req = request();
    req.tools_enabled = false;
    gateway(&url, true, ModelRole::Worker)
        .complete(req)
        .await
        .unwrap();
    let sent = task.await.unwrap();
    assert_eq!(sent[0]["tool_choice"], "none");
    assert_eq!(
        sent[0]["tools"].as_array().unwrap().len(),
        1,
        "stable tools list remains present"
    );
}
#[tokio::test]
async fn master_stream_assembles_fragmented_calls_and_keeps_complete_durable_response() {
    let chunks = [
        json!({"choices":[{"delta":{"content":"checking "},"finish_reason":null}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call1","function":{"name":"read_file","arguments":"{\"pa"}}]},"finish_reason":null}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"synthetic\"}"}}]},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":3}}),
    ];
    let reply = chunks
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    let (url, task) = mock(vec![reply]).await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let capture = observed.clone();
    let sink: StreamSink = Arc::new(move |delta| capture.lock().unwrap().push(delta));
    let response = gateway(&url, true, ModelRole::Master)
        .complete_stream(request(), sink)
        .await
        .unwrap();
    assert_eq!(response.message.content, "checking ");
    assert_eq!(
        response.message.tool_calls[0].arguments,
        "{\"path\":\"synthetic\"}"
    );
    assert_eq!(response.usage.output_tokens, 3);
    let events = observed.lock().unwrap().clone();
    assert!(
        matches!(events.last(),Some(StreamDelta::Complete{response:complete,..}) if complete==&response)
    );
    assert!(events.iter().any(|e| matches!(e, StreamDelta::Tool { .. })));
    let sent = task.await.unwrap();
    assert_eq!(sent[0]["stream"], true);
    assert_eq!(sent[0]["stream_options"]["include_usage"], true);
}
#[tokio::test]
async fn interrupted_or_length_limited_stream_never_returns_partial_tool_calls() {
    for reply in [
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n",
    ] {
        let (url, task) = mock(vec![reply.into()]).await;
        let sink: StreamSink = Arc::new(|_| {});
        assert!(matches!(
            gateway(&url, true, ModelRole::Master)
                .complete_stream(request(), sink)
                .await,
            Err(ModelError::Truncated)
        ));
        assert_eq!(task.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn role_effort_and_external_schema_configuration_fail_before_provider_access() {
    let client = ProviderClient::with_base_url(
        Provider::OpenAi,
        Secret::new("synthetic-only").unwrap(),
        "http://127.0.0.1:1/v1/",
    )
    .unwrap();
    let mut config = GatewayConfig::new("synthetic-model", ModelRole::Worker);
    config.reasoning_effort = Some("low".into());
    let gateway: Box<dyn ModelGateway> = Box::new(OpenAiGateway::new(client, config).unwrap());
    let mut req = request();
    req.reasoning_effort = Some("high".into());
    assert!(matches!(
        gateway.complete(req).await,
        Err(ModelError::Configuration(_))
    ));
    let mut req = request();
    req.output_schema = Some(json!({"$ref":"https://schema.invalid/private"}));
    assert!(matches!(
        gateway.complete(req).await,
        Err(ModelError::Configuration(_))
    ));
}

#[tokio::test]
async fn invalid_native_tool_arguments_are_rejected_and_one_retry_is_bounded() {
    let bad=json!({"choices":[{"finish_reason":"tool_calls","message":{"content":null,"tool_calls":[{"type":"function","id":"call_bad","function":{"name":"read_file","arguments":"{\"path\":42}"}}]}}]}).to_string();
    let (url, task) = mock(vec![bad.clone(), bad]).await;
    let result = gateway(&url, true, ModelRole::Worker)
        .complete(request())
        .await;
    assert!(matches!(result, Err(ModelError::Validation(_))));
    let sent = task.await.unwrap();
    assert_eq!(sent.len(), 2);
    assert!(
        sent[1]["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("/path")
    );
}

#[test]
fn gateway_identity_pins_provider_endpoint_role_and_effort_but_never_credentials() {
    fn configured(endpoint: &str, secret: &str, effort: &str) -> OpenAiGateway {
        let client =
            ProviderClient::with_base_url(Provider::OpenAi, Secret::new(secret).unwrap(), endpoint)
                .unwrap();
        let mut config = GatewayConfig::new("synthetic-model", ModelRole::Worker);
        config.reasoning_effort = Some(effort.into());
        OpenAiGateway::new(client, config).unwrap()
    }
    let first = configured("http://127.0.0.1:1/v1/", "synthetic-secret-a", "low");
    let rotated = configured("http://127.0.0.1:1/v1/", "synthetic-secret-b", "low");
    assert_eq!(first.identity(), rotated.identity());
    assert_ne!(
        first.identity(),
        configured("http://127.0.0.1:2/v1/", "synthetic-secret-a", "low").identity()
    );
    assert_ne!(
        first.identity(),
        configured("http://127.0.0.1:1/v1/", "synthetic-secret-a", "high").identity()
    );
    assert!(!first.identity().contains("synthetic-secret"));
}

#[tokio::test]
async fn omitted_usage_consumes_conservative_estimated_tokens_instead_of_zero() {
    let reply=json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"Synthetic á😀 answer"}}]}).to_string();
    let expected_output = reply.len() as u64;
    let (url, task) = mock(vec![reply]).await;
    let mut req = request();
    req.messages
        .push(Message::text("user", "Synthetic multibyte á😀 input"));
    let response = gateway(&url, true, ModelRole::Worker)
        .complete(req)
        .await
        .unwrap();
    let sent = task.await.unwrap();
    assert!(response.usage.estimated);
    assert_eq!(
        response.usage.input_tokens,
        sent[0].to_string().len() as u64 + 100
    );
    assert_eq!(response.usage.output_tokens, expected_output);
    assert!(response.usage.input_tokens > 0 && response.usage.output_tokens > 0);
}

#[tokio::test]
async fn estimated_usage_flag_and_counts_accumulate_across_the_single_schema_retry() {
    let first=json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"{\"ok\":\"wrong\"}"}}],"usage":{"prompt_tokens":99}}).to_string();
    let first_output = first.len() as u64;
    let (url, task) = mock(vec![first, completion("{\"ok\":true}")]).await;
    let mut req = request();
    req.output_schema =
        Some(json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}));
    let response = gateway(&url, true, ModelRole::Worker)
        .complete(req)
        .await
        .unwrap();
    let sent = task.await.unwrap();
    assert_eq!(sent.len(), 2);
    assert!(response.usage.estimated);
    assert_eq!(
        response.usage.input_tokens,
        sent[0].to_string().len() as u64 + 100 + 5
    );
    assert_eq!(response.usage.output_tokens, first_output + 2);
    let legacy: excalibur_types::runtime::Usage =
        serde_json::from_value(json!({"input_tokens":1,"output_tokens":2,"cost_microusd":0}))
            .unwrap();
    assert!(!legacy.estimated);
}

#[tokio::test]
async fn sampling_is_opt_in_and_preserves_exact_wire_values() {
    let (url, task) = mock(vec![completion("default"), completion("sampled")]).await;
    gateway(&url, true, ModelRole::Worker)
        .complete(request())
        .await
        .unwrap();
    let client = ProviderClient::with_base_url(
        Provider::OpenAi,
        Secret::new("synthetic-key").unwrap(),
        &url,
    )
    .unwrap();
    let mut config = GatewayConfig::new("synthetic-model", ModelRole::Worker);
    config.seed = Some(i64::MAX as u64);
    config.temperature = Some(0.0);
    OpenAiGateway::new(client, config)
        .unwrap()
        .complete(request())
        .await
        .unwrap();
    let sent = task.await.unwrap();
    assert!(sent[0].get("seed").is_none());
    assert!(sent[0].get("temperature").is_none());
    assert_eq!(sent[1]["seed"], json!(i64::MAX));
    assert_eq!(sent[1]["temperature"], json!(0.0));
}

#[test]
fn sampling_validation_and_identity_preserve_legacy_default_configuration() {
    use sha2::{Digest, Sha256};

    let client = || {
        ProviderClient::with_base_url(
            Provider::OpenAi,
            Secret::new("synthetic-key").unwrap(),
            "http://127.0.0.1:1/v1/",
        )
        .unwrap()
    };
    let config = GatewayConfig::new("synthetic-model", ModelRole::Worker);
    let legacy_json = r#"{"model":"synthetic-model","role":"Worker","reasoning_effort":null,"native_tools":true,"max_output_tokens":4096}"#;
    assert_eq!(serde_json::to_string(&config).unwrap(), legacy_json);
    let legacy: GatewayConfig = serde_json::from_str(legacy_json).unwrap();
    assert!(legacy.seed.is_none() && legacy.temperature.is_none());
    let mut hash = Sha256::new();
    hash.update(client().configuration_identity());
    hash.update(legacy_json.as_bytes());
    let default_identity = OpenAiGateway::new(client(), config.clone())
        .unwrap()
        .identity();
    assert_eq!(
        default_identity,
        format!("openai-compatible:{:x}", hash.finalize())
    );
    for (seed, temperature) in [(Some(0), None), (None, Some(2.0)), (Some(1), Some(0.5))] {
        let mut opted_in = config.clone();
        opted_in.seed = seed;
        opted_in.temperature = temperature;
        assert_ne!(
            default_identity,
            OpenAiGateway::new(client(), opted_in).unwrap().identity()
        );
    }
    for seed in [i64::MAX as u64 + 1, u64::MAX] {
        let mut invalid = config.clone();
        invalid.seed = Some(seed);
        assert!(matches!(
            OpenAiGateway::new(client(), invalid),
            Err(ModelError::Configuration(_))
        ));
    }
    for temperature in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, 2.1] {
        let mut invalid = config.clone();
        invalid.temperature = Some(temperature);
        assert!(matches!(
            OpenAiGateway::new(client(), invalid),
            Err(ModelError::Configuration(_))
        ));
    }
}
