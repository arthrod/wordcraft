//! How deep a document nests, measured before it is converted.
//!
//! The conversion recurses once for each table in a table's cell, each group in a group and each
//! equation structure in another, so a document nested deep enough would overflow the stack: a
//! crash, not an error. (The JSON bridge this crate replaced was bounded by `serde_json`'s
//! recursion limit.) Each kind of nesting has its own limit, because a level of each costs the
//! conversion a very different amount of stack. The walk that measures recurses too, but it stops
//! descending the moment a limit is passed, so its own depth is bounded by the limits.
//!
//! The limits are above what the shared readers make (craft-docx reads tables 24 deep;
//! craft-doc-binary groups 12 deep, craft-docx 8; craft-math equations 64 structures deep), and a
//! document at all three limits at once converts on a 512 KiB stack (`tests/depth.rs`).
//!
//! Measured 2026-11-16 on a thread with a 512 KiB stack, the deepest that converted and dropped
//! without overflowing, reading / writing. With the workspace's dev and test profile (`opt-level`
//! 1 for this crate, 2 for its dependencies), each kind alone: tables 92 / 151, groups 276 / 536,
//! equation structures 898 / 1,423; tables with groups and structures at their limits: 79 / 139,
//! so the table limit leaves more than half of the reading headroom unused. With everything at
//! `opt-level` 0, each alone: tables 36 / 86, groups 53 / 133, structures 123 / 261; tables with
//! the other two at their limits: 2 / 50. At `opt-level` 0 a document at the limits does not fit
//! in 512 KiB reading, and neither does one at the readers' own limits (24 tables, 12 groups, 65
//! equation levels); no limit both takes what the readers make and fits there.

/// Tables inside tables' cells.
pub(crate) const MAX_TABLES: usize = 32;
/// Groups inside groups.
pub(crate) const MAX_GROUPS: usize = 16;
/// Equation structures inside structures (a fraction in a fraction…), each node counting one.
pub(crate) const MAX_MATH: usize = 80;

/// How deep the walk is, in each kind of nesting.
#[derive(Clone, Copy, Default)]
struct Depth {
    tables: usize,
    groups: usize,
    math: usize,
}

impl Depth {
    /// One table deeper, or `None` past the limit.
    fn table(self) -> Option<Self> {
        (self.tables < MAX_TABLES).then_some(Depth { tables: self.tables + 1, ..self })
    }

    fn group(self) -> Option<Self> {
        (self.groups < MAX_GROUPS).then_some(Depth { groups: self.groups + 1, ..self })
    }

    fn math(self) -> Option<Self> {
        (self.math < MAX_MATH).then_some(Depth { math: self.math + 1, ..self })
    }
}

