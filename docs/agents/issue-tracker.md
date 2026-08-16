# Issue tracker: GitHub

Relic issues and Wayfinder maps live in [GitHub Issues](https://github.com/yuann3/relic/issues). Use `gh` from this repository; `origin` identifies `yuann3/relic`.

The canonical Relic v0.1 map is [Relic v0.1: ship the evidence-backed terminal experience compiler](https://github.com/yuann3/relic/issues/1). The former `.scratch/relic-v0-1` local tracker was migrated and removed; do not recreate a second canonical map there.

## General operations

- Create: `gh issue create --title "<name>" --body-file <file> --label <label>`
- Read: `gh issue view <number> --comments`
- Comment: `gh issue comment <number> --body-file <file>`
- Claim: `gh issue edit <number> --add-assignee @me` before doing any work
- Close: `gh issue close <number>` after posting the resolution
- Labels: `wayfinder:map`, `wayfinder:research`, `wayfinder:prototype`, `wayfinder:grilling`, `wayfinder:task`

Human-facing narration always uses the issue title as a link, never a bare issue number.

## Wayfinding operations

GitHub's native sub-issue and blocked-by relationships are authoritative. Do not encode dependencies as checkbox/body conventions.

### Add a child to a map

Resolve both issue node IDs with `gh api repos/yuann3/relic/issues/<number> --jq .node_id`, then:

```graphql
mutation($map: ID!, $child: ID!) {
  addSubIssue(input: { issueId: $map, subIssueId: $child }) {
    issue { id }
    subIssue { id }
  }
}
```

Call with `gh api graphql -f query='...' -f map="$MAP_ID" -f child="$CHILD_ID"`.

### Add a blocking edge

`issueId` is the blocked issue and `blockingIssueId` is its prerequisite:

```graphql
mutation($issue: ID!, $blocker: ID!) {
  addBlockedBy(input: { issueId: $issue, blockingIssueId: $blocker }) {
    issue { id }
    blockingIssue { id }
  }
}
```

### Query children and frontier

Query the map's `subIssues` and each child's `state`, `assignees`, and `blockedBy { state }`. The frontier is children that are:

```text
state == OPEN
assignees is empty
every blockedBy issue is CLOSED
```

Claim the selected frontier ticket before reading/researching beyond what is necessary to choose it.

### Resolve a ticket

1. Post the decision as a resolution comment, linking any committed artifact.
2. Close the child issue.
3. Append one title-wrapped link and one-line gist to the map's `Decisions so far`.
4. Add newly visible child issues and native dependency edges; remove graduated text from `Not yet specified`.
5. If the ticket is beyond the destination, close it and link it under `Out of scope`, not `Decisions so far`.

The map remains an index. Detailed decisions live only in their child resolution comments and linked artifacts.
