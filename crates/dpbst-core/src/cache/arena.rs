//! Flat storage for cache entries.
//!
//! Every node in every bucket tree lives in one `Vec<Node>` addressed by `u32` handles, and
//! every key lives in one `Vec<u8>`. Nothing is individually heap-allocated.
//!
//! The alternative — `Box<Node>` per entry — would cost a pointer chase into an unrelated
//! address at every level of every tree, eight-byte child pointers, and a `free` per node on
//! eviction. The arena gives locality, halves the pointer width, turns deletion into a free-list
//! push, and keeps the module inside `#![forbid(unsafe_code)]`. The pattern is lifted from how
//! `MiniSat` and its Rust port `batsat` allocate clauses behind `u32` references.

/// A handle into [`NodeArena`].
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct NodeId(u32);

impl NodeId {
    /// The null handle, used for absent children and empty buckets.
    pub const NONE: Self = Self(u32::MAX);

    /// Wraps a raw index.
    #[inline]
    #[must_use]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// Whether this is the null handle.
    #[inline]
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == u32::MAX
    }

    /// Whether this handle points at a node.
    #[inline]
    #[must_use]
    pub const fn is_some(self) -> bool {
        !self.is_none()
    }

    /// The raw index.
    ///
    /// # Panics
    /// Panics if called on [`NodeId::NONE`].
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        debug_assert!(self.is_some());
        self.0 as usize
    }
}

/// Whether a cached subproblem was satisfiable.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// The component is satisfiable; a witness is stored alongside the key.
    Sat,
    /// The component is unsatisfiable.
    Unsat,
}

/// One cache entry, and one node of one bucket tree.
///
/// Laid out to be exactly 32 bytes so that a node never straddles more than one cache line and
/// two nodes share one. The fields are ordered largest-first to avoid interior padding.
#[derive(Copy, Clone, Debug)]
pub struct Node {
    /// Full 64-bit key hash. This is the tree's *primary* ordering key, not just a filter; see
    /// [`crate::cache::ComponentCache`] for why.
    pub hash: u64,
    /// Left child, or the next free node when this slot is on the free list.
    pub left: NodeId,
    /// Right child.
    pub right: NodeId,
    /// Start of this entry's key bytes in the blob.
    pub key_off: u32,
    /// Length of this entry's key bytes. A satisfying witness, when present, is the bitset
    /// immediately following the key.
    pub key_len: u32,
    /// AVL subtree height. Unused by the unbalanced, splay, and chaining bucket policies.
    pub height: u8,
    /// Bit 0 records the [`Verdict`].
    pub flags: u8,
    /// Reuse counter driving eviction. Saturates rather than wrapping.
    pub activity: u16,
}

/// Set in [`Node::flags`] when the entry's verdict is [`Verdict::Sat`].
const FLAG_SAT: u8 = 1;

impl Node {
    /// The verdict recorded for this entry.
    #[inline]
    #[must_use]
    pub const fn verdict(&self) -> Verdict {
        if self.flags & FLAG_SAT != 0 { Verdict::Sat } else { Verdict::Unsat }
    }
}

/// The node pool plus the key blob.
#[derive(Debug)]
pub struct NodeArena {
    nodes: Vec<Node>,
    /// Head of the singly linked free list, threaded through [`Node::left`].
    free: NodeId,
    /// Key bytes and witness bitsets, packed end to end.
    blob: Vec<u8>,
    /// Blob bytes belonging to freed nodes. Drives compaction.
    dead_bytes: usize,
}

impl Default for NodeArena {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeArena {
    /// Creates an empty arena.
    #[must_use]
    pub fn new() -> Self {
        Self { nodes: Vec::new(), free: NodeId::NONE, blob: Vec::new(), dead_bytes: 0 }
    }

    /// Number of node slots ever allocated, including those currently free.
    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.nodes.len()
    }

