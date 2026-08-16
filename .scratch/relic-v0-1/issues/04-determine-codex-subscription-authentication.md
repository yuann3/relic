# Determine whether Codex subscription authentication is officially usable

Status: closed
Assignee: codex-auth-researcher  
Labels: wayfinder:research  
Parent: [Relic v0.1: ship the evidence-backed terminal experience compiler](../MAP.md)  
Blocked by: none

## Question

Can Relic officially and stably use a ChatGPT Pro/Plus Codex subscription through a documented OAuth/device flow or by delegating to an installed Codex CLI? Identify the supported authentication and automation contracts, licensing/security constraints, credential ownership rules, and the correct fallback if general subscription-backed access is unavailable.

## Comments

### Resolution — 2026-08-16

Research is captured in [Codex subscription authentication for Relic v0.1](../../../docs/research/codex-subscription-authentication.md).

Relic may optionally support ChatGPT Plus/Pro Codex usage by delegating authentication and execution to the installed official CLI: `codex login`, `codex login status`, and `codex exec` are documented Stable commands, `exec` reuses the CLI's saved authentication, and Plus/Pro explicitly include scriptable Codex workflows. Relic must not implement OAuth, access/copy Codex credentials, impersonate the Codex OAuth client, or use experimental app-server APIs for v0.1. Device-code login is official but beta and optional. ChatGPT Pro does not include general OpenAI API use; the nonblocking fallback remains a separately billed, user-configured OpenAI-compatible API provider. Shipping the adapter is conditional on a macOS/Linux prototype proving read-only/no-tool isolation, ephemeral operation, forward-compatible JSONL parsing, and safe auth/rate-limit/cancellation behavior.
