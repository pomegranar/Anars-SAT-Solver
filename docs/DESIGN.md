# DP-BST: design document

Status: living document. Written before implementation, updated as reality intervenes.

---

## 1. What we are building

A SAT solver, `dpbst`, that combines three ideas:

1. **DPLL search** — the backtracking core every practical solver descends from.
2. **Dynamic programming (memoisation)** — the residual formula at a search node is
   decomposed into independent *connected components*, each solved once and its verdict
   remembered. Distinct search paths that arrive at the same subproblem pay for it once.
3. **A hash table whose buckets are binary search trees** — the store behind that memo.

Point 3 is the project's premise: replace the usual "hash table + linked-list chaining"
with "hash table + BST per bucket", and measure whether it actually pays.

The name carries a deliberate double meaning for **DP**:

* **Davis–Putnam (1960)** — variable elimination by resolution. Implemented as a standalone
  mode (`--algorithm dp`) and, in bounded form, as a preprocessor (this is what modern
  solvers actually kept from DP: *bounded variable elimination*).
* **Dynamic programming** — the component memo described above.

Both are real and both ship.

## 2. Why component caching is the right form of "DP for SAT"

The naive idea — "memoise on the partial assignment" — is worthless: there are 3^n partial
assignments and each is visited at most once by DPLL, so the hit rate is zero.

The idea that works (Bayardo & Pehoushek 2000; Sang et al., *Cachet*; Thurley, *sharpSAT*)
is:

> After assigning some variables, the residual formula often splits into groups of clauses
> sharing no variables. Those groups are independent subproblems. Solve each once, cache the
> answer, and reuse it whenever the same subproblem reappears anywhere else in the tree.

Two different decision orders frequently produce the *same* residual component, which is
where the hits come from.

### The cache key

For a component we store the pair

```
key = ( sorted set of active clause indices , sorted set of unassigned variable indices )
```

**Claim.** This pair uniquely determines the residual formula.

*Proof sketch.* A clause is "active" iff no literal in it is currently true. So every
assigned variable occurring in an active clause is assigned *falsely* there. Hence the
residual of active clause `C` is exactly `{ l in C : var(l) in activeVars }` — recoverable
from the key alone. Conversely every variable in `activeVars` is unassigned. ∎

This is the standard sharpSAT/Cachet argument, and it is why the key can be two integer
sets rather than a canonical formula encoding.

### Why this is a SAT cache, not a #SAT cache

Because we only need *satisfiability*, we may additionally apply **pure literal elimination**
inside a component (sound for SAT, unsound for model counting). The key is always computed
at component-discovery time, before any such simplification, so the key ↔ residual mapping
above is preserved.

## 3. The data structure (the point of the project)

```
buckets: Vec<NodeId>            // 2^k bucket roots, each a BST root
nodes:   Vec<Node>              // one flat arena, u32 indices, free-list for deletions
keys:    Vec<u8>                // packed varint key bytes, (offset,len) per node
```

```rust
struct Node {
    left: NodeId, right: NodeId,   // u32, NodeId::NONE sentinel
    hash: u64,                     // FULL 64-bit hash - the primary sort key
    key_off: u32, key_len: u32,    // slice into `keys`
    value: Verdict,                // Sat(model slice) | Unsat
    aux: u32,                      // AVL height / activity score
}
```

### Three design decisions worth defending

**(a) Order the tree by the full 64-bit hash, with raw key bytes only as a tie-break.**

The bucket index uses the *low* k bits of the hash. Every key in a bucket therefore agrees
on those bits, and the remaining 64−k bits are effectively uniform random. Consequences:

* Comparison is a single `u64` compare in ~all cases. Byte-wise key comparison happens only
  on a genuine 64-bit hash collision.
* Insertion order into each BST is *random*, so even the deliberately unbalanced BST has
  expected depth `O(log n)` — which is exactly what makes the AVL-vs-unbalanced comparison
  interesting rather than a foregone conclusion.

