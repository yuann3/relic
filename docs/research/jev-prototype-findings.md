# Jev prototype findings

**Status:** prototype validated
**Prototype:** `spikes/jev_spike.rs` (delete after absorbing)
**Date:** 2026-10-01
**Model:** jev-1.13.0

## Summary

All three integration surfaces work. Jev returns sane, calibrated verdicts with useful probability distributions. No workarounds needed. The thin client approach (ureq + serde, ~80 LOC) works — no external SDK required.

## Surface 1: Assess child model — Choice(4) verdict

**Result: works, no workarounds.**

| Evidence | Verdict | Confidence | Correct? |
|---|---|---|---|
| `certbot renew --nginx` | supports (0.99) | 0.98 → High | Yes |
| `apt-get update && apt-get upgrade -y` | irrelevant (0.94) | 0.93 → High | Yes |
| `certbot certificates` (checking certs) | unclear (0.43) | 0.25 → Low | Reasonable — genuinely ambiguous |
| `git commit -m 'update readme'` | irrelevant (1.00) | 1.00 → High | Yes |

- **Latency:** 433ms for 4 items batched in one HTTP call
- **Tokens:** 845 in, 204 out
- **Verdict distribution:** Not random/uniform. Clear signal on unambiguous items, low confidence on genuinely ambiguous ones.
- **Confidence thresholds:** >0.7=High, 0.4-0.7=Medium, <0.4=Low maps well. The ambiguous `certbot certificates` item correctly gets Low confidence.
- **Batch semantics:** 4 questions in 1 HTTP call works. Objective in shared state, evidence in per-question instructions.

## Surface 2: Search reranking — Score(5) relevance

**Result: works, produces meaningfully better ordering than BM25.**

| Rank | Command | Jev Score | Confidence |
|---|---|---|---|
| 1 | `certbot renew --nginx --dry-run` | 3.21 | 0.63 |
| 2 | `apt install python3-certbot-nginx` | 2.54 | 0.51 |
| 3 | `openssl req -new -key server.key` | 2.50 | 0.56 |
| 4 | `certbot certificates` | 1.89 | 0.61 |
| 5 | `cat /etc/nginx/sites-available/default` | 1.36 | 0.57 |
| 6 | `systemctl restart nginx` | 1.27 | 0.66 |

- **Latency:** 294ms for 6 items batched
- **Tokens:** 956 in, 94 out
- **Quality:** Correctly ranks the actual renewal command highest. Setup (`apt install`) above tangential commands. Config inspection and service restart at bottom.
- **Batch semantics:** 6 Score questions in 1 HTTP call. Search query in shared state, hit text in per-question instructions.

## Surface 3: Query intent routing — Choice

**Result: works, high-confidence classification on all test queries.**

| Query | Intent | Probability | Confidence |
|---|---|---|---|
| "how did I renew the SSL certificate last time" | procedure_recall | 0.99 | 0.98 |
| "git log" | exploration | 0.93 | 0.91 |
| "what failed yesterday evening" | investigation | 1.00 | 0.99 |
| "show me the docker compose setup" | exact_lookup | 0.71 | 0.60 |
| "did I ever run terraform destroy" | exact_lookup | 0.96 | 0.93 |

- **Latency:** 286–376ms per call (one query each, not batched — could batch if multiple intents needed)
- **Quality:** All classifications are reasonable. "docker compose setup" is the hardest — could be lookup or recall — and Jev correctly shows lower confidence (0.60).

## Cross-cutting findings

| Metric | Value |
|---|---|
| API endpoint | `POST https://api.typesafe.ai/v1/systemone` |
| Auth | `Authorization: Bearer <key>` |
| Model | `jev-1.13.0` (via `jev-latest` alias) |
| Client approach | Thin ureq + serde (~80 LOC), no external crate needed |
| Batch latency (4 items) | 433ms |
| Batch latency (6 items) | 294ms |
| Single-query latency | 286–376ms |
| Output tokens | Always low (94–204), charged at $0/M |

## Decisions confirmed

1. **Verdict → Choice(4)** mapping works exactly as designed. No surprises.
2. **Confidence threshold bins** (>0.7/0.4/0.4) map well to High/Medium/Low.
3. **Empty claims vec** is fine — verdicts + probabilities carry full signal.
4. **Batch semantics** work: N questions per HTTP call, shared state, per-question instructions.
5. **Thin client** is sufficient — no need for the `typesafe-ai` crate.
6. **Cost** is negligible: ~1000 tokens per batch at $0.042/M input = ~$0.00004 per batch.

## Open questions

- Should reranking cap be configurable or fixed at the Assess batch size (4)?
- Intent routing latency (286-376ms) adds to every recall. Acceptable?
- The `certbot certificates` ambiguity (unclear vs irrelevant) — is the threshold mapping right, or should the host use raw probabilities instead of binned confidence?
