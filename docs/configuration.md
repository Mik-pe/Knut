# Configuration and connections

[Back to the README](../README.md)

Choose a reasoner provider independently from the optional decision model.

## API provider

For OpenAI API models, including Codex models:

```sh
export KNUT_PROVIDER=openai
export OPENAI_API_KEY="your-api-key"
export KNUT_PROVIDER_MODEL=gpt-6.1-sol
knut models
knut doctor --live
knut tui
```

## ChatGPT connection

For your ChatGPT plan, start `knut`, press **F2** (or `/ settings`), and choose
**Continue with ChatGPT**. Complete consent in your browser, then choose a model
from your account's catalog. The connection activates immediately; the model
choice is saved for the next start and takes priority over environment
configuration, so no launch arguments or exports are needed. Settings also has
**Use environment configuration** to switch back explicitly; this keeps the
account signed in. Escape cancels browser sign-in and preserves your draft.
Connection changes require an idle session, including when a task is awaiting
approval. The account screen also switches accounts, adds another account,
signs out, and opens **Manage usage**. A plan limit exposes `Ctrl+U` to open usage.

This uses OpenAI's documented [open-source / locally hosted flow](https://developers.openai.com/siwc/token-sharing-open-source/)
with the [required ChatGPT labels](https://developers.openai.com/siwc/ui-ux-guidelines).
Knut sends native Responses requests through its own engine; Codex app-server
is optional infrastructure and is not needed for this sign-in.

The same sign-in is available from a normal terminal:

```sh
knut login openai-codex
export KNUT_PROVIDER=openai-codex
knut models
export KNUT_PROVIDER_MODEL=gpt-6.1-sol # choose an ID from the account's catalog
knut tui
```

The OpenAI profiles use the native Responses API with `store: false` and
streaming. They preserve encrypted reasoning items and full input history,
validate completed responses, and report interrupted streams or usage-limit
failures. `gpt-5-codex` and other account-available Responses models can also be
selected with `KNUT_PROVIDER_MODEL`. Access depends on your API account or the
ChatGPT account. The ChatGPT model picker includes GPT-6.1 Sol, GPT-6 Astra and
GPT-6 Luna alongside the account's refreshed catalog, which can omit usable
models. Access is checked when a task runs.

ChatGPT sign-in uses OpenAI's documented public-client OAuth flow with PKCE,
state, nonce and signed ID-token validation. Knut stores separate registrations
under `~/.config/knut/` (`XDG_CONFIG_HOME` or `KNUT_CONFIG_DIR` can override the
location), with owner-only permissions and atomic token rotation. Refreshes
are serialized across processes. Each connected model keeps its own account
registration, so another process changing accounts cannot switch a running task. `knut accounts` lists saved registrations;
`knut accounts <client-id>` selects one; `knut login openai-codex --new` adds an
account. `knut logout openai-codex` revokes the selected session and clears its
local tokens. If remote revocation fails, it reports that explicitly. Sign-in
does not read Codex's credentials or grant access to ChatGPT conversation history.
Review or disconnect Knut under [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage).

See OpenAI's [models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference)
and [sign-in contract](https://developers.openai.com/siwc/token-sharing-open-source/sign-in).
API-key usage is metered; ChatGPT plan usage stays on the selected plan route.
Knut never falls back from plan usage to API billing.

## Environment configuration

Z.ai is the default when no saved ChatGPT model or explicit provider is selected. Other Chat Completions providers can use
`KNUT_PROVIDER=chat-completions` with an explicit base URL and model.

| variable | purpose |
| --- | --- |
| `KNUT_PROVIDER` | `zai` (default), `openai`, `openai-codex`, or `chat-completions` |
| `KNUT_PROVIDER_API_KEY` | explicit API credential; otherwise `OPENAI_API_KEY` for OpenAI or `ZAI_API_KEY` for Z.ai/compatible endpoints |
| `KNUT_PROVIDER_BASE_URL` | endpoint prefix; defaults to Z.ai coding or `https://api.openai.com/v1` for OpenAI; ChatGPT tokens are restricted to the OpenAI origin |
| `KNUT_PROVIDER_MODEL` | model ID; defaults to `glm-5.3-flash` or `gpt-6.1-sol` for OpenAI |
| `KNUT_PROVIDER_REASONING_EFFORT` | explicit effort; GLM-5.3 uses `low`, `high`, `max`; GPT-6.1 Sol accepts `low`, `medium`, `high`, `xhigh`, `max` |
| `KNUT_PROVIDER_TIMEOUT_SECONDS` | positive request timeout; default 120 seconds |
| `TYPESAFE_API_KEY` | optional Jev credential; without it, the reasoner chooses actions directly |
| `TYPESAFE_MODEL` | Jev model override; default pinned to `jev-1.13.0` |
| `KNUT_MODE` | `quality` (default) or `adaptive` |
| `KNUT_PROFILE` | `auto` (default), `general`, or `coding` |

A decision model is optional. With only the reasoner configured, System Zero
hands tasks to the native model/tool loop. The model sees registered tool schemas
and receives results with their original call IDs. Tool policy, exact approvals,
bounded repair and revision-bound checks stay in the runtime. An unusable Jev configuration emits
a warning and uses this path; failures of a configured live Jev call remain
explicit errors.

`knut doctor` reports what is missing with actionable guidance, and makes
no paid request unless you pass `--live`.

## Updating a running installation

An operator can enable self-update by setting `KNUT_UPDATE_TARGET` to a fixed,
absolute installed binary path and opening Knut's source checkout as the
workspace. Credentials and service configuration stay outside the checkout.
Ask Knut to implement the change, inspect its installation and request an update.

`self/inspect_installation` reports the installed binary hash and source revision.
`self/install_update` requires those exact identities and a normal write approval.
It runs offline sandboxed formatting, workspace tests, Clippy and a release build,
then checks the identities again before atomically activating the new executable.
Missing dependencies, unsupported sandboxing, failed checks or changed inputs
refuse activation. No network or unsandboxed fallback is used during installation.

Existing sessions keep their original executable; new sessions use the update.
The previous executable is kept as `knut.previous` beside the installation.
Updating the binary does not restart its host service or publish the source.
Process-crash recovery belongs to the hosting adapter; installation itself never
replays tools or restores approvals.
