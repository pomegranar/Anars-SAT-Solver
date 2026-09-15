---
title: "What DP-BST Actually Contributed"
subtitle: "An audit of the paper's three claims against its own data"
author: Anar N.
date: 2026-09-07
---

## The three claims

The paper advances three contributions: a decision SAT solver built on component
caching instead of clause learning; an evaluation of that solver against five
references; and the claim that ordering a hash table's collision buckets as
balanced binary search trees beats textbook separate chaining.

Two of the three are restatements of settled results. The genuinely new material
is somewhere the paper does not look.

## Claim 1: tree-structured hash buckets

This is standard practice rather than a proposal. `java.util.HashMap` has
treeified its buckets since Java 8 (2014), converting a bucket to a red-black
tree once it exceeds eight entries, ordering that tree primarily by the full
hash, and falling back to key comparison only on a hash tie. That is the design
in `cache/tree.rs`, including the trick the paper presents as its own reasoning
in Section III. It has been shipping in a standard library for a decade.

Worse, the experiment does not measure what it claims to measure. Table III
reports mean bucket depth of 1.39 for AVL against 1.60 for chaining, a 13%
reduction. For a chain, the mean depth of a successful lookup has a closed form:
with `n` entries in `m` buckets and `lambda = n/m`, it is `(lambda + 2) / 2`.
Inverting that on the measured data recovers the bucket count to under one
percent on every large instance:

| instance          | entries | measured depth | implied buckets | actual buckets |
|-------------------|--------:|---------------:|----------------:|---------------:|
| php-11-10         | 582,908 |          2.110 |         262,504 |        262,144 |
| aim-100-2_0-no-3  | 209,929 |          2.598 |          65,690 |         65,536 |
| aim-100-1_6-no-3  |  17,785 |          2.098 |           8,096 |          8,192 |
| bf2670-001        |   5,345 |          2.305 |           2,044 |          2,048 |

Bucket depth is a function of the load factor and nothing else. The load factor
is `--target-load`, a command line flag defaulting to 4, so the table oscillates
between two and four entries per bucket. Table III recovers arithmetic. It
contains no information about SAT, about component caches, or about the key
distribution, and it would read identically if the table held phone numbers.

Two secondary points survive. The metric counts comparisons on a hit, yet 73% of
lookups across the benchmark set are misses, where a chain pays its full length
and a tree pays only its height. And the numbers are per-instance unweighted, so
small instances dominate; weighting by entries moves AVL against chain from
1.39/1.60 to 1.78/2.21, and maximum depth from 2.8/5.6 to 4.0/11.9. The paper
measures the less common case and understates its own effect.

## Claim 2: component caching without clause learning

Settled twenty years ago, and the paper concedes as much. Bayardo and Pehoushek
introduced it in 2000, Cachet combined it with clause learning in 2004, sharpSAT
refined the key representation in 2006. All three targeted model counting, where
component caching remains the correct architecture because exhausting the search
space is the entire task. For decision SAT the answer arrived early: clause
learning dominates, and the two mechanisms conflict, because a learned clause
spanning two components merges them.

A PAR-2 of 2.54 against Kissat's 0.50 reproduces that conclusion. Reproduction
has value. It is not a contribution.

## What is left of the design

One narrow thing, and the paper does not argue for it.

Component caches face a soundness and speed tradeoff on the key. Keys are
variable-length byte strings (here, delta-encoded varint index sets), so an exact
cache pays a `memcmp` per comparison. The counting community's answer has been to
discard the key and keep only its hash, accepting a small probability of a wrong
answer; GANAK is explicit that this is what makes it probabilistic.

Hash-ordered tree buckets occupy the middle. Because the low bits of the hash
select the bucket, the remaining bits are uniform within it, so the full 64-bit
hash works as the tree's *ordering* key rather than as a filter. A comparison is
one machine word in essentially every case, and the byte key is touched only on a
full 64-bit collision. The result is the per-comparison cost of the lossy design
with the exactness of the expensive one, and the same property makes insertion
order random, which is why the unbalanced tree lands at 1.47 against AVL's 1.39
and the balancing machinery earns almost nothing.

