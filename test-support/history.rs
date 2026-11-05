//! Small, bounded history checker shared by runtime storage tests.
//! Workload seeds are reproducible; concurrent thread schedules are not simulated.

#[derive(Clone, Debug)]
pub struct Operation<T> {
    pub start: usize,
    pub end: usize,
    pub value: T,
}

/// Find a sequential explanation that respects completed-before-started order.
/// Keep histories small: checking all legal interleavings is exponential.
pub fn linearizable<M: Clone, T>(
    history: &[Operation<T>],
    model: M,
    apply: impl Fn(&mut M, &T) -> bool,
) -> bool {
    assert!(
        history.len() <= 16,
        "split larger workloads into bounded histories"
    );
    fn search<M: Clone, T>(
        history: &[Operation<T>],
        remaining: u64,
        model: M,
        apply: &impl Fn(&mut M, &T) -> bool,
    ) -> bool {
        if remaining == 0 {
            return true;
        }
        for (i, op) in history.iter().enumerate() {
            if remaining & (1 << i) == 0
                || history
                    .iter()
                    .enumerate()
                    .any(|(j, predecessor)| remaining & (1 << j) != 0 && predecessor.end < op.start)
            {
                continue;
            }
            let mut next = model.clone();
            if apply(&mut next, &op.value) && search(history, remaining & !(1 << i), next, apply) {
                return true;
            }
        }
        false
    }
    search(history, (1 << history.len()) - 1, model, &apply)
}

pub struct Seed(pub u64);

impl Seed {
    pub fn choose(&mut self, choices: usize) -> usize {
        // SplitMix64: fixed algorithm, independent of dependency versions.
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        ((value ^ (value >> 31)) % choices as u64) as usize
    }
}

pub fn seeds() -> Vec<u64> {
    match std::env::var("TAKO_TEST_HISTORY_SEED") {
        Ok(seed) => vec![seed.parse().expect("TAKO_TEST_HISTORY_SEED must be a u64")],
        Err(_) => (0..16).collect(),
    }
}

#[test]
fn checker_respects_real_time_but_allows_overlapping_operations() {
    let apply = |state: &mut usize, expected: &usize| {
        *state += 1;
        *state == *expected
    };
    let mut history = vec![
        Operation {
            start: 0,
            end: 1,
            value: 2,
        },
        Operation {
            start: 2,
            end: 3,
            value: 1,
        },
    ];
    assert!(!linearizable(&history, 0, apply));
    history[0].end = 4;
    assert!(linearizable(&history, 0, apply));
    history[1].value = 2;
    assert!(!linearizable(&history, 0, apply));
}
