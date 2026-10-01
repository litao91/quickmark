//! Whole-tree traversal, replacing `tree_sitter_walker::TreeSitterWalker`.

use super::{FacadeTree, Node};

#[derive(Copy, Clone, Debug)]
pub enum TraversalOrder {
    PreOrder,
    PostOrder,
}

#[derive(Debug)]
pub struct Walker<'a> {
    pub order: TraversalOrder,
    pub tree: &'a FacadeTree,
}

impl<'a> Walker<'a> {
    pub fn new(tree: &'a FacadeTree) -> Self {
        Self {
            tree,
            order: TraversalOrder::PreOrder,
        }
    }

    pub fn with_order(tree: &'a FacadeTree, order: TraversalOrder) -> Self {
        Self { tree, order }
    }

    pub fn walk(&self, mut callback: impl FnMut(Node<'a>)) {
        match self.order {
            // Nodes are stored in pre-order, so the traversal is just the index range.
            TraversalOrder::PreOrder => {
                for index in 0..self.tree.node_count() as u32 {
                    callback(self.tree.node(index));
                }
            }
            TraversalOrder::PostOrder => self.walk_post_order(&mut callback),
        }
    }

    /// Iterative rather than recursive: a deeply nested document must not overflow the stack.
    fn walk_post_order(&self, callback: &mut impl FnMut(Node<'a>)) {
        let mut pending: Vec<(u32, usize)> = vec![(0, 0)];
        while let Some((index, next_child)) = pending.last_mut() {
            let children = self.tree.children_of(*index);
            match children.get(*next_child) {
                Some(&child) => {
                    *next_child += 1;
                    pending.push((child, 0));
                }
                None => {
                    let (index, _) = pending.pop().expect("pending is non-empty");
                    callback(self.tree.node(index));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::build;

    #[test]
    fn pre_order_visits_every_node_once() {
        let tree = build::parse("# H\n\ntext\n\n- a\n- b\n");
        let mut visited = Vec::new();
        Walker::new(&tree).walk(|node| visited.push(node.kind().to_string()));
        assert_eq!(visited.len(), tree.node_count());
        assert_eq!(visited[0], "document");
    }

    #[test]
    fn post_order_visits_children_before_parents() {
        let tree = build::parse("# H\n");
        let mut visited = Vec::new();
        Walker::with_order(&tree, TraversalOrder::PostOrder)
            .walk(|node| visited.push(node.kind().to_string()));
        assert_eq!(visited.len(), tree.node_count());
        assert_eq!(visited.last().map(String::as_str), Some("document"));
        let heading = visited.iter().position(|kind| kind == "atx_heading");
        let marker = visited.iter().position(|kind| kind == "atx_h1_marker");
        assert!(heading > marker, "post-order must visit the marker first");
    }
}