**(b) Arena allocation with `u32` indices, not `Box<Node>`.**

`Box` per node would mean a pointer chase into a random malloc'd address per level, 8-byte
pointers, and no cheap way to bulk-free on eviction. A flat `Vec<Node>` gives cache locality,
halves pointer size, makes eviction a free-list push, and keeps the whole thing
`#![forbid(unsafe_code)]`. Borrowed from how `batsat`/MiniSat allocate clauses (`u32`
`ClauseRef` into one arena) — see §7.

**(c) Bounded memory with an activity-decay sweep.**

An unbounded component cache will exhaust RAM on anything interesting. Budget is set in MiB.
On overflow: halve every entry's activity score, delete entries that reach zero, rebuild the
affected trees. This is sharpSAT's cache-cleanup strategy and it degrades gracefully.

### Bucket implementations (all behind one trait)

| impl          | why it exists                                                |
|---------------|--------------------------------------------------------------|
| `avl`         | the headline: strict `O(log n)` height                        |
| `unbalanced`  | tests decision (a) — is randomised insertion order enough?    |
| `splay`       | self-adjusting; cache workloads are famously non-uniform      |
| `chain`       | **the control**: classic linked-list chaining                 |

`std::collections::HashMap` is measured too, as an external control, in the criterion bench.

If `chain` wins, that is a finding and the README will say so.

## 4. Solver architecture

```
DIMACS bytes
   -> parse            (hand-rolled byte scanner; handles SATLIB's '%' terminator)
   -> normalise        (drop tautologies, dedup literals, detect empty clause)
   -> preprocess       (unit propagation, bounded variable elimination = bounded DP,
                        bounded subsumption)
   -> solve            (DPLL + component decomposition + memo)
   -> verify model     (always, in debug/tests; on --verify in release)
   -> emit             (DIMACS: 's SATISFIABLE' / 'v ... 0', exit 10/20/0)
```

### Propagation: occurrence lists with counters, not watched literals

Each clause keeps `sat_count` (true literals) and `unassigned_count`. Assigning a literal
walks its occurrence lists and updates both. A clause is

* **satisfied** iff `sat_count > 0`
* **active** iff `sat_count == 0`
* **unit** iff `sat_count == 0 && unassigned_count == 1`
* **conflicting** iff `sat_count == 0 && unassigned_count == 0`

Two-watched-literals is strictly faster for propagation and is what every competitive solver
uses. We deliberately do not use it, because component analysis needs to enumerate the
*active* clause set on every node, and watched literals do not tell you which clauses are
satisfied. Counters make both propagation and component analysis fall out of the same
bookkeeping, and undo is symmetric and exact. This is a real, quantifiable cost, and the
README will own it rather than hide it.

### Search skeleton

```
solve_component(C):
    if let Some(v) = cache.get(key(C)):  return v          # the DP hit
    apply pure literals within C; propagate
    v = pick_decision_var(C)
    for polarity in [preferred, !preferred]:
        push level; assign(v, polarity)
        if propagate() != CONFLICT:
            subs = connected_components(C)                  # the DP split
            if all(solve_component(s) == SAT for s in subs):
                cache.insert(key(C), Sat(model)); pop; return SAT
        pop level
    cache.insert(key(C), Unsat); return UNSAT
```

Recursion depth is `O(#vars)`; the solve runs on a spawned thread with a 256 MiB stack so
that deep instances cannot blow the 8 MiB main-thread stack.

### Decision heuristics (selectable)

Component analysis already scans every active clause, so literal statistics are nearly free.

* `jw` — Jeroslow–Wang, `sum over active clauses 2^-|C|` (default)
* `dlis` / `dlcs` — occurrence counting
* `mom` — maximum occurrences in minimum-size clauses
* `static` — lowest index; for reproducible debugging

