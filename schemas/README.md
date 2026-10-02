# Configuration schemas

`engine.schema.json` (`~/.knowell/config.toml`) and `workspace.schema.json` (`knowell.toml`)
are generated from the Rust types in `crates/knowell-config`. Do not edit them by hand.

Regenerate after changing the config types: `KNOWELL_BLESS=1 python scripts/buildlock.py cargo test -p knowell-config schema`
(without the variable the same tests fail when a file is out of date).

For editor completion with Taplo or the Even Better TOML extension, put this on the first
line of a config file: `#:schema <path or URL of the schema>`, for example
`#:schema ./schemas/workspace.schema.json`.
