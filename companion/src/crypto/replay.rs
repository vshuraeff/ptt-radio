//! replay admission for non-wrapping cryptographic packet counters, not media sequences.
//!
//! this helper does not authenticate packets. u16's crypto integration owns one
//! window per key and direction, verifies the tag before committing a counter,
//! and retains old-key windows for as long as those keys remain accepted.

/// a 128-counter replay window with constant storage.
///
/// create a fresh window for each fresh key and direction. counters use ordinary
/// integer ordering: after `u32::MAX`, zero is old, never a wrapped successor.
#[derive(Default)]
pub struct ReplayWindow {
    highest: Option<u32>,
    seen: u128,
}

impl ReplayWindow {
    /// checks replay eligibility without reserving a counter or authenticating it.
    pub fn would_accept(&self, counter: u32) -> bool {
        let Some(highest) = self.highest else {
            return true;
        };
        if counter > highest {
            return true;
        }
        let offset = highest - counter;
        offset < u128::BITS && self.seen & (1_u128 << offset) == 0
    }

    /// records an eligible counter, returning false without mutation otherwise.
    ///
    /// the caller must successfully authenticate the packet's tag under this
    /// window's key and direction before calling this method. eligibility is
    /// checked again because another packet may have committed since a precheck.
    pub fn commit_authenticated(&mut self, counter: u32) -> bool {
        if !self.would_accept(counter) {
            return false;
        }
        match self.highest {
            None => {
                self.highest = Some(counter);
                self.seen = 1;
            }
            Some(highest) if counter > highest => {
                let advance = counter - highest;
                self.seen = if advance >= u128::BITS {
                    1
                } else {
                    (self.seen << advance) | 1
                };
                self.highest = Some(counter);
            }
            Some(highest) => self.seen |= 1_u128 << (highest - counter),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::ReplayWindow;
    use std::collections::BTreeSet;

    #[test]
    fn empty_window_accepts_any_first_counter_including_zero() {
        for first in [0, 1, 128, u32::MAX] {
            let mut window = ReplayWindow::default();
            assert!(window.would_accept(first));
            assert!(window.commit_authenticated(first));
            assert!(!window.would_accept(first));
            assert!(!window.commit_authenticated(first));
        }
    }

    #[test]
    fn reordered_unseen_counters_are_accepted_once() {
        let mut window = ReplayWindow::default();
        for counter in [100, 98, 99, 101, 0] {
            assert!(window.commit_authenticated(counter));
            assert!(!window.commit_authenticated(counter));
        }
        for counter in [0, 98, 99, 100, 101] {
            assert!(!window.would_accept(counter));
        }
    }

    #[test]
    fn offset_127_is_inside_and_128_is_outside() {
        let mut window = ReplayWindow::default();
        assert!(window.commit_authenticated(200));
        assert!(!window.would_accept(72));
        assert!(window.commit_authenticated(73));
        assert!(window.commit_authenticated(201));
        assert!(!window.would_accept(73));
        assert!(window.commit_authenticated(74));
    }

    #[test]
    fn advancing_preserves_bits_until_they_leave_the_window() {
        let mut window = ReplayWindow::default();
        assert!(window.commit_authenticated(0));
        assert!(window.commit_authenticated(127));
        assert!(!window.would_accept(0));
        assert!(window.commit_authenticated(128));
        assert!(!window.would_accept(0));
        assert!(window.commit_authenticated(1));
        assert!(!window.would_accept(127));
    }

    #[test]
    fn jumps_of_at_least_128_clear_old_bits() {
        for advance in [128, 129, 10_000, u32::MAX] {
            let mut window = ReplayWindow::default();
            assert!(window.commit_authenticated(0));
            assert!(window.commit_authenticated(advance));
            assert!(!window.would_accept(0));
            assert!(!window.would_accept(advance));
            assert!(window.commit_authenticated(advance - 127));
            assert!(window.commit_authenticated(advance - 1));
            assert!(!window.would_accept(advance - 128));
        }
    }

    #[test]
    fn maximum_counter_does_not_allow_wrap() {
        let mut window = ReplayWindow::default();
        assert!(window.commit_authenticated(u32::MAX - 1));
        assert!(window.commit_authenticated(u32::MAX));
        assert!(!window.commit_authenticated(0));
        assert!(!window.commit_authenticated(u32::MAX));
        assert!(window.commit_authenticated(u32::MAX - 127));
        assert!(!window.commit_authenticated(u32::MAX - 128));
    }

    #[test]
    fn prechecks_and_rejections_never_mutate_the_window() {
        let mut window = ReplayWindow::default();
        for _ in 0..3 {
            assert!(window.would_accept(u32::MAX));
            assert_eq!((window.highest, window.seen), (None, 0));
        }
        assert!(window.commit_authenticated(200));
        let state = (window.highest, window.seen);
        for _ in 0..3 {
            assert!(window.would_accept(201));
            assert!(window.would_accept(u32::MAX));
            assert!(!window.commit_authenticated(200));
            assert!(!window.commit_authenticated(72));
            assert_eq!((window.highest, window.seen), state);
        }
        assert!(window.commit_authenticated(201));
        assert!(!window.commit_authenticated(201));
    }

    #[test]
    fn separate_key_direction_windows_are_independent() {
        let mut sending_direction = ReplayWindow::default();
        let mut receiving_direction = ReplayWindow::default();
        let mut fresh_key = ReplayWindow::default();
        assert!(sending_direction.commit_authenticated(1000));
        assert!(receiving_direction.commit_authenticated(1000));
        assert!(sending_direction.commit_authenticated(u32::MAX));
        assert!(receiving_direction.commit_authenticated(1001));
        assert!(fresh_key.commit_authenticated(0));
        assert!(!sending_direction.would_accept(0));
        assert!(!receiving_direction.would_accept(0));
        assert!(!fresh_key.would_accept(0));
    }

    #[test]
    fn matches_reference_set_on_fixed_reordered_sequence() {
        let mut window = ReplayWindow::default();
        let mut accepted = BTreeSet::new();
        for counter in (0..600).map(|index| (index * 97) % 521).chain([
            10_000,
            9_873,
            9_872,
            u32::MAX,
            0,
            u32::MAX - 127,
        ]) {
            let expected = !accepted.contains(&counter)
                && accepted
                    .last()
                    .is_none_or(|&highest| counter > highest || highest - counter < 128);
            assert_eq!(window.would_accept(counter), expected, "{counter}");
            assert_eq!(window.commit_authenticated(counter), expected, "{counter}");
            if expected {
                accepted.insert(counter);
            }
            for probe in [0, counter, counter.saturating_sub(127), u32::MAX] {
                let expected = !accepted.contains(&probe)
                    && accepted
                        .last()
                        .is_none_or(|&highest| probe > highest || highest - probe < 128);
                assert_eq!(window.would_accept(probe), expected, "probe {probe}");
            }
        }
    }
}
