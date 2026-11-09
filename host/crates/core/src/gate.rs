use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Bounds the number of frames that are encoded but not yet written to the
/// socket. When the link falls behind, new frames are skipped *before* they
/// are encoded, so the encoder's reference chain is never broken.
pub struct SendGate {
    max_in_flight: usize,
    in_flight: AtomicUsize,
    dropped: AtomicU64,
}

impl SendGate {
    pub fn new(max_in_flight: usize) -> Self {
        SendGate { max_in_flight, in_flight: AtomicUsize::new(0), dropped: AtomicU64::new(0) }
    }

    /// Returns `false` when the frame must be skipped.
    pub fn try_acquire(&self) -> bool {
        let max = self.max_in_flight;
        let ok = self
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < max).then_some(n + 1))
            .is_ok();
        if !ok {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    /// Call once the frame has been fully written (or abandoned).
    pub fn release(&self) {
        let _ = self.in_flight.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquires_up_to_limit() {
        let g = SendGate::new(2);
        assert!(g.try_acquire());
        assert!(g.try_acquire());
        assert!(!g.try_acquire());
        assert_eq!(g.dropped(), 1);
    }

    #[test]
    fn release_frees_slot() {
        let g = SendGate::new(1);
        assert!(g.try_acquire());
        assert!(!g.try_acquire());
        g.release();
        assert!(g.try_acquire());
    }

    #[test]
    fn release_without_acquire_does_not_underflow() {
        let g = SendGate::new(1);
        g.release();
        assert!(g.try_acquire());
        assert!(!g.try_acquire());
    }
}
