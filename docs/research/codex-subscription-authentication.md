# Codex subscription authentication for Relic v0.1

**Research date:** 2026-08-16
**Decision scope:** a third-party, local Rust application used by the signed-in user
**Source policy:** official OpenAI documentation, Help Center articles, and the official `openai/codex` repository only

## Decision

**Qualified go for an optional installed-CLI adapter; no-go for a Relic-owned OAuth implementation.**

A local Relic process can use ChatGPT-plan-backed Codex through official client boundaries: have the user authenticate with the stable `codex login` command, then invoke the stable, documented `codex exec` command. `codex exec` reuses saved CLI authentication by default, and OpenAI lists “Codex SDK, `codex exec`, and scriptable workflows” as available on both ChatGPT Plus and Pro.[^auth][^noninteractive][^pricing]

Relic must not act as an OAuth client, call Codex token/private endpoints, copy or inspect Codex credentials, or impersonate Codex's registered client. The official docs reviewed do not publish a general OAuth client-registration or token contract for an independent Rust client. An official Python SDK can start browser or device-code login, and the experimental app-server exposes deeper authentication integration, but there is no documented stable Rust SDK.[^sdk][^appserver]

**ChatGPT Pro does not include general OpenAI API use.** Pro includes Codex with plan limits, but Platform API billing is separate. A direct OpenAI-compatible HTTP provider therefore requires a separately billed Platform API key (or another separately configured provider), not a ChatGPT subscription token.[^pro][^billing][^auth]

## Findings

### 1. Plan entitlement includes Codex scripting, not general API credit

- OpenAI's Codex pricing matrix marks the Codex CLI and “Codex SDK, `codex exec`, and scriptable workflows” available for ChatGPT Plus and Pro. The plan allowance is subject to Codex/ChatGPT limits; it is not an unlimited API quota.[^pricing]
- The Codex authentication guide distinguishes **ChatGPT sign-in for subscription access** from **API-key sign-in for usage-based access**. With an API key, OpenAI bills the Platform account at standard API rates and does not use included ChatGPT plan credits.[^auth]
- The Plus Help Center article explicitly says API usage is not included and is billed independently. The billing guide says ChatGPT and the API Platform have separate billing systems. The same boundary applies to Pro: Pro includes the Codex product, not arbitrary Responses/Chat Completions API calls.[^plus][^billing][^pro]

**Consequence for Relic:** subscription-backed use is limited to documented Codex surfaces. Relic cannot exchange the ChatGPT/Codex session for a bearer token to call the general OpenAI API.

### 2. Supported authentication paths

The official local-client choices are:

1. `codex login` opens the browser-based ChatGPT OAuth flow. This is the default and the command is marked **Stable**.[^auth][^commands]
2. `codex login --device-auth` provides device-code login for headless/localhost-callback failures. It must be enabled in the user's ChatGPT security settings or by a workspace administrator and is explicitly **beta**, so Relic must not make it a hard dependency.[^auth]
3. `codex login --with-api-key` accepts a Platform API key on stdin and switches usage to separately billed API access.[^auth]
4. Business/Enterprise can issue scoped Codex access tokens for trusted noninteractive local workflows. The plan matrix marks these unavailable for Plus and Pro; they are not a personal-plan automation solution and are not general API credentials.[^access-tokens][^pricing]

A third-party Rust app has two materially different choices:

- **Supported for v0.1:** launch the official `codex login` executable with an attached terminal/browser flow, then launch `codex exec`. Codex, not Relic, is the OAuth client and credential manager.
- **Not supported as a stable Rust contract:** reproduce the OAuth/device flow inside Relic using endpoints or client identifiers observed in source. Open source availability is not OAuth client registration. The app-server is documented for deep product integration, including authentication, but its command/protocol is currently experimental and “may change without notice”/is unsupported for production workloads.[^commands][^appserver]

The stable Python SDK is evidence that OpenAI supports application integration in principle: it reuses Codex authentication and exposes browser/device-code login methods. It is not a stable native Rust API, and adding a Python runtime plus a pinned Codex runtime is a poor fit for Relic's compact v0.1 binary.[^sdk][^python-source]

### 3. `codex exec` is a supported stable subprocess interface

OpenAI documents `codex exec` specifically for scripts, pipelines, scheduled jobs, and machine-readable output. It:

- is labeled **Stable** in the CLI command reference;
- reads a prompt or stdin, sends progress to `stderr`, and normally sends the final response to `stdout`;
- offers JSONL with `--json`, documented event types, and JSON Schema-constrained final output with `--output-schema`;
- reuses saved CLI authentication by default;
- offers `--ephemeral` to avoid persisting session rollout files; and
- defaults to a read-only sandbox in noninteractive mode.[^commands][^noninteractive]

