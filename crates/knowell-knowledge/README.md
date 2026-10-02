# knowell-knowledge

The memory and knowledge model of Knowell. Pure domain logic: no database, no
I/O, no clock. The caller passes timestamps and identifiers in and persists
what comes out; every function is deterministic.

## Model

| Type | Meaning |
|---|---|
| `Scope` | `Organization`, `Workspace(name)`, `Project { workspace, project }`, `Task(id)`, `User(id)`. Organization overlaps every workspace and project; a workspace overlaps its projects; task and user scopes are private and overlap only themselves. |
| `RecordKind` | `Observed` (derived from code, needs evidence), `Human` (ADR, decision, rule), `ModelSuggestion` (agent finding, always a draft). |
| `RecordState` | `Proposed`, `Accepted`, `Rejected`, `Stale`, `Superseded`. Only `Accepted` is ever presented as current. |
| `KnowledgeRecord` | id, scope, kind, `Subject` (normalised key such as `payments.idempotency`), title, Markdown body, state, version, author (`Actor::{Human, Agent, System}`), timestamps, `Evidence` (project, view, commit, path, line range, content hash), related symbols, tags, pinned, history log, earlier versions. |
| `Task` / `Checkpoint` | goal, status, notes, decisions (record ids), open questions, related symbols/files, and a **view manifest** (`ManifestPin`: project, view, commit, local generation). |

A record tagged `rule` is a team rule for bootstrap ranking.

## State machine

```
                       propose
                          |
                          v
   +-----------------[ Proposed ]----------reject----------+
   |                      |                                |
   |                   accept                              |
   |                      v                                v
   |   +---revalidate--[ Accepted ]--------reject------>[ Rejected ]  (terminal)
   |   |                  |   ^                            ^
   |   |              mark_stale                           |
   |   |                  v   |                            |
   |   +-----------------[ Stale ]---------reject---------+
   |                      |
   |  Accepted/Stale --supersede(by)--> [ Superseded ]     (terminal)
   |
   edit: allowed in Proposed, Accepted, Stale. Creates version n+1.
         The policy decides the result: Accepted (auto) or Proposed (review).
         A Stale record only leaves Stale if the edit supplies new evidence.
   pin:  allowed in Proposed, Accepted, Stale; state unchanged.
```

(`revalidate` leads from `Stale` back to `Accepted`; `accept` is only legal from
`Proposed`.) Every other transition is an `IllegalTransition` error and leaves
the record untouched. Every successful transition appends a `HistoryEntry`
(actor, action, from, to, version, reason, time). Reasons are mandatory.

### Who may do what (`AcceptancePolicy`, `Rights`)

* **Auto-accept on write:** `Observed` records authored by the engine
  (`Actor::System`); `Human` records authored by a human holding
  `Rights::can_accept`. Both switchable in the policy.
* **Agents never accept, reject, revalidate, supersede, pin or flag stale.**
  Agent suggestions stay proposals until a human accepts them. An agent may
  edit only its own still-proposed record.
* Humans need `can_accept` to accept, revalidate, supersede, pin, or reject
  (authors may withdraw their own work). The engine may revalidate observed
  records.
* `edit` takes `expected_version` (optimistic concurrency).

## Staleness

`compute_staleness(records, &ChangeSet)` flags accepted records whose evidence
file was deleted or now has a different hash, or whose related symbols changed
or were removed. `apply_staleness` marks them (actor `System`) and reports any it
could not mark. `RevalidationQueue` keeps the pending list. Stale records are
kept, never shown as current, and leave `Stale` only by `revalidate`,
`supersede` or `reject`. Proposed records with changed evidence are listed
separately for the reviewer.

## Conflicts

`detect_conflicts` reports every pair of **accepted** records with the same
subject, overlapping scopes and different bodies (whitespace ignored).
`conflicts_for_candidate` shows what accepting a proposal would create.
Conflicts are never merged; `bootstrap_pack` always reports them.

## Tasks and resume

`resume(task, checkpoints, current_manifest, records)` returns a `ResumeDigest`:
the last checkpoint's summary and next steps, the views whose pinned commit or
local generation moved (or appeared/vanished) since that checkpoint, the task's
decisions that are stale, superseded, rejected, unreviewed or missing, and other
stale records of changed projects. Without a checkpoint the task's own manifest
is the baseline.

## Session bootstrap

`bootstrap_pack(inputs, budget_tokens)` ranks: pinned records, accepted rules,
recent accepted decisions, open tasks, project maps, then other accepted
knowledge. Ordering inside a tier is total (scope breadth or recency, then
subject, then id). Size is estimated as characters / 4. Items are taken while
they fit; a too-big item is cut to the remaining room (flagged) when at least 24
tokens remain, else omitted. Every omission is listed, every item carries its
source (record id and evidence, task id or project). Stale records are excluded
and counted.

## Secret guard

Title, body, tags, reasons, notes, questions, checkpoint text and every rendered
write-back section are scanned with `knowell_secrets::scan`. A hit is an error
naming the field, finding kind and line only; the value is never echoed.

## Repository write-back

`render_section(id, scope, heading, records)` renders the accepted records of one
scope between `<!-- knowell:begin id=... scope=... -->` and
`<!-- knowell:end -->`, ordered by subject then id, with source links and no
timestamps. `replace_section(document, id, section)` swaps exactly that block (or
appends it), preserving the rest and the line endings; applying it twice changes
nothing. Malformed markers are errors. Marker-like text in bodies is
neutralised.

`doc_drift(markdown, known_symbols, known_paths)` lists symbols and paths named
in inline code or link targets that no longer exist. It is a documented
heuristic that favours silence over false alarms (see the function docs).
