//! Admission of senders: v1 serves one at a time (PROTOCOL.md §7.1).
//!
//! A session that is closing still owns the panel until its pipeline is torn
//! down. A sender that reconnects the moment it sees the old socket close
//! would otherwise race that teardown and get BUSY, so a new connection waits
//! (briefly) for a *closing* session, but never for an active one.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Free,
    Active,
    Closing,
}

type Hook = Box<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub struct Slot {
    state: Mutex<State>,
    freed: Condvar,
    on_taken: Option<Hook>,
    on_free: Option<Hook>,
}

impl Slot {
    /// `on_taken` runs when a sender takes the panel, `on_free` once it is
    /// released. Both run under the slot lock, so they never interleave
    /// (e.g. the idle screen cannot reappear after the next sender took over).
    pub fn with_hooks(
        on_taken: impl Fn() + Send + Sync + 'static,
        on_free: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Slot {
            on_taken: Some(Box::new(on_taken)),
            on_free: Some(Box::new(on_free)),
            ..Slot::default()
        }
    }

    /// Takes the slot if it is free, or becomes free within `wait` because
    /// the current session is already closing. False means reply BUSY.
    pub fn try_acquire(&self, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        let mut state = self.lock();
        loop {
            match *state {
                State::Free => {
                    *state = State::Active;
                    if let Some(hook) = &self.on_taken {
                        hook();
                    }
                    return true;
                }
                State::Active => return false,
                State::Closing => {
                    let left = until.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    state = self
                        .freed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        }
    }

    /// The owning session's socket is closed; teardown is under way.
    pub fn mark_closing(&self) {
        let mut state = self.lock();
        if *state == State::Active {
            *state = State::Closing;
        }
    }

    /// Teardown finished: the panel is free for the next sender.
    pub fn release(&self) {
        let mut state = self.lock();
        *state = State::Free;
        if let Some(hook) = &self.on_free {
            hook();
        }
        self.freed.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn active_session_means_busy_immediately() {
        let slot = Slot::default();
        assert!(slot.try_acquire(Duration::ZERO));
        let t = Instant::now();
        assert!(!slot.try_acquire(Duration::from_secs(5)));
        assert!(t.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn closing_session_hands_over_once_released() {
        let slot = Arc::new(Slot::default());
        assert!(slot.try_acquire(Duration::ZERO));
        slot.mark_closing();
        let releaser = {
            let slot = slot.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                slot.release();
            })
        };
        assert!(slot.try_acquire(Duration::from_secs(1)));
        releaser.join().unwrap();
        assert!(
            !slot.try_acquire(Duration::ZERO),
            "now held by the new session"
        );
    }

    #[test]
    fn hooks_run_on_take_and_release() {
        use std::sync::atomic::{AtomicI32, Ordering};
        let taken = Arc::new(AtomicI32::new(0));
        let (t1, t2) = (taken.clone(), taken.clone());
        let slot = Slot::with_hooks(
            move || {
                t1.fetch_add(1, Ordering::SeqCst);
            },
            move || {
                t2.fetch_sub(1, Ordering::SeqCst);
            },
        );
        assert!(slot.try_acquire(Duration::ZERO));
        assert!(!slot.try_acquire(Duration::ZERO));
        assert_eq!(taken.load(Ordering::SeqCst), 1);
        slot.release();
        assert_eq!(taken.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn closing_session_that_never_finishes_means_busy() {
        let slot = Slot::default();
        assert!(slot.try_acquire(Duration::ZERO));
        slot.mark_closing();
        assert!(!slot.try_acquire(Duration::from_millis(20)));
    }
}
