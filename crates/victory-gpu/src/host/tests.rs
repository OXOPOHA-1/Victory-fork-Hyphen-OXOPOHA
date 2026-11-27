use super::{Access, Plan, StepKind};

fn plan_of(access: &[Access]) -> (Vec<usize>, [[u64; 3]; 3]) {
    let mut plan = Plan::default();
    plan.access.extend_from_slice(access);
    plan.plan();
    (plan.starts.clone(), plan.cuts)
}

fn starts(access: &[Access]) -> Vec<usize> {
    plan_of(access).0
}

const PAGE: usize = 64 << 10;

/// The conflict test on byte ranges, at its edges: half-open ranges,
/// adjacency, nesting, identical and empty ranges, chains, a write
/// after a read, ranges that end past the address space.
#[test]
fn parallel_plan_cuts_exactly_where_bytes_are_shared() {
    let copy = Access::copy;
    // Apart: one group.
    assert_eq!(starts(&[copy(0x1000, 0x8000, 0x100), copy(0x2000, 0x9000, 0x100)]), [0]);
    // Adjacent destinations, and a read that begins where a write ends.
    assert_eq!(starts(&[copy(0x100, 0x8000, 0x100), copy(0x200, 0x9000, 0x100)]), [0]);
    assert_eq!(starts(&[copy(0x100, 0x8000, 0x100), copy(0x5000, 0x200, 0x10)]), [0]);
    // The last byte written is the first read: a conflict.
    assert_eq!(starts(&[copy(0x2000, 0x1000, 0x100), copy(0x5000, 0x20ff, 0x10)]), [0, 1]);
    // Nested and identical destinations.
    assert_eq!(starts(&[copy(0x100, 0x8000, 0x300), copy(0x200, 0x9000, 0x10)]), [0, 1]);
    assert_eq!(starts(&[copy(0x200, 0x9000, 0x10), copy(0x100, 0x8000, 0x300)]), [0, 1]);
    assert_eq!(starts(&[copy(0x100, 0x8000, 0x100), copy(0x100, 0x9000, 0x100)]), [0, 1]);
    // Two that read the same bytes do not conflict.
    assert_eq!(starts(&[copy(0x2000, 0x1000, 0x100), copy(0x3000, 0x1000, 0x100)]), [0]);
    // A to B, then B to C: the second reads what the first writes.
    assert_eq!(starts(&[copy(0x2000, 0x1000, 0x100), copy(0x3000, 0x2000, 0x100)]), [0, 1]);
    // C to A after A to B: the second writes what the first reads.
    assert_eq!(starts(&[copy(0x2000, 0x1000, 0x100), copy(0x1000, 0x3000, 0x100)]), [0, 1]);
    // A copy over its own bytes conflicts with nothing else.
    assert_eq!(starts(&[copy(0x2000, 0x2010, 0x100), copy(0x5000, 0x6000, 0x10)]), [0]);
    // A source spanning a destination that is not the nearest one.
    assert_eq!(
        starts(&[copy(0x1000, 0x9000, 0x10), copy(0x1100, 0x9100, 0x10), copy(0x8000, 0x0f00, 0x400)]),
        [0, 2]
    );
    // Empty steps touch nothing.
    assert_eq!(starts(&[copy(0x100, 0x2000, 0), copy(0x2000, 0x100, 0x100), copy(0x100, 0x2000, 0)]), [0]);
    // Greedy: a conflict with a step of an earlier group cuts nothing.
    // 0 and 1 write the same bytes; 2 reads them (conflict with 1);
    // 3 is apart; 4 reads them again (only reads in its group).
    let (at, cuts) = plan_of(&[
        copy(0x100, 0x9000, 4),
        copy(0x100, 0x9100, 4),
        copy(0x7000, 0x100, 4),
        copy(0x7100, 0x9200, 4),
        copy(0x7200, 0x100, 4),
    ]);
    assert_eq!(at, [0, 1, 2]);
    assert_eq!(cuts[StepKind::Copy as usize][StepKind::Copy as usize], 2);
    // Ends past the address space are numbers, not overflows.
    let top = usize::MAX - 4;
    assert_eq!(starts(&[copy(top, 0x10, 100), copy(0x10_000, usize::MAX - 1, 10)]), [0, 1]);
    assert_eq!(starts(&[copy(top, 0x10, 100), copy(0x10_000, 0x20, 10)]), [0]);
    let near_top = Access::texture((usize::MAX - 10, 100), (0x40, 8), PAGE);
    assert!(near_top.read.1 > usize::MAX as u128);
    assert_eq!(starts(&[near_top, copy(usize::MAX - 2, 0x10, 1)]), [0, 1]);
}

