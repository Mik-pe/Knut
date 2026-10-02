# Connection and configuration

## ChatGPT

Start `knut`, press **F2** (or open `/settings`), and choose **Continue with
ChatGPT**. Finish consent in your browser and select a model. Knut activates
the connection immediately and saves the selection ahead of environment defaults.
Changing connections requires an idle session, including when approval is pending.
Escape cancels sign-in and preserves the draft.

Settings can select or add an account, sign out, and open **Manage usage**.
Choose **Use environment configuration** to return to API settings while keeping
the account signed in. A usage-limit error exposes **Alt+U** to open usage.

The terminal commands provide the same account operations:

```sh
knut login openai-codex
knut accounts
knut accounts <client-id>
knut login openai-codex --new
knut logout openai-codex
```

For environment-based selection after sign-in:

```sh
export KNUT_PROVIDER=openai-codex
knut models
export KNUT_PROVIDER_MODEL="model-id-from-the-list"
knut
```

Model listings and picker entries are candidates; a task confirms access.
The plan route does not fall back to API billing.

Knut keeps its own account registrations and renewable tokens under
`~/.config/knut/`. `XDG_CONFIG_HOME` or `KNUT_CONFIG_DIR` changes that location.
Storage uses owner-only permissions and atomic token rotation; refreshes are
serialized across processes. Running connections retain their chosen account
even if another process switches accounts. Sign-out attempts remote revocation
and clears local tokens, reporting revocation failures. Knut does not read
Codex credentials or ChatGPT conversation history.

## API providers

OpenAI API:

```sh
export KNUT_PROVIDER=openai
export OPENAI_API_KEY="your-api-key"
export KNUT_PROVIDER_MODEL="gpt-6.1-sol"
knut doctor
knut models
knut
```

Z.ai is the default when no saved ChatGPT model or explicit provider is selected:

```sh
export KNUT_PROVIDER=zai
export ZAI_API_KEY="your-api-key"
knut
```

For a compatible Chat Completions endpoint, set all connection values explicitly:

```sh
export KNUT_PROVIDER=chat-completions
export KNUT_PROVIDER_API_KEY="your-api-key"
export KNUT_PROVIDER_BASE_URL="https://your-provider.example/v1"
export KNUT_PROVIDER_MODEL="your-model-id"
knut
```

Use **Use environment configuration** in Settings if a saved ChatGPT selection
is active. **F4** opens the current provider's searchable model catalog. API
selections are saved for that provider and endpoint; **Use environment
configuration** clears saved model choices and restores environment defaults.
`doctor` checks configuration offline; `doctor --live` makes one real
call per configured provider and may incur charges.

## Environment reference

Defaults below come from Knut's provider adapters.

| Variable | Purpose / default |
| --- | --- |
| `KNUT_PROVIDER` | `zai` (default), `openai`, `openai-codex`, or `chat-completions` |
| `KNUT_PROVIDER_API_KEY` | Explicit API credential; otherwise `OPENAI_API_KEY` for OpenAI or `ZAI_API_KEY` for other API endpoints |
| `KNUT_PROVIDER_BASE_URL` | `https://api.z.ai/api/coding/paas/v4` or `https://api.openai.com/v1`; set explicitly for compatible endpoints |
| `KNUT_PROVIDER_MODEL` | `glm-5.3-flash` or `gpt-6.1-sol` for OpenAI |
| `KNUT_PROVIDER_REASONING_EFFORT` | Optional `low`, `medium`, `high`, `xhigh`, or `max`; validated against the configured model/endpoint |
| `KNUT_PROVIDER_TIER` | `fast`, `standard`, or `reasoner`; default `reasoner` |
| `KNUT_PROVIDER_TIMEOUT_SECONDS` | Positive request timeout; default 120 seconds |
| `KNUT_PROFILE` | `auto` (default), `general`, or `coding` |
| `KNUT_LOAD_ENV` | Set to `0` to skip the working directory's `.env` |
| `KNUT_CONFIG_DIR` | Override account, model and appearance settings storage |
| `KNUT_SESSION_STORE` | Override the SQLite session/input store |
| `KNUT_TUI_COLORS` | Override detection with `truecolor`, `256`, `16`, or `none` |
| `KNUT_TUI_MOTION` | Set to `off` for reduced motion |
| `TYPESAFE_API_KEY` | Optional Jev decision-model credential |
| `TYPESAFE_MODEL` | Jev model override; default `jev-1.13.0` |
| `TYPESAFE_BASE_URL` | Jev endpoint override; default `https://api.typesafe.ai` |
| `KNUT_UPDATE_TARGET` | Optional absolute installed binary path for [installation updates](development.md#installation-updates) |

The CLI loads simple `KEY=value` lines from the working directory's `.env`
before setup. Exported values take priority. Hosts can set `KNUT_LOAD_ENV=0`
and supply an isolated environment.

OpenAI profiles use streaming Responses requests with `store: false`, preserve
reasoning items and input history, and validate completion before accepting a
response. ChatGPT tokens are restricted to the OpenAI API origin. See the
[provider compatibility matrix](../crates/knut-runtime/src/matrix.rs) for adapter details.

Without Jev, the reasoner chooses tools directly. An unusable Jev configuration
produces a warning and uses that path; a configured live Jev call failing remains
an explicit error. Decision priorities are advisory and cannot grant permission
or replace check evidence.
