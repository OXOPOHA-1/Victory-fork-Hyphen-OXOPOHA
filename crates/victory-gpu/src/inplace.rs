//! Title memory as the card's own memory: a host heap opened on memory
//! the title already has, and a buffer placed in it, so that shaders read
//! and write the title's bytes themselves and no host copy stands between
//! (the console's one memory). This file holds the two openings Direct3D
//! has, a probe that says what this PC's driver makes of them
//! (`VICTORY_INPLACE_PROBE`), and a census of which title memory could be
//! opened.
//!
//! What the Direct3D runtime and the kernel answer, measured on this PC
//! with the software adapter (the rules are theirs, not the adapter's;
//! what the card's driver adds is what the probe is for):
//!
//! - `OpenExistingHeapFromAddress` takes the base address of a whole
//!   `VirtualAlloc` allocation and nothing inside it; every page must be
//!   committed and have one protection. Opening locks every page into the
//!   PC's memory at once (256 MiB never touched: 70 ms and 256 MB of
//!   working set).
//! - `OpenExistingHeapFromAddress1` (`ID3D12Device13`) takes any committed
//!   part of an allocation, with the same rule for its pages; parts may
//!   overlap, and the whole may be opened beside them. It also takes a
//!   committed view of a section.
//! - `OpenExistingHeapFromFileMapping` takes a section made with
//!   `SEC_COMMIT` and refuses one made with `SEC_RESERVE` (how `xmem`
//!   makes the title's page pool with in place off; with it on, the pool
//!   is committed sections of 256 MiB). The heap is the section, not a
//!   view of it: views may be unmapped, mapped elsewhere or made
//!   read-only, and the section's handle closed, with the heap alive.
//!   Opening it locks nothing (256 MiB: 0.2 ms, no working set).
//! - The heap is a custom heap in the PC's memory (write-back pages, pool
//!   L0), shared and cross-adapter shared, 64 KiB aligned. A buffer in it
//!   must have `ALLOW_CROSS_ADAPTER`; unordered access beside it is taken.
//!   A texture must be row-major with one level. Tile mappings into such
//!   a heap are refused and REMOVE THE DEVICE: never call them.
//! - While a heap opened from an address stands, the title cannot
//!   decommit, release or protect the pages in it (`VirtualFree` and
//!   `VirtualProtect` fail with error 87), nor unmap a view in it. Pages
//!   outside the opened part are free to change. So every title call
//!   that frees, decommits or protects memory asks
//!   [`title_memory_freeing`] first, which lets go of what is in place
//!   there and waits for the heaps over it to be destroyed (each heap's
//!   destruction notice says when).

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, ID3DDestructionNotifier};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, s};
use windows_sys::Win32::System::Memory as mem;

/// The unit heaps and placed buffers are cut on, and the console's page.
const K64: usize = 64 << 10;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// `VICTORY_INPLACE_PROBE`: 0 (unset) nothing beyond the start-up test;
/// 1 every line of that test, its cost measurement, and the census 30 and
/// 150 seconds later; 2 also opens one part of the title's own memory for
/// a moment at the first census.
pub(crate) fn probe_level() -> u8 {
    static LEVEL: OnceLock<u8> = OnceLock::new();
    *LEVEL.get_or_init(|| std::env::var("VICTORY_INPLACE_PROBE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}

/// What `VICTORY_INPLACE` asks for. 2 (unset, the default): windows of
/// title memory outside buffers and title buffers, except the big ones in
/// the card's memory, are not copied: the title memory under them is
/// opened as a host heap and a buffer placed over it ([`place_for`]).
/// 1: windows only. 0: off, everything is copied. A number above 2 is 2;
/// anything else that is set is 0 (said in the log by [`level`]).
///
/// `victory_hle` makes the page pool openable whenever this is above 0
/// and reads the switch by the same rule (`xmem::in_place_requested`).
pub(crate) fn requested() -> u8 {
    static LEVEL: OnceLock<u8> = OnceLock::new();
    *LEVEL.get_or_init(|| parse_requested(std::env::var("VICTORY_INPLACE").ok().as_deref()))
}

fn parse_requested(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        None | Some("") => 2,
        Some(v) => v.parse::<u8>().map_or(0, |n| n.min(2)),
    }
}

/// The switch that turns the late wait on explicitly, if one does, read as
/// `crate::host::late_stage` reads them: `VICTORY_LATE_WAIT` 1 to 3, or,
/// with it unset, `VICTORY_RELAY` 1 or 2 (the relay brings the late wait
/// with it). `None` when the late wait would be on only by default, or is
/// off (`VICTORY_GPU_TIMING` turns it off whatever else is set).
fn explicit_late(late_wait: Option<&str>, relay: Option<&str>, timing: bool) -> Option<String> {
    if timing {
        return None;
    }
    match late_wait {
        Some(v @ ("1" | "2" | "3")) => Some(format!("VICTORY_LATE_WAIT={v}")),
        Some(_) => None,
        None => match relay {
            Some(v @ ("1" | "2")) => Some(format!("VICTORY_RELAY={v}")),
            _ => None,
        },
    }
}

fn explicit_late_now() -> Option<String> {
    explicit_late(
        std::env::var("VICTORY_LATE_WAIT").ok().as_deref(),
        std::env::var("VICTORY_RELAY").ok().as_deref(),
        std::env::var_os("VICTORY_GPU_TIMING").is_some(),
    )
}

/// Whether the late wait may come on by default, because the relay is on
/// (`crate::host::late_stage` asks this before it does).
///
/// Title buffers in place (level 2) are written by shaders straight into
/// title memory, and the late wait knows what a submission still in the
/// air writes only by the ranges copies declare: with both, the title or
/// our own preparing can read title memory a shader has not written yet.
/// The two are never on together. By default in place wins: false when
/// level 2 is asked for and no switch turns the late wait on explicitly.
/// A switch that does wins over the default level: the late wait stays
/// on and title buffers are copied ([`level`] says so).
pub(crate) fn late_wait_allowed() -> bool {
    !(requested() == 2 && explicit_late_now().is_none())
}

/// The level in effect, from the level asked for, whether the late wait
/// is on, and the switch that turned it on if one did; and why, for the
/// log. Level 2 never stands beside the late wait.
fn resolve(requested: u8, late_on: bool, explicit: Option<&str>) -> (u8, String) {
    match (requested, late_on, explicit) {
        (0, ..) => (0, "VICTORY_INPLACE=0: everything is copied".to_owned()),
        (1, ..) => (1, "VICTORY_INPLACE=1: windows outside buffers only".to_owned()),
        (_, false, _) => (2, "windows outside buffers and title buffers; the late wait is off".to_owned()),
        (_, true, Some(switch)) => (
            1,
            format!("windows outside buffers only: {switch} keeps the late wait on, which cannot see what shaders write in place, so title buffers are copied"),
        ),
        (_, true, None) => (
            1,
            // Not reached while `crate::host::late_stage` asks
            // [`late_wait_allowed`]; kept so level 2 can never stand beside
            // the late wait.
            "windows outside buffers only: the late wait is on, which cannot see what shaders write in place, \
             so title buffers are copied"
                .to_owned(),
        ),
    }
}

/// The level in effect ([`requested`], [`resolve`]), said once in the log.
/// Windows are never written by shaders, so level 1 is safe with the late
/// wait; level 2 is in effect only with the late wait off.
///
/// Every opening is also gated by the start-up test of the driver
/// ([`tick`]): until it has run nothing is placed, and what it refuses
/// stays copied.
pub(crate) fn level() -> u8 {
    static LEVEL: OnceLock<u8> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        let asked = requested();
        let (level, why) = resolve(asked, asked == 2 && crate::host::late(), explicit_late_now().as_deref());
        tracing::info!(asked, level, "title memory in place: level {level}: {why}");
        tracing::info!("start-up (later): title memory in place: level {level} (asked {asked}): {why}");
        level
    })
}

/// The most title memory locked into the PC's memory by heaps opened on
/// private memory at once, in bytes (`VICTORY_INPLACE_MB`, 4096): such an
/// opening locks every page under it, also those the title never touched.
/// Heaps opened on the page pool's sections lock nothing and do not count.
fn most() -> u64 {
    static MOST: OnceLock<u64> = OnceLock::new();
    *MOST.get_or_init(|| std::env::var("VICTORY_INPLACE_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(4096).saturating_mul(1 << 20))
}

// What [`STATS`] counts, by index. In place now: windows and their bytes,
// title buffers and their bytes. Since the start, asks turned down because:
const WINDOWS: usize = 0;
const BUFFERS: usize = 2;
/// Private memory that is not committed whole and read-write (also view
/// memory the runtime did not tell us of, and pages a guard made
/// read-only).
const NOT_WHOLE: usize = 4;
/// The host refused the opening or the placement.
const HOST_REFUSED: usize = 5;
/// [`most`] would be passed.
const OVER_BUDGET: usize = 6;
/// What was let go because the title or the runtime changed its memory.
const LET_GO: usize = 7;
/// View memory: pages from more than one place of one section.
const VIEW_SCATTERED: usize = 8;
/// View memory with unmapped pages in it.
const VIEW_HOLES: usize = 9;
/// View memory that runs over more than one section: no one heap holds it.
const VIEW_SECTIONS: usize = 10;
/// View memory of a section made reserved (in place off when the pool was
/// made, or Windows refused the openable form).
const VIEW_RESERVED: usize = 11;
/// A title call to free or protect memory came while a heap still covered
/// it after [`RETIRE_WAIT`] (the runtime then holds the call back).
const STILL_PINNED: usize = 12;
/// The heap could not tell us when it goes (no destruction notice): not
/// placed, since nothing could then let the title's calls through.
const NO_NOTICE: usize = 13;
/// The start-up test refused this kind of memory on this driver.
const DEVICE_REFUSED: usize = 14;
/// Private memory the runtime did not make for the title ([`Refusal::NotOwned`]).
const NOT_OWNED: usize = 15;
static STATS: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];

fn refuse(why: usize) -> Option<Placed> {
    STATS[why].fetch_add(1, Ordering::Relaxed);
    use victory_core::causes::Cause;
    let cause = match why {
        NOT_WHOLE => Some(Cause::InPlaceNotWhole),
        NOT_OWNED => Some(Cause::InPlaceNotOwned),
        OVER_BUDGET => Some(Cause::InPlaceOverBudget),
        HOST_REFUSED => Some(Cause::InPlaceHostRefused),
        NO_NOTICE => Some(Cause::InPlaceNoNotice),
        DEVICE_REFUSED => Some(Cause::InPlaceDeviceRefused),
        VIEW_SCATTERED => Some(Cause::InPlaceViewScattered),
        VIEW_HOLES => Some(Cause::InPlaceViewHoles),
        VIEW_SECTIONS => Some(Cause::InPlaceViewSections),
        VIEW_RESERVED => Some(Cause::InPlaceViewReserved),
        _ => None,
    };
    if let Some(cause) = cause {
        victory_core::causes::note(cause);
    }
    None
}

// What the start-up test found ([`tick`], [`judge`]).
const TESTED: u8 = 1;
const PRIVATE_OK: u8 = 2;
const SECTIONS_OK: u8 = 4;
static STATE: AtomicU8 = AtomicU8::new(0);

/// A buffer placed over title memory, and its heap.
pub(crate) struct Placed {
    pub(crate) buffer: ID3D12Resource,
    pub(crate) heap: ID3D12Heap,
    /// The host GPU address of the title memory's first byte.
    pub(crate) gpu: u64,
    /// Bytes from the buffer's first byte to the title memory's first.
    pub(crate) bias: u64,
}

/// Why [`place_part`] placed nothing.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// Not private memory committed whole and read-write: nothing was
    /// asked of the host.
    NotWhole,
    /// Private memory the runtime did not make for the title
    /// (`XMemVirtualAlloc` makes it write-watched): the host's own heaps
    /// and ours free their pages with calls no one can hold back.
    NotOwned,
    /// Over the budget given.
    OverBudget,
    /// The heap gives no destruction notice.
    NoNotice(windows::core::Error),
    /// The host refused the opening or the placement.
    Host(windows::core::Error),
}

