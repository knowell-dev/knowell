# Navigation patterns

Read only the pattern needed for the current question. These are routing examples,
not additional mandatory steps.

## Unknown behavior in a large local codebase

For "where is a malformed drawing rejected?", search for that behavior rather than
requesting the whole architecture. Inspect the returned code regions, then search
the identified module for concrete error types and call sites. Read the decisive
branches and nearby tests locally. Search another project only if a boundary,
wrapper or caller points there. A useful snippet already in the response needs no
second fetch. If documentation dominates while implementation is missing, narrow
to code and relevant project/path scope before increasing the response budget.

## Several projects or repositories

Keep the project name attached to every path and symbol. Obtain project locations
from available workspace metadata or verified local repository configuration. A
monorepo sub-root may be relative to its repository; do not interpret it as an
absolute clone path or guess a sibling directory from the project name. Verify
which checkout a local read uses before joining paths.

Use a concrete endpoint, topic, RPC name, package or symbol to investigate a
boundary. Contract and graph tools can identify candidate participants. Read the
producer and consumer code to verify the important connection, its payload and
error behavior. Structural matches and semantic similarity do not establish a
resolved runtime call. If a project is inaccessible locally, use authorized pinned
source from Knowell and keep that version distinct.

## Local code differs from the indexed view

Determine which version the task needs: the current working checkout, a release,
or the context's pinned source. Knowell's current index status is not a guarantee
that the live filesystem matches every returned passage.

For current edits, read the affected files and relevant local changes. For a
historical explanation, read the named Git objects in a verified repository or
fetch the exact retained source. Personal saved changes may require Knowell's
snapshot rather than the committed Git object. Do not substitute local HEAD for
an unavailable requested version. A historical path that was later deleted can
still be valid evidence for the historical task.

For a historical comparison, establish the exact historical source identity first.
If that side is unavailable, label the comparison incomplete and report useful
working-copy findings separately. A committed Git object cannot stand in for a
requested saved personal snapshot.

## Missing or incomplete evidence

Check whether the issue is scope, filters, unindexed code, an unavailable ref,
missing relation analysis or an exhausted response budget. Diagnose a relevant
index limitation with `index_status`; do not automatically change configuration
or start indexing.

Follow a distinct continuation handle when more of that exact retained passage
is needed. Repeating the displayed excerpt's handle does not advance through the
file. Alternatively read the needed range locally when project and version are
verified. Increase scope or budget only to resolve a concrete missing question.
If it remains unresolved, state the limitation instead of claiming full coverage.
