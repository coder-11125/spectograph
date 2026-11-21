//! Ordered parallel map for per-file work (parsing).
//!
//! Parsing is embarrassingly parallel per file, but the analyzers' tree walks are
//! recursive, so worker threads need a much larger stack than rayon's 2 MiB default
//! (deeply nested real-world code overflows it). Results come back in input order, so a
//! caller that folds them sequentially produces output identical to a sequential run.

use rayon::prelude::*;

/// Worker stack size. The main thread's is typically 8 MiB; match or exceed it.
const WORKER_STACK_BYTES: usize = 32 * 1024 * 1024;

/// Below this many items the thread-pool setup costs more than it saves.
const PARALLEL_THRESHOLD: usize = 16;

/// Map `f` over `items` on a dedicated large-stack thread pool, preserving input order.
/// Falls back to a sequential map for small inputs or if the pool cannot be created.
pub fn par_map<T, R, F>(items: &[T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync + Send,
{
    if items.len() < PARALLEL_THRESHOLD {
        return items.iter().map(f).collect();
    }
    match rayon::ThreadPoolBuilder::new()
        .stack_size(WORKER_STACK_BYTES)
        .build()
    {
        Ok(pool) => pool.install(|| items.par_iter().map(&f).collect()),
        Err(_) => items.iter().map(f).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_order_above_and_below_threshold() {
        for n in [0usize, 3, PARALLEL_THRESHOLD, 500] {
            let input: Vec<usize> = (0..n).collect();
            let out = par_map(&input, |x| x * 2);
            assert_eq!(out, input.iter().map(|x| x * 2).collect::<Vec<_>>());
        }
    }

    #[test]
    fn survives_deep_recursion_on_workers() {
        fn depth(n: usize) -> usize {
            if n == 0 {
                0
            } else {
                1 + depth(n - 1)
            }
        }
        let input = vec![20_000usize; 64];
        let out = par_map(&input, |n| depth(*n));
        assert!(out.iter().all(|&d| d == 20_000));
    }
}
