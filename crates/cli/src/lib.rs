//! Thin argument and host-configuration adapter. Execution remains in shared Rust libraries.
use excalibur_eval::{ExecOptions, ExecutionProfile, profile::ProfileOverrides};
use excalibur_models::{
    GatewayConfig, ModelError, ModelGateway, ModelRole, OpenAiGateway, PriceTable, ReplayGateway,
    ReplayTape,
};
use excalibur_providers::{Provider, ProviderClient, Secret};
use excalibur_security::Autonomy;
use excalibur_vault::{Redactor, SecretValue};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};
pub const HELP: &str = "excalibur exec <request> [--project DIR] [--autonomy A0|A1|A2|A3] [--output-schema FILE] [--criterion FILE] [--required-input NAME=VALUE] [--json] [--budget-usd X] [--max-steps N] [--max-tokens N] [--timeout S] [--logs-dir DIR] [--profile project|eval] [--profile-override FILE] [--container-runtime NAME] [--task-network none|open] [--task-cpus N] [--task-timeout S] [--model ID] [--base-url URL] [--reasoning-effort LEVEL] [--seed N] [--temperature X] [--metadata FILE] [--price-table FILE] [--replay FILE] [--trial-id ID] [--task-id ID] [--task-digest SHA256] [--benchmark VERSION] [--harbor-version VERSION] [--read-root DIR] [--write-root DIR] [--cgroup DIR] [--cgroup-scope task_children|task_container|shared]";
pub struct Command {
    pub options: ExecOptions,
    pub json: bool,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub effort: Option<String>,
    pub prices: Option<PathBuf>,
    pub replay: Option<PathBuf>,
    pub explicit: std::collections::BTreeSet<String>,
}
fn number(s: &str) -> Result<u64, String> {
    s.parse()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| "a positive integer is required".into())
}
pub fn parse_usd(s: &str) -> Result<u64, String> {
    let parts = s.split('.').collect::<Vec<_>>();
    if parts.len() > 2
        || parts[0].is_empty()
        || parts.iter().any(|v| !v.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("invalid USD budget".into());
    }
    let whole = parts[0].parse::<u64>().map_err(|_| "USD budget overflow")?;
    let fraction = parts.get(1).copied().unwrap_or("");
    if fraction.len() > 6 {
        return Err("USD budget supports at most six decimal places".into());
    }
    let frac = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u64>()
            .map_err(|_| "invalid USD fraction")?
            * 10u64.pow((6 - fraction.len()) as u32)
    };
    whole
        .checked_mul(1_000_000)
        .and_then(|v| v.checked_add(frac))
        .filter(|v| *v > 0)
        .ok_or_else(|| "USD budget must be positive and representable".into())
}
fn read_json(path: &str) -> Result<Value, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| "configuration file could not be read")?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "configuration file could not be read")?;
    if bytes.len() > 1024 * 1024 {
        return Err("configuration file exceeds 1 MiB".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "configuration file must contain valid JSON".into())
}
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some("exec") {
        return Err(HELP.into());
    }
    let cwd = std::env::current_dir().map_err(|_| "working directory unavailable")?;
    let logs = std::env::temp_dir()
        .join(format!("excalibur-exec-{}", std::process::id()))
        .join(excalibur_eval::new_episode_id());
    let mut command = Command {
        options: ExecOptions::new("", cwd, logs),
        json: false,
        model: None,
        endpoint: None,
        effort: None,
        prices: None,
        replay: None,
        explicit: Default::default(),
    };
    let mut request = Vec::new();
    let mut explicit_logs = false;
    while let Some(arg) = args.next() {
        if arg == "--json" {
            command.json = true;
            continue;
        }
        if arg == "--" {
            request.extend(args);
            break;
        }
        if !arg.starts_with('-') {
            request.push(arg);
            continue;
        }
        command.explicit.insert(arg.clone());
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--project" => command.options.workspace = value.into(),
            "--logs-dir" => {
                command.options.logs_dir = value.into();
                explicit_logs = true;
            }
            "--profile" => {
                command.options.profile = match value.as_str() {
                    "eval" => ExecutionProfile::Eval,
                    "project" => ExecutionProfile::Project,
                    _ => return Err("unknown execution profile".into()),
                }
            }
            "--autonomy" => {
                command.options.autonomy = match value.as_str() {
                    "A0" => Autonomy::Consultative,
                    "A1" => Autonomy::Supervised,
                    "A2" => Autonomy::Autonomous,
                    "A3" => Autonomy::Full,
                    _ => return Err("unknown autonomy level".into()),
                }
            }
            "--output-schema" => command.options.output_schema = Some(read_json(&value)?),
            "--criterion" => command.options.criteria.push(
                serde_json::from_value(read_json(&value)?)
                    .map_err(|_| "invalid backend criterion")?,
            ),
            "--budget-usd" => command.options.budget.max_cost_microusd = Some(parse_usd(&value)?),
            "--max-steps" => command.options.budget.max_steps = number(&value)?,
            "--max-tokens" => command.options.budget.max_tokens = number(&value)?,
            "--timeout" => command.options.budget.max_wall_seconds = number(&value)?,
            "--task-timeout" => command.options.task_timeout_seconds = Some(number(&value)?),
            "--task-cpus" => {
                command.options.task_cpu_count =
                    usize::try_from(number(&value)?).map_err(|_| "CPU count overflow")?
            }
            "--task-network" => {
                command.options.task_network_allowed = match value.as_str() {
                    "open" => true,
                    "none" => false,
                    _ => return Err("task network must be none or open".into()),
                }
            }
            "--container-runtime" => command.options.container_runtime = Some(value),
            "--profile-override" => {
                command.options.profile_override = Some(
                    serde_json::from_value::<ProfileOverrides>(read_json(&value)?)
                        .map_err(|_| "invalid profile override")?,
                )
            }
            "--model" => command.model = Some(value),
            "--base-url" => command.endpoint = Some(value),
            "--reasoning-effort" => command.effort = Some(value),
            "--price-table" => command.prices = Some(value.into()),
            "--replay" => command.replay = Some(value.into()),
            "--trial-id" => command.options.metadata.trial_id = Some(value),
            "--task-id" => command.options.metadata.task_id = Some(value),
            "--task-digest" => command.options.metadata.task_digest = Some(value),
            "--benchmark" => command.options.metadata.benchmark = Some(value),
            "--harbor-version" => command.options.metadata.harbor_version = Some(value),
            "--metadata" => {
                let metadata: excalibur_telemetry::RunMetadata =
                    serde_json::from_value(read_json(&value)?)
                        .map_err(|_| "invalid host run metadata")?;
                if metadata.seed.is_some() {
                    command.explicit.insert("--seed".into());
                }
                if metadata.temperature.is_some() {
                    command.explicit.insert("--temperature".into());
                }
                if metadata.reasoning_effort.is_some() {
                    command.effort = metadata.reasoning_effort.clone();
                    command.explicit.insert("--reasoning-effort".into());
                }
                command.options.metadata = metadata;
            }
            "--seed" => {
                command.options.metadata.seed = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| "seed must be an unsigned integer")?,
                )
            }
            "--temperature" => {
                let v = value
                    .parse::<f64>()
                    .map_err(|_| "temperature must be numeric")?;
                if !v.is_finite() || !(0.0..=2.0).contains(&v) {
                    return Err("temperature must be finite and between zero and two".into());
                }
                command.options.metadata.temperature = Some(v);
            }
            "--memory-store" => command.options.memory_store = Some(value.into()),
            "--skills-store" => command.options.skills_store = Some(value.into()),
            "--credentials-store" => command.options.credentials_store = Some(value.into()),
            "--required-input" => {
                let (name, v) = value
                    .split_once('=')
                    .ok_or("required input must be NAME=VALUE")?;
                if name.is_empty() {
                    return Err("required input name is empty".into());
                }
                command
                    .options
                    .required_inputs
                    .insert(name.into(), (!v.is_empty()).then(|| v.into()));
            }
            "--read-root" => command.options.extra_read_roots.push(value.into()),
            "--write-root" => command.options.extra_write_roots.push(value.into()),
            "--cgroup" => command.options.cgroup = Some(value.into()),
            "--cgroup-scope" => {
                command.options.cgroup_scope = match value.as_str() {
                    "task_children" => excalibur_telemetry::resources::ResourceScope::TaskChildren,
                    "task_container" => {
                        excalibur_telemetry::resources::ResourceScope::TaskContainer
                    }
                    "shared" => excalibur_telemetry::resources::ResourceScope::Shared,
                    _ => return Err("unknown cgroup scope".into()),
                }
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
    }
    command.options.request = request.join(" ");
    if command.options.profile == ExecutionProfile::Eval && !explicit_logs {
        command.options.logs_dir = "/logs/agent".into();
    }
    command.options.metadata.excalibur_commit =
        option_env!("EXCALIBUR_BUILD_COMMIT").map(str::to_owned);
    command.options.metadata.extra.insert(
        "source_dirty".into(),
        serde_json::json!(option_env!("EXCALIBUR_BUILD_DIRTY").unwrap_or("unknown")),
    );
    Ok(command)
}
struct MissingModel;
impl ModelGateway for MissingModel {
    fn complete<'a>(
        &'a self,
        _: excalibur_types::runtime::ModelRequest,
    ) -> excalibur_types::runtime::BoxFuture<
        'a,
        Result<excalibur_types::runtime::ModelResponse, ModelError>,
    > {
        Box::pin(async { Err(ModelError::Configuration("model is not configured".into())) })
    }
}
pub fn model(
    command: &mut Command,
    env: impl Fn(&str) -> Option<String>,
) -> Result<(Arc<dyn ModelGateway>, Redactor), String> {
    let redactor = Redactor::default();
    if let Some(path) = &command.replay {
        let tape = ReplayTape::load(path).map_err(|e| e.to_string())?;
        if let Some(seed) = &tape.seed
            && let Ok(spec) =
                serde_json::from_value::<excalibur_engine::RunSpec>(seed["spec"].clone())
        {
            if let Ok(layer) = serde_json::from_str::<Value>(&spec.context.layers[1]) {
                if !command.explicit.contains("--seed") {
                    command.options.metadata.seed = layer["sampling"]["seed"].as_u64();
                }
                if !command.explicit.contains("--temperature") {
                    command.options.metadata.temperature =
                        layer["sampling"]["temperature"].as_f64();
                }
            }
            if !command.explicit.contains("--reasoning-effort") {
                command.effort = spec.context.reasoning_effort.clone();
            }
            command.options.metadata.reasoning_effort = command.effort.clone();
            if command.options.request.trim().is_empty() {
                command.options.request = spec.context.layers[3].clone();
            }
            let requested = command.options.budget.clone();
            command.options.budget = spec.budget;
            if command.explicit.contains("--max-steps") {
                command.options.budget.max_steps = requested.max_steps;
            }
            if command.explicit.contains("--max-tokens") {
                command.options.budget.max_tokens = requested.max_tokens;
            }
            if command.explicit.contains("--timeout") {
                command.options.budget.max_wall_seconds = requested.max_wall_seconds;
            }
            if command.explicit.contains("--budget-usd") {
                command.options.budget.max_cost_microusd = requested.max_cost_microusd;
            }
            if !command.explicit.contains("--output-schema") {
                command.options.output_schema = spec.context.output_schema.clone();
            }
            if !command.explicit.contains("--criterion") {
                command.options.criteria = spec.criteria;
                if matches!(command.options.criteria.last(),Some(excalibur_engine::Criterion::FinalResponse{schema})if schema==&command.options.output_schema)
                {
                    command.options.criteria.pop();
                }
            }
            if !command.explicit.contains("--profile")
                && let Ok(layer) = serde_json::from_str::<Value>(&spec.context.layers[1])
                && let Ok(profile) = serde_json::from_value::<excalibur_eval::profile::FrozenProfile>(
                    layer["profile"].clone(),
                )
            {
                command.options.profile = ExecutionProfile::Eval;
                if !command.explicit.contains("--profile-override") {
                    command.options.profile_override = profile.overrides;
                }
            }
        }
        let replay = ReplayGateway::new(tape).map_err(|e| e.to_string())?;
        command.options.metadata.provider = replay.metadata().provider;
        command.options.metadata.model = replay.metadata().model;
        command
            .options
            .metadata
            .extra
            .insert("replay".into(), serde_json::json!(true));
        return Ok((Arc::new(replay), redactor));
    }
    let model = command.model.clone().or_else(|| env("EXCALIBUR_MODEL"));
    let endpoint = command
        .endpoint
        .clone()
        .or_else(|| env("EXCALIBUR_BASE_URL"))
        .unwrap_or_else(|| "https://api.openai.com/v1/".into());
    if model.as_ref().is_none_or(|s| s.trim().is_empty())
        || command.options.request.trim().is_empty()
        || command
            .options
            .required_inputs
            .values()
            .any(|v| v.as_ref().is_none_or(|s| s.trim().is_empty()))
    {
        command.options.blocked_reason =
            Some("mandatory model identifier or task request is missing".into());
        return Ok((Arc::new(MissingModel), redactor));
    }
    let Some(key) = env("EXCALIBUR_API_KEY") else {
        command.options.blocked_reason =
            Some("mandatory model credential is missing from the host environment".into());
        return Ok((Arc::new(MissingModel), redactor));
    };
    redactor.register_secret(
        &SecretValue::new(key.as_bytes().to_vec()).map_err(|_| "invalid host model credential")?,
    );
    let client = ProviderClient::with_base_url(
        Provider::OpenAi,
        Secret::new(key).map_err(|_| "invalid model credential")?,
        &endpoint,
    )
    .map_err(|_| "invalid OpenAI-compatible provider configuration")?;
    let mut config = GatewayConfig::new(model.unwrap(), ModelRole::Worker);
    config.reasoning_effort = command.effort.clone();
    config.seed = command.options.metadata.seed;
    config.temperature = command.options.metadata.temperature;
    config.max_output_tokens = command.options.budget.max_tokens.min(4096) as u32;
    let mut gateway = OpenAiGateway::new(client, config).map_err(|e| e.to_string())?;
    if let Some(path) = &command.prices {
        let table: PriceTable =
            serde_json::from_value(read_json(path.to_str().ok_or("invalid price table path")?)?)
                .map_err(|_| "invalid versioned price table")?;
        gateway = gateway.with_price_table(table).map_err(|e| e.to_string())?;
    }
    if command.options.budget.max_cost_microusd.is_some() && !gateway.cost_is_known() {
        command.options.blocked_reason =
            Some("USD budget requires a matching versioned price table".into());
    }
    if command.options.metadata.provider.is_none() {
        command.options.metadata.provider = Some("openai-compatible".into());
    }
    command
        .options
        .metadata
        .extra
        .insert("provider_connector".into(), serde_json::json!("OpenAi"));
    command.options.metadata.model = gateway.metadata().model;
    command.options.metadata.reasoning_effort = command.effort.clone();
    command.options.metadata.role = Some("coordinator".into());
    Ok((Arc::new(gateway), redactor))
}
pub const BROWSER_HELP: &str = "excalibur browser takeover <session> [--socket PATH] [--tab TAB] [--run RUN] [--timeout-ms N] [--json]\n\nStart human takeover for a blocked browser session (arch 17). Thin wrapper over the webd `takeover.start` RPC (plus `viewer.issue` with --tab); owner checks stay with webd, no new authorization. Socket defaults to EXCALIBUR_WEB_SOCKET. The viewer address is printed by `excalibur-webd --serve`; forward it over SSH.";
pub const BROWSER_SOCKET_ENV: &str = "EXCALIBUR_WEB_SOCKET";
pub struct BrowserTakeover {
    pub session: String,
    pub socket: PathBuf,
    pub tab: Option<String>,
    pub run: String,
    pub timeout_ms: u64,
    pub json: bool,
}
fn flag_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("missing value for {flag}"))
}
fn timeout_ms(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|v| *v > 0 && *v <= 300_000)
        .ok_or_else(|| "timeout must be 1..300000 ms".into())
}
pub fn parse_browser_takeover(args: &[String]) -> Result<BrowserTakeover, String> {
    if args.first().map(String::as_str) != Some("browser") {
        return Err(BROWSER_HELP.into());
    }
    if args.get(1).map(String::as_str) != Some("takeover") {
        return Err(BROWSER_HELP.into());
    }
    let session = args
        .get(2)
        .filter(|s| !s.starts_with('-') && !s.is_empty())
        .cloned()
        .ok_or_else(|| "browser takeover requires a session id".to_string())?;
    let mut socket: Option<PathBuf> = None;
    let mut tab: Option<String> = None;
    let mut run: Option<String> = None;
    let mut timeout = 30_000;
    let mut json = false;
    let mut index = 3;
    while index < args.len() {
        match args[index].as_str() {
            "--socket" => socket = Some(flag_value(args, &mut index, "--socket")?.into()),
            "--tab" => tab = Some(flag_value(args, &mut index, "--tab")?),
            "--run" => run = Some(flag_value(args, &mut index, "--run")?),
            "--timeout-ms" => timeout = timeout_ms(&flag_value(args, &mut index, "--timeout-ms")?)?,
            "--json" => {}
            other => return Err(format!("unknown browser flag: {other}")),
        }
        if args[index].as_str() == "--json" {
            json = true;
        }
        index += 1;
    }
    if tab.as_deref().is_some_and(str::is_empty) {
        return Err("tab id must not be empty".into());
    }
    let socket = socket
        .or_else(|| std::env::var(BROWSER_SOCKET_ENV).ok().map(PathBuf::from))
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            "webd socket is required: pass --socket PATH or set EXCALIBUR_WEB_SOCKET".to_string()
        })?;
    let run = run.unwrap_or_else(|| {
        format!(
            "cli-takeover-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        )
    });
    if run.len() > 256 {
        return Err("run id must fit in 256 bytes".into());
    }
    Ok(BrowserTakeover {
        session,
        socket,
        tab,
        run,
        timeout_ms: timeout,
        json,
    })
}
async fn rpc_unix(
    socket: &PathBuf,
    method: &str,
    params: Value,
    timeout: u64,
) -> Result<Value, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let id = format!(
        "cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let mut body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": method, "params": params,
    }))
    .map_err(|e| e.to_string())?;
    if body.len() > 4 * 1024 * 1024 {
        return Err("browser request exceeds frame limit".into());
    }
    body.push(b'\n');
    let connect = tokio::net::UnixStream::connect(socket);
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_millis(timeout.min(30_000)),
        connect,
    )
    .await
    .map_err(|_| "webd socket connection timed out".to_string())?
    .map_err(|_| "cannot reach webd socket; is the daemon serving?".to_string())?;
    let write = async { stream.write_all(&body).await };
    tokio::time::timeout(std::time::Duration::from_millis(timeout), write)
        .await
        .map_err(|_| "browser request timed out".to_string())?
        .map_err(|_| "browser request interrupted".to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_millis(timeout),
        reader.read_until(b'\n', &mut line),
    )
    .await
    .map_err(|_| "browser response timed out".to_string())?
    .map_err(|_| "browser response interrupted".to_string())?;
    let value: Value =
        serde_json::from_slice(&line).map_err(|_| "invalid browser response".to_string())?;
    if value["id"] != id {
        return Err("browser response id mismatch".to_string());
    }
    if !value["error"].is_null() {
        return Err(format!(
            "{}:{}",
            value["error"]["message"]
                .as_str()
                .unwrap_or("browser error"),
            value["error"]["data"]["detail"].as_str().unwrap_or("")
        ));
    }
    Ok(value["result"].clone())
}
pub async fn browser_takeover(command: &BrowserTakeover) -> Result<Value, String> {
    let takeover = rpc_unix(
        &command.socket,
        "takeover.start",
        serde_json::json!({
            "run_id": command.run,
            "session_id": command.session,
            "deadline_ms": command.timeout_ms,
        }),
        command.timeout_ms,
    )
    .await?;
    let mut outcome = serde_json::json!({
        "session_id": command.session,
        "takeover": takeover,
    });
    if let Some(tab) = &command.tab {
        let viewer = rpc_unix(
            &command.socket,
            "viewer.issue",
            serde_json::json!({
                "run_id": command.run,
                "session_id": command.session,
                "tab_id": tab,
                "deadline_ms": command.timeout_ms,
            }),
            command.timeout_ms,
        )
        .await?;
        outcome["viewer"] = viewer;
    }
    Ok(outcome)
}
