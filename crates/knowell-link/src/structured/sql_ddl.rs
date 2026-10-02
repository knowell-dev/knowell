//! Table definitions from SQL migrations.
//!
//! A tokenizer, not the tree-sitter SQL grammar: the bundled grammar loses
//! columns on valid PostgreSQL (`CHECK (interval IN (...))`, keyword-named
//! columns) and a migration / entity check must not miss columns. The
//! scanner reads `CREATE TABLE` column lists, `ALTER TABLE ... ADD / DROP /
//! RENAME COLUMN`, `ALTER TABLE ... RENAME TO` and `DROP TABLE`, skipping
//! comments, strings and constraint clauses.

use std::collections::BTreeMap;

use knowell_core::LineRange;
use knowell_graph::{ContractKind, EvidenceType};

use super::Ctx;
use crate::model::{ATTR_COLUMN, ATTR_NEW_NAME, ATTR_OP, Extraction, Role};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    text: String,
    line: u32,
    quoted: bool,
}

impl Token {
    fn is(&self, keyword: &str) -> bool {
        !self.quoted && self.text.eq_ignore_ascii_case(keyword)
    }
}

/// Tokens of SQL text: words / numbers, quoted identifiers (quotes
/// removed, `quoted` set) and single-character punctuation. Comments and
/// string literals are dropped.
fn tokenize(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut line: u32 = 1;
    let mut i = 0usize;
    let at = |i: usize| chars.get(i).copied();
    while let Some(c) = at(i) {
        match c {
            '\n' => {
                line = line.saturating_add(1);
                i += 1;
            }
            _ if c.is_whitespace() => i += 1,
            '-' if at(i + 1) == Some('-') => {
                while at(i).is_some_and(|c| c != '\n') {
                    i += 1;
                }
            }
            '/' if at(i + 1) == Some('*') => {
                i += 2;
                while let Some(c) = at(i) {
                    if c == '*' && at(i + 1) == Some('/') {
                        i += 2;
                        break;
                    }
                    if c == '\n' {
                        line = line.saturating_add(1);
                    }
                    i += 1;
                }
            }
            '\'' => {
                i += 1;
                while let Some(c) = at(i) {
                    if c == '\'' {
                        if at(i + 1) == Some('\'') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    if c == '\n' {
                        line = line.saturating_add(1);
                    }
                    i += 1;
                }
            }
            '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let start_line = line;
                let mut text = String::new();
                i += 1;
                while let Some(c) = at(i) {
                    i += 1;
                    if c == close {
                        break;
                    }
                    if c == '\n' {
                        line = line.saturating_add(1);
                    }
                    text.push(c);
                }
                tokens.push(Token {
                    text,
                    line: start_line,
                    quoted: true,
                });
            }
            _ if c.is_alphanumeric() || c == '_' || c == '$' => {
                let mut text = String::new();
                while let Some(c) = at(i) {
                    if c.is_alphanumeric() || c == '_' || c == '$' {
                        text.push(c);
                        i += 1;
                    } else {
                        break;
                    }
                }
                tokens.push(Token {
                    text,
                    line,
                    quoted: false,
                });
            }
            _ => {
                tokens.push(Token {
                    text: c.to_string(),
                    line,
                    quoted: false,
                });
                i += 1;
            }
        }
    }
    tokens
}

fn statements(tokens: &[Token]) -> Vec<&[Token]> {
    tokens
        .split(|t| t.text == ";" && !t.quoted)
        .filter(|s| !s.is_empty())
        .collect()
}

/// A possibly schema-qualified name starting at `index`; returns the name
/// (`schema.table`), its line and the index after it.
fn qualified_name(tokens: &[Token], mut index: usize) -> Option<(String, u32, usize)> {
    let first = tokens.get(index)?;
    if !first.quoted
        && !first
            .text
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
    {
        return None;
    }
    let mut name = first.text.clone();
    let line = first.line;
    index += 1;
    while tokens
        .get(index)
        .is_some_and(|t| t.text == "." && !t.quoted)
    {
        let Some(part) = tokens.get(index + 1) else {
            break;
        };
        name.push('.');
        name.push_str(&part.text);
        index += 2;
    }
    Some((name, line, index))
}

