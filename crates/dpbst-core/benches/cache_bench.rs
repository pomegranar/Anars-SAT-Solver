//! Microbenchmarks for the memo, isolated from the solver.
//!
//! The end-to-end solve time of a SAT instance is dominated by propagation and by how lucky the
//! branching heuristic got, which swamps the difference between one bucket policy and another.
//! These benchmarks drive the table directly, so the numbers are about the data structure and
//! nothing else.
//!
//! The interesting axis is the **load factor**. Separate chaining is conventionally kept near one
//! entry per bucket because a bucket scan is linear; a tree bucket costs `O(log k)` and should in
//! principle tolerate far more. That is the project's central claim, so it is measured rather
//! than asserted: each policy is run at 1, 4, 16 and 64 entries per bucket.
//!
//! `std::collections::HashMap` is included as an external control.

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use dpbst_core::cache::arena::Verdict;
use dpbst_core::cache::{
    Avl, BucketPolicy, Chain, ComponentCache, DEFAULT_BUDGET_BYTES, Splay, Unbalanced, hash_key,
};
use dpbst_core::varint;
use std::collections::HashMap;
use std::hint::black_box;

/// Number of entries driven through the table in each benchmark.
const ENTRIES: u32 = 50_000;

/// Load factors to sweep. 1 is the textbook chaining target; 64 is deliberately abusive.
const LOADS: [usize; 4] = [1, 4, 16, 64];

/// Builds keys shaped like real component keys: a variable count, a clause count, then
/// delta-encoded index lists.
fn make_keys(count: u32) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| {
            let mut key = Vec::with_capacity(24);
            varint::write_u32(&mut key, 32 + i % 17);
            varint::write_u32(&mut key, 8 + i % 11);
            let mut previous = 0;
            for step in 0..6 {
                let value = i.wrapping_mul(7).wrapping_add(step * 13) % 4096;
                varint::write_u32(&mut key, value.wrapping_sub(previous));
                previous = value;
            }
            key
        })
        .collect()
}

fn fill<P: BucketPolicy>(keys: &[Vec<u8>], load: usize) -> ComponentCache<P> {
    let mut cache = ComponentCache::<P>::new(DEFAULT_BUDGET_BYTES, load);
    for key in keys {
        cache.insert(hash_key(key), key, Verdict::Unsat, &[]);
    }
    cache
}

fn bench_insert(c: &mut Criterion) {
    let keys = make_keys(ENTRIES);
    let mut group = c.benchmark_group("cache/insert");
    group.throughput(Throughput::Elements(u64::from(ENTRIES)));

    for load in LOADS {
        macro_rules! case {
            ($policy:ty) => {
                group.bench_with_input(
                    BenchmarkId::new(<$policy>::NAME, load),
                    &load,
                    |b, &load| {
                        b.iter_batched(
                            || (),
                            |()| black_box(fill::<$policy>(&keys, load).len()),
                            BatchSize::LargeInput,
                        );
                    },
                );
            };
        }
        case!(Avl);
        case!(Unbalanced);
        case!(Splay);
        case!(Chain);
    }
    group.finish();
}

fn bench_lookup_hit(c: &mut Criterion) {
    let keys = make_keys(ENTRIES);
    let hashes: Vec<u64> = keys.iter().map(|k| hash_key(k)).collect();
    let mut group = c.benchmark_group("cache/lookup-hit");
    group.throughput(Throughput::Elements(u64::from(ENTRIES)));

    for load in LOADS {
        macro_rules! case {
            ($policy:ty) => {
                group.bench_with_input(
                    BenchmarkId::new(<$policy>::NAME, load),
                    &load,
                    |b, &load| {
                        // Splaying mutates on lookup, so each policy gets a fresh table per
                        // batch rather than one shared across iterations.
                        b.iter_batched_ref(
                            || fill::<$policy>(&keys, load),
                            |cache| {
                                let mut found = 0_usize;
                                for (key, &hash) in keys.iter().zip(&hashes) {
                                    found += usize::from(cache.lookup(hash, key).is_some());
                                }
                                black_box(found)
                            },
                            BatchSize::LargeInput,
                        );
                    },
                );
            };
        }
        case!(Avl);
        case!(Unbalanced);
        case!(Splay);
        case!(Chain);
    }
    group.finish();
}

