//! Cursor: `mcpServers.knowell` in `.cursor/mcp.json` (project) or
//! `~/.cursor/mcp.json` (user), plus the `.cursor/rules/knowell.mdc` rule.

use super::json::{Flavor, apply_server, server_entry};
use super::{ConnectOptions, Scope, instruction_block};
use crate::edit::{Edit, MD_MARKERS, has_block, plan_file};
use crate::error::SetupError;

const FRONTMATTER: &str = "---\ndescription: Use the Knowell MCP server for code search, context and project memory\nalwaysApply: true\n---\n";

pub(super) fn plan(
    opts: &ConnectOptions,
    add: bool,
    notes: &mut Vec<String>,
) -> Result<Vec<Edit>, SetupError> {
    let mcp = match opts.scope {
        Scope::User => opts.home(".cursor/mcp.json"),
        Scope::Project => opts.project(".cursor/mcp.json"),
    };
    let p = mcp.clone();
    let mcp_edit = plan_file(mcp, move |before| {
        let entry = add.then(|| server_entry(opts, Flavor::Cursor));
        apply_server(&p, before, entry)
    })?;
    let mut edits = vec![mcp_edit];

    // Cursor's own user rules are not file based, so the rule file always
    // lives in the project.
    let rule = opts.project(".cursor/rules/knowell.mdc");
    if add && opts.scope == Scope::User {
        notes.push(
            "Cursor user rules are not file based; the startup rule is written to the project's .cursor/rules"
                .into(),
        );
    }
    let p = rule.clone();
    edits.push(plan_file(rule, move |before| match (add, before) {
        (true, Some(text)) if !has_block(text, MD_MARKERS) => Err(SetupError::Conflict {
            path: p.clone(),
            what: "`knowell.mdc` exists and was not written by Knowell".into(),
        }),
        (true, _) => Ok(Some(format!("{FRONTMATTER}\n{}\n", instruction_block()))),
        (false, Some(text)) if has_block(text, MD_MARKERS) => Ok(None),
        (false, other) => Ok(other.map(str::to_owned)),
    })?);
    Ok(edits)
}
