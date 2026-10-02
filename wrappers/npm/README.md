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

The package version is the Knowell version it runs.

| Variable | Purpose |
|---|---|
| `KNOWELL_BIN` | run this binary instead of downloading one |
| `KNOWELL_BINARY_VERSION` | run another release than the package version |
| `KNOWELL_CACHE_DIR` | cache location (default: the OS per-user cache directory) |
| `KNOWELL_DOWNLOAD_BASE` | alternative release download base URL (testing) |

Unpacking uses `tar` (Windows 10+ includes it). Other ways to install Knowell: see the
repository README.

Licensed under MIT OR Apache-2.0.
