//! The memo: a hash table whose buckets are binary search trees.
//!
//! This is the structure the project exists to test. A search node hands it a *component key* —
//! a canonical encoding of an independent subproblem — and gets back a remembered verdict, or
//! nothing.
//!
//! # Shape
//!
//! ```text
//!   buckets: [NodeId; 2^k]        one BST root per slot, indexed by hash & (2^k - 1)
//!        |
//!        +--> tree of Node        ordered by the FULL 64-bit hash, key bytes as tie-break
//!                 |
//!                 +--> blob       key bytes, then the witness bitset for SAT entries
//! ```
//!
//! Nodes and key bytes both live in flat arenas; see [`arena`].
//!
//! # Why order by the hash rather than the key
//!
//! The bucket index consumes the low `k` bits of the hash, so within a bucket those bits are
//! constant and the other `64 - k` are uniformly random. Ordering the tree by the whole hash
//! therefore makes a comparison a single `u64` compare — the variable-length key is only touched
//! when two entries collide on all 64 bits — and makes insertion order random, which bounds the
//! expected depth of even an unbalanced bucket at `O(log k)`.
//!
//! Correctness never depends on the hash being good, only speed: a full 64-bit collision falls
//! through to a byte-wise key comparison, so distinct subproblems are never confused.
//!
//! # Bounded memory
//!
//! An unbounded component cache will eat all available memory on any interesting instance. The
//! table has a byte budget; on overflow it halves every entry's activity counter and drops the
//! entries that reach zero, then rebuilds. This is the cleanup strategy `sharpSAT` uses, and it
//! degrades smoothly instead of falling off a cliff.

pub mod arena;
pub mod tree;

use crate::varint;
use arena::{NodeArena, NodeId, Verdict};
use std::marker::PhantomData;
pub use tree::{Avl, BucketKind, BucketPolicy, Chain, Splay, Unbalanced};

/// Hashes a component key to 64 bits.
///
/// A multiply-xor-rotate absorb over eight-byte chunks, finished with the `SplitMix64` avalanche.
/// The finaliser is what matters here: the low bits pick the bucket and the high bits order the
/// tree, so every bit of the output has to be well mixed.
#[must_use]
pub fn hash_key(bytes: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = 0xcbf2_9ce4_8422_2325_u64 ^ (bytes.len() as u64).wrapping_mul(K);

    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().expect("chunks_exact yields 8 bytes"));
        h = (h ^ word).wrapping_mul(K).rotate_left(31);
    }
    let tail = chunks.remainder();
    if !tail.is_empty() {
        let mut buf = [0_u8; 8];
        buf[..tail.len()].copy_from_slice(tail);
        h = (h ^ u64::from_le_bytes(buf))
            .wrapping_mul(K)
            .rotate_left(31);
    }

    // SplitMix64 finaliser.
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// Counters describing how the memo behaved over a solve.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups attempted.
    pub lookups: u64,
    /// Lookups that found a remembered verdict.
    pub hits: u64,
    /// Entries added.
    pub inserts: u64,
    /// Entries dropped by eviction sweeps.
    pub evicted: u64,
    /// Eviction sweeps performed.
    pub sweeps: u64,
    /// Times the bucket array was grown.
    pub resizes: u64,
}

impl CacheStats {
    /// Fraction of lookups that hit, in `0.0..=1.0`.
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        if self.lookups == 0 {
            0.0
        } else {
            self.hits as f64 / self.lookups as f64
        }
    }
}

/// Structural measurements of the table, computed by walking it.
///
/// Gathered on demand rather than maintained incrementally: the point is to compare bucket
/// policies, and instrumenting the hot path would distort the very thing being measured.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TableShape {
    /// Number of bucket slots.
    pub buckets: usize,
    /// Slots holding at least one entry.
    pub occupied: usize,
    /// Entries stored.
    pub entries: usize,
    /// Entries in the largest bucket.
    pub max_bucket: usize,
    /// Depth of the deepest bucket, in nodes.
    pub max_depth: usize,
    /// Mean depth of an entry, i.e. the expected comparisons for a successful lookup.
    pub mean_depth: f64,
}

/// Default bucket occupancy the table aims for before growing.
///
/// Chaining conventionally targets a load factor near one because a bucket scan is linear. A
/// tree bucket costs `O(log k)`, so it can carry more before it hurts — which is precisely the
/// trade this project is measuring, so the value is a knob rather than a constant.
pub const DEFAULT_TARGET_LOAD: usize = 4;