/// The walk and the iterative teardown, for one model; the two models are twins.
macro_rules! nesting {
    ($name:ident, $model:ident) => {
        pub(crate) mod $name {
            use std::sync::Arc;

            use super::Depth;
            use m::math::MNode;
            use m::para::InlineObject;
            use $model as m;

            /// Whether every story of `document` nests no deeper than the limits.
            pub(crate) fn within(document: &m::Document) -> bool {
                // The stories that hold blocks. (`the_bridge_names_every_document_field` fails when a
                // field is added to the document.)
                let m::Document { body, parts, footnote_separators, endnote_separators, glossary, .. } = document;
                let start = Depth::default();
                blocks(body, start)
                    && parts.values().all(|part| blocks(&part.blocks, start))
                    && [footnote_separators, endnote_separators].into_iter().all(|separators| {
                        let m::NoteSeparators { separator, continuation_separator, continuation_notice } = separators;
                        [separator, continuation_separator, continuation_notice].into_iter().flatten().all(|story| blocks(story, start))
                    })
                    && glossary.iter().all(|entry| blocks(&entry.blocks, start))
            }

            fn blocks(list: &[Arc<m::Block>], depth: Depth) -> bool {
                list.iter().all(|block| match &**block {
                    m::Block::Para(paragraph) => paragraph.objects.iter().all(|object| self::object(object, depth)),
                    m::Block::Table(table) => {
                        depth.table().is_some_and(|depth| table.rows.iter().all(|row| row.cells.iter().all(|cell| blocks(&cell.blocks, depth))))
                    }
                })
            }

            fn object(object: &InlineObject, depth: Depth) -> bool {
                match object {
                    InlineObject::Group { children, .. } => {
                        depth.group().is_some_and(|depth| children.iter().all(|child| self::object(&child.obj, depth)))
                    }
                    InlineObject::Equation { math, .. } => nodes(&math.nodes, depth),
                    // Named one by one, so that an object which comes to hold others is looked at here.
                    InlineObject::Image { .. }
                    | InlineObject::Graphic { .. }
                    | InlineObject::Shape { .. }
                    | InlineObject::Field { .. }
                    | InlineObject::FieldStart { .. }
                    | InlineObject::FieldEnd
                    | InlineObject::ControlStart { .. }
                    | InlineObject::ControlEnd
                    | InlineObject::NoteRef { .. }
                    | InlineObject::BookmarkStart { .. }
                    | InlineObject::BookmarkEnd { .. }
                    | InlineObject::CommentStart { .. }
                    | InlineObject::CommentEnd { .. }
                    | InlineObject::PermStart { .. }
                    | InlineObject::PermEnd { .. }
                    | InlineObject::Ruby { .. }
                    | InlineObject::PositionalTab { .. }
                    | InlineObject::Opaque { .. } => true,
                }
            }

            fn nodes(arg: &[MNode], depth: Depth) -> bool {
                arg.iter().all(|node| depth.math().is_some_and(|depth| args(node).all(|arg| nodes(arg, depth))))
            }

            /// A node's arguments.
            fn args(node: &MNode) -> impl Iterator<Item = &[MNode]> {
                let (lists, rows): (Vec<&[MNode]>, &[Vec<m::math::Arg>]) = match node {
                    MNode::Run(_) => (Vec::new(), &[]),
                    MNode::Frac { num, den, .. } => (vec![num, den], &[]),
                    MNode::Script { base, sub, sup, .. } => (vec![base, sub, sup], &[]),
                    MNode::Rad { deg, e, .. } => (vec![deg, e], &[]),
                    MNode::Nary { sub, sup, e, .. } => (vec![sub, sup, e], &[]),
                    MNode::Delim { elems, .. } => (elems.iter().map(Vec::as_slice).collect(), &[]),
                    MNode::Func { name, e } => (vec![name, e], &[]),
                    MNode::Lim { e, lim, .. } => (vec![e, lim], &[]),
                    MNode::Acc { e, .. }
                    | MNode::Bar { e, .. }
                    | MNode::BorderBox { e, .. }
                    | MNode::Boxed { e }
                    | MNode::GroupChr { e, .. }
                    | MNode::Phant { e, .. } => (vec![e], &[]),
                    MNode::EqArr { rows } => (rows.iter().map(Vec::as_slice).collect(), &[]),
                    MNode::Matrix { rows, .. } => (Vec::new(), rows.as_slice()),
                };
                lists.into_iter().chain(rows.iter().flatten().map(Vec::as_slice))
            }

            /// Drops `document` without recursing: a document nested too deep to convert is too deep
            /// for the recursive `Drop` too. Its blocks, objects and equation nodes are taken out
            /// onto lists and dropped one level at a time.
            #[cfg_attr(not(test), allow(dead_code, reason = "WordCraft's documents are borrowed, not consumed; the tests use it"))]
            pub(crate) fn dismantle(document: m::Document) {
                let m::Document { body, parts, footnote_separators, endnote_separators, glossary, .. } = document;
                let mut blocks: Vec<Arc<m::Block>> = body;
                blocks.extend(parts.into_values().flat_map(|part| part.blocks));
                for separators in [footnote_separators, endnote_separators] {
                    let m::NoteSeparators { separator, continuation_separator, continuation_notice } = separators;
                    blocks.extend([separator, continuation_separator, continuation_notice].into_iter().flatten().flatten());
                }
                blocks.extend(glossary.into_iter().flat_map(|entry| entry.blocks));

                let mut objects: Vec<InlineObject> = Vec::new();
                let mut nodes: Vec<MNode> = Vec::new();
                loop {
                    if let Some(block) = blocks.pop() {
                        // A block another document still points to is only released here.
                        match Arc::into_inner(block) {
                            Some(m::Block::Para(paragraph)) => objects.extend(paragraph.objects),
                            Some(m::Block::Table(table)) => {
                                blocks.extend(table.rows.into_iter().flat_map(|row| row.cells).flat_map(|cell| cell.blocks))
                            }
                            None => {}
                        }
                    } else if let Some(object) = objects.pop() {
                        // Only these two hold more of the tree; see `object`.
                        match object {
                            InlineObject::Group { children, .. } => objects.extend(children.into_iter().map(|child| child.obj)),
                            InlineObject::Equation { math, .. } => nodes.extend(math.nodes),
                            _ => {}
                        }
                    } else if let Some(node) = nodes.pop() {
                        take_args(node, &mut nodes);
                    } else {
                        break;
                    }
                }
            }

            /// Moves a node's arguments' nodes onto `out`.
            fn take_args(node: MNode, out: &mut Vec<MNode>) {
                match node {
                    MNode::Run(_) => {}
                    MNode::Frac { num, den, .. } => out.extend(num.into_iter().chain(den)),
                    MNode::Script { base, sub, sup, .. } => out.extend(base.into_iter().chain(sub).chain(sup)),
                    MNode::Rad { deg, e, .. } => out.extend(deg.into_iter().chain(e)),
                    MNode::Nary { sub, sup, e, .. } => out.extend(sub.into_iter().chain(sup).chain(e)),
                    MNode::Delim { elems, .. } => out.extend(elems.into_iter().flatten()),
                    MNode::Func { name, e } => out.extend(name.into_iter().chain(e)),
                    MNode::Lim { e, lim, .. } => out.extend(e.into_iter().chain(lim)),
                    MNode::Acc { e, .. }
                    | MNode::Bar { e, .. }
                    | MNode::BorderBox { e, .. }
                    | MNode::Boxed { e }
                    | MNode::GroupChr { e, .. }
                    | MNode::Phant { e, .. } => out.extend(e),
                    MNode::EqArr { rows } => out.extend(rows.into_iter().flatten()),
                    MNode::Matrix { rows, .. } => out.extend(rows.into_iter().flatten().flatten()),
                }
            }
        }
    };
}

nesting!(shared, craft_wordmodel);
nesting!(own, wordcraft_doc);
