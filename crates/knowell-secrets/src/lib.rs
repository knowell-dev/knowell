//! Secret boundaries for Knowell.
//!
//! Secrets must never enter an index, an embedding, a summary, a memory, a
//! log line, a panel or any MCP / HTTP / CI output. This crate provides the
//! three layers that enforce that, in the order data meets them:
//!
//! 1. **Path exclusion** ([`exclusion`]) — decided from the *path alone*,
//!    before a single byte of the file is read. Sensitive files (`.env*`,
//!    private keys, credentials, Terraform state, kubeconfig, ...) are never
//!    opened. Built-in rules cannot be disabled; users can only add patterns.
//! 2. **Content scan and redaction** ([`mod@scan`]) — files that pass layer 1
//!    are scanned for secret-shaped content (provider tokens, private key
//!    blocks, URLs with passwords, high-entropy assignments) and each hit is
//!    replaced with `[REDACTED:<kind>]` before the text goes anywhere.
//!    A [`scan::Finding`] records where and what kind, never the secret.
//! 3. **Output masking** ([`mask`]) — the engine knows some secrets for
//!    certain (API keys it resolved via [`resolve()`]). A [`mask::Masker`]
//!    removes those exact values from any text that leaves the process (logs,
//!    MCP responses, errors), as a last line of defence.
//!
//! [`resolve()`] turns a configuration [`knowell_core::SecretRef`] (`env:` /
//! `file:`) into a [`secrecy::SecretString`] at the moment of use. Errors in
//! this crate name the reference and the failure kind only.
//!
//! No layer is perfect (see the limits documented on [`mod@scan`]); they are
//! deliberately redundant and biased towards over-redaction.

pub mod error;
pub mod exclusion;
pub mod mask;
pub mod resolve;
pub mod scan;

pub use error::SecretsError;
pub use exclusion::{Exclusion, ExclusionPolicy, SensitiveKind};
pub use mask::Masker;
pub use resolve::resolve;
pub use scan::{Finding, FindingKind, Redacted, redact, scan};
