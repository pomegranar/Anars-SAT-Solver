# DP-BST

A SAT solver written in Rust. It combines backtracking search with **dynamic programming**, and
stores what it remembers in a **hash table whose buckets are binary search trees**.

---

## Part 1 — What is this, in plain terms

### The problem

A great many practical questions reduce to the same shape: *given a pile of yes/no decisions and
a list of rules about which combinations are allowed, is there any way to satisfy all the rules
at once?*

Timetabling is like this. So is deciding whether a circuit design can ever produce a wrong
output, whether a set of software package versions can be installed together, whether a delivery
schedule is feasible, and whether a Sudoku grid has a solution. The rules look different in each
case, but they can all be written in the same form, and one program can answer all of them.

That form is called **CNF**, and the question is called **SAT** (short for *satisfiability*).
A program that answers it is a **SAT solver**. This is one.

### What a question looks like

Say three people — Ana, Ben and Cal — and we must pick a team, subject to some rules:

* at least one of Ana or Ben,
* not both Ben and Cal,
* if Ana is in, Cal must be too.

Write each rule as a list of alternatives, where a name means "in" and a struck-through name
means "out":

```
(Ana  OR  Ben)          at least one of them
(¬Ben OR ¬Cal)          not both
(¬Ana OR  Cal)          Ana implies Cal
```

Every rule must hold; within a rule, any one alternative is enough. A solver either finds a
combination that works (here: Ana in, Ben out, Cal in) or proves that none exists.

Real instances have hundreds of thousands of names and millions of rules.

### Why it is hard

With `n` yes/no decisions there are `2^n` combinations. At 300 decisions — a small instance by
modern standards — that is more combinations than there are atoms in the observable universe.
Nothing can check them one by one. SAT was the first problem ever proved **NP-complete**, and
nobody knows a method that is fast on every input.

What *does* work is being clever about which combinations you never need to look at. That is what
every SAT solver is: a large, carefully-built pile of ways to avoid work.

### The idea in this solver

Two of those ways, and one data structure to support them.

**1. Notice when a problem falls apart into smaller ones.**

Once you have fixed a few decisions, the rules that are still unsettled often split into groups
that have nothing to do with each other — no shared names. Those groups are separate problems.
Solve each on its own and combine the answers. Two problems of 50 decisions are enormously easier
than one of 100: `2^50 + 2^50` against `2^100`.

**2. Remember answers, because the same sub-problem keeps coming back.**

Search explores many different orders of decisions, and different orders keep arriving at *the
same* leftover sub-problem. Solving it more than once is wasted effort. So the solver writes down
what it has already worked out and looks it up next time.

Remembering solved sub-problems and combining them is exactly what computer scientists call
**dynamic programming**, and it is the DP in the name.

**3. Store those memories in a hash table of binary search trees.**

The notebook needs to be fast — millions of entries, looked up constantly — so its design is the
whole point of the project.

A **hash table** turns a sub-problem into a number and uses it to pick one of many small
"buckets", so a lookup only searches one bucket instead of everything. But two different
sub-problems can land in the same bucket, so buckets still need internal structure. Textbooks
usually chain them into a list, which means scanning it end to end.

This project puts a **binary search tree** in each bucket instead. A tree keeps its contents
sorted, so a lookup halves the remaining candidates at every step: 64 entries take about 6 steps
instead of an average of 32.