The maturity definition says Stable features are fully supported, ready for broad production use, maintain consistent behavior/configuration, and normally use a deprecation process for removals.[^maturity] The official TypeScript Codex SDK also uses the same architectural boundary: it spawns the Codex CLI and exchanges JSONL over stdin/stdout.[^typescript-source]

This is a supported interface, not a forever-frozen ABI. Relic should declare and test a minimum CLI version, tolerate unknown additive JSONL events, treat nonzero exit status as authoritative, and never parse the human TUI.

For longer-lived orchestration, `codex mcp-server` is also documented and marked Stable, and it is explicitly intended for another MCP client/agent to consume. It is more surface area than Relic v0.1 needs; prefer one-shot `exec` unless a prototype proves thread continuity is essential.[^mcp][^commands]

### 4. Credential storage and lifecycle belong to Codex

- Codex caches login material in the OS credential store or in plaintext `$CODEX_HOME/auth.json` (normally `~/.codex/auth.json`). `cli_auth_credentials_store = "keyring"` selects the OS credential store; `auto` may fall back to the plaintext file.[^auth]
- The CLI and IDE extension share the cached login. Codex refreshes ChatGPT tokens during use; logging out of either client invalidates their shared cached login.[^auth]
- The auth file contains access tokens and must be treated like a password. OpenAI says not to commit, paste, or share it. Although OpenAI documents narrowly controlled cache transfer for trusted headless environments, Relic must not use that pattern: it has no need to possess the cache and should preserve the stronger process boundary.[^auth]
- `codex logout` is the credential-removal command. Relic may offer to invoke it only as an explicit user action because it also signs the user's other local Codex client out.[^commands][^auth]

**Relic ownership rule:** Relic owns only child-process lifecycle, bounded prompt input, validated output, and nonsecret provider metadata (for example, executable version and auth-present/auth-absent). Codex owns OAuth, refresh, storage, and logout. Relic must never read, log, back up, migrate, or modify the credential store.

### 5. Open source and terms boundaries

