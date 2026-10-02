//! SQL statements as chunk blocks (migration steps, DDL, DML).

use tree_sitter::{Node, Tree};

use crate::model::{Block, BlockKind};
use crate::text::{LineIndex, bounded, one_line, slice, starts_line, trim_end_offset};

const MAX_LABEL_BYTES: usize = 200;

/// One block per top-level `statement`, extended over its `;` and directly
/// preceding comment lines. One pass over the root's children: sibling
/// lookups per statement would be quadratic on large dumps.
pub(crate) fn blocks(tree: &Tree, text: &str, lines: &LineIndex, max_blocks: usize) -> Vec<Block> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let children: Vec<Node<'_>> = root.children(&mut cursor).collect();
    let mut blocks = Vec::new();
    // `(start byte, last row)` of the comment lines directly above.
    let mut comments: Option<(usize, usize)> = None;
    for (index, node) in children.iter().enumerate() {
        let row = node.start_position().row;
        let adjacent = |(_, last_row): (usize, usize)| last_row + 1 >= row;
        if node.kind().contains("comment") {
            comments = if starts_line(text, node.start_byte()) {
                match comments.filter(|c| adjacent(*c)) {
                    Some((start, _)) => Some((start, end_row(*node))),
                    None => Some((node.start_byte(), end_row(*node))),
                }
            } else {
                None
            };
            continue;
        }
        if node.kind() != "statement" {
            comments = None;
            continue;
        }
        if blocks.len() >= max_blocks {
            break;
        }
        let start = comments
            .filter(|c| adjacent(*c))
            .map_or(node.start_byte(), |(start, _)| start);
        comments = None;
        let mut end = node.end_byte();
        if let Some(next) = children.get(index + 1)
            && next.kind() == ";"
        {
            end = next.end_byte();
        }
        let end = trim_end_offset(text, start, end);
        let byte_range = start..end;
        let Some(range) = lines.range(&byte_range) else {
            continue;
        };
        let statement = slice(text, &node.byte_range());
        let label = bounded(
            &one_line(statement.lines().next().unwrap_or("")),
            MAX_LABEL_BYTES,
        );
        blocks.push(Block {
            kind: BlockKind::Statement,
            range,
            byte_range,
            label,
            subject: subject(*node, text),
        });
    }
    blocks
}

/// Last row of a node, not counting a trailing newline.
fn end_row(node: Node<'_>) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    }
}

/// The object a statement is about: the first `object_reference` directly
/// under the statement's main node (`CREATE TABLE x`, `ALTER TABLE x`,
/// `INSERT INTO x`).
fn subject(statement: Node<'_>, text: &str) -> Option<String> {
    let main = statement.named_child(0)?;
    let mut cursor = main.walk();
    let reference = main
        .named_children(&mut cursor)
        .find(|n| n.kind() == "object_reference")?;
    let name = reference.child_by_field_name("name").unwrap_or(reference);
    let name = one_line(slice(text, &name.byte_range()));
    (!name.is_empty()).then(|| bounded(&name, MAX_LABEL_BYTES))
}