/// The same rule for fills and texture refreshes, and runs of all three.
#[test]
fn parallel_plan_covers_fills_and_textures() {
    let (copy, fill) = (Access::copy, Access::fill);
    let tex = |memory: (usize, usize), staging: usize| Access::texture(memory, (staging, 0x200), PAGE);
    // A fill reads what a copy wrote, or a copy writes what a fill reads.
    assert_eq!(starts(&[copy(0x100, 0x9000, 0x10), fill(0x900_000, 0x108, 4)]), [0, 1]);
    assert_eq!(starts(&[fill(0x900_000, 0x108, 4), copy(0x100, 0x9000, 0x10)]), [0, 1]);
    assert_eq!(starts(&[fill(0x900_000, 0x200, 4), copy(0x100, 0x9000, 0x10), fill(0x900_010, 0x9000, 4)]), [0]);
    // Fills into the same upload bytes.
    assert_eq!(starts(&[fill(0x900_000, 0x200, 8), fill(0x900_004, 0x300, 8)]), [0, 1]);
    // A refresh after a copy: the journal learns of the copy only after
    // its group, so even far apart that is a cut.
    let (_, cuts) = plan_of(&[copy(0x100, 0x9000, 0x10), tex((0x4000_0000, 0x1000), 0x7000_0000)]);
    assert_eq!(cuts[StepKind::Copy as usize][StepKind::Texture as usize], 1);
    // A refresh before a copy: only by the pages they share.
    assert_eq!(starts(&[tex((0x4000_0000, 0x1000), 0x7000_0000), copy(0x4001_0000, 0x9000, 0x10)]), [0]);
    assert_eq!(starts(&[tex((0x4000_0000, 0x1000), 0x7000_0000), copy(0x4000_f000, 0x9000, 0x10)]), [0, 1]);
    assert_eq!(starts(&[tex((0x4000_0000, 0x1000), 0x7000_0000), copy(0x100, 0x4000_ffff, 1)]), [0]);
    // The same staging object twice; two over the same memory.
    assert_eq!(starts(&[tex((0x4000_0000, 0x1000), 0x7000_0000), tex((0x5000_0000, 0x10), 0x7000_0100)]), [0, 1]);
    assert_eq!(starts(&[tex((0x4000_0000, 0x1000), 0x7000_0000), tex((0x4000_0000, 0x1000), 0x7000_0200)]), [0]);
    // Refreshes and fills read; only writes order them.
    assert_eq!(starts(&[tex((0x4000_0000, 0x1000), 0x7000_0000), fill(0x900_000, 0x4000_0000, 8)]), [0]);
    // An empty texture reads nothing.
    assert_eq!(Access::texture((0x4000_0000, 0), (0x10, 8), PAGE).read, (0, 0));
}

