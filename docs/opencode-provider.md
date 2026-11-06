# OpenCode provider

Flint's native agent can use OpenCode's OpenAI-compatible Chat Completions API.
There are two endpoints, and they share one API surface but differ in billing
and model catalogue:

| Preset | Base URL | Key env | Billing |
| :--- | :--- | :--- | :--- |
| OpenCode Zen | `https://opencode.ai/zen/v1` | `OPENCODE_API_KEY` | Pay-as-you-go |
| OpenCode Go | `https://opencode.ai/zen/go/v1` | `OPENCODE_GO_API_KEY` | Subscription |

Both presets stage as a **custom** provider, so they use only their own key
source and never fall back to `FLINT_API_KEY` or `OPENAI_API_KEY`.

1. Create a key at `https://opencode.ai/auth`.
2. Export the environment variable for the preset you use, or write the key to a
   private file and select it in **Settings → API key file**:
   ```sh
   export OPENCODE_GO_API_KEY=...   # or OPENCODE_API_KEY for Zen
   ```
3. Open **Settings → Provider → OpenCode Zen / OpenCode Go**.
4. Keep the default model (`deepseek-v4.1-flash`) or enter any exact model ID from
   that endpoint's listing, for example `glm-5.3-flash`, `kimi-k2.7-code`,
   `claude-sonnet-5`, or `gpt-5.2-codex` on Zen.
5. Optional: set a smaller **Subagent model**; it uses the same endpoint and key.
6. Use **Test connection (GET /models)**, then **Save**. Start a new native
   session; already running sessions keep their captured provider configuration.

`/models` on both endpoints answers without credentials, so a successful listing
shows reachability and model names, not that your key, streaming, or tool calls
work. Flint's connection check never sends a completion request; inference is
verified only by starting a session.

## Related: OpenCode as an agent

Selecting **OpenCode** in the *agent* picker is a different path. It launches
`opencode acp` and inherits your OpenCode CLI login, providers, models, and
project configuration — Flint never sees those credentials. Use the provider
presets above when you want Flint's own native agent (its tools, approvals,
sandbox, queue and worktrees) driving an OpenCode model.

## Limits

- Both endpoints are OpenAI-compatible; Flint does not implement OpenCode's own
  Anthropic-style `/messages` mode. Models that require it must go through the
  agent picker instead.
- Model catalogues change. A model ID accepted by one plan may be rejected by the
  other; the preset's default is listed by both at the time of writing.
- Flint does not read, copy, or modify OpenCode's `auth.json`.