/// The 64 KiB pages that hold `[start, start + len)`: their first address
/// and their bytes. Heaps opened on existing memory are 64 KiB aligned
/// and a buffer is placed on a multiple of its 64 KiB alignment
/// (`CreatePlacedResource`: "HeapOffset must be a multiple of the
/// resource's alignment").
fn pages_around(start: u64, len: u64) -> (u64, u64) {
    let at = start & !(K64 as u64 - 1);
    let end = start.saturating_add(len.max(1)).next_multiple_of(K64 as u64);
    (at, end - at)
}

// Heaps opened on private memory lock its pages: while one stands, the
// title's `VirtualFree` and `VirtualProtect` on them fail. Every such heap
// is a pin here until the runtime destroys it (its destruction notice);
// [`title_memory_freeing`] waits for the pins over memory the title is
// about to change.

struct Pin {
    id: u64,
    at: usize,
    bytes: usize,
}

static PINS: Mutex<Vec<Pin>> = Mutex::new(Vec::new());
/// Signalled when a pin goes.
static PIN_GONE: Condvar = Condvar::new();
/// The pins and their bytes, for a look without the lock.
static PIN_COUNT: AtomicU64 = AtomicU64::new(0);
static PINNED: AtomicU64 = AtomicU64::new(0);

fn pins() -> std::sync::MutexGuard<'static, Vec<Pin>> {
    PINS.lock().unwrap_or_else(|e| e.into_inner())
}

fn pinned_over(pins: &[Pin], at: usize, end: usize) -> bool {
    pins.iter().any(|pin| pin.at < end && at < pin.at + pin.bytes)
}

/// How long a title call to free or protect memory waits for the heaps
/// over it to go: the copies in place there are let go at once, and their
/// heaps go when the last submission and list that hold them do.
const RETIRE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// The runtime destroyed the heap of pin `data`: its pages are free again.
unsafe extern "system" fn heap_destroyed(data: *mut c_void) {
    let id = data as u64;
    let mut pins = pins();
    if let Some(i) = pins.iter().position(|pin| pin.id == id) {
        let pin = pins.swap_remove(i);
        PINNED.fetch_sub(pin.bytes as u64, Ordering::AcqRel);
        PIN_COUNT.fetch_sub(1, Ordering::AcqRel);
    }
    drop(pins);
    PIN_GONE.notify_all();
}

/// Asks `heap` to call `callback` with `data` when it is destroyed.
fn notify_on_destruction(heap: &ID3D12Heap, callback: unsafe extern "system" fn(*mut c_void), data: *mut c_void) -> windows::core::Result<()> {
    let notifier: ID3DDestructionNotifier = heap.cast()?;
    // SAFETY: a static callback; `data` is a number, never dereferenced.
    unsafe { notifier.RegisterDestructionCallback(Some(callback), data)? };
    Ok(())
}

/// Opens the 64 KiB pages that hold title memory `[start, start + len)` as
/// a heap of their own and places one buffer over all of them, if the
/// heap's pages and those of every other pin stay within `most` bytes.
/// The heap is a pin until it is destroyed.
pub(crate) fn place_part(device: &ID3D12Device, start: u64, len: u64, most: u64) -> Result<Placed, Refusal> {
    let (at, bytes) = pages_around(start, len);
    if !private_whole(at as usize, bytes as usize) {
        return Err(Refusal::NotWhole);
    }
    if !write_watched(at as usize, bytes as usize) {
        return Err(Refusal::NotOwned);
    }
    // Taken before the opening, so that two asks at once cannot both pass.
    if PINNED.fetch_add(bytes, Ordering::AcqRel).saturating_add(bytes) > most {
        PINNED.fetch_sub(bytes, Ordering::AcqRel);
        return Err(Refusal::OverBudget);
    }
    let heap = match open(device, From::Part(at as usize, bytes as usize)) {
        Ok(heap) => heap,
        Err(e) => {
            PINNED.fetch_sub(bytes, Ordering::AcqRel);
            return Err(Refusal::Host(e));
        }
    };
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    // The pin is known before the notice is asked for, so that a notice
    // that comes at once finds it.
    pins().push(Pin { id, at: at as usize, bytes: bytes as usize });
    PIN_COUNT.fetch_add(1, Ordering::AcqRel);
    if let Err(e) = notify_on_destruction(&heap, heap_destroyed, id as *mut c_void) {
        // No notice will come: the pin goes here, then the heap.
        {
            let mut pins = pins();
            if let Some(i) = pins.iter().position(|pin| pin.id == id) {
                pins.swap_remove(i);
                PIN_COUNT.fetch_sub(1, Ordering::AcqRel);
                PINNED.fetch_sub(bytes, Ordering::AcqRel);
            }
        }
        drop(heap);
        return Err(Refusal::NoNotice(e));
    }
    // From here the notice takes the pin away when the heap goes.
    let buffer = place(device, &heap, 0, bytes).map_err(Refusal::Host)?;
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() } + (start - at);
    Ok(Placed { buffer, heap, gpu, bias: start - at })
}

/// A buffer over bytes `[offset, offset + len)` of `section`, which a
/// title view shows in one run, in the heap opened on the section (once,
/// kept until the runtime says the section is gone). Called with the view
/// tables locked: the runtime tells of a section's end under the same
/// lock before it closes the handle, so the handle opened here is that
/// section's.
fn in_section(device: &ID3D12Device, v: &mut Views, section: usize, offset: u64, len: u64) -> Result<Placed, usize> {
    let Some(&(_, size, committed)) = v.sections.iter().find(|s| s.0 == section) else {
        return Err(VIEW_RESERVED);
    };
    if !committed {
        return Err(VIEW_RESERVED);
    }
    let (from, bytes) = pages_around(offset, len);
    if from + bytes > size {
        return Err(VIEW_HOLES);
    }
    let heap = match v.heaps.entry(section) {
        std::collections::btree_map::Entry::Occupied(e) => e.get().clone(),
        std::collections::btree_map::Entry::Vacant(e) => {
            let opened = open(device, From::Section(section));
            if let Err(error) = &opened
                && STATS[HOST_REFUSED].load(Ordering::Relaxed) < 8
            {
                tracing::warn!(section, mb = size >> 20, %error, "a section of the page pool not opened as a heap: its memory is copied");
            }
            e.insert(opened.ok()).clone()
        }
    };
    let heap = heap.ok_or(HOST_REFUSED)?;
    let buffer = place(device, &heap, from, bytes).map_err(|error| {
        if STATS[HOST_REFUSED].load(Ordering::Relaxed) < 8 {
            tracing::warn!(section, offset, len, %error, "view memory not placed in place: a copy instead");
        }
        HOST_REFUSED
    })?;
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() } + (offset - from);
    Ok(Placed { buffer, heap, gpu, bias: offset - from })
}

/// Title memory `[start, start + len)` in place, for a window outside
/// buffers or a title buffer, if [`level`] asks for it, the start-up test
/// accepted that kind of memory, and the memory can be opened: private
/// memory committed whole, or view memory that is one run of one
/// committed section. `None` leaves the caller to make a copy, as before;
/// every refusal is counted by its cause.
pub(crate) fn place_for(start: u64, len: u64, window: bool) -> Option<Placed> {
    if level() < if window { 1 } else { 2 } {
        return None;
    }
    let state = STATE.load(Ordering::Acquire);
    if state & TESTED == 0 {
        victory_core::causes::note(victory_core::causes::Cause::InPlaceNotTested);
        return None;
    }
    let host = crate::host::host()?;
    let counted = |placed: Placed| {
        let at = if window { WINDOWS } else { BUFFERS };
        STATS[at].fetch_add(1, Ordering::Relaxed);
        STATS[at + 1].fetch_add(len, Ordering::Relaxed);
        placed
    };
    {
        let mut v = views();
        match cover_in(&v.views, start as usize, len as usize) {
            Cover::None => {}
            Cover::Run(section, offset) => {
                if state & SECTIONS_OK == 0 {
                    return refuse(DEVICE_REFUSED);
                }
                return match in_section(&host.device, &mut v, section, offset, len) {
                    Ok(placed) => Some(counted(placed)),
                    Err(why) => refuse(why),
                };
            }
            Cover::Holes => return refuse(VIEW_HOLES),
            Cover::Scattered => {
                let why = if sections_under(&v.views, start as usize, len as usize) > 1 { VIEW_SECTIONS } else { VIEW_SCATTERED };
                return refuse(why);
            }
        }
    }
    if state & PRIVATE_OK == 0 {
        return refuse(DEVICE_REFUSED);
    }
    match place_part(&host.device, start, len, most()) {
        Ok(placed) => Some(counted(placed)),
        Err(Refusal::NotWhole) => refuse(NOT_WHOLE),
        Err(Refusal::NotOwned) => refuse(NOT_OWNED),
        Err(Refusal::OverBudget) => refuse(OVER_BUDGET),
        Err(Refusal::NoNotice(e)) => {
            if STATS[NO_NOTICE].load(Ordering::Relaxed) == 0 {
                tracing::warn!(error = %e, "title memory not opened in place: the heap gives no destruction notice, a copy instead");
            }
            refuse(NO_NOTICE)
        }
        Err(Refusal::Host(e)) => {
            if STATS[HOST_REFUSED].load(Ordering::Relaxed) < 8 {
                tracing::warn!(start = format_args!("{start:#x}"), len, error = %e, "title memory not opened in place: a copy instead");
            }
            refuse(HOST_REFUSED)
        }
    }
}

/// What [`place_for`] gave is gone.
pub(crate) fn gone(window: bool, bytes: u64) {
    let at = if window { WINDOWS } else { BUFFERS };
    STATS[at].fetch_sub(1, Ordering::Relaxed);
    STATS[at + 1].fetch_sub(bytes, Ordering::Relaxed);
}

/// Title memory `[at, at + len)` is about to show something else, or the
/// title is about to decommit, free or protect it. What is in place there
/// is let go (taken out of the tables, retired until the submissions that
/// may use it are done); then, if a heap opened on private memory covers
/// any of it, this waits up to [`RETIRE_WAIT`] for every such heap to be
/// destroyed, since the title's call fails while one stands. True if one
/// still stands: the caller must not make the change yet.
///
/// Called by the runtime before every title call that frees, decommits,
/// protects or maps memory (`victory_hle::modules::xmem`), and when it
/// maps view memory anew (`crate::watch::title_memory_remapped`; view
/// memory has no pins, so that never waits). Cheap when nothing is in
/// place.
pub fn title_memory_freeing(at: usize, len: usize) -> bool {
    freeing(at, len, true)
}

/// [`title_memory_freeing`] without the wait: lets go of what is in place
/// over `[at, at + len)` and says whether a heap still stands there. For
/// callers that hold a lock other title calls need (the runtime's list of
/// held changes, the mapping lock) and look again later anyway.
pub fn title_memory_still_pinned(at: usize, len: usize) -> bool {
    freeing(at, len, false)
}

