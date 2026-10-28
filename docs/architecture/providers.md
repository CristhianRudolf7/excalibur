# Planned provider support

[Español](providers.es.md)

Excalibur plans to integrate the following 42 providers or access routes for language models, image generation, and video generation. This table combines the previous lists without duplicate providers; modalities are identified targets, not an exhaustive capability list. See [current implementation and limits](provider-implementation.md) for the operations implemented and those still pending.

| Provider or access route | Planned modalities |
|---|---|
| OpenAI | text, image |
| Anthropic | text |
| Google AI (AI Studio) | text, image, video |
| Meta Model API | text, image, video |
| xAI | text, image, video |
| DeepSeek | text |
| Mistral AI | text |
| Moonshot AI | text |
| Alibaba Cloud | text, image, video |
| Z.ai | text |
| MiniMax | text, image, video |
| Cohere | text |
| OpenRouter | text |
| Amazon Bedrock | text |
| Microsoft Foundry | text |
| Google Vertex AI | text, image, video |
| Together AI | text, visual models (verify image/video) |
| Groq | text |
| Fireworks AI | text |
| Ollama | text |
| NVIDIA NIM | text |
| Cerebras | text |
| DeepInfra | text |
| Hugging Face Inference | text |
| Cloudflare Workers AI | text |
| Baseten | text |
| Perplexity | text |
| AI21 Labs | text |
| Replicate | text, image, video |
| Vercel AI Gateway | text |
| Adobe Firefly | image, video |
| Stability AI | image |
| Black Forest Labs | image |
| Ideogram | image |
| Recraft | image |
| Runway | image, video |
| Luma AI | image, video |
| Kling AI | image, video |
| Pika | image, video |
| Vidu | video |
| BytePlus ModelArk | image, video |
| fal | image, video |

Ollama is a local runtime, not a cloud provider. Gateways and hosting platforms may offer overlapping third-party models.

## Planned GPU compute providers

These are integration targets; see [current implementation and limits](provider-implementation.md#gpu-compute) for actual coverage.

| Provider | Intended use |
|---|---|
| Modal | Run GPU workloads on demand. |
| Runpod | Run GPU workloads using pods or serverless endpoints. |
| Lambda Cloud | Create and manage GPU virtual machines. |
| DigitalOcean GPU Droplets | Create and manage GPU-backed servers. |
| Baseten | Deploy and serve models on managed infrastructure. |
