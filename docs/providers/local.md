# Offline Mode: Local Model Fallback (Ollama / llama.cpp / vLLM)

ZeptoClaw agents can keep working when the internet drops. Edge deployments
(factory-floor robots, remote sensors, vehicles) get flaky or no connectivity;
a local model takes over when the cloud provider fails, and the cloud is
retried automatically once it comes back.

## How it works

- Any OpenAI-compatible local server is supported through the OpenAI backend:
  **Ollama** (built-in spec, default `http://localhost:11434/v1`),
  **llama.cpp `server`** (`/v1` OpenAI-compatible API), **vLLM**, **LM Studio**,
  or any generic OpenAI-compatible endpoint.
- Local providers are **keyless**: when a provider spec has
  `api_key_required: false` (ollama, vllm) and no key is configured, requests
  are sent without an `Authorization` header.
- **Fallback chain**: with `providers.fallback.enabled`, providers are tried in
  registry order; `providers.fallback.provider` pins the preferred fallback.
  Cloud errors trigger the fallback, and the cloud provider is retried after a
  cooldown — no config change needed when connectivity returns.
- **Tool calling**: models with native OpenAI-style `tool_calls` support
  (e.g. Qwen2.5-7B-Instruct, Hermes-2-Pro) work fully; smaller text-only models
  degrade gracefully (no tool execution from that model).

## Walkthrough: Raspberry Pi 4/5 with Ollama

1. Install Ollama and pull a small edge model:

   ```bash
   curl -fsSL https://ollama.com/install.sh | sh
   ollama pull qwen2.5:0.5b        # ~400 MB, fast on 4 GB boards
   ```

2. Point ZeptoClaw at it and prefer Claude when online:

   ```json
   {
     "providers": {
       "anthropic": { "api_key": "sk-ant-..." },
       "ollama": { "model": "qwen2.5:0.5b" },
       "fallback": { "enabled": true, "provider": "ollama" }
     }
   }
   ```

   No `api_base` needed — the Ollama spec defaults to
   `http://localhost:11434/v1`. No `api_key` either.

3. Verify resolution:

   ```bash
   zeptoclaw provider status   # shows the resolved chain incl. ollama
   ```

## Other local backends

All use the same OpenAI-compatible wire format — only `api_base` changes:

| Server | Config |
|---|---|
| llama.cpp `server` | `"vllm": { "api_base": "http://localhost:8080/v1", "model": "qwen2.5-0.5b" }` |
| vLLM | `"vllm": { "api_base": "http://localhost:8000/v1", "model": "Qwen/Qwen2.5-0.5B-Instruct" }` |
| LM Studio | `"vllm": { "api_base": "http://localhost:1234/v1", "model": "..." }` |
| Generic OpenAI-compat | any provider entry with `api_base` pointing at the endpoint |

(`vllm` is the generic keyless OpenAI-compatible slot; `ollama` is a dedicated
built-in spec. For servers that need a dummy key, set `api_key` to any value.)

## Reference validation

```bash
ollama pull qwen2.5:7b-instruct   # tool-calling reference model
zeptoclaw                          # then: ask it to run a tool (e.g. "list files")
```

Qwen2.5-7B-Instruct emits native `tool_calls`; tool execution works end to end
against the local server.