fn freeing(at: usize, len: usize, wait: bool) -> bool {
    let end = at.saturating_add(len.max(1));
    let pinned = PIN_COUNT.load(Ordering::Acquire) > 0 && pinned_over(&pins(), at, end);
    let placed = STATS[WINDOWS].load(Ordering::Relaxed) + STATS[BUFFERS].load(Ordering::Relaxed) > 0;
    if placed || pinned {
        let gone = crate::shadow::in_place_let_go(at as u64, len as u64);
        STATS[LET_GO].fetch_add(gone, Ordering::Relaxed);
    }
    if !pinned {
        return false;
    }
    if !wait {
        return pinned_over(&pins(), at, end);
    }
    let deadline = std::time::Instant::now() + RETIRE_WAIT;
    let mut held = pins();
    while pinned_over(&held, at, end) {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        held = match PIN_GONE.wait_timeout(held, deadline - now) {
            Ok((held, _)) => held,
            Err(e) => e.into_inner().0,
        };
    }
    let still = pinned_over(&held, at, end);
    drop(held);
    if still {
        victory_core::causes::note(victory_core::causes::Cause::InPlaceStillPinned);
    }
    if still && STATS[STILL_PINNED].fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::warn!(
            at = format_args!("{at:#x}"),
            len,
            "title memory the title changes is still under a heap in place after {} ms (a list or a slot still holds it): the change waits for the heap",
            RETIRE_WAIT.as_millis()
        );
    }
    still
}

/// The line of what is in place, every ten seconds while [`level`] is on.
fn say() {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 10_000 || LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    tracing::info!(
        "title memory in place: {} windows {} MB, {} title buffers {} MB (no copy, no refresh, no copy-back); {} heaps on private memory lock {} MB; \
         turned down: {} private memory not committed whole, {} private memory not the title's, {} view memory scattered, {} view memory with holes, {} view memory over two sections, \
         {} view memory of a reserved section, {} refused by the host, {} refused by the start-up test, {} over VICTORY_INPLACE_MB, {} without a destruction notice; \
         {} let go because their memory changed, {} title calls that found a heap still standing",
        s[WINDOWS],
        s[WINDOWS + 1] >> 20,
        s[BUFFERS],
        s[BUFFERS + 1] >> 20,
        PIN_COUNT.load(Ordering::Relaxed),
        PINNED.load(Ordering::Relaxed) >> 20,
        s[NOT_WHOLE],
        s[NOT_OWNED],
        s[VIEW_SCATTERED],
        s[VIEW_HOLES],
        s[VIEW_SECTIONS],
        s[VIEW_RESERVED],
        s[HOST_REFUSED],
        s[DEVICE_REFUSED],
        s[OVER_BUDGET],
        s[NO_NOTICE],
        s[LET_GO],
        s[STILL_PINNED]
    );
}

/// What a heap is opened on.
pub(crate) enum From {
    /// A whole `VirtualAlloc` allocation, by its base address.
    Region(usize),
    /// A committed part of one: address and bytes.
    Part(usize, usize),
    /// A section made with `SEC_COMMIT`, by its handle.
    Section(usize),
}

/// Opens a heap on memory the process has.
pub(crate) fn open(device: &ID3D12Device, from: From) -> windows::core::Result<ID3D12Heap> {
    // SAFETY: the calls take an address or a handle and check both
    // themselves; a refusal is an error code.
    unsafe {
        match from {
            From::Region(at) => device.cast::<ID3D12Device3>()?.OpenExistingHeapFromAddress(at as *const c_void),
            From::Part(at, bytes) => device.cast::<ID3D12Device13>()?.OpenExistingHeapFromAddress1(at as *const c_void, bytes),
            From::Section(handle) => device.cast::<ID3D12Device3>()?.OpenExistingHeapFromFileMapping(HANDLE(handle as *mut c_void)),
        }
    }
}

fn buffer_desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

/// A buffer of `size` bytes at `offset` (a multiple of 64 KiB) of an
/// opened heap, which shaders may read and write, in the common state.
pub(crate) fn place(device: &ID3D12Device, heap: &ID3D12Heap, offset: u64, size: u64) -> windows::core::Result<ID3D12Resource> {
    let mut placed: Option<ID3D12Resource> = None;
    // The heap is cross-adapter shared, so the buffer must say so too.
    let flags = D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    // SAFETY: plain creation in a live heap.
    unsafe { device.CreatePlacedResource(heap, offset, &buffer_desc(size, flags), D3D12_RESOURCE_STATE_COMMON, None, &mut placed)? };
    placed.ok_or_else(windows::core::Error::empty)
}

// The title's views of its physical pages, as the runtime maps them: the
// census needs them to say which view memory is one run of a section (one
// placed buffer could show it) and which is scattered pages.

struct Views {
    /// By address: bytes, the section's handle, the offset in it.
    views: BTreeMap<usize, (usize, usize, u64)>,
    /// By section and byte offset of a 64 KiB page: the address of that
    /// page in every view that shows it. A write through one view of a
    /// page is a write of the bytes every other view shows
    /// (`crate::watch` guards them all).
    pages: BTreeMap<(usize, u64), Vec<usize>>,
    /// Sections by handle: bytes, and whether made committed.
    sections: Vec<(usize, u64, bool)>,
    /// The heap opened on a committed section, by handle, for buffers
    /// placed over view memory ([`in_section`]); `None` if the host
    /// refused it (not asked again for that section).
    heaps: BTreeMap<usize, Option<ID3D12Heap>>,
}

static VIEWS: Mutex<Views> =
    Mutex::new(Views { views: BTreeMap::new(), pages: BTreeMap::new(), sections: Vec::new(), heaps: BTreeMap::new() });

fn views() -> std::sync::MutexGuard<'static, Views> {
    VIEWS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The runtime made a section of `bytes` for physical pages; `committed`
/// if with `SEC_COMMIT` (only such a section can be opened as a heap).
/// `bytes` 0: the section is about to be closed (said before its handle
/// is, so that its number cannot name another section here): it and its
/// heap are forgotten. Buffers placed in that heap keep it, and with it
/// the section's memory, until they go; no title memory is locked by it.
pub fn title_memory_section(section: usize, bytes: u64, committed: bool) {
    let heap = {
        let mut v = views();
        v.sections.retain(|s| s.0 != section);
        if bytes > 0 {
            v.sections.push((section, bytes, committed));
        }
        v.heaps.remove(&section)
    };
    // Released outside the lock: a heap's end may call back into us.
    drop(heap);
    if bytes == 0 {
        return;
    }
    // A view of our own made on an earlier section that had this handle's
    // number shows that section's pages, not this one's: it is forgotten
    // (left mapped: nothing reads it any more) and the next store through
    // a view of ours maps this section.
    let gone = OWN.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|own| own.remove(&section)).flatten();
    if gone.is_some() {
        tracing::info!(section, "a section handle's number came round again: our own view of the earlier section is forgotten");
    }
}

/// The runtime mapped `[at, at + len)` as a view of `section` from
/// `offset` on. Call after [`crate::title_memory_remapped`] of the range.
pub fn title_memory_view(at: usize, len: usize, section: usize, offset: u64) {
    {
        let mut v = views();
        v.views.insert(at, (len, section, offset));
        for k in 0..len.div_ceil(K64) {
            v.pages.entry((section, offset + (k * K64) as u64)).or_default().push(at + k * K64);
        }
    }
    // This view is writable from now on: a guard over the same bytes at
    // another view must look at them again, and the pages are noted written
    // there (`crate::watch`).
    crate::watch::views_mapped(section, offset, len);
}

/// What `[at, at + len)` showed is gone ([`crate::title_memory_remapped`]):
/// the views that touch it are forgotten (the runtime tells again those it
/// maps anew).
pub(crate) fn views_gone(at: usize, len: usize) {
    let mut v = views();
    if v.views.is_empty() {
        return;
    }
    let end = at.saturating_add(len);
    let first = v.views.range(..=at).next_back().filter(|(base, view)| *base + view.0 > at).map(|(base, _)| *base);
    let gone: Vec<usize> = first.into_iter().chain(v.views.range(at.saturating_add(1)..end).map(|(base, _)| *base)).collect();
    for base in gone {
        let Some((bytes, section, offset)) = v.views.remove(&base) else {
            continue;
        };
        for k in 0..bytes.div_ceil(K64) {
            let key = (section, offset + (k * K64) as u64);
            if let Some(pages) = v.pages.get_mut(&key) {
                let page = base + k * K64;
                pages.retain(|&shown| shown != page);
                if pages.is_empty() {
                    v.pages.remove(&key);
                }
            }
        }
    }
}

/// The title views that overlap `[start, end)`, as `(address, bytes)`.
pub(crate) fn views_between(start: usize, end: usize) -> Vec<(usize, usize)> {
    if start >= end {
        return Vec::new();
    }
    let v = views();
    let mut out = Vec::new();
    if let Some((&base, &(bytes, _, _))) = v.views.range(..=start).next_back()
        && base + bytes > start
    {
        out.push((base, bytes));
    }
    for (&base, &(bytes, _, _)) in v.views.range(start.saturating_add(1)..end) {
        out.push((base, bytes));
    }
    out
}

/// The address of the 64 KiB page shown at `page` in every title view that
/// shows it, `page`'s own address included; empty if `page` is not a view.
pub(crate) fn pages_of(page: usize) -> Vec<usize> {
    let v = views();
    let Some((&base, &(bytes, section, offset))) = v.views.range(..=page).next_back() else {
        return Vec::new();
    };
    let within = page.wrapping_sub(base);
    if within >= bytes || !within.is_multiple_of(K64) {
        return Vec::new();
    }
    v.pages.get(&(section, offset + within as u64)).cloned().unwrap_or_default()
}

/// The title views that show any of the physical bytes `[offset, offset +
/// len)` of `section`, as `(address, bytes)` ranges in address order; not
/// those of a page shown in very many views (`MANY_VIEWS`).
pub(crate) fn views_of_section(section: usize, offset: u64, len: usize) -> Vec<(usize, usize)> {
    if len == 0 {
        return Vec::new();
    }
    let v = views();
    let end = offset.saturating_add(len as u64);
    let mut pages: Vec<usize> = Vec::new();
    let mut at = offset - offset % K64 as u64;
    while at < end {
        if let Some(shown) = v.pages.get(&(section, at))
            && shown.len() <= crate::watch::MANY_VIEWS
        {
            pages.extend(shown.iter().copied());
        }
        match at.checked_add(K64 as u64) {
            Some(next) => at = next,
            None => break,
        }
    }
    pages.sort_unstable();
    pages.dedup();
    let mut runs: Vec<(usize, usize)> = Vec::with_capacity(pages.len());
    for page in pages {
        match runs.last_mut() {
            Some((first, bytes)) if *first + *bytes == page => *bytes += K64,
            _ => runs.push((page, K64)),
        }
    }
    runs
}

/// Whether what shaders wrote goes back into title view memory through a
/// view of our own (`VICTORY_BACK_OWN_VIEW=1`; off by default).
///
/// The copy-back writes title memory at the title's address, so it opens
/// the guard on the 64 KiB block first (and on every other view of the
/// page), and the block then counts as written by the title: the host
/// copy that just put the bytes there copies all 64 KiB of it back in at
/// its next refresh, and sends them up to the card again if it lives
/// there. In runs 218-222 the bytes copied in went with the bytes copied
/// back, 0.83 MB for each MB (78 readings), some 50 MB of 142 MB a frame.
///
/// With the switch the bytes are stored through a view of our own on the
/// same section, which no guard covers: the title's views stay read-only
/// (a title write still faults and is seen), the write-watch journal is
/// told the page was written (`crate::watch::wrote_own`) so that every
/// other copy of it takes it again, and the copy that wrote it notes the
/// page current (`put_back_own` in `crate::shadow`).
///
/// `VICTORY_BACK_OWN_VIEW=alt`: twenty seconds with, twenty without, in
/// turn, each change said in the log, for measuring it in one scene.
pub(crate) fn back_own() -> bool {
    /// 0 off, 1 on, 2 in turn.
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    match *MODE.get_or_init(|| match std::env::var("VICTORY_BACK_OWN_VIEW").as_deref() {
        Ok("1") => 1,
        Ok("alt") => 2,
        _ => 0,
    }) {
        0 => false,
        1 => true,
        _ => {
            static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let on = (EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs() / 20) % 2 == 1;
            static LAST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);
            if LAST.swap(u8::from(on), std::sync::atomic::Ordering::Relaxed) != u8::from(on) {
                tracing::info!(
                    "copy-back through views of our own from now (VICTORY_BACK_OWN_VIEW=alt): {}",
                    if on { "ON" } else { "off, as usual" }
                );
            }
            on
        }
    }
}

