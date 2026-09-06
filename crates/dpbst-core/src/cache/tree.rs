//! Bucket policies: what lives at the end of each hash slot.
//!
//! The project's premise is that a hash table's buckets should be binary search trees rather
//! than linked lists. Asserting that is easy; measuring it needs a control and some variants, so
//! four policies share one interface and one node arena:
//!
//! | policy         | bucket shape                | why it is here                          |
//! |----------------|-----------------------------|-----------------------------------------|
//! | [`Avl`]        | height-balanced BST         | the claim: `O(log k)` worst case         |
//! | [`Unbalanced`] | plain BST, no rebalancing   | is random insertion order already enough?|
//! | [`Splay`]      | self-adjusting BST          | cache access is skewed; exploit it       |
//! | [`Chain`]      | singly linked list          | the control: textbook separate chaining  |
//!
//! # Ordering
//!
//! Trees are ordered by the **full 64-bit hash**, falling back to the raw key bytes only when
//! two entries collide on all 64 bits. Since the bucket index is the *low* bits of that hash,
//! every key in a bucket agrees on those bits and differs in the remaining, uniformly random,
//! high bits. Two consequences:
//!
//! * A comparison is one `u64` compare in essentially every case, not a `memcmp` over a
//!   variable-length key.
//! * Insertion order within a bucket is effectively random, so even [`Unbalanced`] has expected
//!   depth `O(log k)`. That makes the comparison against [`Avl`] a real question rather than a
//!   foregone conclusion.
//!
//! Bucket occupancy is bounded by the table's load factor, so every policy here sees short
//! sequences; recursion depth is bounded by bucket size and cannot overflow the stack.

use super::arena::{NodeArena, NodeId};
use std::cmp::Ordering;

/// How a bucket's entries are organised.
pub trait BucketPolicy {
    /// Name used by the CLI and by benchmark output.
    const NAME: &'static str;

    /// Finds the entry matching `(hash, key)`, or [`NodeId::NONE`].
    ///
    /// Takes the root by mutable reference because a self-adjusting policy rewrites the bucket
    /// on a *successful lookup* as well as on insertion.
    fn lookup(arena: &mut NodeArena, root: &mut NodeId, hash: u64, key: &[u8]) -> NodeId;

    /// Links `node` into the bucket. The caller guarantees its key is not already present.
    ///
    /// `hash` and `key` describe `node`. They are passed in rather than read back out of the
    /// arena so that a policy can compare against them while the node pool is mutably borrowed —
    /// the caller already holds the key in its own buffer, so this also avoids a copy.
    fn insert(arena: &mut NodeArena, root: &mut NodeId, node: NodeId, hash: u64, key: &[u8]);

    /// Rebuilds a bucket from scratch, used after a resize or an eviction sweep.
    ///
    /// Here the keys *do* live in the arena, so each is copied into `scratch` for the duration of
    /// its insertion. `scratch` is reused across the whole rebuild.
    fn rebuild(arena: &mut NodeArena, ids: &[NodeId], scratch: &mut Vec<u8>) -> NodeId {
        let mut root = NodeId::NONE;
        for &id in ids {
            let (hash, off, len) = {
                let n = arena.get(id);
                (n.hash, n.key_off, n.key_len)
            };
            scratch.clear();
            scratch.extend_from_slice(arena.key_range(off, len));
            let node = arena.get_mut(id);
            node.left = NodeId::NONE;
            node.right = NodeId::NONE;
            node.height = 1;
            // `scratch` is a local buffer, disjoint from the arena, so this borrow is fine.
            let key = std::mem::take(scratch);
            Self::insert(arena, &mut root, id, hash, &key);
            *scratch = key;
        }
        root
    }
}

/// Orders a probe `(hash, key)` against a stored node.
#[inline]
fn cmp_probe(arena: &NodeArena, node: NodeId, hash: u64, key: &[u8]) -> Ordering {
    match hash.cmp(&arena.get(node).hash) {
        Ordering::Equal => key.cmp(arena.key(node)),
        other => other,
    }
}

// ---------------------------------------------------------------------------------------------
// Unbalanced BST
// ---------------------------------------------------------------------------------------------

/// A plain binary search tree with no rebalancing.
#[derive(Debug, Clone, Copy)]
pub struct Unbalanced;

impl BucketPolicy for Unbalanced {
    const NAME: &'static str = "unbalanced";

    fn lookup(arena: &mut NodeArena, root: &mut NodeId, hash: u64, key: &[u8]) -> NodeId {
        let mut cur = *root;
        while cur.is_some() {
            cur = match cmp_probe(arena, cur, hash, key) {
                Ordering::Less => arena.get(cur).left,
                Ordering::Greater => arena.get(cur).right,
                Ordering::Equal => return cur,
            };
        }
        NodeId::NONE
    }

