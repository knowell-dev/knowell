# knowell-source

Source access for Knowell: reading project trees and git objects under the exclusion and
secret policies of `knowell-secrets`, and turning repository activity into change signals.
Only redacted text ever leaves this crate.

```rust,ignore
use knowell_source::{FileRead, WalkOptions, read_file, walk};
use knowell_source::git::GitRepo;

let report = walk(&root, &policy, &WalkOptions::default())?;          // a whole directory
match read_file(&root, &path, &policy, &WalkOptions::default())? {     // one file, same rules
    FileRead::File(file) => { /* file.text is redacted, file.hash covers the original bytes */ }
    FileRead::Skipped(reason) => { /* excluded, too large, binary, not UTF-8, symlink, ... */ }
    FileRead::Missing => { /* absent, a directory, or a special file */ }
}

let repo = GitRepo::open(&root)?;
let commit = repo.resolve(&"branch:main".parse()?)?.commit;          // never another ref
let all = repo.walk_commit(&commit, &policy, &options)?;              // every file of a commit
let some = repo.read_commit_files(&commit, &paths, &policy, &options)?; // a list (missing = error)
let one = repo.read_commit_file(&commit, &path, &policy, &options)?;    // one (missing = Missing)
let changes = repo.diff_scoped(&old, &commit, project_root.as_ref(), &policy, &options)?;
```

## One content pipeline

Every reader applies the same steps, in order, so a file read from a checkout and the same
blob read from git objects produce the same text, findings and hash:

1. **path exclusion** (`ExclusionPolicy`: `.env*`, keys, credentials, tfstate, kubeconfig,
   user patterns) — decided from the path alone, **before the file or blob is touched**;
2. symbolic links are reported (`SkipReason::Symlink`), never followed in git and only on
   request (`WalkOptions::follow_symlinks`) on disk;
3. the **size limit** from metadata or the object header, before reading, then a bounded read;
4. a binary check (NUL in the first 8 KiB) and UTF-8 validation (a BOM is stripped);
5. **secret redaction** (`knowell_secrets::scan::redact`).

The content hash covers the original bytes (BOM and secrets included), so edits to redacted
content are still detected; it is not reversible.

| Entry point | Reads | Missing path |
|---|---|---|
| `walk(root, policy, options)` | every file below `root` (honours `.gitignore` when asked) | — |
| `read_file(root, path, policy, options)` | one file below `root` | `FileRead::Missing` |
| `GitRepo::walk_commit(commit, ...)` | every file of a commit | — |
| `GitRepo::read_commit_files(commit, paths, ...)` | the listed files of a commit | `GitError::PathNotFound` |
| `GitRepo::read_commit_file(commit, path, ...)` | one file of a commit | `FileRead::Missing` |

`SkipReason::as_str()` gives the stable snake-case code of a skip (`excluded`, `too_large`,
`binary`, `not_utf8`, `unreadable`, `invalid_path`, `symlink`) for manifests and reports.

## Git

`git::GitRepo` reads the object database and refs directly (`gix`); nothing writes to the
repository, its index or working tree. It resolves track targets exactly (a missing ref is
`GitError::RefNotFound`, never replaced), lists trees, diffs commits with rename tracking,
checks ancestry (force-push detection), lists worktrees and groups them into task views,
and reports a worktree's uncommitted changes.

`diff_scoped(old, new, root, policy, options)` selects regular-file metadata from both
commit trees before any rename-similarity blob access. The policy matches project-relative
paths beneath `root`; returned paths remain repository-relative. Built-in sensitive-path
exclusions also apply before stripping the root. A move across a root or exclusion boundary
appears only as an allowed addition or deletion. Allowed exact renames retain 100 % similarity;
edited renames require at least 50 %. Similarity reads honor `options.max_file_bytes`; zero
disables inexact matching. Symlinks and gitlinks are omitted.

The filtered trees live only in memory. Diffing uses no attributes from commit trees,
the worktree, `.git/info/attributes` or global configuration, and no external drivers,
clean filters or text conversion. `diff(old, new)` delegates to this same safe path with
the built-in policy, no project root and default `WalkOptions`; consumers with project
exclusions or size limits must call `diff_scoped` explicitly.

`working_changes_scoped(root, policy, options)` uses the same project-relative policy
and repository-relative result convention for saved changes, filtering candidates before
status can hash their content. The legacy `working_changes(policy)` delegates with no
project root and default options.

Saved status captures recognized same-repository Git control rules separately from source
content: ancestor `.gitignore` and `.gitattributes`, and `.git/info/exclude` and
`.git/info/attributes` (the common directory for linked worktrees). Controls are limited
to 64 KiB each, 1 MiB in total and 1,024 present control files, checked without following
symlinks, and held only in memory. Missing controls do not consume the file count. Their
capture does not add an outside-root or excluded path to source results,
indexing or provider inputs. Ordinary source candidates still obey the project policy.
Native line-ending normalization uses these captured attributes on bounded allowed bytes;
external filters, encoding transforms and outside-repository controls are explicit errors.
Entries with line-ending rules are checked through a copy of the stat cache and can be
rehashed even when their filesystem metadata matches. The on-disk index remains unchanged.

Worktree listings canonicalize existing paths, including Windows short-name aliases, so
opening the main checkout or any linked worktree yields the same identities. A missing
worktree retains its registered path and is reported as prunable.

## Watching

`watch::Watcher` is a debounced file-system watcher for one worktree that reports saved
files, `HEAD` moves and ref updates over a `std::sync::mpsc` channel; excluded paths are
dropped before they are reported.

## Tests

```sh
python scripts/buildlock.py cargo test -p knowell-source
```

Git tests create repositories with the git CLI under isolated settings in temporary
directories; no network is used.

Invalid UTF-8 path validation and lossy reporting are tested directly on Unix. Linux also
tests a real directory containing an invalid filename; APFS rejects that filename before
the walker can inspect it.
