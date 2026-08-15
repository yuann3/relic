# Local Wayfinder tracker

The canonical map is [Relic v0.1: ship the evidence-backed terminal experience compiler](MAP.md). Child issues live in `issues/`.

## Wayfinding operations

- **List children:** read `issues/*.md` whose `Parent` links to the map.
- **Claim:** before working, set `Assignee:` on an open issue. A blank assignee is unclaimed.
- **Block:** `Blocked by` contains title-wrapped links. Because local Markdown has no native dependencies, a ticket is unblocked only when every linked blocker says `Status: closed`.
- **Frontier:** open + unclaimed children whose blockers are all closed, sorted by filename.
- **Resolve:** append the answer under `## Comments`, set `Status: closed`, then add a one-line title-wrapped context pointer to the map's `Decisions so far`.
- **Research branch:** research findings live on the branch named in the ticket comments until reviewed and merged.
- **Concurrency:** re-read files before edits; other sessions may claim or resolve tickets concurrently.
