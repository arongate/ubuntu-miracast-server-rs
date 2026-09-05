//! Small reliability helpers shared across modules.
//!
//! The app holds a lot of short-lived `Mutex` guards on the GTK main loop and
//! on worker threads. With plain `.lock().unwrap()`, a panic *while a guard is
//! held* poisons the mutex, and every later `.lock().unwrap()` then panics too
//! — a single fault cascades into a dead app. These helpers recover the guard
//! from a poisoned mutex instead (the protected data is still structurally
//! valid for our usage), so one fault stays contained.

use std::sync::{Mutex, MutexGuard};

/// Extension trait: lock a `Mutex`, recovering the guard even if poisoned.
pub trait LockExt<T> {
    /// Acquire the lock, ignoring poisoning (returns the inner guard either way).
    fn lock_safe(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_safe(&self) -> MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn recovers_after_poisoning() {
        let m = Arc::new(Mutex::new(41));
        let m2 = Arc::clone(&m);
        // Poison the mutex from another thread by panicking while holding it.
        let _ = std::thread::spawn(move || {
            let mut g = m2.lock().unwrap();
            *g = 42;
            panic!("intentional poison");
        })
        .join();
        // Plain lock() would now be Err(Poisoned); lock_safe still works.
        assert!(m.lock().is_err());
        assert_eq!(*m.lock_safe(), 42);
    }
}
