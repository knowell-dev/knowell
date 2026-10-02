# Security policy

## Reporting a vulnerability

Please report vulnerabilities **privately** using GitHub private vulnerability reporting:

<https://github.com/knowell-dev/knowell/security/advisories/new>

Do not open a public issue, pull request, or discussion for a security problem. Include
what you found, how to reproduce it, and the affected commit. Do not include real secrets
or private code in a report.

## Supported versions

Knowell is pre-1.0 and nothing has been released. Only the `main` branch is supported.

## Scope

Of particular interest:

- **Secret leakage**: any path by which a credential, `.env` content, or other excluded
  file content reaches an index, embedding request, memory entry, log, panel view, MCP
  response, diagnostic bundle, or CI output.
- **Authentication or authorization bypass**: reading or writing data outside the caller's
  workspace, project, or memory scope.
- **Path traversal**: reading files outside configured project roots, including through
  symlinks, worktrees, or crafted refs.
- **Sandbox escape**: a rule pack or plugin exceeding its declared file, network, or
  resource permissions.

Also welcome: injection through repository content that turns data into instructions for
an agent, request forgery or DNS rebinding against the local panel, and unsafe handling of
untrusted input in parsers.
