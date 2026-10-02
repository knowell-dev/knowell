//! Merkle-style tree hash of a view's files.
//!
//! Every file is a leaf `H("f", name, content hash)`; every directory is
//! `H("d", name, children...)` over its children in component order; the
//! root is the hash of the top directory. Two views with the same paths and
//! contents have the same hash, whatever order the files were listed in, and
//! a single changed file changes exactly the hashes on its path to the root.
//!
//! The hash is computed with an explicit stack (no recursion), so deeply
//! nested paths from untrusted repositories cannot exhaust the call stack.

use knowell_core::{ContentHash, RepoPath};

const LEAF: &[u8] = b"knowell.tree.v1.file";
const DIR: &[u8] = b"knowell.tree.v1.dir";

struct Frame<'a> {
    name: &'a str,
    children: Vec<ContentHash>,
}

impl Frame<'_> {
    fn close(self) -> ContentHash {
        let mut parts: Vec<&[u8]> = Vec::with_capacity(self.children.len() + 2);
        parts.push(DIR);
        parts.push(self.name.as_bytes());
        for child in &self.children {
            parts.push(child.as_bytes().as_slice());
        }
        ContentHash::of_parts(parts)
    }
}

/// The tree hash of `files` (path and content hash of every indexed file).
/// Duplicate paths are hashed once (the last occurrence wins); an empty view
/// has the hash of an empty root directory.
pub fn tree_hash<'a, I>(files: I) -> ContentHash
where
    I: IntoIterator<Item = (&'a RepoPath, &'a ContentHash)>,
{
    let mut entries: Vec<(Vec<&'a str>, &'a ContentHash)> = files
        .into_iter()
        .map(|(path, hash)| (path.components().collect(), hash))
        .collect();
    // Component order, not byte order: `a/b` sorts before `a.b` because the
    // directory `a` sorts before the file `a.b`.
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.dedup_by(|later, earlier| {
        if later.0 == earlier.0 {
            earlier.1 = later.1;
            true
        } else {
            false
        }
    });

    let mut stack: Vec<Frame<'a>> = vec![Frame {
        name: "",
        children: Vec::new(),
    }];
    for (components, hash) in entries {
        let Some((file, dirs)) = components.split_last() else {
            continue;
        };
        // Directories already open that this path shares.
        let common = stack
            .iter()
            .skip(1)
            .zip(dirs.iter())
            .take_while(|(frame, dir)| frame.name == **dir)
            .count();
        while stack.len() > common + 1 {
            close_top(&mut stack);
        }
        for dir in dirs.iter().skip(common) {
            stack.push(Frame {
                name: dir,
                children: Vec::new(),
            });
        }
        let leaf = ContentHash::of_parts([LEAF, file.as_bytes(), hash.as_bytes().as_slice()]);
        if let Some(top) = stack.last_mut() {
            top.children.push(leaf);
        }
    }
    while stack.len() > 1 {
        close_top(&mut stack);
    }
    stack.pop().map_or_else(
        || {
            Frame {
                name: "",
                children: Vec::new(),
            }
            .close()
        },
        Frame::close,
    )
}

fn close_top(stack: &mut Vec<Frame<'_>>) {
    if let Some(frame) = stack.pop() {
        let hash = frame.close();
        if let Some(parent) = stack.last_mut() {
            parent.children.push(hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn h(s: &str) -> ContentHash {
        ContentHash::of(s.as_bytes())
    }

    #[test]
    fn order_independent_and_content_sensitive() {
        let files = [
            (p("src/a.rs"), h("a")),
            (p("src/b/c.rs"), h("c")),
            (p("README.md"), h("r")),
            (p("a.b"), h("ab")),
            (p("a/b"), h("a/b")),
        ];
        let forward = tree_hash(files.iter().map(|(p, h)| (p, h)));
        let backward = tree_hash(files.iter().rev().map(|(p, h)| (p, h)));
        assert_eq!(forward, backward);

        let mut changed = files.clone();
        changed[1].1 = h("c2");
        assert_ne!(forward, tree_hash(changed.iter().map(|(p, h)| (p, h))));

        let mut moved = files.clone();
        moved[1].0 = p("src/b/d.rs");
        assert_ne!(forward, tree_hash(moved.iter().map(|(p, h)| (p, h))));
    }

    #[test]
    fn structure_matters_not_just_names() {
        // Same leaf names and hashes, different directories.
        let a = [(p("x/y/z"), h("1"))];
        let b = [(p("x/z"), h("1"))];
        let c = [(p("x/y"), h("1")), (p("z"), h("2"))];
        let d = [(p("x"), h("1")), (p("y/z"), h("2"))];
        let hashes = [
            tree_hash(a.iter().map(|(p, h)| (p, h))),
            tree_hash(b.iter().map(|(p, h)| (p, h))),
            tree_hash(c.iter().map(|(p, h)| (p, h))),
            tree_hash(d.iter().map(|(p, h)| (p, h))),
        ];
        for i in 0..hashes.len() {
            for j in (i + 1)..hashes.len() {
                assert_ne!(hashes[i], hashes[j], "{i} vs {j}");
            }
        }
    }

    #[test]
    fn empty_and_duplicates() {
        let empty: [(RepoPath, ContentHash); 0] = [];
        let e = tree_hash(empty.iter().map(|(p, h)| (p, h)));
        assert_eq!(e, tree_hash(empty.iter().map(|(p, h)| (p, h))));
        let dup = [(p("a"), h("1")), (p("a"), h("1"))];
        let one = [(p("a"), h("1"))];
        assert_eq!(
            tree_hash(dup.iter().map(|(p, h)| (p, h))),
            tree_hash(one.iter().map(|(p, h)| (p, h)))
        );
        assert_ne!(e, tree_hash(one.iter().map(|(p, h)| (p, h))));
    }

    #[test]
    fn deep_paths_do_not_recurse() {
        let deep = vec!["d"; 5000].join("/");
        let files = [(p(&deep), h("x"))];
        let _ = tree_hash(files.iter().map(|(p, h)| (p, h)));
    }
}