No VSIDS: VSIDS is driven by conflict analysis, and we do not learn clauses. Noted as
future work (VSADS, Sang et al., is the right target for a caching solver).

## 5. What we are *not* doing, and why

* **No clause learning (CDCL).** It is the single biggest reason kissat will beat us, and we
  say so up front. Learned clauses also interact badly with component caching — a learned
  clause spanning two components merges them, destroying the decomposition. Real caching
  solvers handle this; we do not.
* **No DRAT proof output.** Without clause learning there is no natural resolution proof to
  emit. UNSAT answers are instead cross-validated against CaDiCaL/Kissat in the test harness.
* **No unsafe code.** `splr` gates bounds-check elision behind an `unsafe_access` feature; we
  keep `#![forbid(unsafe_code)]` and eat the bounds checks. Measured, then reported.

## 6. Testing strategy

1. **Differential vs brute force** — random CNFs with n ≤ 12 solved by exhaustive truth
   table and by every solver configuration; verdicts must agree.
2. **Configuration equivalence** — cache on/off, all four bucket impls, all heuristics, and
   both algorithms must return identical verdicts on the same instance.
3. **Model verification** — every SAT verdict is checked against the *original* parsed
   formula, not the preprocessed one.
4. **Cross-validation vs CaDiCaL/Kissat/MiniSat/varisat/splr** on the SATLIB suites.
5. **Structure invariants** — AVL height bound, BST ordering, arena free-list integrity,
   eviction leaving no dangling `NodeId`.
6. **Parser** — malformed headers, `%` terminators, comments, negative/zero literals, counts
   that disagree with the body.

## 7. Prior art actually consulted

Sources cloned and read before writing code:

* **varisat** (`jix/varisat`) — took: `Var`/`Lit` as `#[repr(transparent)]` newtypes over
  `u32`, 0-based internally and 1-based only at the DIMACS boundary; `max_var()` reserving
  tag bits so a literal plus flags fits one word; splitting the workspace so the formula
  types, the parser, and the solver are separate crates. Also took the *idea* of an
  independent checker, though we cross-check against other solvers instead of using proofs.
* **splr** (`shnarazk/splr`) — took: heuristics behind cargo features so ablations are a
  build flag, not a fork; a pinned `rust-version`. Deliberately did **not** take its
  `unsafe_access` feature (unchecked indexing).
* **batsat** (`c-cube/batsat`) — a MiniSat port; took the arena-with-`u32`-handles allocation
  pattern that our BST node arena uses.
* **sharpSAT** / **Cachet** (literature) — the component-caching formulation, the cache key
  soundness argument, and the activity-decay cache cleanup.

## 8. Benchmark plan

Instances (7,586 real SATLIB files, fetched by `scripts/fetch_benchmarks.sh`):

| suite | n | what it tests |
|---|---|---|
| `uf20-91`, `uf50-218`, `uf100-430` | 3000 | satisfiable random 3-SAT at the phase transition |
| `uuf50-218`, `uuf75-325`, `uuf100-430` | 2100 | unsatisfiable random 3-SAT — the hard case |
| `flat30-60` … `flat100-239` | 1299 | graph colouring — *structured*, should decompose |
| `ais`, `blocksworld`, `logistics` | 15 | all-interval series, planning |
| `aim` | 72 | artificially generated, some with tiny backbones |
| `CBS_k3_n100_m403_b10` | 1000 | controlled backbone size |
| generated pigeonhole | — | exponential for any resolution-based solver; UNSAT stress |

Reference solvers, all built from source at pinned versions: CaDiCaL 3.0.1, Kissat 4.0.4,
MiniSat 2.2.1, varisat 0.2.1, splr.

**Prediction, recorded now so the README cannot cheat later:** random 3-SAT will *not*
decompose (the constraint graph is an expander), so the cache will be pure overhead there
and we will lose badly to every CDCL solver. The interesting question is whether the
structured families (flat/planning) decompose enough for the memo to earn its keep.
