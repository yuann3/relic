# Agent conventions

## Issue naming

Every issue gets a short prefix tag chosen per effort/session, followed by a sequential number. The prefix is decided by whichever Claude session creates the tickets — pick a letter or short code that fits the effort's domain.

Example: a map with 5 prototype tickets and 3 task tickets might use `P1`–`P5` and `T1`–`T3`. Another effort might use `R1`–`R4` for research and `G1`–`G2` for grilling.

Rules:
- Prefix is a GitHub label on the issue (e.g. label `P1`)
- Sequential within that prefix for that map — no gaps, no reuse
- The prefix vocabulary is not global — each effort picks what fits
- Include the tag in the issue title: e.g. `[P1] Prove Jev across all three integration surfaces`
