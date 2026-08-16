# Specify the bounded RLM and model boundary

Status: closed
Assignee: rlm-researcher  
Labels: wayfinder:research  
Parent: [Relic v0.1: ship the evidence-backed terminal experience compiler](../MAP.md)  
Blocked by: none

## Question

What is the smallest typed query-plan kernel, evidence-handle protocol, child-call budget, cache/provenance contract, and OpenAI-compatible model interface that preserves genuine RLM external-artifact traversal without adding a scripting runtime, provider framework, or uncontrolled context growth?

## Comments

Resolved in [`docs/research/rlm-model-boundary.md`](../../../docs/research/rlm-model-boundary.md): use a seven-operator, depth-one typed RLM kernel with scoped result/evidence handles, pre-reserved hierarchical budgets, bounded stateless root context, typed child/final contracts with deterministic abstention gates, and one explicit-capability Chat Completions adapter; no scripting runtime, generic agent/provider framework, or model streaming in v0.1.
