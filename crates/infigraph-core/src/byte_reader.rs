//! Bounds-checked cursor over the little-endian sidecar formats this
//! workspace writes and reads back off disk (`embeddings.bin`, its
//! `.sidecar`, and both BM25 caches).
//!
//! These formats are all length-prefixed, and every reader had open-coded
//! the same three operations -- take N bytes, read a fixed-width integer,
//! read a length-prefixed string -- with three different levels of rigour:
//! `infigraph-docs::search` bounds-checked everything through local
//! closures, `embed` checked each entry with its own `ensure!`, and
//! `infigraph-core::search` checked nothing and sliced raw. Divergent copies
//! of one routine are how a fix reaches one of them and not the others,
//! which is exactly what happened (see [`ByteReader::count`]).
//!
//! The contract, borrowed verbatim from the one reader that already had it
//! right: **any structural problem is an `Err`, never a panic and never an
//! abort.** Callers treat a bad sidecar as a cache miss and rebuild it.

use anyhow::{bail, Result};

/// A forward-only, bounds-checked cursor over `data`. `what` names the
/// format in error messages, so a failure says which file was malformed.
pub struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
    what: &'static str,
}

impl<'a> ByteReader<'a> {
    pub fn new(data: &'a [u8], what: &'static str) -> Self {
        Self { data, pos: 0, what }
    }

    /// Start at `pos`, for a format whose fixed header the caller already
    /// parsed (e.g. a magic/version probe that decides the layout).
    pub fn at(data: &'a [u8], pos: usize, what: &'static str) -> Self {
        Self {
            data,
            pos: pos.min(data.len()),
            what,
        }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Advance over `n` bytes, or fail. The single bounds check every other
    /// method funnels through -- `checked_add` so a corrupt length near
    /// `usize::MAX` cannot wrap the comparison into looking valid.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = match self.pos.checked_add(n) {
            Some(e) if e <= self.data.len() => e,
            _ => bail!(
                "{}: truncated — wanted {n} bytes at offset {}, only {} remain",
                self.what,
                self.pos,
                self.remaining()
            ),
        };
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    /// A `u32`-length-prefixed string, decoded lossily -- for payloads where
    /// invalid UTF-8 is cosmetic (a search term, a document id) and losing
    /// the whole cache over one bad byte would be the worse outcome.
    pub fn string_u32(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        Ok(String::from_utf8_lossy(self.bytes(len)?).into_owned())
    }

    /// A `u32`-length-prefixed string that must be valid UTF-8 -- for
    /// payloads where a mangled value would silently fail to match later
    /// (an embedding's symbol id is a lookup key, not display text).
    pub fn utf8_u32(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        let raw = self.bytes(len)?;
        match std::str::from_utf8(raw) {
            Ok(s) => Ok(s.to_string()),
            Err(e) => bail!(
                "{}: invalid utf8 at offset {}: {e}",
                self.what,
                self.pos - len
            ),
        }
    }

    /// Read a `u32` element count and reject one the remaining bytes could
    /// not possibly describe -- **before** the caller reserves capacity for
    /// it. `min_entry_bytes` is the smallest an entry can encode to (its
    /// length prefixes, with every variable-length part empty).
    ///
    /// This method is why this type exists. Every loader here used to read a
    /// count straight off disk and hand it to `with_capacity`, so a corrupt
    /// file named an arbitrary number of entries and the allocator was asked
    /// for whatever that implied. glibc refuses outright and Rust's
    /// allocation-failure handler aborts the process -- before any of the
    /// per-entry checks run, with no `Result` for a caller to catch. macOS
    /// overcommits and satisfies the reservation lazily, which is why it
    /// only ever surfaced on Linux.
    ///
    /// Observed for real, twice, in one CI run: `b"garbage"` reads "garb" as
    /// 1_651_663_207 entries of 56 bytes, which is exactly the
    /// 92,493,139,592-byte allocation that killed the `verify` suite.
    ///
    /// The bound is derived rather than a magic clamp: it is what this file's
    /// own remaining length permits, so it cannot reject a valid file and
    /// cannot admit one that would over-allocate.
    pub fn count(&mut self, min_entry_bytes: usize) -> Result<usize> {
        let n = self.u32()? as usize;
        let max = self.remaining() / min_entry_bytes.max(1);
        if n > max {
            bail!(
                "{}: claims {n} entries but only {} bytes remain (at most {max}) — corrupt",
                self.what,
                self.remaining()
            );
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian_scalars_in_order() {
        let mut buf = vec![7u8];
        buf.extend_from_slice(&1234u32.to_le_bytes());
        buf.extend_from_slice(&5678u64.to_le_bytes());
        buf.extend_from_slice(&1.5f32.to_le_bytes());
        let mut r = ByteReader::new(&buf, "test");
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u32().unwrap(), 1234);
        assert_eq!(r.u64().unwrap(), 5678);
        assert_eq!(r.f32().unwrap(), 1.5);
        assert!(r.is_empty());
    }

    #[test]
    fn a_read_past_the_end_is_an_error_not_a_panic() {
        let mut r = ByteReader::new(&[1, 2], "test");
        assert!(r.u32().is_err(), "wanted 4 bytes with only 2 available");
    }

    /// The length prefix is attacker/corruption-controlled, so a huge one
    /// must not wrap `pos + len` into looking in-bounds.
    #[test]
    fn a_length_near_usize_max_cannot_wrap_the_bounds_check() {
        let mut buf = (u32::MAX).to_le_bytes().to_vec();
        buf.extend_from_slice(b"hi");
        let mut r = ByteReader::new(&buf, "test");
        assert!(r.string_u32().is_err());
    }

    /// The regression this whole type exists for.
    #[test]
    fn a_count_beyond_the_remaining_bytes_is_rejected_before_allocating() {
        // Claims u32::MAX entries, then supplies nothing to back it up.
        let buf = (u32::MAX).to_le_bytes().to_vec();
        let mut r = ByteReader::new(&buf, "test cache");
        let err = r.count(8).expect_err("a bogus count must not be trusted");
        assert!(err.to_string().contains("corrupt"), "got: {err}");
    }

    /// The exact bytes and arithmetic from the CI abort: "garb" as a count
    /// of 56-byte entries asked for 92,493,139,592 bytes.
    #[test]
    fn the_garbage_count_that_aborted_ci_is_rejected() {
        let mut r = ByteReader::new(b"garbage", "embeddings");
        assert_eq!(
            u32::from_le_bytes(*b"garb") as usize,
            1_651_663_207,
            "pinning the count the real file produced"
        );
        assert!(r.count(8).is_err());
    }

    #[test]
    fn a_count_the_bytes_can_actually_hold_is_accepted() {
        let mut buf = 2u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 16]); // 2 entries * 8 bytes
        let mut r = ByteReader::new(&buf, "test");
        assert_eq!(r.count(8).unwrap(), 2);
    }

    #[test]
    fn utf8_is_strict_where_ids_are_lookup_keys_and_lossy_where_cosmetic() {
        let mut buf = 2u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0xff, 0xfe]);
        assert!(ByteReader::new(&buf, "t").utf8_u32().is_err());
        assert!(ByteReader::new(&buf, "t").string_u32().is_ok());
    }
}