/// A view of our own on a whole section of the title's physical pages:
/// where it is, its bytes, and which of its 64 KiB blocks we have asked
/// to be committed through it (a bit each).
struct Own {
    base: usize,
    bytes: u64,
    committed: Vec<u64>,
}

/// Our own views by section handle; `None` in a section's place if the
/// host refused the view (not asked again).
static OWN: Mutex<Option<std::collections::HashMap<usize, Option<Own>>>> = Mutex::new(None);

/// Stores `bytes` at title address `at` through a view of our own
/// ([`back_own`]), if `[at, at + bytes.len())` lies in one 64 KiB page of
/// a title view of a section. Gives the addresses of that page in every
/// title view that shows it (the page of `at` among them), or `None` with
/// nothing written: not view memory, a page of very many views, the
/// runtime changing mappings at this moment, or the host refusing.
pub(crate) fn write_own(at: usize, bytes: &[u8]) -> Option<Vec<usize>> {
    if bytes.is_empty() || at / K64 != (at + bytes.len() - 1) / K64 {
        return None;
    }
    // No mapping changes until the bytes are in: the view found below
    // shows these pages for as long as this is held.
    let _still = crate::watch::title_memory_reading_now()?;
    let (section, offset, shown, section_bytes) = {
        let v = views();
        let (&base, &(len, section, offset)) = v.views.range(..=at).next_back()?;
        if at + bytes.len() > base + len {
            return None;
        }
        let offset = offset + (at - base) as u64;
        let shown = v.pages.get(&(section, offset - offset % K64 as u64))?.clone();
        if shown.len() > crate::watch::MANY_VIEWS {
            return None;
        }
        let section_bytes = v.sections.iter().find(|s| s.0 == section)?.1;
        (section, offset, shown, section_bytes)
    };
    if offset + bytes.len() as u64 > section_bytes {
        return None;
    }
    let mut own = OWN.lock().unwrap_or_else(|e| e.into_inner());
    let view = own
        .get_or_insert_with(Default::default)
        .entry(section)
        .or_insert_with(|| {
            // SAFETY: a section handle the runtime gave us, alive as long
            // as the title's views of it are; the view is ours and stays.
            let at = unsafe { mem::MapViewOfFile(section as *mut c_void, mem::FILE_MAP_WRITE, 0, 0, 0) };
            if at.Value.is_null() {
                tracing::warn!(
                    section,
                    error = %std::io::Error::last_os_error(),
                    "no view of our own on a section of title pages: what shaders wrote goes back the old way there"
                );
                return None;
            }
            tracing::info!(
                section,
                mb = section_bytes >> 20,
                "a view of our own on a section of title pages, for what shaders wrote (VICTORY_BACK_OWN_VIEW=1)"
            );
            Some(Own {
                base: at.Value as usize,
                bytes: section_bytes,
                committed: vec![0; (section_bytes as usize).div_ceil(K64).div_ceil(64)],
            })
        })
        .as_mut()?;
    if offset + bytes.len() as u64 > view.bytes {
        return None;
    }
    let block = offset as usize / K64;
    if (view.committed[block / 64] & (1u64 << (block % 64))) == 0 {
        // The title's view committed these pages of the section; asked
        // once more through ours, which changes nothing if they are.
        // SAFETY: a block inside our own view.
        let there = unsafe {
            mem::VirtualAlloc((view.base + block * K64) as *const c_void, K64, mem::MEM_COMMIT, mem::PAGE_READWRITE)
        };
        if there.is_null() {
            return None;
        }
        view.committed[block / 64] |= 1u64 << (block % 64);
    }
    // SAFETY: committed bytes inside our own view; the source is the
    // caller's slice.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (view.base + offset as usize) as *mut u8, bytes.len()) };
    Some(shown)
}

/// How the views cover `[at, at + len)`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Cover {
    /// No view holds its first byte.
    None,
    /// Views hold a part of it only.
    Holes,
    /// Views hold all of it, from more than one place.
    Scattered,
    /// All of it is one run of one section: the section and the offset of
    /// `at` in it.
    Run(usize, u64),
}

fn cover_in(views: &BTreeMap<usize, (usize, usize, u64)>, at: usize, len: usize) -> Cover {
    let end = at + len.max(1);
    let Some((&base, &(bytes, section, offset))) = views.range(..=at).next_back().filter(|(base, view)| *base + view.0 > at) else {
        return Cover::None;
    };
    let first = (section, offset + (at - base) as u64);
    let (mut reached, mut one) = (base + bytes, true);
    while reached < end {
        let Some(&(bytes, s, o)) = views.get(&reached) else {
            return Cover::Holes;
        };
        one &= s == first.0 && o == first.1 + (reached - at) as u64;
        reached += bytes;
    }
    if one { Cover::Run(first.0, first.1) } else { Cover::Scattered }
}

/// How many sections the views over `[at, at + len)` show: more than one
/// means no one heap holds the range, whatever the order of its pages.
fn sections_under(views: &BTreeMap<usize, (usize, usize, u64)>, at: usize, len: usize) -> usize {
    let end = at.saturating_add(len.max(1));
    let first = views.range(..=at).next_back().filter(|(base, view)| *base + view.0 > at).map(|(_, view)| view.1);
    let mut seen: Vec<usize> = first.into_iter().chain(views.range(at.saturating_add(1)..end).map(|(_, view)| view.1)).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

/// [`Cover`] of `[at, at + len)` by the title's views as they are now.
pub(crate) fn cover(at: usize, len: usize) -> Cover {
    cover_in(&views().views, at, len)
}

/// Whether `[at, at + len)` lies in a write-watched allocation: the title
/// memory `XMemVirtualAlloc` reserves at an address of its choice. Asks
/// without resetting anything (Windows refuses the asking for memory
/// reserved without `MEM_WRITE_WATCH`).
fn write_watched(at: usize, len: usize) -> bool {
    let (mut found, mut count, mut granularity) = ([std::ptr::null_mut::<c_void>(); 1], 1usize, 0u32);
    // SAFETY: room for one address; the range is only looked at.
    unsafe { mem::GetWriteWatch(0, at as *const c_void, len, found.as_mut_ptr(), &mut count, &mut granularity) == 0 }
}

/// Whether `[at, at + len)` is private memory, all committed and
/// read-write: what [`From::Part`] takes. One question to Windows (it
/// walks the page tables to the end of the pages that share the first
/// one's state: a millisecond in the title's big blocks, so ask when a
/// copy is made, not when it is used).
pub(crate) fn private_whole(at: usize, len: usize) -> bool {
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    n != 0
        && info.State == mem::MEM_COMMIT
        && info.Type == mem::MEM_PRIVATE
        && info.Protect == mem::PAGE_READWRITE
        && (info.BaseAddress as usize).saturating_add(info.RegionSize) >= at.saturating_add(len)
}

fn hr<T>(result: &windows::core::Result<T>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(e) => format!("REFUSED {:#010x} ({})", e.code().0, e.message().trim()),
    }
}

/// A queue, a list and a fence of the probe's own on `device`: nothing of
/// the host's submissions is touched.
struct Rig {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    done: u64,
    event: HANDLE,
}

impl Drop for Rig {
    fn drop(&mut self) {
        // SAFETY: our event.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.event);
        }
    }
}

impl Rig {
    fn new(device: &ID3D12Device) -> windows::core::Result<Rig> {
        // SAFETY: plain creation of objects of our own.
        unsafe {
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })?;
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
            list.Close()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;
            Ok(Rig { device: device.clone(), queue, allocator, list, fence, done: 0, event })
        }
    }

    /// Records with `record`, runs it and waits (ten seconds at most).
    fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> windows::core::Result<()> {
        // SAFETY: our own list and queue, one run at a time.
        unsafe {
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator, None)?;
            record(&self.list);
            self.list.Close()?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast()?)]);
            self.done += 1;
            self.queue.Signal(&self.fence, self.done)?;
            self.fence.SetEventOnCompletion(self.done, self.event)?;
            if WaitForSingleObject(self.event, 10_000).0 != 0 {
                return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL));
            }
            self.device.GetDeviceRemovedReason()
        }
    }

    fn committed(&self, kind: D3D12_HEAP_TYPE, size: u64, state: D3D12_RESOURCE_STATES) -> windows::core::Result<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            self.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer_desc(size, D3D12_RESOURCE_FLAG_NONE), state, None, &mut buffer)?
        };
        buffer.ok_or_else(windows::core::Error::empty)
    }

    /// What the GPU reads of `size` bytes of `resource` from `offset`.
    fn fetch(&mut self, resource: &ID3D12Resource, offset: u64, size: usize) -> windows::core::Result<Vec<u8>> {
        let back = self.committed(D3D12_HEAP_TYPE_READBACK, size as u64, D3D12_RESOURCE_STATE_COPY_DEST)?;
        // SAFETY: two live buffers, the range inside both.
        self.run(|list| unsafe { list.CopyBufferRegion(&back, 0, resource, offset, size as u64) })?;
        let mut data = std::ptr::null_mut();
        // SAFETY: the mapping holds `size` bytes until unmapped.
        unsafe {
            back.Map(0, None, Some(&mut data))?;
            let bytes = std::slice::from_raw_parts(data.cast::<u8>(), size).to_vec();
            back.Unmap(0, None);
            Ok(bytes)
        }
    }

    /// The GPU writes `bytes` at `offset` of `resource` (a copy).
    fn store(&mut self, resource: &ID3D12Resource, offset: u64, bytes: &[u8]) -> windows::core::Result<()> {
        let up = self.committed(D3D12_HEAP_TYPE_UPLOAD, bytes.len() as u64, D3D12_RESOURCE_STATE_GENERIC_READ)?;
        let mut data = std::ptr::null_mut();
        // SAFETY: the mapping holds the buffer's bytes until unmapped.
        unsafe {
            up.Map(0, None, Some(&mut data))?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len());
            up.Unmap(0, None);
        }
        // SAFETY: two live buffers, the range inside both.
        self.run(|list| unsafe { list.CopyBufferRegion(resource, offset, &up, 0, bytes.len() as u64) })
    }

    /// A compute shader stores, through a root unordered-access view at
    /// `gpu`, the sum of the buffer's first two words in its third.
    fn shade(&mut self, gpu: u64) -> windows::core::Result<()> {
        const SOURCE: &str = "#define RS \"UAV(u0)\"\nRWByteAddressBuffer b : register(u0);\n[RootSignature(RS)]\n[numthreads(1, 1, 1)]\nvoid main() { b.Store(8, b.Load(0) + b.Load(4)); }\n";
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; then the
        // blob's own bytes, which hold the root signature too.
        let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
            D3DCompile(SOURCE.as_ptr().cast(), SOURCE.len(), s!("inplace"), None, None, s!("main"), s!("cs_5_1"), 0, 0, &mut code, None)?;
            let code = code.ok_or_else(windows::core::Error::empty)?;
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
            let pso = self.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                ..Default::default()
            })?;
            (root, pso)
        };
        // SAFETY: an open list of ours; the view names a live buffer.
        self.run(|list| unsafe {
            list.SetComputeRootSignature(&root);
            list.SetPipelineState(&pso);
            list.SetComputeRootUnorderedAccessView(0, gpu);
            list.Dispatch(1, 1, 1);
        })
    }

    /// Milliseconds the card takes to copy `bytes` of `from` into its own
    /// memory, the best of three.
    fn copy_ms(&mut self, from: &ID3D12Resource, bytes: u64) -> windows::core::Result<f64> {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let mut to: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            self.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer_desc(bytes, D3D12_RESOURCE_FLAG_NONE), D3D12_RESOURCE_STATE_COMMON, None, &mut to)?
        };
        let to = to.ok_or_else(windows::core::Error::empty)?;
        let mut best = f64::MAX;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            // SAFETY: two live buffers of `bytes` bytes or more.
            self.run(|list| unsafe { list.CopyBufferRegion(&to, 0, from, 0, bytes) })?;
            best = best.min(started.elapsed().as_secs_f64() * 1000.0);
        }
        Ok(best)
    }
}