Whether that is worth it in practice is an empirical question, so this repository measures it
against a linked list rather than assuming. [Part 3](#part-3--results) reports what happened,
including where the answer is "it doesn't matter".

---

## Part 2 — Using it

### Solve something

```console
$ dpbst instance.cnf
s SATISFIABLE
v 1 -2 3 4 -5 0
```

Output follows the SAT competition convention that MiniSat, CaDiCaL and Kissat also use, so
`dpbst` slots into existing tooling unchanged:

| output | meaning | exit code |
|---|---|---:|
| `s SATISFIABLE` + `v` lines | a solution was found, and is listed | 10 |
| `s UNSATISFIABLE` | proved that no solution exists | 20 |
| `s UNKNOWN` | gave up: a limit was reached | 0 |

The `v` line lists every variable: `3` means variable 3 is true, `-5` means variable 5 is false.

### Useful flags

```console
$ dpbst instance.cnf --verify           # re-check the solution against the input
$ dpbst instance.cnf --stats            # report what the solve cost
$ dpbst instance.cnf -t 60              # give up after 60 seconds
$ dpbst instance.cnf --bucket chain     # use list buckets instead of trees
$ dpbst instance.cnf --no-cache         # turn the memo off
$ dpbst instance.cnf -a dp              # run Davis and Putnam's 1960 procedure instead
$ cat instance.cnf | dpbst              # reads standard input too
```

`--stats` explains where the time went:

```console
$ dpbst benchmarks/clean/php/php-10-9.cnf --no-model --stats
s UNSATISFIABLE
c algorithm            : dpll-memo
c bucket policy        : avl
c input                : 90 vars, 415 clauses
c search nodes         : 128401
c decisions            : 256802
c conflicts            : 94577
c components           : 128401
c memo                 : 128401 lookups, 63496 hits (49.5%), 64905 entries
c memo table           : 32768 buckets, 28843 occupied, max bucket 8, mean depth 2.29
c cpu time             : 0.4127 s
```

Half the sub-problems on that instance were answered from memory rather than searched.

### Input format

DIMACS CNF, the standard:

```
c comments start with c
p cnf 3 3        <- 3 variables, 3 clauses
1 2 0            <- (x1 OR x2)
-2 -3 0          <- (NOT x2 OR NOT x3)
-1 3 0           <- (NOT x1 OR x3)
```

One practical note: most of the SATLIB corpus terminates with a `%` line followed by a stray `0`.
MiniSat, CaDiCaL and Kissat all **refuse to parse those files**; `dpbst` handles them. Reading
past the `%` invents an empty clause and reports every instance unsatisfiable, so this is a
correctness trap rather than a cosmetic one. `scripts/normalize_cnf.py` strips the marker for the
benefit of the other solvers.

---

## Part 4 — How it works

### 4.1 The search

The backbone is **DPLL** (Davis–Putnam–Logemann–Loveland, 1962), which every practical solver
still descends from. Pick an unassigned variable, try it true, simplify, recurse; if that fails,
try it false; if both fail, the sub-problem has no solution.

Two rules do most of the work before any guessing happens:

* **Unit propagation.** If a rule has one alternative left and it is not yet settled, that
  alternative is forced. Forcing it often forces another, and so on.
* **Pure literal elimination.** If a variable is only ever used one way round among the rules
  still in play, set it that way. It cannot break anything.

Pure literal elimination preserves satisfiability but not the *number* of solutions, which is why
this is sound here and would not be in a model counter.

### 4.2 Decomposition — the dynamic programming

After some assignments, build the graph whose nodes are the unassigned variables, with an edge
between two variables that still share a rule. The connected components of that graph are
independent sub-problems:

```
        before assigning x                    after assigning x
   ┌─────────────────────────────┐     ┌──────────────┐   ┌──────────────┐
   │  a ─ b ─ x ─ d ─ e          │     │  a ─ b       │   │  d ─ e       │
   │      │       │              │  →  │      │       │   │      │       │
   │      c       f              │     │      c       │   │      f       │
   └─────────────────────────────┘     └──────────────┘   └──────────────┘
     one problem, 6 variables            two problems, 3 variables each
              2^6 = 64                        2^3 + 2^3 = 16
```

The whole is satisfiable exactly when every component is, so components are solved separately and
one unsatisfiable component ends the branch immediately.

### 4.3 The memo — what is remembered, and how it is named

A component is stored under the key

```
key = ( its unassigned variables , its active clauses )        both as sorted index lists
```

**That pair determines the sub-problem exactly.** A clause is "active" only if none of its
literals is true, so every *assigned* variable inside an active clause is assigned falsely; the
part of clause `C` that survives is precisely `{ l ∈ C : var(l) is in the component }`, which the
key already describes. This is the argument `Cachet` and `sharpSAT` rest on, and it is why the
key can be two integer lists rather than a canonicalised copy of the formula.

Keys are delta-encoded as varints, which for the dense index ranges a component produces costs
roughly one byte per element.

Satisfiable entries also store a witness — one bit per variable of the component — so that a
cache hit contributes its part of the final solution instead of only saying "yes".

### 4.4 The table

```
    hash(key) ──┬── low bits ──►  bucket index
                └── all 64 bits ─►  position within the bucket's tree

    buckets: [NodeId; 2^k]
        │
        └──► binary search tree of Node ──► blob: key bytes, then witness bits
```

Three decisions are worth defending.

**Trees are ordered by the full 64-bit hash, with the raw key only as a tie-break.**

The bucket index already consumed the low bits of the hash, so within a bucket those bits are
constant and the remaining ones are effectively uniform random. Two things follow. A comparison
is a single `u64` compare — the variable-length key is touched only on a genuine 64-bit
collision. And insertion order within a bucket is random, so even a deliberately *unbalanced*
tree has expected depth `O(log k)`. That last point is what makes the AVL-versus-unbalanced
comparison a real question instead of a foregone conclusion.

Correctness never depends on the hash being good, only speed: a full 64-bit collision falls
through to a byte-wise key comparison, so two different sub-problems are never confused. There is
a test that forces exactly this.

**Nodes live in one flat arena addressed by `u32` handles, not `Box`.**

A `Box` per node would mean a pointer chase into an unrelated address at every level of every
tree, eight-byte child pointers, and a `free` per node on eviction. The arena gives locality,
halves the pointer width, turns deletion into a free-list push, and keeps the module inside
`#![forbid(unsafe_code)]`. The pattern is taken from how MiniSat and its Rust port `batsat`
allocate clauses. A node is exactly 32 bytes, and there is a test asserting it stays that way.

**Memory is bounded, and eviction decays rather than clears.**

An unbounded component cache will exhaust RAM on anything interesting. The table has a byte
budget; on overflow it halves every entry's activity counter and drops the ones that reach zero.
Entries reused since the last sweep survive, cold ones do not. Clearing everything would be
simpler and would throw away the hot working set along with the cold tail.

### 4.5 Propagation: counters, not watched literals

Every competitive solver propagates with **two watched literals**, which lets a clause be ignored
until one of two specific literals is falsified. It has one property that rules it out here: it
cannot tell you whether a clause is *satisfied*.

Component analysis runs at every search node and must enumerate the clauses still active, which
is exactly that question. So this solver keeps two counters per clause — true literals and
unassigned literals — from which satisfied, active, unit and conflicting all follow in constant
time, and which undo exactly on backtracking.

The cost is real. Assigning a variable touches every clause it occurs in, where watched literals
would touch a fraction. It is the single largest reason `dpbst` is slower per propagation than
CaDiCaL, and it is a deliberate trade, not an oversight.

### 4.6 The other DP: Davis–Putnam, 1960

"DP" names two things here, and both ship.

Davis and Putnam's original procedure predates backtracking search. It eliminates a variable
outright by replacing every clause containing it with all **resolvents** of its positive and
negative occurrences:

```
   (a ∨ x)   and   (b ∨ ¬x)      resolve on x to      (a ∨ b)
```

Repeat until no clauses remain (satisfiable) or the empty clause appears (unsatisfiable). No
search, no backtracking — and no bound on memory, which is why 1962 replaced it. Eliminating a
variable occurring `p` times positively and `n` times negatively can produce `p × n` clauses, and
that compounds.

`dpbst -a dp` runs it. It is worth watching it lose.

What survived from 1960 is the same operation under a size limit: eliminate a variable only when
doing so does not make the formula bigger. That is **bounded variable elimination**, every
serious solver preprocesses with it, and `dpbst` does too — the same `elim` module serves both,
with and without the bound.

### 4.7 Branching heuristics

Component analysis has already walked the active clauses, so literal statistics are nearly free.
Five rules are selectable with `--heuristic`:

| name | rule |
|---|---|
| `jw` | Jeroslow–Wang: weight each occurrence by `2^-len`, so short clauses dominate (default) |
| `dlis` | pick the single literal occurring most often |
| `dlcs` | pick the variable whose two literals occur most often combined |
| `mom` | Maximum Occurrences in clauses of Minimum Size |
| `static` | lowest index first — no heuristic at all, for reproducible measurement |

**VSIDS is deliberately absent.** It scores variables by how often they appear in *learned*
clauses, and this solver learns none. The right target for a caching solver is VSADS (Sang,
Beame and Kautz), which blends VSIDS with occurrence counting; see
[limitations](#part-6--limitations-and-what-would-come-next).

---

## Part 5 — Building, running and reproducing

### Requirements

| | version | why |
|---|---|---|
| Rust | **1.85 or newer** (`rust-version` in `Cargo.toml`) | edition 2024, `let`-chains |
| Cargo | ships with Rust | build, test, bench |
| Python | 3.9+ | benchmark harness and instance generators only |
| `curl`, `tar`, a C/C++ compiler | any | only to fetch/build the *reference* solvers |

Developed and measured on Rust 1.98.1, macOS 15 (Darwin 25.6), Apple M1, 8 cores.

### Build

```console
$ git clone https://github.com/pomegranar/Anars-SAT-Solver
$ cd Anars-SAT-Solver
$ cargo build --release
$ ./target/release/dpbst --help
```

`rust-toolchain.toml` pins the channel, so `rustup` fetches a matching toolchain automatically.

### Dependencies

The solver library depends on **one** crate:

| crate | version | used for |
|---|---|---|
| [`thiserror`](https://crates.io/crates/thiserror) | 2 | derive `std::error::Error` on the DIMACS parse errors |

The command line front end adds two:

| crate | version | used for |
|---|---|---|
| [`clap`](https://crates.io/crates/clap) | 4, `derive` | argument parsing |
| [`anyhow`](https://crates.io/crates/anyhow) | 1 | error reporting at the top level |

Development only:

| crate | version | used for |
|---|---|---|
| [`criterion`](https://crates.io/crates/criterion) | 0.7 | statistical microbenchmarks |

Everything else — the hash, the varint codec, the arena, the trees, the DIMACS parser, the JSON
statistics output — is written here, because each is small, each is on a hot path or a
correctness path, and a dependency for any of them would buy nothing. There is no `unsafe`
anywhere: the workspace sets `unsafe_code = "forbid"`.

### Tests

```console
$ cargo test                         # unit + integration, debug
$ cargo test --release --test differential   # exhaustive cross-check; slow unoptimised
$ cargo clippy --all-targets         # pedantic lints, clean
$ cargo fmt --all -- --check
```

Five layers, deliberately:

1. **Unit tests** next to each module — the arena free list, AVL height bounds, varint edges,
   DIMACS malformed input, counter restoration after backtracking.
2. **Differential tests against brute force** (`tests/differential.rs`). Around 1,100 random
   formulas small enough to settle by enumerating all `2^n` assignments, each run through **15
   configurations** — every bucket policy, every heuristic, memo on and off, pure literals on and
   off, preprocessing on and off, plain DPLL, Davis–Putnam, and a cache budget small enough to
   force eviction mid-search. Every configuration must match ground truth, and every satisfiable
   answer must produce a model that satisfies the *original* formula.
   *This is the test that matters.* Configurations agreeing with each other proves nothing if
   they are all wrong the same way.
3. **Structure invariants** — AVL height stays within `1.44·log₂(n+2)`; distinct keys colliding
   on all 64 hash bits are still distinguished; eviction keeps reused entries and drops cold ones.
4. **Known-verdict sweeps** (`scripts/check_verdicts.sh`) over SATLIB, where `uf*` is satisfiable
   and `uuf*` is not by construction — thousands of instances of free ground truth.
5. **Cross-validation** against CaDiCaL, Kissat, MiniSat, varisat and splr. `scripts/bench.py`
   treats any conflicting verdict as a hard failure rather than a statistic.

### Reproducing the benchmarks

```console
$ scripts/fetch_tools.sh                    # build CaDiCaL, Kissat, MiniSat, varisat, splr
$ scripts/fetch_benchmarks.sh               # ~8,400 SATLIB instances (~40 MB)
$ python3 scripts/gen_instances.py benchmarks/generated
$ cp -R benchmarks/generated/* benchmarks/clean/

$ python3 scripts/bench.py --suites uf250-1065:25 php chain --timeout 20
$ cargo bench                               # criterion microbenchmarks
```

`bench.py` runs sequentially by default; timing several solvers at once on one machine is how
benchmark tables become fiction. Instances are not committed — there are thousands of them — so
the fetch scripts stand in for them.

### Layout

```
crates/dpbst-core/          the library
  src/lit.rs                Var and Lit newtypes over u32
  src/cnf.rs                flat CNF storage, models
  src/dimacs.rs             byte-level DIMACS parser and writer
  src/varint.rs             LEB128, for component keys
  src/cache/
    mod.rs                  the hash table, eviction, statistics
    arena.rs                32-byte nodes, u32 handles, free list
    tree.rs                 AVL / unbalanced / splay / chaining bucket policies
  src/search/
    state.rs                assignment, trail, per-clause counters
    component.rs            connected components and canonical keys
    heuristic.rs            branching rules
    mod.rs                  the DPLL driver
  src/elim.rs               resolution and model reconstruction
  src/preprocess.rs         bounded variable elimination and friends
  src/dp.rs                 Davis-Putnam, 1960
  src/solver.rs             public API, statistics, competition exit codes
  tests/differential.rs     brute-force cross-checking
  benches/                  criterion

crates/dpbst-cli/           the `dpbst` binary
scripts/                    fetch, generate, normalise, benchmark, verify
docs/DESIGN.md              the design document, written before the code
```

---

## Part 6 — Limitations, and what would come next

Stated plainly, because a benchmark table without them is marketing.

**No clause learning.** This is the big one. Every solver that beats `dpbst` does CDCL: on a
conflict it derives a new clause explaining the failure and adds it to the formula, so the same
dead end is never re-entered. It is the single most valuable idea in modern SAT solving and this
project does not implement it.

It is also not a free addition here. A learned clause spanning two components *merges* them,
destroying the decomposition the memo depends on. Solvers that do both — `Cachet`, `sharpSAT` —
handle the interaction carefully. Doing it properly is the obvious next piece of work, not a
line to add.

**No watched literals.** Explained in §4.5: component analysis needs to know which clauses are
satisfied, and watched literals cannot say. The cost is a constant factor on every assignment.

**No proof output.** CaDiCaL and Kissat emit DRAT proofs, so an independent checker can verify an
`UNSATISFIABLE` answer. Without clause learning there is no natural resolution proof to emit.
Unsatisfiable answers here are instead cross-checked against other solvers and, for small
instances, against exhaustive enumeration.

**No VSIDS.** It needs conflict analysis, which needs clause learning. VSADS is the right target
once that exists.

**Single threaded.**

**Component keys use absolute indices.** Two structurally identical but disjoint sub-problems get
different keys and are each solved from scratch. Isomorphism-aware keys would help on symmetric
instances such as pigeonhole; they are also expensive to compute, and the trade has not been
measured here.

---

## Part 7 — Prior art

Three open-source Rust solvers were cloned and read before any code was written here.

**[varisat](https://github.com/jix/varisat)** (Jannis Harder). Taken: `Var` and `Lit` as
`#[repr(transparent)]` newtypes over `u32`, zero-based internally with the one-based DIMACS
convention confined to the I/O boundary — which removes an entire class of off-by-one bug;
reserving tag bits below `u32::MAX` so a literal plus flags fits one word; splitting the
workspace so formula types, parser and solver are separate. Its independent proof checker is the
right idea and is the thing this project most conspicuously lacks.

**[splr](https://github.com/shnarazk/splr)** (Narazaki Shuji). Taken: putting heuristics behind
cargo features, so an ablation is a build flag rather than a fork. Deliberately **not** taken:
its `unsafe_access` feature, which elides bounds checks. This project keeps
`#![forbid(unsafe_code)]` and pays for it.

**[batsat](https://github.com/c-cube/batsat)** (Simon Cruanes), a MiniSat port. Taken: the
arena-with-`u32`-handles allocation pattern, which is what the cache's node arena is.

From the literature: the component-caching formulation, the soundness argument for the cache key,
and the activity-decay cleanup come from **Cachet** (Sang, Bacchus, Beame, Kautz, Pipatsrisawat)
and **sharpSAT** (Thurley), by way of Bayardo and Pehoushek's earlier work on connected
components. Bounded variable elimination is Eén and Biere, *Effective Preprocessing in SAT
through Variable and Clause Elimination*, along with the trail that reconstructs a model
afterwards.

---

## Licence

GNU General Public License v3.0 or later. See [`LICENSE`](LICENSE).
