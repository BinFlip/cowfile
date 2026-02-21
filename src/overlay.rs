//! Sparse copy-on-write overlay for tracking byte-level modifications.
//!
//! The [`Overlay`] tracks modifications as `(offset, bytes)` entries in two layers:
//!
//! - **Pending**: Uncommitted modifications from the current pass.
//! - **Committed**: Consolidated modifications from previous [`commit`](Overlay::commit) calls.
//!
//! When reading, layers are composited with priority: **pending > committed > base**.
//! The [`commit`](Overlay::commit) operation merges pending entries into the committed
//! layer and consolidates overlapping/adjacent regions.

use std::collections::BTreeMap;

/// A sparse copy-on-write overlay tracking modifications over immutable base data.
///
/// Modifications are stored as `(offset, bytes)` entries in sorted maps.
/// The overlay supports accumulating changes, committing (merging) them into
/// a consolidated state, and reading back the effective data at any offset.
pub(crate) struct Overlay {
    /// Pending modifications not yet committed.
    pending: BTreeMap<u64, Vec<u8>>,
    /// Committed (consolidated) modifications from previous commit cycles.
    committed: BTreeMap<u64, Vec<u8>>,
}

impl Overlay {
    /// Creates a new empty overlay with no modifications.
    pub(crate) fn new() -> Self {
        Overlay {
            pending: BTreeMap::new(),
            committed: BTreeMap::new(),
        }
    }

    /// Records a write of `data` at `offset` into the pending layer.
    ///
    /// If a previous pending write overlaps with this one, the overlapping
    /// entries are trimmed or removed so that the newest write takes priority.
    ///
    /// Empty writes are ignored.
    pub(crate) fn write(&mut self, offset: u64, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let write_end = offset + data.len() as u64;

        // Collect existing pending entries that overlap with this write.
        let overlapping: Vec<u64> = self
            .pending
            .range(..write_end)
            .filter(|(&off, d)| off + d.len() as u64 > offset)
            .map(|(&off, _)| off)
            .collect();

        for existing_offset in overlapping {
            let existing_data = self.pending.remove(&existing_offset).unwrap();
            let existing_end = existing_offset + existing_data.len() as u64;

            // Keep the portion before our write, if any.
            if existing_offset < offset {
                let keep_len = (offset - existing_offset) as usize;
                self.pending
                    .insert(existing_offset, existing_data[..keep_len].to_vec());
            }

            // Keep the portion after our write, if any.
            if existing_end > write_end {
                let skip = (write_end - existing_offset) as usize;
                self.pending
                    .insert(write_end, existing_data[skip..].to_vec());
            }
        }

        self.pending.insert(offset, data.to_vec());
    }

    /// Reads `length` bytes starting at `offset`, compositing all layers.
    ///
    /// The result is built by starting from the `base` data, then applying
    /// committed modifications, then pending modifications. Each subsequent
    /// layer overwrites the previous one where they overlap.
    ///
    /// # Panics
    ///
    /// Panics if `offset + length` exceeds `base.len()`. The caller is
    /// responsible for bounds checking before calling this method.
    pub(crate) fn read(&self, offset: u64, length: u64, base: &[u8]) -> Vec<u8> {
        let start = offset as usize;
        let end = start + length as usize;
        let mut result = base[start..end].to_vec();

        Self::apply_layer(&self.committed, offset, &mut result);
        Self::apply_layer(&self.pending, offset, &mut result);

        result
    }

    /// Merges all pending modifications into the committed layer.
    ///
    /// Each pending entry is applied over the committed layer, handling overlaps
    /// correctly (pending data wins where it overlaps with committed data).
    /// After merging, the committed layer is consolidated to merge
    /// adjacent/overlapping regions into minimal non-overlapping entries.
    ///
    /// After this call, the pending layer is empty.
    pub(crate) fn commit(&mut self) {
        if self.pending.is_empty() {
            return;
        }

        let pending = std::mem::take(&mut self.pending);
        for (offset, data) in pending {
            Self::merge_into_committed(&mut self.committed, offset, &data);
        }

        Self::consolidate(&mut self.committed);
    }

    /// Returns `true` if there are pending (uncommitted) modifications.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Returns `true` if there are any modifications (pending or committed).
    pub(crate) fn has_modifications(&self) -> bool {
        !self.pending.is_empty() || !self.committed.is_empty()
    }

