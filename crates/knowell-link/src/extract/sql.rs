//! Table references inside SQL text embedded in code (`postprocess = "sql"`).
//!
//! A small scanner, not a parser: it looks at upper-case keywords only
//! (`SELECT ... FROM t`, `JOIN t`, `INSERT INTO t`, `UPDATE t SET`,
//! `DELETE FROM t`, `MERGE INTO t`), so prose such as "Select a bay from the
//! list" is not mistaken for SQL. Lower-case SQL is a documented limit.

use crate::model::Role;

/// Whether `text` starts like an SQL statement.
pub(crate) fn looks_like_sql(text: &str) -> bool {
    let first = text.split_whitespace().next().unwrap_or("");
    let rest_has = |kw: &str| text.split_whitespace().any(|w| w == kw);
    match first {
        "SELECT" => rest_has("FROM"),
        "INSERT" | "MERGE" => rest_has("INTO"),
        "UPDATE" => rest_has("SET"),
        "DELETE" => rest_has("FROM"),
        "WITH" => {
            rest_has("AS") && (rest_has("SELECT") || rest_has("INSERT") || rest_has("UPDATE"))
        }
        _ => false,
    }
}

/// `(table, role)` pairs referenced by the statement, in order of
/// appearance, without duplicates.
pub(crate) fn tables(text: &str) -> Vec<(String, Role)> {
    if !looks_like_sql(text) {
        return Vec::new();
    }
    let tokens = tokenize(text);
    let mut ctes: Vec<String> = Vec::new();
    let mut out: Vec<(String, Role)> = Vec::new();
    let push = |name: &str, role: Role, out: &mut Vec<(String, Role)>| {
        if !out.iter().any(|(n, r)| n == name && *r == role) {
            out.push((name.to_owned(), role));
        }
    };
    for (index, token) in tokens.iter().enumerate() {
        let next = tokens.get(index + 1).map(String::as_str);
        let after = tokens.get(index + 2).map(String::as_str);
        // `WITH name AS (` declares a CTE that is not a table.
        if (token == "WITH" || token == ",")
            && after == Some("AS")
            && let Some(name) = next
        {
            ctes.push(name.to_lowercase());
        }
        let (role, target) = match token.as_str() {
            "FROM" | "JOIN" => (Role::Reads, next),
            "INTO" => (Role::Writes, next),
            "UPDATE" => (Role::Writes, next),
            _ => continue,
        };
        // `DELETE FROM t` writes.
        let role = if token == "FROM"
            && index
                .checked_sub(1)
                .and_then(|i| tokens.get(i))
                .is_some_and(|t| t == "DELETE")
        {
            Role::Writes
        } else {
            role
        };
        let Some(name) = target else {
            continue;
        };
        if !is_table_name(name) {
            continue;
        }
        // A function call (`FROM jsonb_to_recordset(...)`) is not a table;
        // `INSERT INTO t (columns)` is.
        if role == Role::Reads && after.is_some_and(|a| a.starts_with('(')) {
            continue;
        }
        let lower = name.to_lowercase();
        if ctes.contains(&lower) {
            continue;
        }
        push(name, role, &mut out);
    }
    out
}

fn is_table_name(token: &str) -> bool {
    let trimmed = token.trim_matches(|c| c == '"' || c == '`');
    !trimmed.is_empty()
        && trimmed
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        && trimmed
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '"' | '`'))
        && !matches!(
            trimmed.to_ascii_uppercase().as_str(),
            "SELECT" | "LATERAL" | "UNNEST" | "ONLY" | "VALUES"
        )
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in text.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' => {
                flush(&mut current, &mut tokens);
                quote = Some('\'');
            }
            '(' | ')' | ',' | ';' => {
                flush(&mut current, &mut tokens);
                tokens.push(c.to_string());
            }
            _ if c.is_whitespace() => flush(&mut current, &mut tokens),
            _ => current.push(c),
        }
    }
    flush(&mut current, &mut tokens);
    tokens
}

fn flush(current: &mut String, tokens: &mut Vec<String>) {
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<String> {
        tables(text)
            .into_iter()
            .map(|(n, r)| format!("{} {n}", r.as_str()))
            .collect()
    }

    #[test]
    fn statements() {
        assert_eq!(
            names("SELECT b.id FROM bays b JOIN zones z ON z.id = b.zone_id"),
            ["reads bays", "reads zones"]
        );
        assert_eq!(
            names("UPDATE bays SET name = $2 WHERE id = $1"),
            ["writes bays"]
        );
        assert_eq!(
            names(
                "INSERT INTO orders (id) SELECT x FROM jsonb_to_recordset($3::jsonb) AS l(sku text) JOIN products p ON p.sku = l.sku"
            ),
            ["writes orders", "reads products"]
        );
        assert_eq!(names("DELETE FROM bays WHERE id = %s"), ["writes bays"]);
        assert_eq!(
            names("WITH recent AS (SELECT id FROM orders) SELECT * FROM recent"),
            ["reads orders"]
        );
        assert_eq!(names("SELECT 'FROM x' FROM y"), ["reads y"]);
    }

    #[test]
    fn prose_is_not_sql() {
        assert!(names("Select a bay from the list and update the form.").is_empty());
        assert!(names("select * from somewhere").is_empty());
        assert!(names("UPDATE your profile").is_empty());
        assert!(names("").is_empty());
    }
}
