# Provider library: current coverage

[Español](provider-implementation.es.md)

`excalibur-providers` is an initial outbound Rust library, independent of a CLI and an HTTP server. This is partial implementation, not completion of the provider plan. No provider account, model availability, billing, or live operation has been tested. The caller supplies the model, credentials and messages; there is no credential store, discovery, tool-call normalization, structured output, automatic retry, fallback, token-cost calculation or agent loop. Text responses expose usage when provided; requests/responses can carry private content. Do not log them. Secrets are redacted from `Debug` and zeroized when their copies are dropped, but HTTP/TLS internals may make transient copies. HTTPS is required except literal loopback; redirects are disabled. A custom base URL sends credentials to the chosen host and must be trusted. Timeouts on billable submission leave the outcome unknown; do not retry blindly.

The basic `chat` method is non-streaming plain text: system/user/assistant messages, model and optional maximum output tokens. Provider and model specific restrictions apply. `native_chat` accepts native tool bodies and returns the complete response; `stream_native_chat` exposes SSE events without assembling deltas for compatible routes, Anthropic, Cohere, Google and Vertex. The caller must assemble calls and provide results in the provider format. Support varies by model; providers without the corresponding contract do not use this interface. Anthropic requires `max_tokens`. Google uses the native `generateContent` API. The basic method does not preserve reasoning or tool history; callers of the native interface must retain it in full. The common `generate_image` method only supports prompt/model and returns raw data. xAI video submission and status are separate; the caller handles polling, failed/expired jobs and temporary URLs. DigitalOcean creates a Droplet from caller-selected region, size and image; the library does not verify that the size has a GPU or provision workloads. Creating a Droplet or submitting inference may incur charges. Status errors intentionally exclude response bodies.

## Status by provider