fn fill(at: *mut u8, len: usize, seed: u8) {
    for i in 0..len {
        // SAFETY: the caller's own committed memory of `len` bytes.
        unsafe { at.add(i).write((i as u8).wrapping_mul(31).wrapping_add(seed)) };
    }
}

fn same(at: *const u8, bytes: &[u8]) -> bool {
    // SAFETY: the caller's own committed memory of that length.
    unsafe { std::slice::from_raw_parts(at, bytes.len()) == bytes }
}

/// The process's working set in MB.
fn working_set_mb() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    let mut counters = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(windows_sys::Win32::System::Threading::GetCurrentProcess(), &mut counters, counters.cb) };
    counters.WorkingSetSize as u64 >> 20
}

/// What the probe found. The lines are the whole account; the flags are
/// what the stages build on.
#[derive(Default)]
pub(crate) struct Probe {
    pub(crate) lines: Vec<String>,
    /// A part of private, write-watched memory opens, a buffer in it shows
    /// the memory's bytes to the GPU, also those the CPU wrote afterwards.
    pub(crate) part_reads: bool,
    /// What the GPU writes there (a copy, and a shader through unordered
    /// access) the CPU reads at the memory's address.
    pub(crate) part_writes: bool,
    /// The same two for a committed section, through a view of it.
    pub(crate) section_reads: bool,
    pub(crate) section_writes: bool,
    /// The section's heap outlives the view: the GPU's write with no view
    /// mapped shows in a view mapped afterwards.
    pub(crate) section_outlives_views: bool,
    /// A section made with `SEC_RESERVE` (as `xmem` makes the page pool
    /// with in place off) was refused.
    pub(crate) reserved_section_refused: bool,
    /// The heap on private memory told of its end (its destruction
    /// notice), and the memory could be released right after: what
    /// letting the title's own calls through relies on.
    pub(crate) part_notice: bool,
    pub(crate) part_released: bool,
    /// Why private memory, or the page pool's sections, cannot be in place
    /// on this device: the first step that failed.
    pub(crate) private_why: Option<String>,
    pub(crate) section_why: Option<String>,
    /// The adapter, its driver's version, and whether the device says it
    /// can open heaps on existing memory (`D3D12_FEATURE_EXISTING_HEAPS`).
    pub(crate) facts: String,
}

/// Notes the first reason only.
fn because(slot: &mut Option<String>, why: impl FnOnce() -> String) {
    if slot.is_none() {
        *slot = Some(why());
    }
}

/// The adapter's name and its driver's version, and whether the device
/// says it can open heaps on existing memory.
fn device_facts(device: &ID3D12Device) -> (String, bool) {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIDevice, IDXGIFactory4};
    let mut existing = D3D12_FEATURE_DATA_EXISTING_HEAPS::default();
    // SAFETY: a plain query that fills our structure.
    let asked = unsafe { device.CheckFeatureSupport(D3D12_FEATURE_EXISTING_HEAPS, (&raw mut existing).cast(), size_of_val(&existing) as u32) };
    let supported = asked.is_ok() && existing.Supported.as_bool();
    // SAFETY: plain DXGI queries about the device's own adapter.
    let adapter = unsafe {
        let luid = device.GetAdapterLuid();
        CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)).and_then(|f| f.EnumAdapterByLuid::<IDXGIAdapter1>(luid))
    };
    let (name, driver) = match &adapter {
        Ok(adapter) => {
            // SAFETY: plain queries of a live adapter.
            let name = unsafe { adapter.GetDesc1() }.map_or_else(
                |_| "?".to_owned(),
                |d| {
                    let len = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
                    format!("{} (vendor {:#06x}, device {:#06x})", String::from_utf16_lossy(&d.Description[..len]).trim(), d.VendorId, d.DeviceId)
                },
            );
            // SAFETY: as above.
            let driver = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }.map_or_else(
                |_| "?".to_owned(),
                |v| format!("{}.{}.{}.{}", (v >> 48) & 0xffff, (v >> 32) & 0xffff, (v >> 16) & 0xffff, v & 0xffff),
            );
            (name, driver)
        }
        Err(_) => ("?".to_owned(), "?".to_owned()),
    };
    (
        format!(
            "adapter {name}, driver {driver}, heaps on existing memory supported {}",
            if asked.is_ok() { supported.to_string() } else { "? (the device does not answer)".to_owned() }
        ),
        supported,
    )
}

/// What the start-up test lets be in place, and the one line that says it.
fn judge(p: &Probe) -> (u8, String) {
    let private = p.part_reads && p.part_writes && p.part_notice && p.part_released;
    let sections = p.section_reads && p.section_writes && p.section_outlives_views;
    let why = |slot: &Option<String>| slot.clone().unwrap_or_else(|| "the test did not finish".to_owned());
    let state = TESTED | if private { PRIVATE_OK } else { 0 } | if sections { SECTIONS_OK } else { 0 };
    let line = if !private && !sections {
        format!(
            "in-place memory: REFUSED by the driver: the title's private memory: {}; the page pool: {}; copying instead ({})",
            why(&p.private_why),
            why(&p.section_why),
            p.facts
        )
    } else {
        format!(
            "in-place memory: ACCEPTED: the title's private memory {}; the page pool {} ({})",
            if private { "in place".to_owned() } else { format!("REFUSED, copied ({})", why(&p.private_why)) },
            if sections { "in place".to_owned() } else { format!("REFUSED, copied ({})", why(&p.section_why)) },
            p.facts
        )
    };
    (state, line)
}

/// Tries the openings on memory of the probe's own, made the way `xmem`
/// makes title memory, with a queue of its own on `device`; `memory`
/// gives the card's memory account (`Host::video_memory`) where there is
/// one. Nothing of the title's is touched. `mb` is the size of the cost
/// measurement (0 leaves it out).
pub(crate) fn probe(device: &ID3D12Device, memory: &dyn Fn() -> Option<[u64; 4]>, mb: usize) -> Probe {
    let mut p = Probe::default();
    let (facts, supported) = device_facts(device);
    p.facts = facts;
    p.lines.push(p.facts.clone());
    if !supported {
        because(&mut p.private_why, || "the device says it cannot open heaps on existing memory".to_owned());
        because(&mut p.section_why, || "the device says it cannot open heaps on existing memory".to_owned());
    }
    let mut rig = match Rig::new(device) {
        Ok(rig) => rig,
        Err(e) => {
            p.lines.push(format!("no queue of its own: {e}"));
            because(&mut p.private_why, || format!("no queue for the test: {e}"));
            because(&mut p.section_why, || format!("no queue for the test: {e}"));
            return p;
        }
    };
    if device.cast::<ID3D12Device13>().is_err() {
        because(&mut p.private_why, || "no ID3D12Device13 (OpenExistingHeapFromAddress1): the runtime or driver is too old".to_owned());
    }
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    // SAFETY: a plain query that fills our structure.
    let _ = unsafe { device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, (&raw mut options).cast(), size_of_val(&options) as u32) };
    p.lines.push(format!(
        "the device: ID3D12Device3 {}, ID3D12Device13 {} (parts of an allocation); cross-adapter row-major textures {}; heap tier {}",
        device.cast::<ID3D12Device3>().is_ok(),
        device.cast::<ID3D12Device13>().is_ok(),
        options.CrossAdapterRowMajorTextureSupported.as_bool(),
        options.ResourceHeapTier.0
    ));
    // SAFETY: memory of the probe's own, made and released here; every
    // address used lies inside it.
    unsafe {
        private(&mut p, &mut rig);
        section(&mut p, &mut rig);
        if mb > 0 {
            cost(&mut p, &mut rig, memory, mb);
        }
    }
    p.lines.push(format!(
        "VERDICT: private memory in place: reads {}, writes {}, the heap tells of its end {}, the memory released after it {}; \
         the page pool's section in place: reads {}, writes {}, whatever the views do {}; \
         a reserved section is refused {} (the pool must be made committed to be opened)",
        p.part_reads,
        p.part_writes,
        p.part_notice,
        p.part_released,
        p.section_reads,
        p.section_writes,
        p.section_outlives_views,
        p.reserved_section_refused
    ));
    p.lines.push(judge(&p).1);
    p
}

fn describe(heap: &ID3D12Heap) -> String {
    // SAFETY: a plain query.
    let d = unsafe { heap.GetDesc() };
    format!(
        "heap of {} KiB: type {} (4 = custom), CPU pages {} (3 = write-back), pool {} (1 = the PC's memory), alignment {}, flags {:#x} (0x1 shared, 0x20 cross-adapter, 0x200 write watch, 0x400 shader atomics)",
        d.SizeInBytes >> 10,
        d.Properties.Type.0,
        d.Properties.CPUPageProperty.0,
        d.Properties.MemoryPoolPreference.0,
        d.Alignment,
        d.Flags.0
    )
}