Every ingredient is standard. The framing, that this is how an exact component
cache buys back the cost that pushed the field toward lossy ones, is the part I
have not seen stated, which is a modest claim about presentation.

## What the paper found and did not recognise

The strongest result in the project is sitting unremarked in `ablation.csv`.

On `php-11-10`, bounded variable elimination removes ten of the formula's 110
variables and ten of its 561 clauses, a 1.8% reduction. The effect on search:

| configuration    | nodes     | cache entries | hit rate | time    |
|------------------|----------:|--------------:|---------:|--------:|
| preprocessing on | 1,132,265 |       582,908 |    48.5% | 2.204 s |
| preprocessing off|     8,105 |         4,097 |    49.5% | 0.028 s |

Removing ten variables costs a factor of 140. The effect decides outcomes:
`php-12-11` and `php-13-12` both time out at ten seconds with preprocessing
enabled and are solved in 0.099 s and 0.263 s with it disabled. The paper's
default configuration is two orders of magnitude slower than one flag away, on
the family it showcases as its best result.

The paper notices the opposite direction of the same interaction. It reports that
dubois is solved almost entirely by preprocessing with the cache contributing
nothing, and presents that as an honest null result. It does not check whether
the interference runs the other way.

## The explanation

The pigeonhole encoding for 11 pigeons and 10 holes is 11 clauses of width 10
(each pigeon occupies some hole) and 550 binary clauses (no two pigeons share a
hole). Take a variable `p(1,1)`. It occurs positively in exactly one clause, the
width-10 clause for pigeon 1, and negatively in ten binary clauses pairing pigeon
1 with each other pigeon in hole 1. Eliminating it produces `1 x 10 = 10`
resolvents replacing 11 clauses, so the clause count falls by one and bounded
variable elimination accepts the trade. Each resolvent has the form

```
p(1,2) v p(1,3) v ... v p(1,10) v ~p(j,1)
```

Thirty literals in eleven clauses become one hundred literals in ten. The binary
clause `~p(1,1) v ~p(j,1)` touched two variables; its replacement touches ten,
and couples pigeon 1's entire row to pigeon j.

That is exactly the object the paper's own Discussion identifies as fatal: "a
learned clause that spans two otherwise independent components merges them,
destroying the decomposition the cache depends on." A BVE resolvent spans
components for the same reason and does the same damage. The paper states the
principle and fails to apply it to the mechanism it shipped.

The diagnostic confirming this is that the hit rate barely moves, 49.5% against
48.5%. The cache recognises repeats equally well in both configurations. What
changes is the number of distinct components reachable at all, 4,097 against
582,908. Preprocessing did not break the cache. It multiplied the universe the
cache has to cover.

The general statement: bounded variable elimination's acceptance criterion is
clause count, which is the right proxy for a watched-literal CDCL solver and
blind to the quantity a decomposing solver depends on, namely the connectivity of
the variable interaction graph. A preprocessor tuned for one architecture is
contraindicated for the other.

## How much of this do I believe

**The php result and its mechanism: high confidence.** The ablation numbers
reproduce from the committed binary, the encoding was verified directly from the
CNF file, the resolvent arithmetic is exact, and the eliminated variable count
matches the clause delta. The unchanged hit rate rules out the competing
explanation that preprocessing degraded the cache itself.

**The closed-form critique of Table III: high confidence.** The formula recovers
the true bucket counts to under one percent across four orders of magnitude of
table size. This is arithmetic rather than an interpretation.

**The Java 8 prior art: fairly high confidence.** The treeification threshold,
the red-black structure, and the hash-primary ordering are recalled from memory
rather than checked against the source, so verify before publishing. The broader
point, that tree buckets are established engineering practice rather than a
research question, does not depend on those details.

**The GANAK framing: moderate confidence.** Hash-only component caching is
certainly standard in modern counters, and GANAK is explicit about the resulting
error probability. Whether anyone has published the hash-as-ordering-key argument
for exact caches is much less certain. Treat "not seen stated" as weaker than
"not stated".

**The strongest caveat: none of this is new to the counting community.** That
preprocessing for model counting must differ from preprocessing for SAT is
established; the B+E preprocessor exists for that reason. The php result is a
rediscovery. Its value is that it is sharp, quantified, and sitting in this
project's own data, unrecognised.