    fn insert(arena: &mut NodeArena, root: &mut NodeId, node: NodeId, hash: u64, key: &[u8]) {
        if root.is_none() {
            *root = node;
            return;
        }
        let mut cur = *root;
        loop {
            let go_left = cmp_probe(arena, cur, hash, key) == Ordering::Less;
            let child = if go_left { arena.get(cur).left } else { arena.get(cur).right };
            if child.is_none() {
                if go_left {
                    arena.get_mut(cur).left = node;
                } else {
                    arena.get_mut(cur).right = node;
                }
                return;
            }
            cur = child;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// AVL
// ---------------------------------------------------------------------------------------------

/// A height-balanced binary search tree.
#[derive(Debug, Clone, Copy)]
pub struct Avl;

#[inline]
fn height(arena: &NodeArena, id: NodeId) -> i32 {
    if id.is_none() { 0 } else { i32::from(arena.get(id).height) }
}

fn refresh_height(arena: &mut NodeArena, id: NodeId) {
    let h = 1 + height(arena, arena.get(id).left).max(height(arena, arena.get(id).right));
    arena.get_mut(id).height = h as u8;
}

#[inline]
fn balance_factor(arena: &NodeArena, id: NodeId) -> i32 {
    height(arena, arena.get(id).left) - height(arena, arena.get(id).right)
}

/// Rotates right around `y`, returning the new subtree root.
fn rotate_right(arena: &mut NodeArena, y: NodeId) -> NodeId {
    let x = arena.get(y).left;
    let t2 = arena.get(x).right;
    arena.get_mut(x).right = y;
    arena.get_mut(y).left = t2;
    refresh_height(arena, y);
    refresh_height(arena, x);
    x
}

/// Rotates left around `x`, returning the new subtree root.
fn rotate_left(arena: &mut NodeArena, x: NodeId) -> NodeId {
    let y = arena.get(x).right;
    let t2 = arena.get(y).left;
    arena.get_mut(y).left = x;
    arena.get_mut(x).right = t2;
    refresh_height(arena, x);
    refresh_height(arena, y);
    y
}

fn avl_rebalance(arena: &mut NodeArena, id: NodeId) -> NodeId {
    refresh_height(arena, id);
    let bf = balance_factor(arena, id);
    if bf > 1 {
        let left = arena.get(id).left;
        if balance_factor(arena, left) < 0 {
            let rotated = rotate_left(arena, left);
            arena.get_mut(id).left = rotated;
        }
        rotate_right(arena, id)
    } else if bf < -1 {
        let right = arena.get(id).right;
        if balance_factor(arena, right) > 0 {
            let rotated = rotate_right(arena, right);
            arena.get_mut(id).right = rotated;
        }
        rotate_left(arena, id)
    } else {
        id
    }
}

fn avl_insert(
    arena: &mut NodeArena,
    root: NodeId,
    node: NodeId,
    hash: u64,
    key: &[u8],
) -> NodeId {
    if root.is_none() {
        return node;
    }
    if cmp_probe(arena, root, hash, key) == Ordering::Less {
        let left = arena.get(root).left;
        let new_left = avl_insert(arena, left, node, hash, key);
        arena.get_mut(root).left = new_left;
    } else {
        let right = arena.get(root).right;
        let new_right = avl_insert(arena, right, node, hash, key);
        arena.get_mut(root).right = new_right;
    }
    avl_rebalance(arena, root)
}

impl BucketPolicy for Avl {
    const NAME: &'static str = "avl";

    fn lookup(arena: &mut NodeArena, root: &mut NodeId, hash: u64, key: &[u8]) -> NodeId {
        // Lookup does not restructure, so it is the same descent as the unbalanced tree.
        Unbalanced::lookup(arena, root, hash, key)
    }

    fn insert(arena: &mut NodeArena, root: &mut NodeId, node: NodeId, hash: u64, key: &[u8]) {
        *root = avl_insert(arena, *root, node, hash, key);
    }
}

// ---------------------------------------------------------------------------------------------
// Splay
// ---------------------------------------------------------------------------------------------

/// A self-adjusting binary search tree.
///
/// Every access rotates the touched entry to the root, so the entries a search is currently
/// hammering drift to the top. Component-cache traffic is heavily skewed — a handful of
/// subproblems are revisited constantly — which is exactly the access pattern splaying is for.
#[derive(Debug, Clone, Copy)]
pub struct Splay;

/// Sleator and Tarjan's recursive splay: brings the entry matching `(hash, key)` to the root, or
/// the last node on its search path if it is absent.
fn splay(arena: &mut NodeArena, root: NodeId, hash: u64, key: &[u8]) -> NodeId {
    if root.is_none() {
        return root;
    }
    match cmp_probe(arena, root, hash, key) {
        Ordering::Equal => root,
        Ordering::Less => {
            let left = arena.get(root).left;
            if left.is_none() {
                return root;
            }
            let mut root = root;
            match cmp_probe(arena, left, hash, key) {
                Ordering::Less => {
                    // zig-zig
                    let grand = arena.get(left).left;
                    let splayed = splay(arena, grand, hash, key);
                    arena.get_mut(left).left = splayed;
                    root = rotate_right(arena, root);
                }
                Ordering::Greater => {
                    // zig-zag
                    let grand = arena.get(left).right;
                    let splayed = splay(arena, grand, hash, key);
                    arena.get_mut(left).right = splayed;
                    if splayed.is_some() {
                        let rotated = rotate_left(arena, left);
                        arena.get_mut(root).left = rotated;
                    }
                }
                Ordering::Equal => {}
            }
            if arena.get(root).left.is_none() { root } else { rotate_right(arena, root) }
        }
        Ordering::Greater => {
            let right = arena.get(root).right;
            if right.is_none() {
                return root;
            }
            let mut root = root;
            match cmp_probe(arena, right, hash, key) {
                Ordering::Greater => {
                    // zig-zig
                    let grand = arena.get(right).right;
                    let splayed = splay(arena, grand, hash, key);
                    arena.get_mut(right).right = splayed;
                    root = rotate_left(arena, root);
                }
                Ordering::Less => {
                    // zig-zag
                    let grand = arena.get(right).left;
                    let splayed = splay(arena, grand, hash, key);
                    arena.get_mut(right).left = splayed;
                    if splayed.is_some() {
                        let rotated = rotate_right(arena, right);
                        arena.get_mut(root).right = rotated;
                    }
                }
                Ordering::Equal => {}
            }
            if arena.get(root).right.is_none() { root } else { rotate_left(arena, root) }
        }
    }
}

impl BucketPolicy for Splay {
    const NAME: &'static str = "splay";

    fn lookup(arena: &mut NodeArena, root: &mut NodeId, hash: u64, key: &[u8]) -> NodeId {
        if root.is_none() {
            return NodeId::NONE;
        }
        *root = splay(arena, *root, hash, key);
        if cmp_probe(arena, *root, hash, key) == Ordering::Equal { *root } else { NodeId::NONE }
    }

    fn insert(arena: &mut NodeArena, root: &mut NodeId, node: NodeId, hash: u64, key: &[u8]) {
        if root.is_none() {
            *root = node;
            return;
        }
        let top = splay(arena, *root, hash, key);
        // Split the splayed tree at the new node and hang both halves off it.
        if cmp_probe(arena, top, hash, key) == Ordering::Greater {
            arena.get_mut(node).left = top;
            arena.get_mut(node).right = arena.get(top).right;
            arena.get_mut(top).right = NodeId::NONE;
        } else {
            arena.get_mut(node).right = top;
            arena.get_mut(node).left = arena.get(top).left;
            arena.get_mut(top).left = NodeId::NONE;
        }
        *root = node;
    }
}

// ---------------------------------------------------------------------------------------------
// Chaining (the control)
// ---------------------------------------------------------------------------------------------

/// Classic separate chaining: a singly linked list threaded through [`super::arena::Node::left`].
///
/// This is the structure the project is arguing against, so it is implemented as carefully as the
/// others. Insertion is `O(1)` at the head; lookup is a linear scan.
#[derive(Debug, Clone, Copy)]
pub struct Chain;

impl BucketPolicy for Chain {
    const NAME: &'static str = "chain";

    fn lookup(arena: &mut NodeArena, root: &mut NodeId, hash: u64, key: &[u8]) -> NodeId {
        let mut cur = *root;
        while cur.is_some() {
            // Compare the cheap hash first; only touch the key on a full 64-bit match.
            if arena.get(cur).hash == hash && arena.key(cur) == key {
                return cur;
            }
            cur = arena.get(cur).left;
        }
        NodeId::NONE
    }

    fn insert(arena: &mut NodeArena, root: &mut NodeId, node: NodeId, _hash: u64, _key: &[u8]) {
        arena.get_mut(node).left = *root;
        arena.get_mut(node).right = NodeId::NONE;
        *root = node;
    }
}

/// Runtime selection of a bucket policy, for the CLI and the benchmark harness.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum BucketKind {
    /// [`Avl`] — the default.
    #[default]
    Avl,
    /// [`Unbalanced`].
    Unbalanced,
    /// [`Splay`].
    Splay,
    /// [`Chain`].
    Chain,
}

impl BucketKind {
    /// Every policy, in a stable order for benchmark tables.
    pub const ALL: [Self; 4] = [Self::Avl, Self::Unbalanced, Self::Splay, Self::Chain];

    /// The policy's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Avl => Avl::NAME,
            Self::Unbalanced => Unbalanced::NAME,
            Self::Splay => Splay::NAME,
            Self::Chain => Chain::NAME,
        }
    }
}

impl std::str::FromStr for BucketKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|k| k.name() == s)
            .ok_or_else(|| format!("unknown bucket policy `{s}`"))
    }
}

impl std::fmt::Display for BucketKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