/// Default memory budget for the memo.
pub const DEFAULT_BUDGET_BYTES: usize = 512 * 1024 * 1024;

/// A component memo parameterised by its bucket policy.
#[derive(Debug)]
pub struct ComponentCache<P: BucketPolicy> {
    buckets: Vec<NodeId>,
    mask: u64,
    arena: NodeArena,
    len: usize,
    budget_bytes: usize,
    target_load: usize,
    /// Reused across rebuilds so a sweep does not allocate per entry.
    scratch: Vec<u8>,
    stats: CacheStats,
    policy: PhantomData<fn() -> P>,
}

impl<P: BucketPolicy> ComponentCache<P> {
    /// Creates a memo with the given byte budget and target bucket occupancy.
    ///
    /// # Panics
    /// Panics if `target_load` is zero.
    #[must_use]
    pub fn new(budget_bytes: usize, target_load: usize) -> Self {
        assert!(target_load > 0, "target load factor must be positive");
        let initial_buckets = 1024;
        Self {
            buckets: vec![NodeId::NONE; initial_buckets],
            mask: initial_buckets as u64 - 1,
            arena: NodeArena::new(),
            len: 0,
            budget_bytes,
            target_load,
            scratch: Vec::new(),
            stats: CacheStats::default(),
            policy: PhantomData,
        }
    }