/// Skips `IF NOT EXISTS` / `IF EXISTS` / `ONLY`.
fn skip_modifiers(tokens: &[Token], mut index: usize) -> usize {
    loop {
        match tokens.get(index) {
            Some(t) if t.is("IF") => {
                index += 1;
                if tokens.get(index).is_some_and(|t| t.is("NOT")) {
                    index += 1;
                }
                if tokens.get(index).is_some_and(|t| t.is("EXISTS")) {
                    index += 1;
                }
            }
            Some(t) if t.is("ONLY") => index += 1,
            _ => return index,
        }
    }
}

/// Top-level comma-separated items of the parenthesised list starting at
/// `open` (which must be `(`).
fn list_items(tokens: &[Token], open: usize) -> Vec<&[Token]> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = open + 1;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        if token.quoted {
            continue;
        }
        match token.text.as_str() {
            "(" => depth += 1,
            ")" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(item) = tokens.get(start..index) {
                        items.push(item);
                    }
                    break;
                }
            }
            "," if depth == 1 => {
                if let Some(item) = tokens.get(start..index) {
                    items.push(item);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    items.into_iter().filter(|i| !i.is_empty()).collect()
}

/// Top-level comma-separated actions of an `ALTER TABLE`.
fn actions(tokens: &[Token]) -> Vec<&[Token]> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        if token.quoted {
            continue;
        }
        match token.text.as_str() {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            "," if depth == 0 => {
                if let Some(action) = tokens.get(start..index) {
                    out.push(action);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    if let Some(action) = tokens.get(start..) {
        out.push(action);
    }
    out.into_iter().filter(|a| !a.is_empty()).collect()
}

const CONSTRAINT_WORDS: &[&str] = &[
    "CONSTRAINT",
    "PRIMARY",
    "FOREIGN",
    "UNIQUE",
    "CHECK",
    "EXCLUDE",
    "LIKE",
    "PERIOD",
];

/// Words that start a MySQL index clause (`KEY idx (a, b)`) but are also
/// common column names (`key varchar(128)`).
const INDEX_WORDS: &[&str] = &["KEY", "INDEX", "FULLTEXT", "SPATIAL"];

fn is_constraint(token: &Token) -> bool {
    CONSTRAINT_WORDS.iter().any(|w| token.is(w))
}

/// Whether a table item is a constraint or index clause rather than a
/// column. `KEY idx (email)` is an index; `key varchar(128) PRIMARY KEY` is
/// a column, told apart by the parenthesised list holding names, not sizes.
fn is_not_a_column(item: &[Token]) -> bool {
    let Some(first) = item.first() else {
        return true;
    };
    if is_constraint(first) {
        return true;
    }
    if !INDEX_WORDS.iter().any(|w| first.is(w)) {
        return false;
    }
    let Some(open) = item.iter().position(|t| t.text == "(" && !t.quoted) else {
        return false;
    };
    let inner: Vec<&Token> = item
        .iter()
        .skip(open + 1)
        .take_while(|t| t.text != ")")
        .filter(|t| t.text != ",")
        .collect();
    !inner.is_empty()
        && inner.iter().all(|t| {
            t.quoted
                || t.text
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_')
        })
        && !item.iter().any(|t| {
            t.is("NOT") || t.is("NULL") || t.is("DEFAULT") || t.is("PRIMARY") || t.is("REFERENCES")
        })
}

/// One schema operation found in a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Op {
    table: String,
    op: &'static str,
    column: Option<String>,
    new_name: Option<String>,
    line: u32,
}

fn parse_statement(statement: &[Token], out: &mut Vec<Op>) {
    let Some(first) = statement.first() else {
        return;
    };
    if first.is("CREATE") {
        let mut index = 1;
        while statement.get(index).is_some_and(|t| {
            t.is("OR")
                || t.is("REPLACE")
                || t.is("TEMP")
                || t.is("TEMPORARY")
                || t.is("UNLOGGED")
                || t.is("GLOBAL")
                || t.is("LOCAL")
        }) {
            index += 1;
        }
        if !statement.get(index).is_some_and(|t| t.is("TABLE")) {
            return;
        }
        index = skip_modifiers(statement, index + 1);
        let Some((table, line, after)) = qualified_name(statement, index) else {
            return;
        };
        out.push(Op {
            table: table.clone(),
            op: "create",
            column: None,
            new_name: None,
            line,
        });
        if statement
            .get(after)
            .is_some_and(|t| t.text == "(" && !t.quoted)
        {
            for item in list_items(statement, after) {
                let Some(name) = item.first() else {
                    continue;
                };
                if is_not_a_column(item) {
                    continue;
                }
                out.push(Op {
                    table: table.clone(),
                    op: "create",
                    column: Some(name.text.clone()),
                    new_name: None,
                    line: name.line,
                });
            }
        }
        return;
    }
    if first.is("ALTER") && statement.get(1).is_some_and(|t| t.is("TABLE")) {
        let index = skip_modifiers(statement, 2);
        let Some((table, _, after)) = qualified_name(statement, index) else {
            return;
        };
        let rest = statement.get(after..).unwrap_or(&[]);
        for action in actions(rest) {
            alter_action(&table, action, out);
        }
        return;
    }
    if first.is("DROP") && statement.get(1).is_some_and(|t| t.is("TABLE")) {
        let mut index = skip_modifiers(statement, 2);
        while let Some((table, line, after)) = qualified_name(statement, index) {
            if statement
                .get(index)
                .is_some_and(|t| t.is("CASCADE") || t.is("RESTRICT"))
            {
                break;
            }
            out.push(Op {
                table,
                op: "drop_table",
                column: None,
                new_name: None,
                line,
            });
            if statement.get(after).is_some_and(|t| t.text == ",") {
                index = after + 1;
            } else {
                break;
            }
        }
    }
}

fn alter_action(table: &str, action: &[Token], out: &mut Vec<Op>) {
    let Some(verb) = action.first() else {
        return;
    };
    let mut index = 1;
    let column_keyword = |i: usize| action.get(i).is_some_and(|t| t.is("COLUMN"));
    if verb.is("ADD") {
        if action.get(index).is_some_and(is_constraint)
            || is_not_a_column(action.get(index..).unwrap_or(&[]))
        {
            return;
        }
        if column_keyword(index) {
            index += 1;
        }
        index = skip_modifiers(action, index);
        if let Some(name) = action.get(index) {
            out.push(Op {
                table: table.to_owned(),
                op: "add_column",
                column: Some(name.text.clone()),
                new_name: None,
                line: name.line,
            });
        }
    } else if verb.is("DROP") {
        if action
            .get(index)
            .is_some_and(|t| is_constraint(t) || t.is("DEFAULT") || t.is("NOT"))
        {
            return;
        }
        if column_keyword(index) {
            index += 1;
        }
        index = skip_modifiers(action, index);
        if let Some(name) = action.get(index) {
            out.push(Op {
                table: table.to_owned(),
                op: "drop_column",
                column: Some(name.text.clone()),
                new_name: None,
                line: name.line,
            });
        }
    } else if verb.is("RENAME") {
        if action.get(index).is_some_and(|t| t.is("TO")) {
            if let Some((new_table, line, _)) = qualified_name(action, index + 1) {
                out.push(Op {
                    table: table.to_owned(),
                    op: "rename_table",
                    column: None,
                    new_name: Some(new_table),
                    line,
                });
            }
            return;
        }
        if action.get(index).is_some_and(is_constraint) {
            return;
        }
        if column_keyword(index) {
            index += 1;
        }
        let (Some(old), Some(to), Some(new)) = (
            action.get(index),
            action.get(index + 1),
            action.get(index + 2),
        ) else {
            return;
        };
        if to.is("TO") {
            out.push(Op {
                table: table.to_owned(),
                op: "rename_column",
                column: Some(old.text.clone()),
                new_name: Some(new.text.clone()),
                line: old.line,
            });
        }
    }
}

/// Table definitions of a `.sql` file, in statement order.
pub(crate) fn extract(ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    let is_sql = ctx.path.extension().is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "sql" | "psql" | "pgsql" | "ddl"
        )
    });
    if !is_sql {
        return Vec::new();
    }
    let tokens = tokenize(text);
    let mut ops = Vec::new();
    for statement in statements(&tokens) {
        parse_statement(statement, &mut ops);
    }
    let mut out = Vec::new();
    for op in ops {
        let Ok(range) = LineRange::new(op.line.max(1), op.line.max(1)) else {
            continue;
        };
        let mut attrs = BTreeMap::new();
        attrs.insert(ATTR_OP.to_owned(), op.op.to_owned());
        if let Some(column) = op.column {
            attrs.insert(ATTR_COLUMN.to_owned(), column.to_lowercase());
        }
        if let Some(new_name) = op.new_name {
            attrs.insert(ATTR_NEW_NAME.to_owned(), new_name.to_lowercase());
        }
        out.extend(ctx.extraction(
            ContractKind::Table,
            Role::Definition,
            &op.table,
            range,
            None,
            EvidenceType::ContractDerived,
            attrs,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(sql: &str) -> Vec<String> {
        let tokens = tokenize(sql);
        let mut out = Vec::new();
        for statement in statements(&tokens) {
            parse_statement(statement, &mut out);
        }
        out.into_iter()
            .map(|o| {
                format!(
                    "{}:{} {} {}{}",
                    o.line,
                    o.op,
                    o.table,
                    o.column.unwrap_or_default(),
                    o.new_name.map(|n| format!("->{n}")).unwrap_or_default()
                )
            })
            .collect()
    }

    #[test]
    fn create_with_constraints_and_keywords() {
        let sql = "-- plans\nCREATE TABLE IF NOT EXISTS public.plans (\n  id varchar(64) PRIMARY KEY,\n  interval varchar(8) NOT NULL CHECK (interval IN ('month', 'year')),\n  \"Active\" boolean DEFAULT true,\n  CONSTRAINT plans_pk PRIMARY KEY (id),\n  UNIQUE (interval)\n);";
        assert_eq!(
            ops(sql),
            [
                "2:create public.plans ",
                "3:create public.plans id",
                "4:create public.plans interval",
                "5:create public.plans Active"
            ]
        );
    }

    #[test]
    fn key_columns_and_index_clauses() {
        let sql = "CREATE TABLE k (\n  key varchar(128) PRIMARY KEY,\n  index int,\n  KEY idx_email (email),\n  INDEX (a, b),\n  email text\n);";
        assert_eq!(
            ops(sql),
            [
                "1:create k ",
                "2:create k key",
                "3:create k index",
                "6:create k email"
            ]
        );
    }

    #[test]
    fn alter_and_drop() {
        let sql = "ALTER TABLE subs ADD COLUMN reason varchar(32), ADD COLUMN feedback text;\nALTER TABLE subs ADD CONSTRAINT c CHECK (reason IS NULL);\nALTER TABLE ONLY subs DROP COLUMN IF EXISTS old, RENAME COLUMN a TO b;\nALTER TABLE subs RENAME TO subscriptions;\nALTER TABLE x ALTER COLUMN y SET NOT NULL;\nDROP TABLE IF EXISTS legacy, other CASCADE;";
        assert_eq!(
            ops(sql),
            [
                "1:add_column subs reason",
                "1:add_column subs feedback",
                "3:drop_column subs old",
                "3:rename_column subs a->b",
                "4:rename_table subs ->subscriptions",
                "6:drop_table legacy ",
                "6:drop_table other "
            ]
        );
    }

    #[test]
    fn dml_strings_and_comments_are_ignored() {
        assert!(ops("INSERT INTO t (a) VALUES ('CREATE TABLE x (y int)'); /* CREATE TABLE z (w int) */ SELECT 1;").is_empty());
        assert!(ops("CREATE INDEX i ON t (a); CREATE VIEW v AS SELECT 1;").is_empty());
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for sql in [
            "CREATE TABLE",
            "CREATE TABLE (",
            "ALTER TABLE",
            "'unterminated",
            "\"open",
            "/* open",
            "DROP TABLE ,,",
            "ALTER TABLE t RENAME COLUMN a",
            ")))(((",
            "CREATE TABLE t (((((",
        ] {
            let _ = ops(sql);
        }
        let deep = "(".repeat(10_000);
        let _ = ops(&format!("CREATE TABLE t {deep}"));
    }
}
