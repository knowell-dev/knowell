# knowell (npm launcher)

Thin launcher for [Knowell](https://github.com/knowell-dev/knowell), a code-intelligence and
shared-memory engine for AI coding agents. The installed command is `know`.

```sh
npx -y knowell --help
npx -y knowell mcp        # as an MCP stdio server entry in your agent's config
```

There is no `postinstall` script. On first run the launcher downloads the release binary for
your platform (Linux, macOS or Windows; x64 or arm64) from GitHub Releases, verifies it
against the release's `SHA256SUMS`, caches it, and runs it with your terminal attached. All
launcher messages go to stderr, so stdout stays clean for the MCP protocol.

The package version pins the runtime. Updating the npm package selects a new runtime;
`know update` directs npm installations back to npm instead of rewriting its cache.
Downloads use bounded raw engine files; archive extraction tools are unnecessary.
Every cache hit rechecks its npm ownership receipt, exact version, size and SHA-256,
including a cache published by a concurrent launcher. Invalid or incomplete caches fail
explicitly; remove the reported version cache and retry. Cache directories are private
to the current user, with a protected Windows ACL.

The first npm download trusts GitHub release checksums. This is separate from native
TUF verification used by direct installations; a checksum receipt is not a TUF signature.

| Variable | Purpose |
|---|---|
| `KNOWELL_BIN` | run this binary instead of downloading one |
| `KNOWELL_BINARY_VERSION` | run another release than the package version |
| `KNOWELL_CACHE_DIR` | cache location (default: the OS per-user cache directory) |
| `KNOWELL_DOWNLOAD_BASE` | alternative release download base URL (testing) |

The binary and version environment overrides are explicit operator choices. Other ways
to install Knowell: see the repository README.

Licensed under MIT OR Apache-2.0.