    /// The name of the bucket policy in use.
    #[must_use]
    pub const fn policy_name(&self) -> &'static str {
        P::NAME
    }

    /// Entries currently stored.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the memo is empty.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Counters for this memo.
    #[inline]
    #[must_use]
    pub const fn stats(&self) -> CacheStats {
        self.stats
    }

    /// Approximate resident size, in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.arena.memory_bytes() + self.buckets.capacity() * size_of::<NodeId>()
    }

    /// Bytes attributable to entries that are still live.
    ///
    /// The memory budget is enforced against this rather than [`Self::memory_bytes`]: `Vec`
    /// capacity never shrinks, so a budget checked against resident size could not be satisfied
    /// once exceeded, and every later insert would trigger another full sweep.
    #[must_use]
    pub fn live_bytes(&self) -> usize {
        self.arena.live_bytes(self.len) + self.buckets.len() * size_of::<NodeId>()
    }

    /// Looks up a component key.
    ///
    /// On a hit the entry's activity counter is bumped, which is what keeps hot subproblems alive
    /// across eviction sweeps. Read the result with [`Self::verdict`] and [`Self::witness`].
    pub fn lookup(&mut self, hash: u64, key: &[u8]) -> Option<NodeId> {
        self.stats.lookups += 1;
        let slot = (hash & self.mask) as usize;
        let found = P::lookup(&mut self.arena, &mut self.buckets[slot], hash, key);
        if found.is_none() {
            return None;
        }
        self.stats.hits += 1;
        let node = self.arena.get_mut(found);
        node.activity = node.activity.saturating_add(1);
        Some(found)
    }

    /// The verdict recorded at `id`.
    #[inline]
    #[must_use]
    pub fn verdict(&self, id: NodeId) -> Verdict {
        self.arena.get(id).verdict()
    }

    /// The witness bitset recorded at `id`, for satisfiable entries.
    ///
    /// Bit `i` is the value of the component's `i`th variable, in the same order the key encodes
    /// them.
    #[inline]
    #[must_use]
    pub fn witness(&self, id: NodeId) -> &[u8] {
        let key = self.arena.key(id);
        self.arena.witness(id, witness_len_for(key))
    }

    /// Records a verdict for a component key.
    ///
    /// `witness` is ignored for [`Verdict::Unsat`]; for [`Verdict::Sat`] it must hold at least
    /// `ceil(component_vars / 8)` bytes.
    ///
    /// The caller is responsible for not inserting a key that is already present; the search only
    /// inserts after a miss.
    pub fn insert(&mut self, hash: u64, key: &[u8], verdict: Verdict, witness: &[u8]) {
        if self.len >= self.buckets.len().saturating_mul(self.target_load) {
            self.resize();
        }
        if self.live_bytes() >= self.budget_bytes {
            self.sweep();
        }

        let node = self.arena.alloc(hash, key, verdict, witness);
        let slot = (hash & self.mask) as usize;
        P::insert(&mut self.arena, &mut self.buckets[slot], node, hash, key);
        self.len += 1;
        self.stats.inserts += 1;
    }

    /// Doubles the bucket array and redistributes every entry.
    fn resize(&mut self) {
        let new_len = self.buckets.len() * 2;
        let ids = self.collect_entries();
        self.buckets.clear();
        self.buckets.resize(new_len, NodeId::NONE);
        self.mask = new_len as u64 - 1;
        self.rebuild_from(&ids);
        self.stats.resizes += 1;
    }

    /// Halves every activity counter and drops the entries that reach zero.
    ///
    /// Entries reused since the last sweep survive; entries inserted once and never revisited do
    /// not. Dropping *all* entries would be simpler but throws away the hot working set along
    /// with the cold tail.
    fn sweep(&mut self) {
        let ids = self.collect_entries();
        let mut survivors = Vec::with_capacity(ids.len());
        let mut freed = 0_usize;
        for id in ids {
            let node = self.arena.get_mut(id);
            node.activity /= 2;
            if node.activity == 0 {
                let witness_len = witness_len_for(self.arena.key(id));
                self.arena.free(id, witness_len);
                self.len -= 1;
                self.stats.evicted += 1;
                freed += 1;
            } else {
                survivors.push(id);
            }
        }

        // Reclaim the key bytes of everything just dropped; `free` can only mark them dead.
        self.arena.compact_blob(&survivors, |key, verdict| {
            key.len()
                + if verdict == Verdict::Sat {
                    witness_len_for(key)
                } else {
                    0
                }
        });

        // If every entry survived, the table is entirely hot and sweeping again would achieve
        // nothing but another full walk on the next insert. Raising the budget keeps the solver
        // running rather than thrashing. Note this triggers only when *nothing* was freed:
        // comparing survivor count against `self.len` would always match, because `self.len` is
        // decremented as entries are dropped.
        if freed == 0 && self.len > 0 {
            self.budget_bytes = self.budget_bytes.saturating_mul(2);
        }

        self.buckets.fill(NodeId::NONE);
        self.rebuild_from(&survivors);
        self.stats.sweeps += 1;
    }

    /// Walks every bucket and returns the ids of all live entries.
    fn collect_entries(&self) -> Vec<NodeId> {
        let mut ids = Vec::with_capacity(self.len);
        for &root in &self.buckets {
            self.arena.collect_subtree(root, &mut ids);
        }
        ids
    }

    /// Re-links the given entries into the (already cleared) bucket array.
    fn rebuild_from(&mut self, ids: &[NodeId]) {
        let mut per_bucket: Vec<Vec<NodeId>> = vec![Vec::new(); self.buckets.len()];
        for &id in ids {
            let slot = (self.arena.get(id).hash & self.mask) as usize;
            per_bucket[slot].push(id);
        }
        let mut scratch = std::mem::take(&mut self.scratch);
        for (slot, members) in per_bucket.into_iter().enumerate() {
            if !members.is_empty() {
                self.buckets[slot] = P::rebuild(&mut self.arena, &members, &mut scratch);
            }
        }
        self.scratch = scratch;
    }

    /// Drops every entry.
    pub fn clear(&mut self) {
        self.arena.clear();
        self.buckets.fill(NodeId::NONE);
        self.len = 0;
    }

    /// Measures bucket occupancy and depth by walking the whole table.
    ///
    /// `mean_depth` is the headline number when comparing policies: it is the expected number of
    /// comparisons a successful lookup performs.
    #[must_use]
    pub fn shape(&self) -> TableShape {
        let mut shape = TableShape {
            buckets: self.buckets.len(),
            entries: self.len,
            ..TableShape::default()
        };
        let mut depth_total = 0_u64;

        for &root in &self.buckets {
            if root.is_none() {
                continue;
            }
            shape.occupied += 1;
            let mut count = 0_usize;
            let mut deepest = 0_usize;
            // Depth-first walk carrying each node's depth; `Chain` threads its list through the
            // left child, so the same walk measures a chain's length.
            let mut stack = vec![(root, 1_usize)];
            while let Some((id, depth)) = stack.pop() {
                count += 1;
                deepest = deepest.max(depth);
                depth_total += depth as u64;
                let node = self.arena.get(id);
                if node.left.is_some() {
                    stack.push((node.left, depth + 1));
                }
                if node.right.is_some() {
                    stack.push((node.right, depth + 1));
                }
            }
            shape.max_bucket = shape.max_bucket.max(count);
            shape.max_depth = shape.max_depth.max(deepest);
        }

        shape.mean_depth = if self.len == 0 {
            0.0
        } else {
            depth_total as f64 / self.len as f64
        };
        shape
    }
}

