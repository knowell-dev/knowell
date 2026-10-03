# Roadmap

Knowell ships as a single complete 1.0. The milestones below are internal build stages, not
releases. There are no tags or releases before 1.0. The order reflects implementation
dependencies; completion of the whole is defined by the acceptance scenarios at the end.

| Status | Meaning |
|---|---|
| In progress | Work has started |
| Planned | Not started |

## Milestones

### M0: Foundation (in progress)

- **Deliverables:** Rust workspace and `AGENTS.md` rules; fast-lane CI; configuration
  schema; secret boundaries (path exclusion, scanning, redaction); evaluation harness with a
  synthetic multi-repository fixture generator; lexical search baseline.
- **Done when:** CI is green, and the same queries give a repeatable comparison across
  runs.

### M1: Data layer (planned)

- **Deliverables:** schema and migrations; identifiers from organization down to view;
  authorization boundaries; durable job queue; managed PostgreSQL on three operating
  systems with a self-built, attested pgvector; Docker Compose deployment; external
  PostgreSQL support.
- **Done when:** all install paths work on Windows, Linux, and macOS; migration and
  backup/restore tests pass.

### M2: Source and index engine (planned)

- **Deliverables:** ref tracking, file watching, webhooks, reconciliation; personal
  worktree layer; view manifests; generation fencing; priorities and resource budgets.
- **Done when:** freshness is measured, and the rebase and force-push scenarios pass.

### M3: Parsing and search (planned)

- **Deliverables:** language capability matrix; chunking; symbols and skeletons; Tantivy
  index; embedding profiles (Gemini, Ollama, OpenAI-compatible endpoints); pgvector search;
  hybrid search with explainability; reduced mode when no model is reachable.
- **Done when:** a metric baseline exists and per-language tier tests pass.

### M4: Graph and contracts (planned)

- **Deliverables:** SCIP import; edge evidence types; `trace_flow`, `analyze_impact`, and
  patch preview; rule packs; cross-project links; `know check`.
- **Done when:** every link type is found in the synthetic full-stack workspace, and
  relation accuracy is measured.

### M5: Memory and knowledge (planned)

- **Deliverables:** scopes and states; tasks; domains and glossary; rules; write-back to the
  repository as pull requests; documentation drift detection.
- **Done when:** memory state-transition tests pass.

### M6: Agent integration (in progress)

- **Deliverables:** MCP over stdio and Streamable HTTP; `open_workspace`; `build_context`;
  `know connect`; hub, worker, and edge roles; identity, RBAC, and audit.
- **Done when:** the two-machine acceptance test and permission-leak tests pass.
- **Current evidence:** database-backed MCP HTTP tests cover project isolation across
  search, graph, context and memory, token scopes, changed grants and revoked credentials.
  CLI tests issue private-file credentials, verify login against a real hub and revoke
  them. Two-machine task continuation and live OIDC verification remain open.

### M7: Panel (planned)

- **Deliverables:** all panel screens.
- **Done when:** the end-to-end browser scenarios pass.

### M8: Advanced modules (planned)

- **Deliverables:** the advanced modules listed in the architecture (behavior-focused change
  explanation, patch preview, architecture rules, historical rationale, and others);
  optional rerankers (local models, hosted rerank APIs); plugin system.
- **Done when:** each module has its own tests; experimental ones sit behind flags.

### M9: Release readiness (planned)

- **Deliverables:** scale and performance work; recovery; distribution channels; GitHub
  Action and templates; user documentation; security hardening; dogfooding on real
  multi-project workspaces.
- **Done when:** every acceptance scenario below is green. Then, and only then, v1.0.0.

## 1.0 acceptance scenarios

1.0 is not released until all of these pass.

1. **Clean install** (Windows, Linux, macOS): `know init` brings up managed PostgreSQL;
   workspace, project, panel, and MCP work. The Compose and external PostgreSQL paths are
   verified too.
2. **Large import:** a workspace of 10 or more projects imports from `.gitmodules` or a
   directory; projects track different refs (branch, `release/2.x`, tag); the panel shows
   where each setting comes from; a missing ref is reported explicitly.
3. **Commit tracking:** a commit to a tracked branch updates changed code and affected
   relations automatically; other projects are not re-embedded; freshness is measured.
4. **History rewrites:** rename, delete, rebase, reset, force-push, and branch switch leave
   no stale results; a late job cannot activate a newer view.
5. **Worktrees:** saved changes in a worktree appear only in that user's and worktree's
   view, on top of its own HEAD; worktrees sharing a feature slug group into one task view.
6. **Cross-project flow:** an example flow across ten projects (UI, API, service, event,
   worker, table) is found with evidence.
7. **Contract change:** a proto or OpenAPI change shows consumers and drift with evidence,
   and `know check` catches it in a pull request.
8. **Migration without entity:** a migration added without a matching entity update is
   reported.
9. **Second machine:** a task started on machine A is continued on machine B by a fresh
   agent connected to the hub, without re-explaining the project; intervening changes and
   stale knowledge are visible.
10. **Profile switches:** moves between the 768, 1536, and 3072 profiles are seamless,
    traceable, and reversible.
11. **Crash recovery:** indexing jobs are recovered after the engine stops mid-run.
12. **Reduced mode:** with no model access, lexical, symbol, graph, and memory features
    work and the gap is reported explicitly.
13. **Authorization:** unauthorized sources never appear in search, graph expansion,
    context, or memory; caches update when permissions change.
14. **Canary secrets:** planted canary secrets appear in no outbound request, log, panel
    view, MCP output, or support bundle; even a `.env` file tracked by git is excluded.
15. **Operations:** backup, restore, schema migration, and managed PostgreSQL major-version
    upgrade pass.
16. **Release hygiene:** release packages contain no private code, indexes, `.env` files, or
    real credentials; checksums and attestations verify.
17. **Quality:** pre-defined thresholds on the evaluation set pass, and an agent
    comparison with the engine on versus off is reported.

Parser tests, database integration tests, MCP contract tests, and panel end-to-end tests
accompany these scenarios.