| Provider | Implemented operation and official reference; remaining planned modalities |
|---|---|
| OpenAI | Text chat and image generation ([chat](https://developers.openai.com/api/reference/resources/chat), [images](https://developers.openai.com/api/reference/resources/images)). |
| Anthropic | Text messages ([API](https://docs.anthropic.com/en/api/messages)). |
| Google AI | Text and Gemini image generation via native body; async video submission/status ([image](https://ai.google.dev/gemini-api/docs/generate-content/image-generation), [video](https://ai.google.dev/gemini-api/docs/veo)). Video download pending. |
| Meta Model API | Compatible chat and Muse image; Responses API with native tool/reasoning history and SSE ([responses](https://dev.meta.ai/docs/api-reference/responses/create-response), [image](https://dev.meta.ai/docs/image-generation)). The [official resource list](https://dev.meta.ai/docs/api-reference) does not document a direct video-generation operation; video understanding is distinct from generation. Chat Completions does not retain reasoning across turns. |
| xAI | Text, image, video submit/status ([chat](https://docs.x.ai/developers/cost-tracking), [image](https://docs.x.ai/developers/quickstart), [video](https://docs.x.ai/developers/model-capabilities/video/generation)). |
| DeepSeek | Text ([API](https://api-docs.deepseek.com/api/create-chat-completion/)). |
| Mistral AI | Text ([API](https://docs.mistral.ai/api)). |
| Moonshot AI | Text ([quickstart](https://platform.kimi.ai/docs/get-your-api-key)). |
| Alibaba Cloud | Text via international endpoint; async text-to-image and image-to-video via regional `AlibabaMedia` ([image](https://help.aliyun.com/en/model-studio/first-call-to-image-and-video-api), [video](https://help.aliyun.com/en/model-studio/animateanyone-video-generation-api)); support depends on model and region. |
| Z.ai | Text ([API](https://docs.z.ai/guides/capabilities/mcp-call)). |
| MiniMax | Text ([API](https://platform.minimax.io/docs/api-reference/text-openai-api)); image and async video via `MiniMaxMedia` ([image](https://platform.minimax.io/docs/api-reference/image-generation-t2i), [video](https://platform.minimax.io/docs/api-reference/video-generation-v2-create)); reasoning history must be retained in the native body. |
| Cohere | Text via native v2 chat ([API](https://docs.cohere.com/v2/reference/chat)). |
| OpenRouter | Text ([API](https://openrouter.ai/docs/api/api-reference/chat/send-chat-completion-request)). |
| Amazon Bedrock | Compatible chat and native Converse/ConverseStream with tool bodies using a Bedrock API key ([ConverseStream](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html), [wire format](https://docs.aws.amazon.com/lexv2/latest/dg/event-stream-encoding.html)). The incremental decoder validates both CRCs and requires messageStop; consumer assembles events and retains reasoning signatures. SigV4 and ARN model IDs are pending; only string event headers are supported. |
| Microsoft Foundry | Text chat v1 with resource URL and `api-key`; model is the deployment name ([API](https://learn.microsoft.com/en-us/rest/api/microsoft-foundry/azureopenai/chat?view=rest-microsoft-foundry-v1)). Entra identity refresh not implemented. |
| Google Vertex AI | Gemini text and image through generateContent with supplied OAuth token and project/region ([API](https://docs.cloud.google.com/gemini-enterprise-agent-platform/reference/rest/v1/projects.locations.publishers.models/generateContent)); OAuth refresh pending; submit and query Veo video ([API](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/video/generate-videos-from-references)). |
| Together AI | Text, images and async video via `TogetherVideo` ([chat](https://docs.together.ai/docs/inference/openai-compatibility), [image](https://docs.together.ai/docs/inference/images/overview), [video](https://docs.together.ai/reference/create-videos)). |
| Groq | Text ([API](https://console.groq.com/docs/api-reference)). |
| Fireworks AI | Text ([API](https://docs.fireworks.ai/api-reference/post-chatcompletions)). |
| Ollama | Local text only ([API](https://docs.ollama.com/api/openai-compatibility)); loopback, no authentication. |
| NVIDIA NIM | Text through hosted NVIDIA endpoint ([reference](https://docs.nvidia.com/nemo/evaluator/nightly/get-started/quickstart)); other deployments require configuration. |
| Cerebras | Text ([API example](https://inference-docs.cerebras.ai/resources/glm-47-migration)). |
| DeepInfra | Text ([API example](https://deepinfra.com/blog/deepseek-v4-1-flash-model-overview-integration)). |
| Hugging Face Inference | Text via router ([reference](https://huggingface.co/docs/chat-ui/main/index)). |
| Cloudflare Workers AI | Compatible chat with account ID and token ([API](https://developers.cloudflare.com/workers-ai/configuration/open-ai-compatibility/)). |
| Baseten | Text via hosted Model APIs or a caller-configured compatible deployment URL ([API](https://docs.baseten.co/inference/model-apis/overview)); Model Labs deployment creation is covered below. |
| Perplexity | Agent API synchronous and SSE requests with native tools, plus background status and asynchronous cancellation ([quickstart](https://docs.perplexity.ai/docs/agent-api/quickstart), [background](https://docs.perplexity.ai/docs/agent-api/background-mode)); caller replays tool results. Legacy Sonar route remains but support ended. |
| AI21 Labs | Compatible Jamba chat ([API](https://docs.ai21.com/reference/jamba-1-6-api-ref)). |
| Replicate | Community, official model and existing deployment prediction submission, status and cancellation ([API](https://replicate.com/docs/reference/http/)); each model defines its input/output. SSE streaming for provider-issued URLs ([streaming](https://replicate.com/docs/topics/predictions/streaming)) handles text, terminal events and trusted hosts; file download pending. |
| Vercel AI Gateway | Text ([API](https://vercel.com/docs/ai-gateway/sdks-and-apis)); supplied gateway key or OIDC token. |
| Adobe Firefly | Submit and query asynchronous images with supplied client ID and OAuth token ([API](https://developer.adobe.com/firefly-services/docs/firefly-api/guides/how-tos/using-async-apis)); OAuth refresh pending. Video exists ([changelog](https://developer.adobe.com/firefly-services/docs/firefly-api/getting-started/changelog/)) but its [API reference](https://developer.adobe.com/firefly-services/docs/firefly-api/api/) links `/firefly-services/docs/firefly-api/firefly-api.json`, which returned HTTP 404 during verification; request schema and endpoint could not be verified in Adobe sources, so no video request is implemented. |
| Stability AI | Stable Image Core image through multipart and JSON response ([API](https://docs.stability.ai/api-reference)); preserve `finish_reason` to detect filtering. |
| Black Forest Labs | FLUX image submission and polling with `x-key` and validated `polling_url` ([API](https://docs.bfl.ai/quick_start/generating_images)); result URLs are temporary. |
| Ideogram | Synchronous Ideogram 4.5 image via JSON ([API](https://developer.ideogram.ai/ideogram-api/api-overview)). |
| Recraft | Image generation via multipart ([API](https://external.api.recraft.ai/doc/ui/?spec=external-api.yaml)). |
| Runway | Submit image/video tasks and query status ([API](https://docs.dev.runwayml.com/api/)); fields depend on model. |
| Luma AI | Submit and query image/video generation via Luma Agents ([API](https://docs.agents.lumalabs.ai/)); fields depend on model. |
| Kling AI | Image and text-to-video submission and status using a Bearer API key ([authentication](https://kling.ai/document-api/api/get-started/authentication), [image](https://kling.ai/document-api/api/image/3-0-omni), [video](https://kling.ai/document-api/api/video/3-0-omni/text-to-video/legacy)); legacy JWT and other routes pending. |
| Pika | Submit and query Pika aggregator image/video operations ([guide](https://dev.pika.art/llms.txt), [Pika 2.5](https://dev.pika.art/llms/pika/pika-2.5/text-to-video)); caller must verify the live catalog and operation schema before submission. |
| Vidu | Text-to-video submission and task status with `Token` authentication ([API](https://platform.vidu.com/docs/text-to-video)). |
| BytePlus ModelArk | Images and asynchronous video with a regional key ([image](https://docs.byteplus.com/en/docs/modelark/image-generation-api), [video](https://docs.byteplus.com/en/docs/modelark/create-video-generation-task-api)); fields and output depend on model. |
| fal | Submit, query and retrieve queued model-specific inputs ([API](https://fal.ai/docs/documentation/model-apis/inference/queue)); caller supplies application path for polling. |

## GPU compute

| Provider | Status |
|---|---|
| Modal | Invoke a deployed Web Function with proxy token ([web functions](https://modal.com/docs/guide/webhooks)); contract and GPU usage depend on the function. Creating deployments remains pending: [official guidance](https://modal.com/docs/guide/managing-deployments) documents the Python SDK and CLI, not a public Rust deployment contract. |
| Runpod | Create/list/delete, start/stop/restart GPU pods and submit/query/cancel/retry serverless jobs and fetch incremental outputs ([serverless operations](https://docs.runpod.io/serverless/endpoints/operation-reference), [pods](https://docs.runpod.io/api-reference/pods/POST/pods), [start](https://docs.runpod.io/api-reference/pods/POST/pods/podId/start), [jobs](https://docs.runpod.io/serverless/endpoints/send-requests)); readiness and other management operations remain caller responsibilities. |
| Lambda Cloud | List, launch one instance with an existing SSH key, restart and terminate ([reference](https://docs.lambda.ai/api/cloud)); other management operations pending. |
| DigitalOcean GPU Droplets | Create and delete one Droplet; list GPU Droplets by explicit page and page size ([API](https://docs.digitalocean.com/products/droplets/reference/api/droplets/)); the ordinary list excludes GPUs by default. Caller follows pages and selects a GPU size. |
| Baseten | Create a deployment from a Model Labs listing, then query, activate and deactivate it ([listing](https://docs.baseten.co/examples/model-labs/deploy-a-published-model), [lifecycle](https://docs.baseten.co/deployment/manage/lifecycle)); access to the listing is required. Own-archive creation remains pending: [official flow](https://docs.baseten.co/examples/create-a-model-with-rest) requires temporary AWS credentials and an S3 upload. |

Run `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`. Tests use local mock servers and synthetic credentials; they do not prove a provider accepts a request. There are no paid or live inference tests.