/// Random runs of copy-, fill- and texture-like steps over a small arena
/// with heavy overlap: run through [`Plan::plan`] and the worker threads
/// (`crate::shadow::wide_run`), with the journal noted after each
/// group as the real path does, they leave the arena byte for byte as
/// running them in order does. Each plan is also checked against the
/// rule pair by pair: no two steps of a group conflict, and each group
/// but the first begins with a step that conflicts with the one before.
#[test]
fn parallel_plan_random_runs_equal_serial() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering::Relaxed};
    const TITLE: usize = 160;
    const UPLOAD: (usize, usize) = (160, 64);
    const SLOT: usize = 24;
    const SLOTS: usize = 6;
    const ARENA: usize = 224 + SLOT * SLOTS;
    #[derive(Clone, Copy, Debug)]
    enum T {
        Copy(usize, usize, usize),
        Fill(usize, usize, usize),
        Tex(usize, usize, usize),
    }
    struct Arena {
        bytes: Vec<AtomicU8>,
        journal: AtomicUsize,
    }
    fn run(a: &Arena, t: T) {
        let get = |i: usize| a.bytes[i].load(Relaxed);
        let put = |i: usize, v: u8| {
            a.bytes[i].store(v, Relaxed);
            if i.is_multiple_of(5) {
                std::thread::yield_now();
            }
        };
        match t {
            // As a move: read whole, then written.
            T::Copy(d, s, n) | T::Fill(d, s, n) => {
                let held: Vec<u8> = (0..n).map(|j| get(s + j)).collect();
                for (j, v) in held.into_iter().enumerate() {
                    put(d + j, v);
                }
            }
            T::Tex(slot, s, n) => {
                let seen = a.journal.load(Relaxed) as u8;
                let at = 224 + slot * SLOT;
                for j in 0..SLOT {
                    let v = if j < n { get(s + j) } else { 0 };
                    put(at + j, v.wrapping_add(seen).wrapping_add(j as u8));
                }
            }
        }
    }
    fn access(t: T, grain: usize) -> Access {
        match t {
            T::Copy(d, s, n) => Access::copy(d, s, n),
            T::Fill(d, s, n) => Access::fill(d, s, n),
            T::Tex(slot, s, n) => Access::texture((s, n), (224 + slot * SLOT, SLOT), grain),
        }
    }
    fn overlap(a: (u128, u128), b: (u128, u128)) -> bool {
        a.0 < a.1 && b.0 < b.1 && a.0 < b.1 && b.0 < a.1
    }
    fn conflict(x: &Access, y: &Access) -> bool {
        overlap(x.write, y.read)
            || overlap(x.write, y.write)
            || overlap(x.read, y.write)
            || (x.kind == StepKind::Copy && y.kind == StepKind::Texture)
    }
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move |below: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % below as u64) as usize
    };
    let mut plan = Plan::default();
    let (mut beside, mut cut) = (0usize, 0usize);
    for round in 0..3000 {
        let grain = if round % 2 == 0 { 1 } else { 8 };
        let n = 2 + next(60);
        let steps: Vec<T> = (0..n)
            .map(|_| {
                let len = next(25);
                match next(3) {
                    0 => T::Copy(next(TITLE - len + 1), next(TITLE - len + 1), len),
                    1 => T::Fill(UPLOAD.0 + next(UPLOAD.1 - len + 1), next(TITLE - len + 1), len),
                    _ => T::Tex(next(SLOTS), next(TITLE - len + 1), len),
                }
            })
            .collect();
        let fresh = || {
            let arena = Arena { bytes: (0..ARENA).map(|i| AtomicU8::new((i * 7 + 3) as u8)).collect(), journal: AtomicUsize::new(0) };
            Arc::new(arena)
        };
        // In order, the journal noted after each copy that moved bytes.
        let serial = fresh();
        for &t in &steps {
            run(&serial, t);
            if let T::Copy(_, _, len) = t
                && len > 0
            {
                serial.journal.fetch_add(1, Relaxed);
            }
        }
        // Planned as the real path plans: empty copies and fills left out.
        let live: Vec<T> = steps
            .iter()
            .copied()
            .filter(|t| !matches!(t, T::Copy(_, _, 0) | T::Fill(_, _, 0)))
            .collect();
        plan.access.clear();
        plan.access.extend(live.iter().map(|&t| access(t, grain)));
        plan.plan();
        let groups: Vec<std::ops::Range<usize>> = plan.groups().collect();
        for (k, g) in groups.iter().enumerate() {
            for i in g.clone() {
                for j in i + 1..g.end {
                    assert!(!conflict(&plan.access[i], &plan.access[j]), "round {round}: {i} and {j} in one group");
                }
            }
            if k > 0 {
                let prev = &groups[k - 1];
                assert!(
                    prev.clone().any(|i| conflict(&plan.access[i], &plan.access[g.start])),
                    "round {round}: group {k} needlessly cut"
                );
                cut += 1;
            }
            if g.len() >= 2 {
                beside += g.len();
            }
        }
        let wide = fresh();
        for g in &groups {
            let jobs: Vec<Box<dyn FnOnce() + Send>> = g
                .clone()
                .map(|i| {
                    let (arena, t) = (wide.clone(), live[i]);
                    Box::new(move || run(&arena, t)) as Box<dyn FnOnce() + Send>
                })
                .collect();
            crate::shadow::wide_run("test steps", jobs);
            for i in g.clone() {
                if let T::Copy(..) = live[i] {
                    wide.journal.fetch_add(1, Relaxed);
                }
            }
        }
        let bytes = |a: &Arena| a.bytes.iter().map(|b| b.load(Relaxed)).collect::<Vec<u8>>();
        assert_eq!(bytes(&serial), bytes(&wide), "round {round}: {steps:?}");
    }
    // The runs were dense enough to test both sides.
    assert!(beside > 10_000 && cut > 10_000, "{beside} steps beside others, {cut} cuts");
}