fn bench_lookup_miss(c: &mut Criterion) {
    let keys = make_keys(ENTRIES);
    // Keys the table has never seen: a miss walks a bucket to its end, which is the worst case
    // for chaining and the case a tree is supposed to fix.
    let absent = make_keys(2 * ENTRIES)[ENTRIES as usize..].to_vec();
    let hashes: Vec<u64> = absent.iter().map(|k| hash_key(k)).collect();

    let mut group = c.benchmark_group("cache/lookup-miss");
    group.throughput(Throughput::Elements(u64::from(ENTRIES)));

    for load in LOADS {
        macro_rules! case {
            ($policy:ty) => {
                group.bench_with_input(
                    BenchmarkId::new(<$policy>::NAME, load),
                    &load,
                    |b, &load| {
                        b.iter_batched_ref(
                            || fill::<$policy>(&keys, load),
                            |cache| {
                                let mut missed = 0_usize;
                                for (key, &hash) in absent.iter().zip(&hashes) {
                                    missed += usize::from(cache.lookup(hash, key).is_none());
                                }
                                black_box(missed)
                            },
                            BatchSize::LargeInput,
                        );
                    },
                );
            };
        }
        case!(Avl);
        case!(Unbalanced);
        case!(Splay);
        case!(Chain);
    }
    group.finish();
}

/// A skewed workload: a small hot set queried far more often than the rest.
///
/// Real component-cache traffic looks like this, and it is the pattern splay trees exist for.
fn bench_skewed(c: &mut Criterion) {
    let keys = make_keys(ENTRIES);
    let hashes: Vec<u64> = keys.iter().map(|k| hash_key(k)).collect();
    // 90% of queries hit 5% of the keys.
    let plan: Vec<usize> = (0..ENTRIES as usize)
        .map(|i| {
            if i % 10 == 0 {
                i % ENTRIES as usize
            } else {
                (i * 7) % (ENTRIES as usize / 20)
            }
        })
        .collect();

    let mut group = c.benchmark_group("cache/lookup-skewed");
    group.throughput(Throughput::Elements(plan.len() as u64));

    macro_rules! case {
        ($policy:ty) => {
            group.bench_function(<$policy>::NAME, |b| {
                b.iter_batched_ref(
                    || fill::<$policy>(&keys, 16),
                    |cache| {
                        let mut found = 0_usize;
                        for &i in &plan {
                            found += usize::from(cache.lookup(hashes[i], &keys[i]).is_some());
                        }
                        black_box(found)
                    },
                    BatchSize::LargeInput,
                );
            });
        };
    }
    case!(Avl);
    case!(Unbalanced);
    case!(Splay);
    case!(Chain);
    group.finish();
}

/// The external control: the standard library's hash map, which resolves collisions by open
/// addressing rather than by chaining or by a tree.
fn bench_std_hashmap(c: &mut Criterion) {
    let keys = make_keys(ENTRIES);
    let hashes: Vec<u64> = keys.iter().map(|k| hash_key(k)).collect();

    let mut group = c.benchmark_group("cache/std-hashmap");
    group.throughput(Throughput::Elements(u64::from(ENTRIES)));

    group.bench_function("insert", |b| {
        b.iter_batched(
            || (),
            |()| {
                let mut map: HashMap<Vec<u8>, Verdict> = HashMap::with_capacity(ENTRIES as usize);
                for key in &keys {
                    map.insert(key.clone(), Verdict::Unsat);
                }
                black_box(map.len())
            },
            BatchSize::LargeInput,
        );
    });

    let map: HashMap<Vec<u8>, Verdict> = keys.iter().map(|k| (k.clone(), Verdict::Unsat)).collect();
    group.bench_function("lookup-hit", |b| {
        b.iter(|| {
            let mut found = 0_usize;
            for key in &keys {
                found += usize::from(map.contains_key(key));
            }
            black_box(found)
        });
    });

    // Same traffic through the project's own table, for a like-for-like reading.
    group.bench_function("lookup-hit-dpbst-avl", |b| {
        b.iter_batched_ref(
            || fill::<Avl>(&keys, 4),
            |cache| {
                let mut found = 0_usize;
                for (key, &hash) in keys.iter().zip(&hashes) {
                    found += usize::from(cache.lookup(hash, key).is_some());
                }
                black_box(found)
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_insert,
    bench_lookup_hit,
    bench_lookup_miss,
    bench_skewed,
    bench_std_hashmap
);
criterion_main!(benches);
