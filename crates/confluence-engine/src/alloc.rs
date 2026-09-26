//! First-fit allocator of contiguous channel ranges, with reuse after free.

/// Hands out contiguous `[start, start + len)` channel ranges from `0..capacity`.
pub struct ChannelAllocator {
    /// Free ranges as (start, len), sorted by start, never adjacent (coalesced).
    free: Vec<(u32, u32)>,
}

impl ChannelAllocator {
    pub fn new(capacity: u32) -> Self {
        let free = if capacity > 0 { vec![(0, capacity)] } else { Vec::new() };
        Self { free }
    }

    /// Reserves `len` contiguous channels, lowest start first. `None` if `len`
    /// is zero or no free range is large enough.
    pub fn alloc(&mut self, len: u32) -> Option<u32> {
        if len == 0 {
            return None;
        }
        let i = self.free.iter().position(|&(_, n)| n >= len)?;
        let (start, n) = self.free[i];
        if n == len {
            self.free.remove(i);
        } else {
            self.free[i] = (start + len, n - len);
        }
        Some(start)
    }

    /// Reserves exactly `[start, start + len)` if it is entirely free. Used to
    /// restore a saved layout so channel numbers (and routes) stay stable.
    pub fn reserve(&mut self, start: u32, len: u32) -> bool {
        if len == 0 {
            return false;
        }
        let Some(end) = start.checked_add(len) else { return false };
        let Some(i) = self.free.iter().position(|&(s, n)| s <= start && end <= s + n) else { return false };
        let (s, n) = self.free[i];
        let before = (s, start - s);
        let after = (end, s + n - end);
        self.free.remove(i);
        let mut at = i;
        for r in [before, after] {
            if r.1 > 0 {
                self.free.insert(at, r);
                at += 1;
            }
        }
        true
    }

    /// Returns a range obtained from `alloc`, merging it with free neighbours.
    pub fn free(&mut self, start: u32, len: u32) {
        if len == 0 {
            return;
        }
        let i = self.free.partition_point(|&(s, _)| s < start);
        self.free.insert(i, (start, len));
        // Merge with the following range, then with the preceding one.
        if i + 1 < self.free.len() && self.free[i].0 + self.free[i].1 == self.free[i + 1].0 {
            self.free[i].1 += self.free[i + 1].1;
            self.free.remove(i + 1);
        }
        if i > 0 && self.free[i - 1].0 + self.free[i - 1].1 == self.free[i].0 {
            self.free[i - 1].1 += self.free[i].1;
            self.free.remove(i);
        }
    }

    /// Total free channels (for diagnostics and tests).
    pub fn available(&self) -> u32 {
        self.free.iter().map(|&(_, n)| n).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_contiguously_from_the_bottom() {
        let mut a = ChannelAllocator::new(16);
        assert_eq!(a.alloc(2), Some(0));
        assert_eq!(a.alloc(8), Some(2));
        assert_eq!(a.alloc(6), Some(10));
        assert_eq!(a.alloc(1), None);
        assert_eq!(a.alloc(0), None);
    }

    #[test]
    fn freed_ranges_are_reused_and_coalesced() {
        let mut a = ChannelAllocator::new(12);
        let x = a.alloc(4).unwrap();
        let y = a.alloc(4).unwrap();
        let z = a.alloc(4).unwrap();
        a.free(x, 4);
        a.free(z, 4);
        assert_eq!(a.alloc(8), None, "free space is fragmented: 0..4 and 8..12");
        a.free(y, 4);
        assert_eq!(a.available(), 12);
        assert_eq!(a.alloc(12), Some(0), "all three ranges merged back");
    }

    #[test]
    fn reserve_takes_an_exact_free_range_only() {
        let mut a = ChannelAllocator::new(32);
        assert!(a.reserve(8, 4));
        assert!(!a.reserve(10, 4), "overlaps the reserved range");
        assert!(!a.reserve(30, 4), "runs past capacity");
        assert!(!a.reserve(u32::MAX, 2), "overflow is rejected, not wrapped");
        assert_eq!(a.alloc(8), Some(0));
        assert_eq!(a.alloc(1), Some(12), "hole 0..8 used, next free is after the reservation");
        a.free(8, 4);
        assert_eq!(a.available(), 32 - 8 - 1);
    }

    #[test]
    fn first_fit_prefers_the_lowest_hole() {
        let mut a = ChannelAllocator::new(32);
        let r: Vec<u32> = (0..4).map(|_| a.alloc(8).unwrap()).collect();
        a.free(r[1], 8);
        a.free(r[3], 8);
        assert_eq!(a.alloc(2), Some(8));
    }
}