/// Run once with VICTORY_CARD_SYNC_BATCH=0 and once with it set to 1.
#[test]
#[ignore = "requires a D3D12 adapter and the card hash pipeline; run alone"]
fn two_dirty_card_buffers_reach_readback_in_the_same_submission() {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    const MIB: usize = 1 << 20;
    const PIECE: usize = 64 << 10;
    let host = super::host().expect("a D3D12 device");
    assert!(host.card_ready(), "the card hash pipeline must be available");
    // SAFETY: a private allocation held through the fenced submissions.
    let allocation = unsafe {
        VirtualAlloc(std::ptr::null(), 2 * MIB, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE)
    };
    assert!(!allocation.is_null());
    let bases = [allocation as usize, allocation as usize + MIB];
    let shadows = bases.map(|base| {
        crate::shadow::register(base, base as u64, MIB as u64);
        let copy = crate::shadow::find(base as u64, MIB as u64).unwrap();
        assert!(copy.carded(), "both title buffers must have a card copy");
        copy
    });
    let readback = host.read_back_buffer(16).unwrap();
    let hash_readback = host.read_back_buffer(64).unwrap();
    let mut previous = [0u128; 4];
    for round in [0x11u32, 0x55] {
        let list = host.new_list().unwrap();
        let mut expected = [0u32; 4];
        for (i, (shadow, &base)) in shadows.iter().zip(&bases).enumerate() {
            for (j, offset) in [32, PIECE + 20].into_iter().enumerate() {
                let k = 2 * i + j;
                expected[k] = round * 0x0101_0101 + k as u32;
                // SAFETY: aligned dword in the private allocation.
                unsafe { ((base + offset) as *mut u32).write_volatile(expected[k]); }
                shadow.refresh(&list, (base + offset) as u64, 4);
                // Request the before-hash for each of the two distinct pieces.
                shadow.gpu_writes(&list, (base + offset) as u64, 4);
                // SAFETY: the readback and the copy outlive this fenced list.
                unsafe { list.list.CopyBufferRegion(&readback, (k * 4) as u64, &shadow.buffer, offset as u64, 4); }
            }
        }
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: execute waited for all four writes, and the buffer holds 16 bytes.
        let got = unsafe {
            readback.Map(0, None, Some(&mut mapped)).unwrap();
            let got = mapped.cast::<[u32; 4]>().read_unaligned();
            readback.Unmap(0, None);
            got
        };
        assert_eq!(got, expected, "both copied pieces of each buffer");
        let hash_list = host.new_list().unwrap();
        for (i, shadow) in shadows.iter().enumerate() {
            let hashes = shadow.card_sides().unwrap().1.expect("before-hashes were made");
            // SAFETY: two consecutive 16-byte piece hashes from each live buffer.
            unsafe { hash_list.list.CopyBufferRegion(&hash_readback, (32 * i) as u64, &hashes, 0, 32); }
        }
        hash_list.close();
        host.execute(&[&hash_list]);
        // SAFETY: all 64 bytes of the readback were written and fenced.
        let current = unsafe {
            hash_readback.Map(0, None, Some(&mut mapped)).unwrap();
            let words = mapped.cast::<[u128; 4]>().read_unaligned();
            hash_readback.Unmap(0, None);
            words
        };
        for (i, hash) in current.iter().enumerate() {
            assert_ne!(*hash, previous[i], "piece {i} must be rehashed");
            assert!(!current[..i].contains(hash), "each piece has different input");
        }
        previous = current;
    }
    for base in bases {
        crate::shadow::unregister(base, base as u64);
    }
    drop(shadows);
    // SAFETY: the lists completed and the copies were unregistered.
    unsafe { VirtualFree(allocation, 0, MEM_RELEASE); }
}

#[test]
fn sparse_copies_preserve_islands_and_observe_remaps() {
    use windows_sys::Win32::System::Memory::*;
    const N: usize = 5 << 16;
    // SAFETY: we reserve a private region, commit selected pages inside it,
    // and only directly access the pages while they are writable.
    unsafe {
        let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
        assert!(!base.is_null());
        let mut expected = vec![0; N];
        for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
            let at = base.add(offset);
            assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
            at.write_bytes(value, 4096);
            expected[offset..offset + 4096].fill(value);
        }
        for remap in [false, true] {
            if remap {
                let mut old = 0;
                assert_ne!(VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old), 0);
                expected[..4096].fill(0);
                let at = base.add(1 << 16);
                assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                at.write_bytes(0xc7, 4096);
                expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
            }
            let mut out = vec![0xee; N];
            super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
            assert_eq!(out[27..N - 27], expected[27..N - 27]);
            assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
        }
        assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
    }
}

use super::*;

#[test]
fn warp_adapter_accepts_only_software() {
    // This probes DXGI without creating a D3D12 device or running a title.
    // SAFETY: plain DXGI adapter queries.
    let raw = unsafe {
        let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
        else {
            return;
        };
        factory.EnumWarpAdapter::<IDXGIAdapter>()
    };
    let Ok(raw) = raw else { return };
    // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
    // IDXGIAdapter1; `raw` owns that interface.
    let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
    eprintln!(
        "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
        desc.VendorId, desc.Flags
    );
    assert_eq!(
        warp_adapter().is_ok(),
        desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
    );
}