/// Bytes of witness stored for a component key.
///
/// The key begins with the component's variable count, so an entry describes the size of its own
/// witness. That keeps [`arena::Node`] at 32 bytes: no separate length field is needed.
#[must_use]
fn witness_len_for(key: &[u8]) -> usize {
    let (num_vars, _) = varint::read_u32(key).unwrap_or((0, 0));
    num_vars.div_ceil(8) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a key in the format the cache expects: variable count first.
    fn key_for(num_vars: u32, tag: u32) -> Vec<u8> {
        let mut k = Vec::new();
        varint::write_u32(&mut k, num_vars);
        varint::write_u32(&mut k, tag);
        k
    }

    fn cache<P: BucketPolicy>() -> ComponentCache<P> {
        ComponentCache::<P>::new(DEFAULT_BUDGET_BYTES, DEFAULT_TARGET_LOAD)
    }

    #[test]
    fn hash_avalanches() {
        // Inputs one bit apart must not land in the same bucket or adjacent tree positions.
        let a = hash_key(&[1, 0, 0, 0]);
        let b = hash_key(&[0, 0, 0, 0]);
        assert_ne!(a, b);
        let differing_bits = (a ^ b).count_ones();
        assert!(
            (16..=48).contains(&differing_bits),
            "poor avalanche: {differing_bits} bits"
        );
    }

    #[test]
    fn hash_is_deterministic_and_length_sensitive() {
        assert_eq!(hash_key(b"abc"), hash_key(b"abc"));
        assert_ne!(hash_key(b"abc"), hash_key(b"abc\0"));
        assert_ne!(hash_key(b""), hash_key(b"\0"));
    }

    fn round_trip_for<P: BucketPolicy>() {
        let mut c = cache::<P>();
        assert!(c.is_empty());

        let k1 = key_for(8, 1);
        let k2 = key_for(8, 2);
        assert!(
            c.lookup(hash_key(&k1), &k1).is_none(),
            "{}: empty table",
            P::NAME
        );

        c.insert(hash_key(&k1), &k1, Verdict::Sat, &[0b0000_1101]);
        c.insert(hash_key(&k2), &k2, Verdict::Unsat, &[]);
        assert_eq!(c.len(), 2);

        let hit = c.lookup(hash_key(&k1), &k1).expect("k1 present");
        assert_eq!(c.verdict(hit), Verdict::Sat, "{}", P::NAME);
        assert_eq!(c.witness(hit), &[0b0000_1101], "{}", P::NAME);

        let hit2 = c.lookup(hash_key(&k2), &k2).expect("k2 present");
        assert_eq!(c.verdict(hit2), Verdict::Unsat, "{}", P::NAME);

        let absent = key_for(8, 99);
        assert!(
            c.lookup(hash_key(&absent), &absent).is_none(),
            "{}",
            P::NAME
        );
    }

    #[test]
    fn every_policy_round_trips() {
        round_trip_for::<Avl>();
        round_trip_for::<Unbalanced>();
        round_trip_for::<Splay>();
        round_trip_for::<Chain>();
    }

    fn bulk_for<P: BucketPolicy>(n: u32) {
        let mut c = cache::<P>();
        for i in 0..n {
            let k = key_for(16, i);
            c.insert(
                hash_key(&k),
                &k,
                if i % 3 == 0 {
                    Verdict::Sat
                } else {
                    Verdict::Unsat
                },
                &[i as u8, 0],
            );
        }
        assert_eq!(c.len(), n as usize, "{}", P::NAME);

        for i in 0..n {
            let k = key_for(16, i);
            let hit = c
                .lookup(hash_key(&k), &k)
                .unwrap_or_else(|| panic!("{}: missing {i}", P::NAME));
            let expected = if i % 3 == 0 {
                Verdict::Sat
            } else {
                Verdict::Unsat
            };
            assert_eq!(
                c.verdict(hit),
                expected,
                "{}: wrong verdict for {i}",
                P::NAME
            );
            if expected == Verdict::Sat {
                assert_eq!(
                    c.witness(hit),
                    &[i as u8, 0],
                    "{}: wrong witness for {i}",
                    P::NAME
                );
            }
        }
        for i in n..n * 2 {
            let k = key_for(16, i);
            assert!(
                c.lookup(hash_key(&k), &k).is_none(),
                "{}: phantom hit {i}",
                P::NAME
            );
        }
    }

    #[test]
    fn every_policy_survives_bulk_traffic_and_resizes() {
        // 20_000 entries against 1024 initial buckets forces several resizes.
        bulk_for::<Avl>(20_000);
        bulk_for::<Unbalanced>(20_000);
        bulk_for::<Splay>(20_000);
        bulk_for::<Chain>(20_000);
    }

    #[test]
    fn avl_keeps_buckets_logarithmic() {
        let mut c = cache::<Avl>();
        for i in 0..50_000 {
            let k = key_for(8, i);
            c.insert(hash_key(&k), &k, Verdict::Unsat, &[]);
        }
        let shape = c.shape();
        assert!(shape.entries > 0);
        // AVL height is at most 1.44 * log2(n + 2); for a bucket of ~4 entries that is tiny.
        let bound = 1.44 * ((shape.max_bucket + 2) as f64).log2() + 1.0;
        assert!(
            shape.max_depth as f64 <= bound,
            "AVL depth {} exceeds the height bound {bound:.2} for {} entries",
            shape.max_depth,
            shape.max_bucket
        );
    }

    #[test]
    fn tree_policies_are_shallower_than_chaining_at_high_load() {
        // Force long buckets by refusing to grow the table, which is the regime where the whole
        // premise of the project is supposed to pay off.
        let entries = 20_000_u32;
        let mut avl = ComponentCache::<Avl>::new(DEFAULT_BUDGET_BYTES, usize::MAX);
        let mut chain = ComponentCache::<Chain>::new(DEFAULT_BUDGET_BYTES, usize::MAX);
        for i in 0..entries {
            let k = key_for(8, i);
            let h = hash_key(&k);
            avl.insert(h, &k, Verdict::Unsat, &[]);
            chain.insert(h, &k, Verdict::Unsat, &[]);
        }
        let (a, c) = (avl.shape(), chain.shape());
        assert_eq!(a.buckets, 1024);
        assert_eq!(c.buckets, 1024);
        assert!(
            a.mean_depth * 2.0 < c.mean_depth,
            "expected AVL to be far shallower: avl {:.2} vs chain {:.2}",
            a.mean_depth,
            c.mean_depth
        );
    }

    #[test]
    fn eviction_keeps_reused_entries_and_drops_cold_ones() {
        // A budget small enough that inserts trigger sweeps almost immediately.
        let mut c = ComponentCache::<Avl>::new(64 * 1024, DEFAULT_TARGET_LOAD);
        let hot = key_for(8, 0);
        c.insert(hash_key(&hot), &hot, Verdict::Sat, &[1]);

        for i in 1..5_000 {
            let k = key_for(8, i);
            c.insert(hash_key(&k), &k, Verdict::Unsat, &[]);
            // Keep touching the hot key so its activity counter stays ahead of the sweeps.
            c.lookup(hash_key(&hot), &hot);
        }

        assert!(
            c.stats().sweeps > 0,
            "the budget should have forced at least one sweep"
        );
        assert!(
            c.stats().evicted > 0,
            "sweeps should have dropped cold entries"
        );
        assert!(
            c.lookup(hash_key(&hot), &hot).is_some(),
            "the repeatedly reused entry should have survived"
        );
        assert!(
            c.len() < 5_000,
            "the table should be smaller than the number of inserts"
        );
    }

    /// The memory budget must actually bound the table.
    ///
    /// Regression test. The eviction sweep used to compare `survivors.len()` against `self.len`
    /// to detect "freed nothing", but `self.len` is decremented as entries are dropped, so the
    /// comparison always matched and the budget doubled on *every* sweep. The table then grew
    /// without limit and `--cache-mb` did nothing.
    #[test]
    fn the_memory_budget_is_respected_under_sustained_pressure() {
        const BUDGET: usize = 256 * 1024;
        const INSERTS: u32 = 400_000;
        let mut c = ComponentCache::<Avl>::new(BUDGET, DEFAULT_TARGET_LOAD);

        for i in 0..INSERTS {
            let k = key_for(64, i);
            c.insert(hash_key(&k), &k, Verdict::Unsat, &[]);
            // Warm one entry in ten. A sweep must then find both survivors *and* victims, which
            // is the state the old guard mis-detected as "nothing could be freed".
            if i % 10 == 0 {
                c.lookup(hash_key(&k), &k);
            }
        }

        assert!(
            c.stats().sweeps > 0,
            "sustained inserts should have forced sweeps"
        );
        assert!(
            c.stats().evicted > 0,
            "sweeps should have dropped the cold entries"
        );
        assert!(
            c.live_bytes() <= BUDGET * 4,
            "live bytes {} escaped the {BUDGET}-byte budget after {} sweeps \
             ({} entries retained of {INSERTS})",
            c.live_bytes(),
            c.stats().sweeps,
            c.len()
        );
    }

    /// Freed key bytes have to be reclaimed, not merely marked dead.
    #[test]
    fn sweeping_reclaims_the_key_blob() {
        let mut c = ComponentCache::<Avl>::new(128 * 1024, DEFAULT_TARGET_LOAD);
        for i in 0..80_000_u32 {
            let k = key_for(32, i);
            c.insert(hash_key(&k), &k, Verdict::Unsat, &[]);
        }
        assert!(c.stats().sweeps > 0);

        // Every surviving entry must still be readable after its bytes were moved.
        let mut readable = 0;
        for i in 0..80_000_u32 {
            let k = key_for(32, i);
            if let Some(node) = c.lookup(hash_key(&k), &k) {
                assert_eq!(c.verdict(node), Verdict::Unsat);
                readable += 1;
            }
        }
        assert_eq!(
            readable,
            c.len(),
            "every live entry must survive compaction intact"
        );
    }

    /// Compaction moves witness bytes too, so a satisfiable entry must keep its witness.
    #[test]
    fn compaction_preserves_witnesses() {
        let mut c = ComponentCache::<Avl>::new(128 * 1024, DEFAULT_TARGET_LOAD);
        let witness_of = |i: u32| [i as u8, (i >> 8) as u8];

        for i in 0..60_000_u32 {
            let k = key_for(16, i);
            c.insert(hash_key(&k), &k, Verdict::Sat, &witness_of(i));
            // Keep the early entries hot so some of them survive the sweeps.
            if i % 500 == 0 {
                let hot = key_for(16, 0);
                c.lookup(hash_key(&hot), &hot);
            }
        }
        assert!(c.stats().sweeps > 0);

        let mut checked = 0;
        for i in 0..60_000_u32 {
            let k = key_for(16, i);
            if let Some(node) = c.lookup(hash_key(&k), &k) {
                assert_eq!(c.verdict(node), Verdict::Sat);
                assert_eq!(
                    c.witness(node),
                    witness_of(i),
                    "witness corrupted for entry {i}"
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "expected some entries to survive");
    }

    #[test]
    fn distinct_keys_sharing_a_hash_are_not_confused() {
        // Correctness must not rest on the hash. Force two different keys into the same tree
        // position by handing the cache a hash it did not compute.
        let mut c = cache::<Avl>();
        let k1 = key_for(8, 1);
        let k2 = key_for(8, 2);
        c.insert(7, &k1, Verdict::Sat, &[0xaa]);
        c.insert(7, &k2, Verdict::Unsat, &[]);

        let a = c.lookup(7, &k1).expect("k1");
        let b = c.lookup(7, &k2).expect("k2");
        assert_ne!(a, b);
        assert_eq!(c.verdict(a), Verdict::Sat);
        assert_eq!(c.verdict(b), Verdict::Unsat);
    }

    #[test]
    fn stats_track_hits_and_misses() {
        let mut c = cache::<Avl>();
        let k = key_for(8, 1);
        c.lookup(hash_key(&k), &k);
        c.insert(hash_key(&k), &k, Verdict::Unsat, &[]);
        c.lookup(hash_key(&k), &k);
        c.lookup(hash_key(&k), &k);

        let s = c.stats();
        assert_eq!(s.lookups, 3);
        assert_eq!(s.hits, 2);
        assert_eq!(s.inserts, 1);
        assert!((s.hit_rate() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn clearing_empties_the_table() {
        let mut c = cache::<Avl>();
        let k = key_for(8, 1);
        c.insert(hash_key(&k), &k, Verdict::Unsat, &[]);
        c.clear();
        assert!(c.is_empty());
        assert!(c.lookup(hash_key(&k), &k).is_none());
    }

    #[test]
    fn witness_length_is_recovered_from_the_key() {
        assert_eq!(witness_len_for(&key_for(0, 0)), 0);
        assert_eq!(witness_len_for(&key_for(1, 0)), 1);
        assert_eq!(witness_len_for(&key_for(8, 0)), 1);
        assert_eq!(witness_len_for(&key_for(9, 0)), 2);
        assert_eq!(witness_len_for(&key_for(1000, 0)), 125);
    }
}
