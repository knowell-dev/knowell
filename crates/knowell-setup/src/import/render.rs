//! Rendering an [`ImportPlan`] as commented `knowell.toml` text.

use super::{ImportPlan, RenameReason, WorktreeKind};

fn q(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

/// Makes arbitrary text safe inside a `#` comment line.
fn comment(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Renders the plan as `knowell.toml` text with explanatory comments.
///
/// The output always parses with `knowell_config::parse_workspace`. It
/// resolves only when every project has a track target; projects without one
/// carry a `TODO` comment instead of a guessed value, and resolution reports
/// them until the user fills them in.
pub fn render_toml(plan: &ImportPlan) -> String {
    let mut out = String::new();
    out.push_str(
        "#:schema https://raw.githubusercontent.com/knowell-dev/knowell/main/schemas/workspace.schema.json\n",
    );
    out.push_str("# Workspace imported from the existing folder layout. Review before use.\n");
    out.push_str("version = 1\n\n[workspace]\n");
    out.push_str(&format!("name = {}\n", q(plan.workspace_name.as_str())));

    // Hoist a shared track target into the workspace so it is written once.
    let shared = match plan.projects.split_first() {
        Some((first, rest))
            if !rest.is_empty()
                && first.track.is_some()
                && rest.iter().all(|p| p.track == first.track) =>
        {
            first.track.clone()
        }
        _ => None,
    };
    match &shared {
        Some(t) => {
            out.push_str("# Every project follows this ref (override per project with `track`).\n");
            out.push_str(&format!("track = {}\n", q(&t.to_string())));
        }
        None => {
            out.push_str(
                "# Knowell never guesses a branch: set `track` here for all projects, or per project.\n",
            );
            out.push_str("# track = \"branch:<name>\"   (also: remote:origin/<name>, tag:<name>, commit:<id>, worktree)\n");
        }
    }
    out.push_str(
        "# Content may be sent to a cloud embedding provider only with data_policy = \"cloud\".\n",
    );
    out.push_str("# data_policy = \"local-only\"\n");

    if !plan.warnings.is_empty() || !plan.renames.is_empty() {
        out.push_str("\n# Import notes\n");
        for w in &plan.warnings {
            out.push_str(&format!("# - {}\n", comment(w)));
        }
        for r in &plan.renames {
            let why = match r.reason {
                RenameReason::Slugified => "made a valid name",
                RenameReason::Collision => "name already taken, suffix added",
            };
            out.push_str(&format!(
                "# - {}: `{}` became `{}` ({why})\n",
                comment(&r.location),
                comment(&r.original),
                r.assigned
            ));
        }
    }
    if !plan.worktrees.is_empty() {
        out.push_str("\n# Worktrees found (not projects; their repository is tracked once):\n");
        for w in &plan.worktrees {
            let how = match w.kind {
                WorktreeKind::Pattern => "worktree folder pattern",
                WorktreeKind::LinkedWorktree => "linked worktree",
            };
            out.push_str(&format!("# - {} ({how})\n", comment(&w.path)));
        }
    }

    for p in &plan.projects {
        out.push_str(&format!("\n# from {}\n[[project]]\n", p.source.label()));
        out.push_str(&format!("name = {}\n", q(p.name.as_str())));
        out.push_str(&format!("path = {}\n", q(&p.path)));
        if let Some(root) = &p.root {
            out.push_str("# Only this sub-directory of the repository belongs to the project.\n");
            out.push_str(&format!("root = {}\n", q(root.as_str())));
        }
        if let Some(remote) = &p.remote {
            out.push_str(&format!("remote = {}\n", q(remote)));
        }
        match (&p.track, &shared) {
            (Some(_), Some(_)) => {}
            (Some(t), None) => out.push_str(&format!("track = {}\n", q(&t.to_string()))),
            (None, _) => out.push_str(
                "# TODO: choose the ref to follow, e.g. track = \"branch:<name>\" (Knowell does not guess it)\n",
            ),
        }
    }
    out
}
