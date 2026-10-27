//! Bounded parallel map for blocking work such as `git` and `gh` child
//! processes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Most jobs [`map`] runs at once, so a big folder of repositories
/// doesn't start hundreds of processes together.
const MAX_PARALLEL: usize = 8;

/// `f` over `items` on up to [`MAX_PARALLEL`] threads; results keep the
/// order of `items`. A panic in `f` resumes on the caller.
pub fn map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if items.len() <= 1 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let slots: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..items.len().min(MAX_PARALLEL) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else {
                    break;
                };
                let result = f(item);
                if let Ok(mut slots) = slots.lock() {
                    slots[i] = Some(result);
                }
            });
        }
    });
    slots
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .map(|r| r.expect("every item is mapped"))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn map_keeps_order() {
        let items: Vec<usize> = (0..50).collect();
        assert_eq!(
            super::map(&items, |i| i * 2),
            (0..50).map(|i| i * 2).collect::<Vec<_>>()
        );
        assert!(super::map(&[] as &[u8], |_| ()).is_empty());
    }
}