At source snapshot [`00f6a8a60e5c5e93d185c7fe67fd596b7e62240f`](https://github.com/openai/codex/tree/00f6a8a60e5c5e93d185c7fe67fd596b7e62240f) (2026-08-15), the official repository recommends ChatGPT-plan sign-in and is Apache-2.0 licensed.[^repo-readme][^license] Apache-2.0 permits use, modification, and redistribution subject to its conditions, but does not grant OpenAI trademark rights and does not create a service entitlement.[^license]

Service use remains governed by the applicable ChatGPT terms and plan controls. The Pro Help Center article calls out prohibitions on credential sharing, resale, using ChatGPT to power third-party services, and certain automatic/programmatic extraction.[^pro] The Codex SDK and pricing docs simultaneously endorse app integration and scriptable workflows.[^sdk][^pricing] The safe product boundary is therefore a **single-user local tool using that same user's official Codex client**, not a Relic-hosted relay, pooled account, resale, or multi-tenant service. See “Clarification needed” below for the remaining wording ambiguity.

## Feasible options, ranked

| Rank | Option | Subscription-backed? | v0.1 assessment |
|---|---|---:|---|
| 1 | Spawn installed `codex login`/`codex exec --json` | Yes, when the user's CLI is signed in with ChatGPT | **Preferred optional adapter.** Stable, documented, no credential custody, and compatible with Rust subprocess I/O. |
| 2 | Direct OpenAI-compatible HTTP with a Platform API key | No | **Required fallback/default boundary.** Simplest pure-model contract, but billed separately from ChatGPT. Keep provider-configurable. |
| 3 | Official Python Codex SDK sidecar | Yes | Stable and supports login, but adds Python and a bundled/pinned Codex runtime; defer unless the CLI adapter cannot meet output or lifecycle needs. |
| 4 | `codex mcp-server` over stdio | Likely reuses the authenticated Codex client; verify in prototype | Stable and intended for agent consumers, but stateful and broader than one-shot recall. Defer. |
| 5 | Codex app-server client | Potentially | **No-go for production v0.1:** authentication-rich but experimental/unsupported for production. |
| — | Relic-native OAuth, token-file reuse, private endpoint, or client impersonation | Would attempt to be | **No-go.** No documented independent Rust OAuth contract; violates the credential boundary. |

## Recommended v0.1 subprocess contract

If the go gates below pass, the adapter should:

1. Discover `codex` on `PATH`, record `codex --version`, and require a tested minimum version.
2. Run `codex login status` and use only its exit status plus a coarse authenticated/unauthenticated result. If unauthenticated, explain that the user will authenticate with OpenAI and offer an explicit, attached-terminal `codex login` action. Device auth is an opt-in fallback, labeled beta.
3. Pipe redacted prompt/context over stdin rather than putting it in the process argument list. Use the documented `codex exec -` stdin form.[^noninteractive]
4. Request `--json`, `--ephemeral`, an explicit read-only sandbox, and no approval escalation. Run in a dedicated empty temporary Git workspace rather than the user's source tree. Do not inherit arbitrary MCP/plugin configuration if documented flags can exclude it; verify the effective invocation in the prototype.
5. Accept only the expected final agent/turn result, reject tool/action events that the adapter does not support, enforce timeout and cancellation, and never place raw JSONL, prompts, or model output in routine logs.
6. Display that input leaves the machine for OpenAI and that usage draws from the user's Codex plan allowance. Preserve Relic's redaction-before-egress rule.

The read-only sandbox reduces mutation risk, but Codex is an agent rather than a raw completion endpoint. OpenAI's sandbox docs explain that sandboxing and approvals bound local commands, while user configuration and other tools can add capabilities.[^sandbox] A prototype must prove the effective invocation cannot silently execute or exfiltrate reconstructed procedures before this adapter ships.

## Security and UX implications

- **Best security property:** Relic never holds a refresh token, access token, API key, OAuth client secret, or auth cache.
- **Shared-session surprise:** CLI/IDE login state is shared. A user logout or workspace policy change can break Relic; Relic must report “Codex authentication unavailable” rather than trying to repair files.
- **Visible consent:** the first login is an OpenAI browser/device experience. Do not hide it inside a background task or call it “Relic sign-in.” Show the account/provider boundary and make cancellation safe.
- **Plan limits:** subscription-backed calls can rate-limit or become unavailable. They have plan/workspace entitlements, not a general API service-level guarantee. Fail closed and offer the configured fallback.
- **Local agent risk:** even read-only commands may inspect more than intended if the process receives a broad working directory, environment, user rules, MCP servers, or plugins. Use an isolated workspace, a minimal environment, explicit restrictions, and adversarial tests.
- **Data governance:** ChatGPT-authenticated Codex follows ChatGPT workspace permissions and data-handling controls; API-key use follows the API organization's settings. Surface which mode is active without exposing credential material.[^auth]
- **Terms boundary:** do not pool accounts, run a hosted Relic proxy on a user's personal plan, resell access, or allow one user's credentials to serve other users.[^pro]

## Go/no-go rule

Ship the optional subscription adapter only if a pinned CLI prototype on both macOS and Linux proves all of the following:

1. only documented **Stable** commands/flags are required (`login`, `login status`, `exec`); device auth is optional;
2. Plus and Pro smoke tests successfully run `exec --json` using normal user-owned ChatGPT sign-in, without Relic accessing credentials;
3. prompt input stays off argv, `--ephemeral` leaves no rollout, JSONL can be parsed forward-compatibly, and auth/rate-limit/cancellation failures are distinguishable;
4. the effective sandbox/configuration prevents writes, network/tool escalation, reconstructed-command execution, and reads outside the intentionally supplied temporary workspace—or OpenAI documents a supported no-tools mode that does so;
5. product use remains local, single-user, user-initiated, and is not a hosted/pool/resale service; and
6. direct API-key fallback remains available and the Codex adapter is nonblocking for v0.1.

**No-go if any security gate requires reading/copying `auth.json`, calling OAuth/token endpoints, using app-server experimental APIs, trusting human-formatted output, or silently executing agent tools.** In that case, omit subscription support and ship the configured OpenAI-compatible HTTP boundary.

## Fallback

The unconditional fallback is Relic's small OpenAI-compatible HTTP provider using a user-supplied Platform API key (or another compatible provider) with its own billing and data policy. Explain clearly that ChatGPT Plus/Pro does not fund this API use. If no network provider is configured, model-backed recall is unavailable; local import, FTS narrowing, and evidence inspection should continue offline as designed.

## Prototype and API questions

### Prototype before implementation

- Which minimum Codex CLI release first contains the exact stable flags Relic needs? What does `codex login status` return and which exit codes distinguish missing, expired, rate-limited, policy-denied, and revoked auth?
- Does `codex exec --json --ephemeral --sandbox read-only --ask-for-approval never --ignore-user-config --ignore-rules -` behave consistently on supported macOS/Linux releases? Which flags are rejected by older stable builds?
- Can an injection-shaped prompt cause `command_execution`, `file_change`, MCP, plugin, connector, web-search, or network events despite those restrictions? Can a documented configuration disable all tools rather than merely reject escalation?
- Does `--ephemeral` avoid all session artifacts, and do diagnostic/login logs contain prompts, URLs, account identifiers, or other data Relic must not collect?
- Are JSONL event changes additive across supported releases? Can Relic ignore unknown events while requiring one successful `turn.completed` and one final agent message?
- How should cancellation kill the child process and descendants without leaving a session, lock, or subprocess behind?
- Does the isolated stdin workflow work with both Plus and Pro rate limits and with managed workspaces that enforce `forced_login_method`/workspace restrictions?

### Official clarification needed

OpenAI documentation supports local SDK/script integration but does not expressly discuss a separately distributed Rust application that launches each user's CLI, while the Pro plan article warns against using ChatGPT to power third-party services. Before marketing subscription support broadly, ask OpenAI to confirm in writing that a single-user, local, bring-your-own-Codex subprocess integration is an allowed “scriptable workflow” for Plus/Pro and is not the prohibited hosted/resold third-party-service pattern.

Also ask OpenAI whether there is a supported stable **no-tools/generation-only** mode for `codex exec`, and whether the documented JSONL event schema carries an explicit compatibility policy. No clarification is needed to reject native OAuth: unless OpenAI publishes an OAuth registration/integration contract for third-party native clients, Relic should continue delegating authentication to official Codex software.

## Primary sources

[^auth]: OpenAI, [Codex authentication](https://learn.chatgpt.com/docs/auth) — sign-in methods, browser/device flow, cache reuse, storage, and API billing boundary.
[^noninteractive]: OpenAI, [Non-interactive mode](https://learn.chatgpt.com/docs/non-interactive-mode) — `codex exec`, JSONL, stdin, ephemeral mode, sandbox defaults, and auth reuse.
[^commands]: OpenAI, [Codex CLI developer commands](https://learn.chatgpt.com/docs/developer-commands?surface=cli) — command maturity and flag reference.
[^maturity]: OpenAI, [Feature Maturity](https://learn.chatgpt.com/docs/feature-maturity) — Stable/Beta/Experimental meanings.
[^pricing]: OpenAI, [Codex pricing and feature availability](https://learn.chatgpt.com/docs/pricing) — Plus/Pro entitlement for CLI, SDK, `exec`, and scripting; access-token availability.
[^sdk]: OpenAI, [Codex SDK](https://learn.chatgpt.com/docs/codex-sdk) — app integration, stable Python SDK, and pinned runtime.
[^appserver]: OpenAI, [Codex app-server](https://learn.chatgpt.com/docs/app-server) — deep integration/authentication scope and experimental production status.
[^mcp]: OpenAI, [Running Codex as an MCP server](https://learn.chatgpt.com/docs/mcp-server) — documented MCP tools and agent-consumer interface.
[^access-tokens]: OpenAI, [Codex access tokens](https://learn.chatgpt.com/docs/enterprise/access-tokens) — Business/Enterprise trusted automation and the general-API boundary.
[^sandbox]: OpenAI, [Codex sandbox](https://learn.chatgpt.com/docs/sandboxing) — command sandbox and approval boundaries.
[^plus]: OpenAI Help Center, [What is ChatGPT Plus?](https://help.openai.com/en/articles/6950777-what-is-chatgpt-plus) — API usage is separate and independently billed.
[^pro]: OpenAI Help Center, [About ChatGPT Pro tiers](https://help.openai.com/en/articles/9793128-what-is-chatgpt-pro) — Codex inclusion and prohibited account/service-use patterns.
[^billing]: OpenAI Help Center, [Managing billing settings on ChatGPT web and Platform](https://help.openai.com/en/articles/9039756-billing-settings-in-chatgpt-vs-platform) — separate ChatGPT/API billing systems.
[^repo-readme]: OpenAI, [`openai/codex` README at the reviewed commit](https://github.com/openai/codex/blob/00f6a8a60e5c5e93d185c7fe67fd596b7e62240f/README.md#L68-L81) — recommended ChatGPT-plan sign-in and repository license.
[^license]: OpenAI, [`openai/codex` Apache-2.0 license at the reviewed commit](https://github.com/openai/codex/blob/00f6a8a60e5c5e93d185c7fe67fd596b7e62240f/LICENSE#L64-L76) and [trademark clause](https://github.com/openai/codex/blob/00f6a8a60e5c5e93d185c7fe67fd596b7e62240f/LICENSE#L138-L141).
[^typescript-source]: OpenAI, [Codex TypeScript SDK README at the reviewed commit](https://github.com/openai/codex/blob/00f6a8a60e5c5e93d185c7fe67fd596b7e62240f/sdk/typescript/README.md#L1-L6) — the SDK spawns the CLI and exchanges JSONL.
[^python-source]: OpenAI, [Codex Python SDK authentication README at the reviewed commit](https://github.com/openai/codex/blob/00f6a8a60e5c5e93d185c7fe67fd596b7e62240f/sdk/python/README.md#L14-L52) — reuse, browser login, and device-code login.