## What the paper should have said

The bucket policy question is not a research question and the measurement did not
answer one. Cut it to an implementation note.

The finding is the interference between the solver's two dynamic programming
mechanisms. Resolution-based elimination and component caching both run over the
same formula, they pull in opposite directions, and which one wins is a property
of the instance family. Dubois is where elimination does everything. Pigeonhole
is where elimination destroys what caching needs, at a cost of 140x and two
solved instances. That is one sentence with two supporting families and a
verified mechanism, more than the paper currently claims and considerably more
defensible.

The obvious next experiment is a width-aware acceptance rule for elimination:
reject a resolvent that increases the variable interaction graph's connectivity,
even when it reduces the clause count. On pigeonhole that rule declines every
elimination BVE currently accepts.

---

## Postscript, 2026-09-15: clause learning was added

Everything above audits the solver as it stood on 7 September, which did not learn
clauses. It does now, and Claim 2 above — "component caching *without* clause
learning" — no longer describes what shipped. The audit is left as written rather
than revised, because an audit that edits itself to stay correct is not an audit.
What follows is what changed and which of the findings above survived.

**The headline number moved.** PAR-2 against Kissat's 0.46 is 1.68, not 2.54.
Unsolved instances fell from fourteen to seven. DP-BST now ties Kissat exactly on
`aim` and matches it on `php`. Reproduction is still not a contribution, but the
reproduction is a good deal closer to the thing being reproduced.

**Claim 2 is now a different claim, and a better one.** "The two mechanisms
conflict, because a learned clause spanning two components merges them" is the
received reason for not combining them, and it is the reason the design document
gave. Having built it, the received reason is not the binding one. What actually
happens is that learned clauses must appear in the cache key — concealing them to
protect the decomposition is *unsound*, not merely awkward — so the key changes
every time a clause is learned, and the same subproblem is solved twice under two
names. The cost lands on the hit rate, not on the decomposition.

The data shows this cutting both ways, which is the part worth keeping. On `aim`,
learning takes the search from 2,622,919 nodes to 591 and the hit rate falls from
17.9% to 1.5%: the cache is starved of work. On `dubois` the hit rate goes from
0% to 45.1%, because the learned clauses fix enough variables for the residual to
decompose into components that then recur. On `ssa` and `blocks` learning slightly
*increases* the node count, 7,292 to 8,724 and 506 to 603, which is the clean
statement of the effect: the clauses learned there do not pay for the component
identities they disturb.

**The finding above generalises to a third mechanism.** The audit's conclusion was
that this solver's two dynamic programming mechanisms interfere, and that which one
wins is a property of the instance family. That holds with three. Learning is inert
on `php` — every derived clause exceeds the retention limit and is discarded, so
the configurations with and without it produce byte-identical counters — for the
same reason BVE is destructive there: pigeonhole has no short resolution proof and
no low-connectivity elimination. One property of the family predicts both.

**A new finding, and it is about the propagation scheme, not the cache.** The
retention limit is three literals. That is absurd by CDCL standards, and it is
forced: counter-based propagation charges a stored clause on every assignment to
every variable it mentions. Measured, the three configurations are

| | solved | PAR-2 | nodes on commonly solved instances |
|---|---:|---:|---:|
| learning, limit 3 | 129/136 | 1.68 | 370,447 |
| no learning | 126/136 | 2.10 | 3,249,334 |
| learning, no limit | 117/136 | 2.96 | 198,801 |

Unbounded learning produces the *smallest search tree of the three* and the
*worst runtime of the three*, losing nine instances relative to not learning at
all. The propagation scheme adopted to make decomposition possible is the same one
that makes most learned clauses unaffordable. Those two design decisions are
individually defensible and jointly constraining, and that is a sharper statement
than either of the paper's original three claims.

**The bucket-policy verdict is unchanged, and slightly worse.** Mean depth is now
1.14 for AVL against 1.19 for chaining, a 4% reduction rather than 13%, because
learning removes components from the cache on the families that used to fill it.
The recommendation above — cut it to an implementation note — stands, and the new
number strengthens it.