/// Private memory, reserved as `xmem` reserves the title's heaps
/// (committed, read-write, write-watched): a part of it opened.
unsafe fn private(p: &mut Probe, rig: &mut Rig) {
    const SIZE: usize = 64 * K64;
    // SAFETY (the whole function): the allocation below, `SIZE` bytes.
    unsafe {
        let base = mem::VirtualAlloc(std::ptr::null(), SIZE, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE).cast::<u8>();
        if base.is_null() {
            p.lines.push("private memory: none to try with".to_owned());
            because(&mut p.private_why, || "no private memory to test with".to_owned());
            return;
        }
        // The part: 16 pages from the 16th on.
        let (at, bytes) = (base.add(16 * K64), 16 * K64);
        let whole = open(&rig.device, From::Region(base as usize));
        p.lines.push(format!("private memory, the whole allocation (OpenExistingHeapFromAddress): {}", hr(&whole)));
        drop(whole);
        let opened = open(&rig.device, From::Part(at as usize, bytes));
        p.lines.push(format!("private memory, a part of it (OpenExistingHeapFromAddress1): {}", hr(&opened)));
        if opened.is_err() {
            because(&mut p.private_why, || format!("OpenExistingHeapFromAddress1 {}", hr(&opened)));
        }
        static GONE: AtomicBool = AtomicBool::new(false);
        unsafe extern "system" fn gone(_: *mut c_void) {
            GONE.store(true, Ordering::Release);
        }
        GONE.store(false, Ordering::Release);
        let mut noticed = false;
        if let Ok(heap) = &opened {
            p.lines.push(format!("  {}", describe(heap)));
            let notice = notify_on_destruction(heap, gone, std::ptr::null_mut());
            noticed = notice.is_ok();
            p.lines.push(format!("  a destruction notice asked of the heap: {}", hr(&notice)));
            let placed = place(&rig.device, heap, K64 as u64, 4 * K64 as u64);
            p.lines.push(format!("  a buffer placed in it (cross-adapter, unordered access): {}", hr(&placed)));
            if placed.is_err() {
                because(&mut p.private_why, || format!("a buffer placed in the heap {}", hr(&placed)));
            }
            if let Ok(buffer) = &placed {
                let memory = at.add(K64);
                fill(memory, 4 * K64, 1);
                let first = rig.fetch(buffer, 0, 4 * K64).is_ok_and(|got| same(memory, &got));
                fill(memory, 4 * K64, 77);
                let second = rig.fetch(buffer, 0, 4 * K64).is_ok_and(|got| same(memory, &got));
                p.part_reads = first && second;
                if !p.part_reads {
                    because(&mut p.private_why, || "the GPU did not read the memory's bytes through the heap".to_owned());
                }
                p.lines.push(format!("  the GPU reads the memory's bytes: {first}; and those the CPU wrote afterwards, with no call in between: {second}"));
                mem::ResetWriteWatch(base.cast(), SIZE);
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x5a).collect();
                let copied = rig.store(buffer, 4096, &new).is_ok() && same(memory.add(4096), &new);
                let (mut found, mut count, mut granularity) = ([0usize; 64], 64usize, 0u32);
                let watched = mem::GetWriteWatch(0, base.cast(), SIZE, found.as_mut_ptr().cast(), &mut count, &mut granularity) == 0;
                memory.cast::<[u32; 3]>().write([40, 2, 0]);
                let shaded = rig.shade(buffer.GetGPUVirtualAddress());
                let shader = shaded.is_ok() && memory.cast::<[u32; 3]>().read()[2] == 42;
                p.part_writes = copied && shader;
                if !p.part_writes {
                    because(&mut p.private_why, || format!("the CPU did not see what the GPU wrote (copy {copied}, shader {shader})"));
                }
                p.lines.push(format!(
                    "  the CPU reads at the memory's address what the GPU wrote: by a copy {copied}; by a shader through unordered access {shader} ({}); \
                     write watching saw {} of the copy's 16 pages{}",
                    hr(&shaded),
                    if watched { count } else { 0 },
                    if watched { "" } else { " (the asking failed)" }
                ));
            }
            // What the title can still do with the pages while the heap stands.
            let mut old = 0;
            let protect_in = mem::VirtualProtect(at.add(8 * K64).cast(), K64, mem::PAGE_READONLY, &mut old) != 0;
            if protect_in {
                mem::VirtualProtect(at.add(8 * K64).cast(), K64, mem::PAGE_READWRITE, &mut old);
            }
            let decommit_in = mem::VirtualFree(at.add(9 * K64).cast(), K64, mem::MEM_DECOMMIT) != 0;
            let decommit_out = mem::VirtualFree(base.add(40 * K64).cast(), K64, mem::MEM_DECOMMIT) != 0;
            p.lines.push(format!(
                "  with the heap standing, the title's own calls: read-only inside the part {protect_in}, decommit inside {decommit_in}, decommit outside {decommit_out} \
                 (false inside means the heap must go before the title frees or guards those pages)"
            ));
            drop(placed);
        }
        drop(opened);
        p.part_notice = noticed && GONE.load(Ordering::Acquire);
        if !p.part_notice {
            because(&mut p.private_why, || "the heap gave no destruction notice, so the title's own frees could not wait for it".to_owned());
        }
        let freed = mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE) != 0;
        p.part_released = freed;
        if !freed {
            because(&mut p.private_why, || format!("the memory could not be released right after its heap went ({})", std::io::Error::last_os_error()));
        }
        p.lines.push(format!("  the heap told of its end: {}; released after the heap went: {freed}", p.part_notice));
    }
}

/// A section and a view of it in a placeholder, as `xmem` maps physical
/// pages: reserved (as it is made today) and committed.
unsafe fn section(p: &mut Probe, rig: &mut Rig) {
    const SIZE: usize = 32 * K64;
    // SAFETY (the whole function): the sections, placeholder and views
    // made below.
    unsafe {
        let process = windows_sys::Win32::System::Threading::GetCurrentProcess();
        let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        let reserved = mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_RESERVE, 0, SIZE as u32, std::ptr::null());
        if !reserved.is_null() {
            let opened = open(&rig.device, From::Section(reserved as usize));
            p.reserved_section_refused = opened.is_err();
            p.lines.push(format!("a section made reserved, as the page pool is today (OpenExistingHeapFromFileMapping): {}", hr(&opened)));
            drop(opened);
            windows_sys::Win32::Foundation::CloseHandle(reserved);
        }
        let handle = mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT, 0, SIZE as u32, std::ptr::null());
        let hole = mem::VirtualAlloc2(process, std::ptr::null(), SIZE, mem::MEM_RESERVE | mem::MEM_RESERVE_PLACEHOLDER, mem::PAGE_NOACCESS, std::ptr::null_mut(), 0).cast::<u8>();
        if handle.is_null() || hole.is_null() {
            p.lines.push("a committed section: none to try with".to_owned());
            because(&mut p.section_why, || "no committed section to test with".to_owned());
            if !handle.is_null() {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
            if !hole.is_null() {
                mem::VirtualFree(hole.cast(), 0, mem::MEM_RELEASE);
            }
            return;
        }
        // Pages 4..12 of the section at the placeholder's start.
        mem::VirtualFree(hole.cast(), 8 * K64, mem::MEM_RELEASE | mem::MEM_PRESERVE_PLACEHOLDER);
        let view = mem::MapViewOfFile3(handle, process, hole.cast(), (4 * K64) as u64, 8 * K64, mem::MEM_REPLACE_PLACEHOLDER, mem::PAGE_READWRITE, std::ptr::null_mut(), 0);
        let opened = open(&rig.device, From::Section(handle as usize));
        p.lines.push(format!("a section made committed (OpenExistingHeapFromFileMapping): {}", hr(&opened)));
        if opened.is_err() {
            because(&mut p.section_why, || format!("OpenExistingHeapFromFileMapping {}", hr(&opened)));
        }
        if view.Value.is_null() {
            because(&mut p.section_why, || "no view of the test section".to_owned());
        }
        let mut unmapped = false;
        if let (Ok(heap), false) = (&opened, view.Value.is_null()) {
            p.lines.push(format!("  {}", describe(heap)));
            // The section's page 5 is the view's second page.
            let placed = place(&rig.device, heap, 5 * K64 as u64, 2 * K64 as u64);
            p.lines.push(format!("  a buffer placed at the section's offset of a view's pages: {}", hr(&placed)));
            if placed.is_err() {
                because(&mut p.section_why, || format!("a buffer placed in the section's heap {}", hr(&placed)));
            }
            if let Ok(buffer) = &placed {
                let memory = hole.add(K64);
                fill(memory, 2 * K64, 3);
                p.section_reads = rig.fetch(buffer, 0, 2 * K64).is_ok_and(|got| same(memory, &got));
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x33).collect();
                p.section_writes = rig.store(buffer, 0, &new).is_ok() && same(memory, &new);
                // The guard of `watch.rs`: the view read-only.
                let mut old = 0;
                let guarded = mem::VirtualProtect(memory.cast(), K64, mem::PAGE_READONLY, &mut old) != 0;
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x44).collect();
                let through = rig.store(buffer, 0, &new).is_ok() && same(memory, &new);
                // The view goes; the GPU writes; another view shows it.
                unmapped = mem::UnmapViewOfFile2(process, view, mem::MEM_PRESERVE_PLACEHOLDER) != 0;
                let new: Vec<u8> = (0..K64).map(|i| (i as u8) ^ 0x55).collect();
                let stored = rig.store(buffer, 0, &new).is_ok();
                let other = mem::MapViewOfFile(handle, mem::FILE_MAP_WRITE, 0, (5 * K64) as u32, K64);
                p.section_outlives_views = unmapped && stored && !other.Value.is_null() && same(other.Value.cast(), &new);
                if !(p.section_reads && p.section_writes) {
                    because(&mut p.section_why, || format!("through the section's heap the GPU read {} and wrote {}", p.section_reads, p.section_writes));
                } else if !p.section_outlives_views {
                    because(&mut p.section_why, || "the section's heap did not outlive a view of it".to_owned());
                }
                p.lines.push(format!(
                    "  the GPU reads the view's bytes {}; the CPU reads through the view what the GPU wrote {}; the view made read-only under the heap {guarded}, \
                     and the GPU writes it all the same {through}; the view unmapped under the heap {unmapped}, the GPU's write then shows in a view mapped afterwards {}",
                    p.section_reads, p.section_writes, p.section_outlives_views
                ));
                if !other.Value.is_null() {
                    mem::UnmapViewOfFile(other);
                }
            }
        }
        drop(opened);
        if !unmapped && !view.Value.is_null() {
            mem::UnmapViewOfFile2(process, view, mem::MEM_PRESERVE_PLACEHOLDER);
        }
        windows_sys::Win32::Foundation::CloseHandle(handle);
        // The placeholder was split in two: each half is released.
        mem::VirtualFree(hole.cast(), 0, mem::MEM_RELEASE);
        mem::VirtualFree(hole.add(8 * K64).cast(), 0, mem::MEM_RELEASE);
    }
}

/// What opening costs, on `mb` MB each of private memory never touched
/// and of a committed section never touched: the time, the working set,
/// the card's account of the PC's memory shared with it; and how fast the
/// card copies out of such a heap against a shared buffer of today's kind.
unsafe fn cost(p: &mut Probe, rig: &mut Rig, memory: &dyn Fn() -> Option<[u64; 4]>, mb: usize) {
    let bytes = mb << 20;
    let shared = || memory().map_or(0, |m| m[3] >> 20);
    let ms = |speed: Option<f64>| speed.map_or("?".to_owned(), |ms| format!("{ms:.1}"));
    // SAFETY (the whole function): the allocation and the section below.
    unsafe {
        let base = mem::VirtualAlloc(std::ptr::null(), bytes, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE).cast::<u8>();
        if !base.is_null() {
            let (set, account, started) = (working_set_mb(), shared(), std::time::Instant::now());
            let opened = open(&rig.device, From::Part(base as usize, bytes));
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let placed = opened.as_ref().ok().and_then(|heap| place(&rig.device, heap, 0, bytes as u64).ok());
            let speed = placed.as_ref().and_then(|buffer| rig.copy_ms(buffer, bytes as u64).ok());
            p.lines.push(format!(
                "cost, {mb} MB of private memory never touched: {}; opened in {took:.1} ms; working set {set} -> {} MB; shared with the card {account} -> {} MB; \
                 the card copies it out in {} ms",
                hr(&opened),
                working_set_mb(),
                shared(),
                ms(speed)
            ));
            drop((placed, opened));
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
        let invalid = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        let handle = mem::CreateFileMappingW(invalid, std::ptr::null(), mem::PAGE_READWRITE | mem::SEC_COMMIT, (bytes as u64 >> 32) as u32, bytes as u32, std::ptr::null());
        if !handle.is_null() {
            let (set, account, started) = (working_set_mb(), shared(), std::time::Instant::now());
            let opened = open(&rig.device, From::Section(handle as usize));
            let took = started.elapsed().as_secs_f64() * 1000.0;
            let (set_opened, account_opened) = (working_set_mb(), shared());
            let placed = opened.as_ref().ok().and_then(|heap| place(&rig.device, heap, 0, bytes as u64).ok());
            let speed = placed.as_ref().and_then(|buffer| rig.copy_ms(buffer, bytes as u64).ok());
            p.lines.push(format!(
                "cost, {mb} MB of a committed section never touched: {}; opened in {took:.1} ms; working set {set} -> {set_opened} -> after the card read all of it {} MB; \
                 shared with the card {account} -> {account_opened} -> {} MB; the card copies it out in {} ms",
                hr(&opened),
                working_set_mb(),
                shared(),
                ms(speed)
            ));
            drop((placed, opened));
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        // Today's kind of host copy, for the comparison.
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let made = rig.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(bytes as u64, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut buffer,
        );
        if let (Ok(()), Some(buffer)) = (made, buffer) {
            let speed = rig.copy_ms(&buffer, bytes as u64).ok();
            p.lines.push(format!("cost, for comparison: the card copies {mb} MB out of a shared buffer of today's kind in {} ms", ms(speed)));
        }
    }
}

// The census: which of the title memory that has host copies today could
// be opened, and by which opening.

/// Counts and bytes by kind, in the order of [`KINDS`].
#[derive(Default, Clone, Copy)]
struct Tally([(u64, u64); 6]);

const KINDS: [&str; 6] = [
    "private, committed whole (a part can be opened)",
    "view memory, one run of a section (one placed buffer)",
    "view memory, scattered pages",
    "view memory with unmapped pages",
    "private but not committed whole, or guarded",
    "other",
];

fn kind_of(at: usize, len: usize, known: &mut Vec<(usize, usize)>) -> usize {
    match cover(at, len) {
        Cover::Run(..) => return 1,
        Cover::Scattered => return 2,
        Cover::Holes => return 3,
        Cover::None => {}
    }
    let end = at + len;
    if known.iter().any(|&(from, to)| from <= at && end <= to) {
        return 0;
    }
    // SAFETY: plain data filled in by the query.
    let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) };
    if n == 0 {
        return 5;
    }
    let reach = (info.BaseAddress as usize).saturating_add(info.RegionSize);
    if info.Type == mem::MEM_PRIVATE && info.State == mem::MEM_COMMIT && info.Protect == mem::PAGE_READWRITE {
        known.push((info.BaseAddress as usize, reach));
        return if reach >= end { 0 } else { 4 };
    }
    match info.Type {
        mem::MEM_PRIVATE => 4,
        mem::MEM_MAPPED => 3,
        _ => 5,
    }
}

