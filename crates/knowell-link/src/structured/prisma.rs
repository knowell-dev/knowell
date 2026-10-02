//! Prisma schema models (no tree-sitter grammar is bundled for Prisma, so a
//! small line scanner reads `model` blocks).
//!
//! Each scalar field becomes a column (`@map("x")` renames it); relation
//! fields (whose type is another model, or that carry `@relation`) are not
//! columns. The table is `@@map("x")` or the model name.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::LineRange;
use knowell_graph::{ContractKind, EvidenceType};

use super::Ctx;
use crate::model::{ATTR_COLUMN, ATTR_ENTITY, Extraction, Role, SymbolRef};

struct Model {
    name: String,
    start: u32,
    end: u32,
    table: Option<String>,
    fields: Vec<(String, String, u32, bool)>,
}

fn quoted_arg(text: &str, attribute: &str) -> Option<String> {
    let start = text.find(attribute)? + attribute.len();
    let rest = text.get(start..)?.trim_start();
    let rest = rest.strip_prefix('(')?;
    let rest = rest
        .trim_start()
        .strip_prefix("name:")
        .unwrap_or(rest)
        .trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    rest.get(..end).map(str::to_owned)
}

fn parse(text: &str) -> (Vec<Model>, BTreeSet<String>) {
    let mut models = Vec::new();
    let mut model_names = BTreeSet::new();
    let mut current: Option<Model> = None;
    for (index, raw_line) in text.lines().enumerate() {
        let line_number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let line = raw_line.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(model) = current.as_mut() {
            if line.starts_with('}') {
                model.end = line_number;
                if let Some(done) = current.take() {
                    models.push(done);
                }
                continue;
            }
            if line.starts_with("@@") {
                if line.starts_with("@@map") {
                    model.table = quoted_arg(line, "@@map");
                }
                continue;
            }
            let mut words = line.split_whitespace();
            let (Some(name), Some(ty)) = (words.next(), words.next()) else {
                continue;
            };
            let relation = line.contains("@relation");
            let column = quoted_arg(line, "@map").unwrap_or_else(|| name.to_owned());
            let base = ty
                .trim_end_matches(['?', '!'])
                .trim_end_matches("[]")
                .to_owned();
            model.fields.push((column, base, line_number, relation));
            continue;
        }
        let mut words = line.split_whitespace();
        if words.next() == Some("model")
            && let Some(name) = words.next()
            && line.ends_with('{')
        {
            model_names.insert(name.to_owned());
            current = Some(Model {
                name: name.to_owned(),
                start: line_number,
                end: line_number,
                table: None,
                fields: Vec::new(),
            });
        }
    }
    (models, model_names)
}

/// Table mappings of a `.prisma` schema.
pub(crate) fn extract(ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    if ctx.path.extension() != Some("prisma") {
        return Vec::new();
    }
    let (models, model_names) = parse(text);
    let mut out = Vec::new();
    for model in models {
        let table = model.table.clone().unwrap_or_else(|| model.name.clone());
        let Ok(model_range) = LineRange::new(model.start, model.end.max(model.start)) else {
            continue;
        };
        let symbol = SymbolRef {
            qualified_name: model.name.clone(),
            range: model_range,
        };
        let mut entity_attrs = BTreeMap::new();
        entity_attrs.insert(ATTR_ENTITY.to_owned(), model.name.clone());
        out.extend(ctx.extraction(
            ContractKind::Table,
            Role::Reads,
            &table,
            model_range,
            Some(symbol.clone()),
            EvidenceType::Syntactic,
            entity_attrs.clone(),
        ));
        for (column, base, line, relation) in &model.fields {
            if *relation || model_names.contains(base) {
                continue;
            }
            let Ok(range) = LineRange::new(*line, *line) else {
                continue;
            };
            let mut attrs = entity_attrs.clone();
            attrs.insert(ATTR_COLUMN.to_owned(), column.clone());
            out.extend(ctx.extraction(
                ContractKind::Table,
                Role::Reads,
                &table,
                range,
                Some(symbol.clone()),
                EvidenceType::Syntactic,
                attrs,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_models() {
        let (models, names) = parse(
            "model A {\n  id String @id\n  b  B[]\n  x  Int @map(\"x_col\") // c\n  @@map(name: \"as\")\n}\nmodel B {\n}\n",
        );
        assert_eq!(names.into_iter().collect::<Vec<_>>(), ["A", "B"]);
        let a = models.first().unwrap();
        assert_eq!(a.table.as_deref(), Some("as"));
        let cols: Vec<&str> = a.fields.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(cols, ["id", "b", "x_col"]);
        assert_eq!(quoted_arg("@@map(\"t\")", "@@map").as_deref(), Some("t"));
        assert_eq!(quoted_arg("@@map(", "@@map"), None);
        let (models, _) = parse("model Broken {\n  id String\n");
        assert!(models.is_empty());
    }
}