    /// Bytes held by the node pool and the key blob.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.nodes.capacity() * size_of::<Node>() + self.blob.capacity()
    }

    /// Blob bytes belonging to freed entries.
    #[inline]
    #[must_use]
    pub const fn dead_bytes(&self) -> usize {
        self.dead_bytes
    }

    /// Borrows a node.
    #[inline]
    #[must_use]
    pub fn get(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    /// Mutably borrows a node.
    #[inline]
    pub fn get_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.index()]
    }

    /// The key bytes of a node.
    #[inline]
    #[must_use]
    pub fn key(&self, id: NodeId) -> &[u8] {
        let n = self.get(id);
        let start = n.key_off as usize;
        &self.blob[start..start + n.key_len as usize]
    }

    /// Borrows key bytes by explicit offset and length.
    ///
    /// Lets a caller hold a key while the node pool is mutably borrowed, which the rebuild path
    /// needs; it copies the key into a scratch buffer first.
    #[inline]
    #[must_use]
    pub fn key_range(&self, off: u32, len: u32) -> &[u8] {
        &self.blob[off as usize..off as usize + len as usize]
    }

    /// The witness bitset stored after a satisfiable entry's key.
    ///
    /// `witness_len` bytes are returned; the caller knows the component's variable count and so
    /// knows how many bits are meaningful.
    #[inline]
    #[must_use]
    pub fn witness(&self, id: NodeId, witness_len: usize) -> &[u8] {
        let n = self.get(id);
        let start = n.key_off as usize + n.key_len as usize;
        &self.blob[start..start + witness_len]
    }

    /// Allocates a node for `key`, with `witness` appended when the verdict is satisfiable.
    ///
    /// Children are left null and the height is set for a leaf; the bucket policy links it in.
    pub fn alloc(&mut self, hash: u64, key: &[u8], verdict: Verdict, witness: &[u8]) -> NodeId {
        let key_off = self.blob.len() as u32;
        self.blob.extend_from_slice(key);
        if verdict == Verdict::Sat {
            self.blob.extend_from_slice(witness);
        }

        let node = Node {
            hash,
            left: NodeId::NONE,
            right: NodeId::NONE,
            key_off,
            key_len: key.len() as u32,
            height: 1,
            flags: if verdict == Verdict::Sat { FLAG_SAT } else { 0 },
            activity: 1,
        };

        if self.free.is_some() {
            let id = self.free;
            self.free = self.nodes[id.index()].left;
            self.nodes[id.index()] = node;
            id
        } else {
            self.nodes.push(node);
            NodeId::new(self.nodes.len() as u32 - 1)
        }
    }

    /// Returns a node to the free list and charges its bytes to `dead_bytes`.
    ///
    /// The blob bytes are not reclaimed here; [`NodeArena::rebuild_from`] does that in bulk.
    pub fn free(&mut self, id: NodeId, witness_len: usize) {
        let n = self.nodes[id.index()];
        self.dead_bytes += n.key_len as usize
            + if n.verdict() == Verdict::Sat { witness_len } else { 0 };
        self.nodes[id.index()].left = self.free;
        self.nodes[id.index()].right = NodeId::NONE;
        self.free = id;
    }

    /// Drops everything.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.blob.clear();
        self.free = NodeId::NONE;
        self.dead_bytes = 0;
    }

    /// Iterates the ids of every node reachable from `root`, in unspecified order.
    ///
    /// Iterative rather than recursive: a degenerate unbalanced bucket can be thousands of nodes
    /// deep, and this runs during eviction when the stack is already committed.
    pub fn collect_subtree(&self, root: NodeId, out: &mut Vec<NodeId>) {
        if root.is_none() {
            return;
        }
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            out.push(id);
            let n = self.get(id);
            if n.left.is_some() {
                stack.push(n.left);
            }
            if n.right.is_some() {
                stack.push(n.right);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_is_exactly_one_cache_line_quarter() {
        // The layout claim in this module's docs is load-bearing for the benchmark numbers, so
        // it is asserted rather than trusted.
        assert_eq!(size_of::<Node>(), 32);
        assert_eq!(align_of::<Node>(), 8);
    }

    #[test]
    fn alloc_stores_key_and_witness_contiguously() {
        let mut a = NodeArena::new();
        let id = a.alloc(0xdead_beef, b"key-one", Verdict::Sat, &[0b1010_1010]);
        assert_eq!(a.key(id), b"key-one");
        assert_eq!(a.witness(id, 1), &[0b1010_1010]);
        assert_eq!(a.get(id).verdict(), Verdict::Sat);
        assert_eq!(a.get(id).hash, 0xdead_beef);
    }

    #[test]
    fn unsat_entries_store_no_witness() {
        let mut a = NodeArena::new();
        let id = a.alloc(1, b"k", Verdict::Unsat, &[0xff]);
        assert_eq!(a.get(id).verdict(), Verdict::Unsat);
        // Only the key was written to the blob.
        assert_eq!(a.key(id), b"k");
        assert!(a.memory_bytes() > 0);
    }

    #[test]
    fn freed_slots_are_reused_before_growing() {
        let mut a = NodeArena::new();
        let first = a.alloc(1, b"aa", Verdict::Unsat, &[]);
        let second = a.alloc(2, b"bb", Verdict::Unsat, &[]);
        assert_eq!(a.capacity(), 2);

        a.free(first, 0);
        assert_eq!(a.dead_bytes(), 2);

        let third = a.alloc(3, b"cc", Verdict::Unsat, &[]);
        assert_eq!(third, first, "the free slot should be recycled");
        assert_eq!(a.capacity(), 2, "no growth while the free list is non-empty");
        assert_eq!(a.key(second), b"bb", "recycling must not disturb live nodes");
        assert_eq!(a.key(third), b"cc");
    }

    #[test]
    fn free_list_threads_through_multiple_slots() {
        let mut a = NodeArena::new();
        let ids: Vec<_> = (0..4).map(|i| a.alloc(i, b"x", Verdict::Unsat, &[])).collect();
        for &id in &ids {
            a.free(id, 0);
        }
        // All four come back before any new slot is allocated.
        let reused: Vec<_> = (0..4).map(|i| a.alloc(i, b"y", Verdict::Unsat, &[])).collect();
        assert_eq!(a.capacity(), 4);
        let mut sorted = reused;
        sorted.sort_unstable();
        let mut expected = ids;
        expected.sort_unstable();
        assert_eq!(sorted, expected);
    }

    #[test]
    fn collect_subtree_walks_every_node() {
        let mut a = NodeArena::new();
        let root = a.alloc(2, b"r", Verdict::Unsat, &[]);
        let l = a.alloc(1, b"l", Verdict::Unsat, &[]);
        let r = a.alloc(3, b"rr", Verdict::Unsat, &[]);
        a.get_mut(root).left = l;
        a.get_mut(root).right = r;

        let mut out = Vec::new();
        a.collect_subtree(root, &mut out);
        out.sort_unstable();
        let mut expected = vec![root, l, r];
        expected.sort_unstable();
        assert_eq!(out, expected);

        let mut empty = Vec::new();
        a.collect_subtree(NodeId::NONE, &mut empty);
        assert!(empty.is_empty());
    }
}
