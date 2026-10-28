# Soporte previsto de proveedores

[English](providers.md)

Excalibur prevé integrar los siguientes 42 proveedores o vías de acceso para modelos de lenguaje, generación de imágenes y video. La tabla reúne las listas anteriores sin duplicar proveedores; las modalidades indican los objetivos identificados, no una lista exclusiva de capacidades. Consulta [implementación y límites actuales](provider-implementation.es.md) para conocer las operaciones implementadas y las pendientes.

| Proveedor o vía de acceso | Modalidades previstas |
|---|---|
| OpenAI | texto, imagen |
| Anthropic | texto |
| Google AI (AI Studio) | texto, imagen, video |
| Meta Model API | texto, imagen, video |
| xAI | texto, imagen, video |
| DeepSeek | texto |
| Mistral AI | texto |
| Moonshot AI | texto |
| Alibaba Cloud | texto, imagen, video |
| Z.ai | texto |
| MiniMax | texto, imagen, video |
| Cohere | texto |
| OpenRouter | texto |
| Amazon Bedrock | texto |
| Microsoft Foundry | texto |
| Google Vertex AI | texto, imagen, video |
| Together AI | texto, modelos visuales (verificar imagen/video) |
| Groq | texto |
| Fireworks AI | texto |
| Ollama | texto |
| NVIDIA NIM | texto |
| Cerebras | texto |
| DeepInfra | texto |
| Hugging Face Inference | texto |
| Cloudflare Workers AI | texto |
| Baseten | texto |
| Perplexity | texto |
| AI21 Labs | texto |
| Replicate | texto, imagen, video |
| Vercel AI Gateway | texto |
| Adobe Firefly | imagen, video |
| Stability AI | imagen |
| Black Forest Labs | imagen |
| Ideogram | imagen |
| Recraft | imagen |
| Runway | imagen, video |
| Luma AI | imagen, video |
| Kling AI | imagen, video |
| Pika | imagen, video |
| Vidu | video |
| BytePlus ModelArk | imagen, video |
| fal | imagen, video |

Ollama es un entorno de ejecución local, no un proveedor en la nube. Las pasarelas y plataformas de alojamiento pueden ofrecer modelos de terceros coincidentes.

## Proveedores de cómputo GPU previstos

Son objetivos de integración; consulta [implementación y límites actuales](provider-implementation.es.md#cómputo-gpu) para conocer la cobertura real.

| Proveedor | Uso previsto |
|---|---|
| Modal | Ejecutar cargas de trabajo en GPU bajo demanda. |
| Runpod | Ejecutar cargas de trabajo en GPU mediante pods o endpoints serverless. |
| Lambda Cloud | Crear y gestionar máquinas virtuales con GPU. |
| DigitalOcean GPU Droplets | Crear y gestionar servidores con GPU. |
| Baseten | Desplegar y servir modelos en infraestructura administrada. |