/// The census lines for `spans` (start, bytes, whether a window outside
/// buffers).
fn census_of(spans: &[(u64, u64, bool)]) -> Vec<String> {
    let mut known = Vec::new();
    let (mut buffers, mut windows) = (Tally::default(), Tally::default());
    for &(start, len, window) in spans {
        let kind = kind_of(start as usize, len as usize, &mut known);
        let tally = if window { &mut windows } else { &mut buffers };
        tally.0[kind].0 += 1;
        tally.0[kind].1 += len;
    }
    let mut lines = Vec::new();
    for (name, tally) in [("title buffers", buffers), ("windows outside buffers", windows)] {
        let all = tally.0.iter().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        lines.push(format!("census, {name}: {} of {} MB", all.0, all.1 >> 20));
        for (kind, &(n, bytes)) in KINDS.iter().zip(&tally.0) {
            if n > 0 {
                lines.push(format!("census,   {kind}: {n} of {} MB", bytes >> 20));
            }
        }
    }
    let v = views();
    let (count, bytes) = (v.views.len(), v.views.values().map(|view| view.0 as u64).sum::<u64>());
    let pages = v.views.values().filter(|view| view.0 == K64).count();
    let big = v.views.values().map(|view| view.0).max().unwrap_or(0);
    lines.push(format!(
        "census, the title's views of physical pages: {count} of {} MB; {pages} of them one 64 KiB page; the longest {} MB",
        bytes >> 20,
        big >> 20
    ));
    for &(_, bytes, committed) in &v.sections {
        lines.push(format!(
            "census, a section of {} MB, {}",
            bytes >> 20,
            if committed { "committed: it can be opened as one heap" } else { "reserved: refused as a heap until it is made with SEC_COMMIT" }
        ));
    }
    lines
}

/// With [`probe_level`] 2: one part of the title's own private memory
/// (the start of the biggest such title buffer, 4 MiB at most) is opened
/// for a moment, a buffer placed, 64 KiB read by the GPU and compared
/// with what the CPU reads there (the title may be writing it meanwhile:
/// the count of equal bytes is given), and all of it released. While the
/// heap stands (milliseconds) a `VirtualFree` of the title's on those
/// pages would fail.
fn real_part(spans: &[(u64, u64, bool)]) -> Vec<String> {
    let Some(host) = crate::host::host() else {
        return Vec::new();
    };
    let mut known = Vec::new();
    let found = spans
        .iter()
        .filter(|s| !s.2 && s.1 >= 2 * K64 as u64)
        .filter(|s| kind_of(s.0 as usize, s.1 as usize, &mut known) == 0)
        .max_by_key(|s| s.1);
    let Some(&(start, len, _)) = found else {
        return vec!["the title's memory: no title buffer in private, committed memory to try".to_owned()];
    };
    let at = (start as usize).next_multiple_of(K64);
    let bytes = ((start + len) as usize).saturating_sub(at).min(4 << 20) & !(K64 - 1);
    if bytes == 0 || !private_whole(at, bytes) {
        return vec!["the title's memory: the buffer found holds no whole 64 KiB page that is still committed".to_owned()];
    }
    let mut lines = Vec::new();
    let (set, started) = (working_set_mb(), std::time::Instant::now());
    // As the live path opens it: a pin, so that a title call to free or
    // protect these pages meanwhile waits for the heap instead of failing.
    let placed = place_part(&host.device, at as u64, bytes as u64, u64::MAX);
    lines.push(format!(
        "the title's memory, {} KiB at {at:#x} (of the title buffer {start:#x}+{len:#x}), opened and a buffer placed over it: {}; in {:.1} ms; working set {set} -> {} MB",
        bytes >> 10,
        match &placed {
            Ok(_) => "ok".to_owned(),
            Err(e) => format!("REFUSED {e:?}"),
        },
        started.elapsed().as_secs_f64() * 1000.0,
        working_set_mb()
    ));
    if let Ok(placed) = &placed
        && let Ok(mut rig) = Rig::new(&host.device)
    {
        let buffer = &placed.buffer;
        let mut before = vec![0u8; K64];
        crate::host::copy_mapped(before.as_mut_ptr(), at as *const u8, K64, false);
        let got = rig.fetch(buffer, 0, K64);
        let mut after = vec![0u8; K64];
        crate::host::copy_mapped(after.as_mut_ptr(), at as *const u8, K64, false);
        match got {
            Ok(got) => {
                let equal = (0..K64).filter(|&i| got[i] == before[i] || got[i] == after[i]).count();
                let zeros = got.iter().filter(|&&b| b == 0).count();
                lines.push(format!(
                    "  the GPU read 64 KiB of the title's own bytes: {equal} of {K64} equal to what the CPU read just before or just after ({zeros} of them zero)"
                ));
            }
            Err(e) => lines.push(format!("  the GPU's read failed: {e}")),
        }
    }
    lines
}