    /// Returns the number of discrete pending modification regions.
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Returns the number of discrete committed modification regions.
    #[cfg(test)]
    pub(crate) fn committed_count(&self) -> usize {
        self.committed.len()
    }

    /// Returns an iterator over committed entries (offset, data).
    pub(crate) fn committed_entries(&self) -> std::collections::btree_map::Iter<'_, u64, Vec<u8>> {
        self.committed.iter()
    }

    /// Returns an iterator over pending entries (offset, data).
    pub(crate) fn pending_entries(&self) -> std::collections::btree_map::Iter<'_, u64, Vec<u8>> {
        self.pending.iter()
    }

    /// Applies a modification layer onto a read buffer.
    ///
    /// For each entry in the layer that overlaps with the read range
    /// `[read_offset, read_offset + buf.len())`, the overlapping bytes
    /// are copied into `buf`.
    fn apply_layer(layer: &BTreeMap<u64, Vec<u8>>, read_offset: u64, buf: &mut [u8]) {
        let read_end = read_offset + buf.len() as u64;

        // Iterate entries that could overlap with our read range.
        // Any entry with offset < read_end could potentially overlap if
        // entry_offset + entry_data.len() > read_offset.
        for (&entry_offset, entry_data) in layer.range(..read_end) {
            let entry_end = entry_offset + entry_data.len() as u64;
            if entry_end <= read_offset {
                continue;
            }

            let overlap_start = read_offset.max(entry_offset);
            let overlap_end = read_end.min(entry_end);

            let buf_start = (overlap_start - read_offset) as usize;
            let entry_start = (overlap_start - entry_offset) as usize;
            let overlap_len = (overlap_end - overlap_start) as usize;

            buf[buf_start..buf_start + overlap_len]
                .copy_from_slice(&entry_data[entry_start..entry_start + overlap_len]);
        }
    }

    /// Merges a single write into the committed layer, handling overlaps.
    ///
    /// Finds all committed entries that overlap with `[write_offset, write_end)`,
    /// removes them, and creates a single merged entry spanning the union of
    /// the write and all overlapping committed regions. The write data takes
    /// priority where it overlaps with committed data.
    fn merge_into_committed(
        committed: &mut BTreeMap<u64, Vec<u8>>,
        write_offset: u64,
        write_data: &[u8],
    ) {
        let write_end = write_offset + write_data.len() as u64;

        // Collect all committed entries that overlap with this write.
        let overlapping: Vec<(u64, Vec<u8>)> = committed
            .range(..write_end)
            .filter(|(&off, data)| off + data.len() as u64 > write_offset)
            .map(|(&off, data)| (off, data.clone()))
            .collect();

        if overlapping.is_empty() {
            committed.insert(write_offset, write_data.to_vec());
            return;
        }

        // Remove overlapping entries.
        for (off, _) in &overlapping {
            committed.remove(off);
        }

        // Compute the merged region bounds.
        let mut merged_start = write_offset;
        let mut merged_end = write_end;

        for (off, data) in &overlapping {
            merged_start = merged_start.min(*off);
            merged_end = merged_end.max(*off + data.len() as u64);
        }

        let mut merged = vec![0u8; (merged_end - merged_start) as usize];

        // Apply existing committed entries first.
        for (off, data) in &overlapping {
            let dst_start = (*off - merged_start) as usize;
            merged[dst_start..dst_start + data.len()].copy_from_slice(data);
        }

        // Apply the write on top (write wins over committed).
        let dst_start = (write_offset - merged_start) as usize;
        merged[dst_start..dst_start + write_data.len()].copy_from_slice(write_data);

        committed.insert(merged_start, merged);
    }

    /// Consolidates committed entries by merging adjacent or overlapping regions.
    ///
    /// After this operation, all committed entries are non-overlapping and
    /// non-adjacent — there is at least a 1-byte gap between consecutive entries.
    fn consolidate(committed: &mut BTreeMap<u64, Vec<u8>>) {
        let entries: Vec<(u64, Vec<u8>)> = committed
            .iter()
            .map(|(&off, data)| (off, data.clone()))
            .collect();

        if entries.len() <= 1 {
            return;
        }

        committed.clear();

        let mut iter = entries.into_iter();
        let (mut cur_offset, mut cur_data) = iter.next().unwrap();

        for (next_offset, next_data) in iter {
            let cur_end = cur_offset + cur_data.len() as u64;
            if next_offset <= cur_end {
                // Overlapping or adjacent — merge.
                let new_end = (next_offset + next_data.len() as u64).max(cur_end);
                let new_len = (new_end - cur_offset) as usize;
                cur_data.resize(new_len, 0);
                let copy_start = (next_offset - cur_offset) as usize;
                let copy_len = next_data.len();
                cur_data[copy_start..copy_start + copy_len].copy_from_slice(&next_data);
            } else {
                // Gap — emit current, start new.
                committed.insert(cur_offset, cur_data);
                cur_offset = next_offset;
                cur_data = next_data;
            }
        }
        committed.insert(cur_offset, cur_data);
    }

    /// Applies all modifications (committed + pending) to the given base data,
    /// returning a fully materialized byte vector.
    ///
    /// This is used by [`CowFile::to_vec`](crate::CowFile::to_vec) and
    /// [`CowFile::to_file`](crate::CowFile::to_file) to produce the final output.
    pub(crate) fn materialize(&self, base: &[u8]) -> Vec<u8> {
        let mut output = base.to_vec();

        for (&offset, data) in &self.committed {
            let start = offset as usize;
            output[start..start + data.len()].copy_from_slice(data);
        }

        for (&offset, data) in &self.pending {
            let start = offset as usize;
            output[start..start + data.len()].copy_from_slice(data);
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use crate::overlay::Overlay;

    #[test]
    fn test_overlay_empty() {
        let overlay = Overlay::new();
        assert!(!overlay.has_pending());
        assert!(!overlay.has_modifications());
        assert_eq!(overlay.pending_count(), 0);
        assert_eq!(overlay.committed_count(), 0);
    }

    #[test]
    fn test_overlay_single_write() {
        let base = vec![0u8; 10];
        let mut overlay = Overlay::new();
        overlay.write(2, &[0xFF, 0xFE]);

        let result = overlay.read(0, 10, &base);
        assert_eq!(result, vec![0, 0, 0xFF, 0xFE, 0, 0, 0, 0, 0, 0]);
        assert!(overlay.has_pending());
        assert!(overlay.has_modifications());
        assert_eq!(overlay.pending_count(), 1);
    }

    #[test]
    fn test_overlay_multiple_nonoverlapping_writes() {
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA]);
        overlay.write(5, &[0xBB, 0xCC]);
        overlay.write(10, &[0xDD]);

        let result = overlay.read(0, 20, &base);
        assert_eq!(result[0], 0xAA);
        assert_eq!(result[5], 0xBB);
        assert_eq!(result[6], 0xCC);
        assert_eq!(result[10], 0xDD);
        assert_eq!(result[1], 0x00);
        assert_eq!(overlay.pending_count(), 3);
    }

    #[test]
    fn test_overlay_overlapping_writes_last_wins() {
        let base = vec![0u8; 10];
        let mut overlay = Overlay::new();
        overlay.write(2, &[0xAA, 0xBB, 0xCC]);
        overlay.write(3, &[0xFF]); // Overlaps byte at offset 3

        let result = overlay.read(0, 10, &base);
        assert_eq!(result[2], 0xAA);
        assert_eq!(result[3], 0xFF); // Last write wins
        assert_eq!(result[4], 0xCC);
    }

    #[test]
    fn test_overlay_same_offset_write_replaces() {
        let base = vec![0u8; 10];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA, 0xBB]);
        overlay.write(0, &[0xCC, 0xDD]); // Replaces previous at same offset

        let result = overlay.read(0, 10, &base);
        assert_eq!(result[0], 0xCC);
        assert_eq!(result[1], 0xDD);
        assert_eq!(overlay.pending_count(), 1);
    }

    #[test]
    fn test_overlay_commit_merges_pending() {
        let base = vec![0u8; 10];
        let mut overlay = Overlay::new();
        overlay.write(2, &[0xFF, 0xFE]);

        assert!(overlay.has_pending());
        overlay.commit();
        assert!(!overlay.has_pending());
        assert!(overlay.has_modifications());
        assert_eq!(overlay.committed_count(), 1);

        let result = overlay.read(0, 10, &base);
        assert_eq!(result[2], 0xFF);
        assert_eq!(result[3], 0xFE);
    }

    #[test]
    fn test_overlay_commit_empty_is_noop() {
        let mut overlay = Overlay::new();
        overlay.commit();
        assert!(!overlay.has_modifications());
    }

    #[test]
    fn test_overlay_commit_consolidates_adjacent() {
        let base = vec![0u8; 10];
        let mut overlay = Overlay::new();
        overlay.write(2, &[0xAA, 0xBB]);
        overlay.write(4, &[0xCC, 0xDD]); // Adjacent to previous
        overlay.commit();

        // Adjacent entries should be consolidated into one.
        assert_eq!(overlay.committed_count(), 1);

        let result = overlay.read(0, 10, &base);
        assert_eq!(result[2], 0xAA);
        assert_eq!(result[3], 0xBB);
        assert_eq!(result[4], 0xCC);
        assert_eq!(result[5], 0xDD);
    }

    #[test]
    fn test_overlay_commit_consolidates_overlapping() {
        let base = vec![0u8; 10];
        let mut overlay = Overlay::new();
        overlay.write(2, &[0xAA, 0xBB, 0xCC]);
        overlay.write(3, &[0xDD, 0xEE, 0xFF]); // Overlaps at offset 3-4
        overlay.commit();

        assert_eq!(overlay.committed_count(), 1);

        let result = overlay.read(0, 10, &base);
        assert_eq!(result[2], 0xAA);
        assert_eq!(result[3], 0xDD); // Pending write at offset 3 wins
        assert_eq!(result[4], 0xEE);
        assert_eq!(result[5], 0xFF);
    }

    #[test]
    fn test_overlay_multi_commit_cycle() {
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();

        // Pass 1
        overlay.write(0, &[0xAA]);
        overlay.write(10, &[0xBB]);
        overlay.commit();

        // Pass 2
        overlay.write(5, &[0xCC]);
        overlay.write(15, &[0xDD]);
        overlay.commit();

        let result = overlay.read(0, 20, &base);
        assert_eq!(result[0], 0xAA);
        assert_eq!(result[5], 0xCC);
        assert_eq!(result[10], 0xBB);
        assert_eq!(result[15], 0xDD);
    }

    #[test]
    fn test_overlay_pending_overwrites_committed() {
        let base = vec![0u8; 10];
        let mut overlay = Overlay::new();

        overlay.write(2, &[0xAA]);
        overlay.commit();

        overlay.write(2, &[0xFF]); // Pending overwrites committed

        let result = overlay.read(2, 1, &base);
        assert_eq!(result[0], 0xFF);
    }

    #[test]
    fn test_overlay_read_unmodified_returns_base() {
        let base = vec![1, 2, 3, 4, 5];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xFF]);

        let result = overlay.read(3, 2, &base);
        assert_eq!(result, vec![4, 5]);
    }

    #[test]
    fn test_overlay_read_partially_modified() {
        let base = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let mut overlay = Overlay::new();
        overlay.write(3, &[0xAA, 0xBB]);

        // Read range [2, 7) spans unmodified and modified data.
        let result = overlay.read(2, 5, &base);
        assert_eq!(result, vec![3, 0xAA, 0xBB, 6, 7]);
    }

    #[test]
    fn test_overlay_write_at_offset_zero() {
        let base = vec![1, 2, 3];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xFF]);

        let result = overlay.read(0, 3, &base);
        assert_eq!(result, vec![0xFF, 2, 3]);
    }

    #[test]
    fn test_overlay_write_at_end() {
        let base = vec![1, 2, 3, 4, 5];
        let mut overlay = Overlay::new();
        overlay.write(4, &[0xFF]);

        let result = overlay.read(0, 5, &base);
        assert_eq!(result, vec![1, 2, 3, 4, 0xFF]);
    }

    #[test]
    fn test_overlay_write_entire_file() {
        let base = vec![0u8; 4];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xDE, 0xAD, 0xBE, 0xEF]);

        let result = overlay.read(0, 4, &base);
        assert_eq!(result, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn test_overlay_empty_write_ignored() {
        let mut overlay = Overlay::new();
        overlay.write(5, &[]);
        assert!(!overlay.has_pending());
        assert_eq!(overlay.pending_count(), 0);
    }

    #[test]
    fn test_overlay_materialize() {
        let base = vec![0u8; 10];
        let mut overlay = Overlay::new();

        overlay.write(0, &[0xAA]);
        overlay.commit();
        overlay.write(5, &[0xBB]);

        let output = overlay.materialize(&base);
        assert_eq!(output[0], 0xAA);
        assert_eq!(output[5], 0xBB);
        assert_eq!(output[1], 0x00);
    }

    #[test]
    fn test_overlay_materialize_pending_over_committed() {
        let base = vec![0u8; 10];
        let mut overlay = Overlay::new();

        overlay.write(2, &[0xAA]);
        overlay.commit();
        overlay.write(2, &[0xFF]);

        let output = overlay.materialize(&base);
        assert_eq!(output[2], 0xFF);
    }

    #[test]
    fn test_overlay_commit_with_gap_between_entries() {
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();

        overlay.write(0, &[0xAA]);
        overlay.write(10, &[0xBB]);
        overlay.commit();

        // Entries with a gap should remain separate.
        assert_eq!(overlay.committed_count(), 2);

        let result = overlay.read(0, 20, &base);
        assert_eq!(result[0], 0xAA);
        assert_eq!(result[10], 0xBB);
    }

    #[test]
    fn test_overlay_second_commit_merges_with_existing() {
        let base = vec![0u8; 10];
        let mut overlay = Overlay::new();

        overlay.write(2, &[0xAA, 0xBB]);
        overlay.commit();

        // Write adjacent to the committed region.
        overlay.write(4, &[0xCC]);
        overlay.commit();

        // Should be consolidated into one region.
        assert_eq!(overlay.committed_count(), 1);

        let result = overlay.read(0, 10, &base);
        assert_eq!(result[2], 0xAA);
        assert_eq!(result[3], 0xBB);
        assert_eq!(result[4], 0xCC);
    }

    #[test]
    fn test_pending_containment_small_inside_large() {
        // Write [0..20), then [5..10) — second is completely contained.
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 20]);
        overlay.write(5, &[0xBB; 5]);

        let result = overlay.read(0, 20, &base);
        assert!(result[..5].iter().all(|&b| b == 0xAA));
        assert!(result[5..10].iter().all(|&b| b == 0xBB));
        assert!(result[10..20].iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn test_pending_containment_large_over_small() {
        // Write [5..10), then [0..20) — second completely covers first.
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(5, &[0xAA; 5]);
        overlay.write(0, &[0xBB; 20]);

        let result = overlay.read(0, 20, &base);
        assert!(result.iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn test_pending_overlap_at_start_boundary() {
        // Write [5..15), then [0..8) — second overlaps at the start.
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(5, &[0xAA; 10]);
        overlay.write(0, &[0xBB; 8]);

        let result = overlay.read(0, 20, &base);
        assert!(result[..8].iter().all(|&b| b == 0xBB));
        assert!(result[8..15].iter().all(|&b| b == 0xAA));
        assert!(result[15..20].iter().all(|&b| b == 0x00));
    }

    #[test]
    fn test_pending_overlap_at_end_boundary() {
        // Write [0..10), then [8..20) — second overlaps at the end.
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 10]);
        overlay.write(8, &[0xBB; 12]);

        let result = overlay.read(0, 20, &base);
        assert!(result[..8].iter().all(|&b| b == 0xAA));
        assert!(result[8..20].iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn test_three_pending_cascading_overlaps() {
        // Three overlapping writes: [0..10), [5..15), [12..20).
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 10]);
        overlay.write(5, &[0xBB; 10]);
        overlay.write(12, &[0xCC; 8]);

        let result = overlay.read(0, 20, &base);
        assert!(result[..5].iter().all(|&b| b == 0xAA));
        assert!(result[5..12].iter().all(|&b| b == 0xBB));
        assert!(result[12..20].iter().all(|&b| b == 0xCC));
    }

    #[test]
    fn test_pending_before_committed_partial_overlap() {
        // Committed [5..15), then pending [0..10) — pending overlaps from left.
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(5, &[0xAA; 10]);
        overlay.commit();
        overlay.write(0, &[0xBB; 10]);

        let result = overlay.read(0, 20, &base);
        assert!(result[..10].iter().all(|&b| b == 0xBB));
        assert!(result[10..15].iter().all(|&b| b == 0xAA));
        assert!(result[15..20].iter().all(|&b| b == 0x00));
    }

    #[test]
    fn test_pending_after_committed_partial_overlap() {
        // Committed [0..10), then pending [5..20) — pending overlaps from right.
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 10]);
        overlay.commit();
        overlay.write(5, &[0xBB; 15]);

        let result = overlay.read(0, 20, &base);
        assert!(result[..5].iter().all(|&b| b == 0xAA));
        assert!(result[5..20].iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn test_pending_completely_contains_committed() {
        // Committed [5..10), then pending [0..20) covers it entirely.
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(5, &[0xAA; 5]);
        overlay.commit();
        overlay.write(0, &[0xBB; 20]);

        let result = overlay.read(0, 20, &base);
        assert!(result.iter().all(|&b| b == 0xBB));

        // After commit, everything should still be 0xBB.
        overlay.commit();
        let output = overlay.materialize(&base);
        assert!(output.iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn test_committed_completely_contains_pending() {
        // Committed [0..20), then pending [5..10) is entirely inside.
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 20]);
        overlay.commit();
        overlay.write(5, &[0xBB; 5]);

        let result = overlay.read(0, 20, &base);
        assert!(result[..5].iter().all(|&b| b == 0xAA));
        assert!(result[5..10].iter().all(|&b| b == 0xBB));
        assert!(result[10..20].iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn test_read_spanning_multiple_pending_entries() {
        // Pending [0..5), [10..15); read [2..12) crosses both plus a gap.
        let base: Vec<u8> = (0..20).collect();
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 5]);
        overlay.write(10, &[0xBB; 5]);

        let result = overlay.read(2, 10, &base);
        // [2..5) from pending 0xAA, [5..10) from base, [10..12) from pending 0xBB.
        assert!(result[..3].iter().all(|&b| b == 0xAA));
        assert_eq!(&result[3..8], &[5, 6, 7, 8, 9]);
        assert!(result[8..10].iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn test_read_spanning_committed_and_pending() {
        // Committed [0..5), pending [10..15); read [3..13) crosses both layers.
        let base: Vec<u8> = (0..20).collect();
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 5]);
        overlay.commit();
        overlay.write(10, &[0xBB; 5]);

        let result = overlay.read(3, 10, &base);
        // [3..5) from committed 0xAA, [5..10) from base, [10..13) from pending 0xBB.
        assert!(result[..2].iter().all(|&b| b == 0xAA));
        assert_eq!(&result[2..7], &[5, 6, 7, 8, 9]);
        assert!(result[7..10].iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn test_read_spanning_overlapping_committed_and_pending() {
        // Committed [5..10), pending [7..12) (overlap at [7..10)); read [4..13).
        let base: Vec<u8> = (0..20).collect();
        let mut overlay = Overlay::new();
        overlay.write(5, &[0xAA; 5]);
        overlay.commit();
        overlay.write(7, &[0xBB; 5]);

        let result = overlay.read(4, 9, &base);
        // [4..5) from base=4, [5..7) from committed=0xAA, [7..12) from pending=0xBB, [12..13) from base=12.
        assert_eq!(result[0], 4);
        assert!(result[1..3].iter().all(|&b| b == 0xAA));
        assert!(result[3..8].iter().all(|&b| b == 0xBB));
        assert_eq!(result[8], 12);
    }

    #[test]
    fn test_commit_three_overlapping_entries() {
        // Three overlapping pending entries consolidated into one committed.
        let base = vec![0u8; 30];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 10]);
        overlay.write(5, &[0xBB; 10]);
        overlay.write(12, &[0xCC; 8]);
        overlay.commit();

        // All three should consolidate: [0..20) is contiguous.
        assert_eq!(overlay.committed_count(), 1);

        let result = overlay.read(0, 30, &base);
        assert!(result[..5].iter().all(|&b| b == 0xAA));
        assert!(result[5..12].iter().all(|&b| b == 0xBB));
        assert!(result[12..20].iter().all(|&b| b == 0xCC));
        assert!(result[20..30].iter().all(|&b| b == 0x00));
    }

    #[test]
    fn test_pending_spanning_multiple_committed_entries() {
        // Committed [0..5), [10..15), [20..25); pending [3..22) spans all three.
        let base = vec![0u8; 30];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 5]);
        overlay.write(10, &[0xAA; 5]);
        overlay.write(20, &[0xAA; 5]);
        overlay.commit();
        assert_eq!(overlay.committed_count(), 3);

        overlay.write(3, &[0xBB; 19]);

        let result = overlay.read(0, 30, &base);
        assert!(result[..3].iter().all(|&b| b == 0xAA));
        assert!(result[3..22].iter().all(|&b| b == 0xBB));
        assert!(result[22..25].iter().all(|&b| b == 0xAA));
        assert!(result[25..30].iter().all(|&b| b == 0x00));

        // After commit, verify consolidation.
        overlay.commit();
        let output = overlay.materialize(&base);
        assert!(output[..3].iter().all(|&b| b == 0xAA));
        assert!(output[3..22].iter().all(|&b| b == 0xBB));
        assert!(output[22..25].iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn test_pending_between_two_committed_with_gaps() {
        // Committed [0..5), [15..20); pending [8..12) sits between with gaps.
        let base = vec![0u8; 25];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 5]);
        overlay.write(15, &[0xAA; 5]);
        overlay.commit();

        overlay.write(8, &[0xBB; 4]);
        overlay.commit();

        // Three separate committed entries.
        assert_eq!(overlay.committed_count(), 3);

        let result = overlay.read(0, 25, &base);
        assert!(result[..5].iter().all(|&b| b == 0xAA));
        assert!(result[5..8].iter().all(|&b| b == 0x00));
        assert!(result[8..12].iter().all(|&b| b == 0xBB));
        assert!(result[12..15].iter().all(|&b| b == 0x00));
        assert!(result[15..20].iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn test_one_byte_gap_vs_adjacent() {
        let base = vec![0u8; 20];

        // Adjacent (zero-byte gap) → should consolidate.
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 5]);
        overlay.write(5, &[0xBB; 5]);
        overlay.commit();
        assert_eq!(overlay.committed_count(), 1);

        // One-byte gap → should NOT consolidate.
        let mut overlay2 = Overlay::new();
        overlay2.write(0, &[0xAA; 5]);
        overlay2.write(6, &[0xBB; 5]);
        overlay2.commit();
        assert_eq!(overlay2.committed_count(), 2);

        let result = overlay2.read(0, 20, &base);
        assert!(result[..5].iter().all(|&b| b == 0xAA));
        assert_eq!(result[5], 0x00); // The gap byte stays as base.
        assert!(result[6..11].iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn test_single_byte_overlap_at_boundary() {
        // Two writes that overlap at exactly one byte.
        let base = vec![0u8; 10];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 5]); // [0..5)
        overlay.write(4, &[0xBB; 5]); // [4..9) — overlap at byte 4

        let result = overlay.read(0, 10, &base);
        assert!(result[..4].iter().all(|&b| b == 0xAA));
        assert!(result[4..9].iter().all(|&b| b == 0xBB)); // Later write wins.
        assert_eq!(result[9], 0x00);
    }

    #[test]
    fn test_same_range_different_data() {
        // Same exact range [5..10), written twice with different data.
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(5, &[0xAA; 5]);
        overlay.commit();
        overlay.write(5, &[0xBB; 5]); // Exact same range, different data.

        let result = overlay.read(5, 5, &base);
        assert!(result.iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn test_commit_merges_pending_overlapping_multiple_committed() {
        // Committed: [0..5)=0xAA, [10..15)=0xBB.
        // Pending: [3..12)=0xCC — overlaps both.
        let base = vec![0u8; 20];
        let mut overlay = Overlay::new();
        overlay.write(0, &[0xAA; 5]);
        overlay.write(10, &[0xBB; 5]);
        overlay.commit();

        overlay.write(3, &[0xCC; 9]);
        overlay.commit();

        // After commit, all three should consolidate into one.
        assert_eq!(overlay.committed_count(), 1);

        let result = overlay.read(0, 20, &base);
        assert!(result[..3].iter().all(|&b| b == 0xAA));
        assert!(result[3..12].iter().all(|&b| b == 0xCC));
        assert!(result[12..15].iter().all(|&b| b == 0xBB));
        assert!(result[15..20].iter().all(|&b| b == 0x00));
    }
}
