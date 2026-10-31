//! IG-1 regression: overlapping stored zip entries must be refused before any entry is
//! materialized. The first guard copied every stored entry and checked overlap afterwards;
//! the audit's 2.36 MB sample aborted the process that way. A counting global
//! allocator bounds the peak heap growth of the inspection.
//! The audit sample used 3,000 entries; this test uses 200 so CRC set-up stays fast.
use excalibur_ingest::{Code, ContentGuard};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Counting;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// `count` stored entries, each starting at its own local header and claiming every byte up
/// to the central directory, with valid CRCs and sizes.
fn overlapping_stored_zip(count: usize, payload: usize) -> Vec<u8> {
    let name = |i: usize| format!("f{i:05}");
    let header = 30 + name(0).len();
    let mut body = Vec::new();
    for i in 0..count {
        body.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        body.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        body.extend_from_slice(&[0; 12]); // crc, csize, size: patched below
        body.extend_from_slice(&(name(i).len() as u16).to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(name(i).as_bytes());
    }
    body.extend((0..payload).map(|i| (i % 251) as u8));
    let cd_offset = body.len();
    let mut fields = Vec::with_capacity(count);
    for i in 0..count {
        let start = i * header + header;
        let data = &body[start..cd_offset];
        fields.push((crc32fast::hash(data), data.len() as u32));
    }
    for (i, (crc, len)) in fields.iter().enumerate() {
        let at = i * header + 14;
        body[at..at + 4].copy_from_slice(&crc.to_le_bytes());
        body[at + 4..at + 8].copy_from_slice(&len.to_le_bytes());
        body[at + 8..at + 12].copy_from_slice(&len.to_le_bytes());
    }
    let mut central = Vec::new();
    for (i, (crc, len)) in fields.iter().enumerate() {
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&(name(i).len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&((i * header) as u32).to_le_bytes());
        central.extend_from_slice(name(i).as_bytes());
    }
    let mut out = body;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(count as u16).to_le_bytes());
    out.extend_from_slice(&(count as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&(cd_offset as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[test]
fn overlapping_stored_entries_are_refused_with_bounded_memory() {
    // 200 entries keep the CRC set-up cheap; copying them would still need about 400 MiB.
    let input = overlapping_stored_zip(200, 2 * 1024 * 1024);
    assert!(input.len() < 3 * 1024 * 1024);
    let guard = ContentGuard::default();
    let before = CURRENT.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    let r = guard
        .examine(Some("bomb.zip"), "application/zip", &input)
        .unwrap_err();
    let growth = PEAK.load(Ordering::SeqCst) - before;
    assert_eq!(r.code, Code::Malformed, "{r}");
    assert!(r.message.contains("overlapping"), "{r}");
    assert!(
        growth < 16 * 1024 * 1024,
        "inspection grew the heap by {growth} bytes"
    );
}