/// Runs the start-up test once, on memory of its own and a queue of its
/// own: the cheap part of the probe (a few 64 KiB pages; the cost
/// measurement only with [`probe_level`]). Until it has run nothing is
/// placed; what it refuses is copied. Its last line says in plain words
/// what was accepted or refused and why.
fn start_up_test() -> bool {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if STATE.load(Ordering::Acquire) & TESTED != 0 {
        return false;
    }
    if RUNNING.swap(true, Ordering::AcqRel) {
        return false;
    }
    let Some(host) = crate::host::host() else {
        RUNNING.store(false, Ordering::Release);
        return false;
    };
    let mb = if probe_level() > 0 {
        std::env::var("VICTORY_INPLACE_PROBE_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(64usize).min(1024)
    } else {
        0
    };
    let took = std::time::Instant::now();
    let probe = probe(&host.device, &|| host.video_memory(), mb);
    if probe_level() > 0 {
        for line in &probe.lines {
            tracing::info!("in place probe: {line}");
        }
        tracing::info!("in place probe: took {:.0} ms", took.elapsed().as_secs_f64() * 1000.0);
    }
    let (state, line) = judge(&probe);
    STATE.store(state, Ordering::Release);
    if state & (PRIVATE_OK | SECTIONS_OK) == 0 {
        tracing::warn!("{line}");
    } else {
        tracing::info!("{line}");
    }
    tracing::info!("start-up (later): {line}");
    true
}

/// Called once a second by the thread that submits, before a submission
/// (`host::sweep`): the start-up test at the first call while in place is
/// on or the probe is asked for; the status line every ten seconds; with
/// [`probe_level`], the census (on a thread of its own: it asks Windows
/// about every copy) 30 and 150 seconds after the test.
pub(crate) fn tick() {
    static STARTED: AtomicU64 = AtomicU64::new(0);
    if (level() > 0 || probe_level() > 0) && start_up_test() {
        STARTED.store(crate::host::now_ms().max(1), Ordering::Relaxed);
    }
    if level() > 0 {
        say();
    }
    if probe_level() == 0 {
        return;
    }
    let started = STARTED.load(Ordering::Relaxed);
    if started == 0 {
        return;
    }
    static DONE: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let due = match DONE.load(Ordering::Relaxed) {
        0 => now >= started + 30_000,
        1 => now >= started + 150_000,
        _ => false,
    };
    if due {
        let which = DONE.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let spans = crate::shadow::spans();
            for line in census_of(&spans) {
                tracing::info!("in place {line}");
            }
            if which == 0 && probe_level() >= 2 {
                for line in real_part(&spans) {
                    tracing::info!("in place probe: {line}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The switch: unset is level 2, numbers above 2 are 2, anything else
    /// set is off.
    #[test]
    fn the_level_asked_for() {
        assert_eq!(parse_requested(None), 2);
        assert_eq!(parse_requested(Some("")), 2);
        assert_eq!(parse_requested(Some(" 1 ")), 1);
        assert_eq!(parse_requested(Some("0")), 0);
        assert_eq!(parse_requested(Some("7")), 2);
        assert_eq!(parse_requested(Some("on")), 0);
    }

    /// Level 2 never stands beside the late wait, whichever way it came on;
    /// only explicit switches keep the late wait on.
    #[test]
    fn level_two_and_the_late_wait_are_never_both_on() {
        assert_eq!(explicit_late(None, None, false), None, "on by default only");
        assert_eq!(explicit_late(Some("0"), Some("2"), false), None, "late wait off explicitly");
        assert_eq!(explicit_late(Some("3"), None, false).as_deref(), Some("VICTORY_LATE_WAIT=3"));
        assert_eq!(explicit_late(None, Some("1"), false).as_deref(), Some("VICTORY_RELAY=1"));
        assert_eq!(explicit_late(None, Some("0"), false), None);
        assert_eq!(explicit_late(Some("1"), None, true), None, "the timing run waits for everything");
        for requested in 0..=2 {
            for late in [false, true] {
                for explicit in [None, Some("VICTORY_LATE_WAIT=1")] {
                    let (level, why) = resolve(requested, late, explicit);
                    assert!(level <= requested, "{why}");
                    assert!(!(level == 2 && late), "level 2 with the late wait: {why}");
                }
            }
        }
        assert_eq!(resolve(2, false, None).0, 2);
        assert_eq!(resolve(2, true, Some("VICTORY_RELAY=2")).0, 1);
        assert_eq!(resolve(1, true, None).0, 1, "windows are never written by shaders");
    }

    /// Heaps and placed buffers are cut on 64 KiB pages around the range.
    #[test]
    fn pages_around_a_range() {
        let k = K64 as u64;
        assert_eq!(pages_around(0x10_0000, 1), (0x10_0000, k));
        assert_eq!(pages_around(0x10_0000 + 0x1230, 2 * k + 5), (0x10_0000, 3 * k));
        assert_eq!(pages_around(0x10_0000 + k - 1, 2), (0x10_0000, 2 * k), "two bytes over a line");
        assert_eq!(pages_around(5 * k, 0), (5 * k, k), "an unsized ask holds its first byte");
    }

    /// A range over views of two sections is no one heap's, even when the
    /// pages continue each other in address.
    #[test]
    fn views_over_two_sections_are_told_apart() {
        let mut views = BTreeMap::new();
        let at = 0x9_0000_0000usize;
        views.insert(at, (2 * K64, 11, 0));
        views.insert(at + 2 * K64, (2 * K64, 12, 0));
        views.insert(at + 4 * K64, (K64, 11, 9 * K64 as u64));
        assert_eq!(cover_in(&views, at, 3 * K64), Cover::Scattered);
        assert_eq!(sections_under(&views, at, 3 * K64), 2);
        assert_eq!(sections_under(&views, at, 2 * K64), 1);
        assert_eq!(cover_in(&views, at + 3 * K64, 2 * K64), Cover::Scattered);
        assert_eq!(sections_under(&views, at + 3 * K64, 2 * K64), 2);
        assert_eq!(cover_in(&views, at + K64, K64), Cover::Run(11, K64 as u64));
    }

    fn pin_for_test(id: u64, at: usize, bytes: usize) {
        pins().push(Pin { id, at, bytes });
        PIN_COUNT.fetch_add(1, Ordering::AcqRel);
        PINNED.fetch_add(bytes as u64, Ordering::AcqRel);
    }

    /// A title call over a pinned range waits until the heap's notice
    /// comes, and goes on at once over memory no heap covers; one that
    /// outlasts the wait is said to the caller, which holds the call back.
    #[test]
    fn title_calls_wait_for_the_heap_over_their_memory() {
        let at = 0x6_0000_0000usize;
        pin_for_test(0xdead_0001, at, 4 * K64);
        assert!(!title_memory_freeing(at + 8 * K64, K64), "outside the pin: no wait");
        let notice = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            // SAFETY: the callback reads its argument as a number only.
            unsafe { heap_destroyed(0xdead_0001usize as *mut c_void) };
        });
        let started = std::time::Instant::now();
        assert!(!title_memory_freeing(at + K64, K64), "the heap went: the call may go on");
        assert!(started.elapsed() >= std::time::Duration::from_millis(40), "it waited for the notice");
        notice.join().unwrap();
        assert!(!pinned_over(&pins(), at, at + 4 * K64));
        // A heap that does not go in time.
        pin_for_test(0xdead_0002, at, K64);
        let started = std::time::Instant::now();
        assert!(title_memory_still_pinned(at, K64), "the look without the wait sees the heap");
        assert!(!title_memory_still_pinned(at + 8 * K64, K64));
        assert!(started.elapsed() < RETIRE_WAIT, "and does not wait");
        let started = std::time::Instant::now();
        assert!(title_memory_freeing(at, K64), "still under a heap");
        assert!(started.elapsed() >= RETIRE_WAIT);
        // SAFETY: as above.
        unsafe { heap_destroyed(0xdead_0002usize as *mut c_void) };
        assert!(!title_memory_freeing(at, K64));
    }

    /// Views as the runtime tells them: a range inside one view, or over
    /// views that continue each other in one section, is one run; over
    /// views from elsewhere it is scattered; over a gap it has holes.
    #[test]
    fn views_say_what_one_placed_buffer_could_show() {
        let mut views = BTreeMap::new();
        let at = 0x7_0000_0000usize;
        views.insert(at, (4 * K64, 9, 16 * K64 as u64));
        // Continues the first in the section.
        views.insert(at + 4 * K64, (2 * K64, 9, 20 * K64 as u64));
        // Follows in address, from elsewhere in the section.
        views.insert(at + 6 * K64, (K64, 9, 3 * K64 as u64));
        // After a gap of one page.
        views.insert(at + 8 * K64, (K64, 9, 4 * K64 as u64));
        assert_eq!(cover_in(&views, at + K64, 2 * K64), Cover::Run(9, 17 * K64 as u64));
        assert_eq!(cover_in(&views, at + K64, 5 * K64), Cover::Run(9, 17 * K64 as u64));
        assert_eq!(cover_in(&views, at, 7 * K64), Cover::Scattered);
        assert_eq!(cover_in(&views, at + 6 * K64, 3 * K64), Cover::Holes);
        assert_eq!(cover_in(&views, at + 7 * K64, K64), Cover::None);
        assert_eq!(cover_in(&views, at - K64, 2 * K64), Cover::None);
    }

    /// What the runtime forgets when memory is mapped anew.
    #[test]
    fn remapped_memory_forgets_its_views() {
        let at = 0x7_4000_0000usize;
        title_memory_view(at, 4 * K64, 77, 0);
        title_memory_view(at + 4 * K64, K64, 77, 4 * K64 as u64);
        assert_eq!(cover(at, 5 * K64), Cover::Run(77, 0));
        // One page inside the first view goes: the whole view is forgotten
        // (the runtime maps and tells again what stays of it).
        views_gone(at + K64, K64);
        assert_eq!(cover(at, K64), Cover::None);
        assert_eq!(cover(at + 4 * K64, K64), Cover::Run(77, 4 * K64 as u64));
        views_gone(at + 4 * K64, K64);
        assert_eq!(cover(at + 4 * K64, K64), Cover::None);
    }

    /// Only private memory committed whole, read-write, is taken for a part.
    #[test]
    fn only_private_memory_committed_whole_is_a_part() {
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE, mem::PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert!(!private_whole(base as usize, K64), "reserved only");
            mem::VirtualAlloc(base.cast(), 4 * K64, mem::MEM_COMMIT, mem::PAGE_READWRITE);
            assert!(private_whole(base as usize, 4 * K64));
            assert!(private_whole(base as usize + K64, 2 * K64));
            assert!(!private_whole(base as usize, 5 * K64), "reaches into the reserved half");
            let mut old = 0;
            mem::VirtualProtect(base.cast(), K64, mem::PAGE_READONLY, &mut old);
            assert!(!private_whole(base as usize, 2 * K64), "a guarded page");
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
    }

    pub(crate) fn warp() -> Option<ID3D12Device> {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::*;
        // SAFETY: plain DXGI queries and device creation on the software
        // adapter; a card is never opened.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            let desc = adapter.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()?;
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                return None;
            }
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    /// The probe, whole, on the software adapter: the same calls the card
    /// gets. The runtime's and the kernel's rules hold there; the card's
    /// driver is what a run with `VICTORY_INPLACE_PROBE=1` adds.
    #[test]
    fn warp_probe_opens_title_like_memory_and_the_gpu_sees_its_bytes() {
        let Some(device) = warp() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            eprintln!("no ID3D12Device13 here: parts of an allocation cannot be opened");
            return;
        }
        let probe = probe(&device, &|| None, 4);
        for line in &probe.lines {
            eprintln!("in place probe: {line}");
        }
        assert!(probe.part_reads, "the GPU reads private memory in place");
        assert!(probe.part_writes, "GPU writes land in private memory");
        assert!(probe.section_reads && probe.section_writes, "a committed section in place");
        assert!(probe.section_outlives_views, "the section's heap does not depend on views");
        assert!(probe.reserved_section_refused, "a reserved section is refused");
    }

    /// Stage 1's own call on the software adapter: a title buffer that
    /// begins and ends off the 64 KiB lines is placed over its pages, the
    /// GPU reads the buffer's bytes at the offset the views use, and its
    /// write lands at the title's address. Memory that is not committed
    /// whole is turned down without asking the host.
    #[test]
    fn warp_a_title_buffer_in_place_shows_its_bytes_at_its_offset() {
        let Some(device) = warp() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            return;
        }
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE).cast::<u8>();
            let (start, len) = (base as u64 + K64 as u64 + 0x1230, 2 * K64 as u64 + 5);
            let placed = place_part(&device, start, len, u64::MAX).unwrap_or_else(|e| panic!("not placed: {e:?}"));
            assert_eq!(placed.bias, 0x1230);
            assert_eq!(placed.gpu, placed.buffer.GetGPUVirtualAddress() + 0x1230);
            assert_eq!(placed.buffer.GetDesc().Width, 3 * K64 as u64, "the pages that hold the buffer");
            let mut rig = Rig::new(&device).unwrap();
            fill(start as *mut u8, len as usize, 7);
            let got = rig.fetch(&placed.buffer, placed.bias, len as usize).unwrap();
            assert!(same(start as *const u8, &got), "the GPU reads the title buffer's bytes");
            rig.store(&placed.buffer, placed.bias + 16, &[0xa5; 64]).unwrap();
            assert!(same((start + 16) as *const u8, &[0xa5; 64]), "the GPU's write is at the title's address");
            drop(placed);
            // Half of it decommitted: turned down, the host not asked.
            assert_ne!(mem::VirtualFree(base.add(2 * K64).cast(), K64, mem::MEM_DECOMMIT), 0);
            assert!(matches!(place_part(&device, start, len, u64::MAX), Err(Refusal::NotWhole)));
            mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE);
        }
    }

    /// On the software adapter: an opening over the budget is turned down
    /// before the host is asked; one within it counts its pages until the
    /// heap's notice says it is gone, and then the memory is free to go.
    #[test]
    fn warp_pinned_bytes_are_counted_until_the_heap_goes() {
        let Some(device) = warp() else { return };
        if device.cast::<ID3D12Device13>().is_err() {
            return;
        }
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 4 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE).cast::<u8>();
            let before = PINNED.load(Ordering::Acquire);
            assert!(matches!(place_part(&device, base as u64, 2 * K64 as u64, 0), Err(Refusal::OverBudget)));
            assert_eq!(PINNED.load(Ordering::Acquire), before, "nothing kept of a refusal");
            let placed = place_part(&device, base as u64 + 10, K64 as u64, u64::MAX).unwrap();
            assert!(pinned_over(&pins(), base as usize, base as usize + 2 * K64));
            assert!(PINNED.load(Ordering::Acquire) >= before + 2 * K64 as u64, "two pages under the heap");
            drop(placed);
            assert!(!pinned_over(&pins(), base as usize, base as usize + 2 * K64), "the notice came");
            assert_ne!(mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE), 0, "released after the heap went");
        }
    }

    /// The census names each kind of memory.
    #[test]
    fn census_tells_private_memory_from_views() {
        // SAFETY: an allocation of the test's own.
        unsafe {
            let base = mem::VirtualAlloc(std::ptr::null(), 8 * K64, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE) as u64;
            let view = 0x7_8000_0000u64;
            title_memory_view(view as usize, 2 * K64, 5, 0);
            title_memory_view(view as usize + 2 * K64, K64, 5, 9 * K64 as u64);
            let lines = census_of(&[(base, 2 * K64 as u64, false), (base + 4 * K64 as u64, K64 as u64, true), (view, K64 as u64, true), (view, 3 * K64 as u64, false)]);
            let text = lines.join("\n");
            assert!(text.contains("census, title buffers: 2 of"), "{text}");
            assert!(text.contains("census,   private, committed whole (a part can be opened): 1 of"), "{text}");
            assert!(text.contains("census,   view memory, scattered pages: 1 of"), "{text}");
            assert!(text.contains("census,   view memory, one run of a section (one placed buffer): 1 of"), "{text}");
            views_gone(view as usize, 3 * K64);
            mem::VirtualFree(base as *mut c_void, 0, mem::MEM_RELEASE);
        }
    }
}
