//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_COMMITMENT_LIMIT, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER,
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    MemExtendedParameterAddressRequirements, PAGE_NOACCESS, PAGE_READWRITE, SEC_COMMIT,
    SEC_RESERVE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

const MIB: usize = 1 << 20;

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
pub(crate) fn on_wine() -> bool {
    static WINE: LazyLock<bool> = LazyLock::new(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: plain lookups in ntdll, always loaded.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    });
    *WINE
}

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

// Console additions to the host's flags.
/// 2 MiB pages; the host reads this bit as large pages, which need a
/// privilege.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages; the host reads it as AWE memory.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000). Write-combining and uncached are
/// dropped too: our GPU side reads this memory through the CPU.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (
/// the title's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    virtual_alloc(
        base,
        size,
        kind,
        flags,
        protect,
        victory_gpu::title_memory_committed,
    )
}

fn virtual_alloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
    on_commit: impl FnOnce(usize, usize),
) -> *mut c_void {
    let (lowest, highest) = address_range(flags);
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        Alignment: if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
            2 * MIB
        } else {
            0
        },
    };
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`); on the host only a placeholder can.
    // shortcut: committing part of such a reservation later would fail
    // (the title's 23 calls never do); replace that part of the
    // placeholder then.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a placeholder the title split: it stays one.
            return base;
        }
        protect = PAGE_NOACCESS;
    }
    let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
    }
    // A commit or reset at an address the title names may meet memory the
    // GPU side has in place (`title_calls`).
    if !base.is_null() {
        title_calls::prepare(base as usize, size, kind & MEM_COMMIT != 0);
    }
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // Address requirements apply to reservations at an address of our choice.
    let constrained = base.is_null();
    if constrained {
        kind |= MEM_RESERVE;
        // The GPU side copies only the pages the title wrote since its last
        // copy (`victory_gpu`, decision 0007); placeholders cannot be
        // watched (their physical-page views are copied whole).
        if kind & MEM_RESERVE_PLACEHOLDER == 0 {
            kind |= MEM_WRITE_WATCH;
        }
    }
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            kind,
            protect,
            if constrained {
                &mut parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    if base.is_null() {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{at:p}"),
            "XMemVirtualAlloc"
        );
    }
    // Memory committed for the title: the GPU side's notes that blocks there
    // are uncommitted go (`VICTORY_ABSENT_ONCE`).
    if !at.is_null() && kind & MEM_COMMIT != 0 {
        // Windows rounds a fixed commit's address down to a page. The
        // caller's last byte can be in the next 64 KiB watch block.
        let start = if base.is_null() { at } else { base };
        on_commit(start as usize, size);
    }
    at
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, since
// all of it is GPU-reachable here (the title's BVH builder, the
// only caller, wants graphics memory); remember the flags of
// `XMemVirtualAlloc` regions if a title tells CPU-only memory apart.
pub extern "system" fn XMemVirtualQuery(
    process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: the title passes room for the whole structure; the host
    // fails on a null one.
    unsafe {
        if VirtualQueryEx(process, address, out.cast(), size) == 0 {
            return 0;
        }
        let info = &mut *out;
        info.flags = if info.host.State == MEM_COMMIT {
            GRAPHICS
        } else {
            0
        };
        info.tag_code = 0;
        info.tag = [0; 4];
    }
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "system" fn(*mut c_void, u64);

/// The allocation and free routines the title's exe exports as
/// `__XMemAllocationHooks__` (the title does): `XMemAlloc` and
/// `XMemFree` call them, for the title's own calls and its middleware's;
/// the `…Default` exports do not.
fn hooks() -> [usize; 2] {
    static AT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = match AT.get() {
        Some(&at) => at,
        // Looked up once the title is mapped, which is before it runs.
        None => match crate::runtime::main_image() {
            Some(base) => *AT.get_or_init(|| {
                use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
                // SAFETY: a mapped image and a NUL-terminated name.
                let at = unsafe {
                    GetProcAddress(base as _, c"__XMemAllocationHooks__".as_ptr().cast())
                }
                .map_or(0, |f| f as usize);
                tracing::info!(found = at != 0, "xmem allocation hooks");
                at
            }),
            None => 0,
        },
    };
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers.
    unsafe { (at as *const [usize; 2]).read() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks()[0] {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc: AllocRoutine = unsafe { std::mem::transmute(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`XMemAlloc`].
pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    match hooks()[1] {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free: FreeRoutine = unsafe { std::mem::transmute(hook) };
            free(address, attributes)
        }
    }
}

/// The system allocator behind [`XMemAlloc`].
pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ALLOC_32BIT != 0 {
        return XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the host heap's addresses lie above 2^40, unlike the
    // console's; nothing seen builds ray-tracing structures in this memory
    // (the title uses its GPU heap). Sub-allocate from a
    // reservation below 2^40 if one does.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        return block.cast();
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    // SAFETY: the address came from `XMemAlloc` with these attributes: a
    // whole reservation, or a header-prefixed block.
    unsafe {
        if attributes & ALLOC_32BIT != 0 {
            // As the title's own frees go: what is in place there goes first.
            title_calls::free(address, 0, MEM_RELEASE);
            return;
        }
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a pagefile-backed section, its pages are 64 KiB slices of it, and a
// mapping is a view of consecutive slices in place of a host placeholder.

const PAGE: usize = 64 << 10;

/// Whether the GPU side places title memory in place (`VICTORY_INPLACE`
/// above 0; unset is 2, on): then the page pool is made so that a host
/// heap can be opened on it. The same rule as `victory_gpu`'s
/// `inplace::requested`: unset or empty is on, a number is its level, any
/// other value is off.
pub(crate) fn in_place_requested() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| match std::env::var("VICTORY_INPLACE").as_deref().map(str::trim) {
        Err(_) | Ok("") => true,
        Ok(v) => v.parse::<u8>().is_ok_and(|n| n > 0),
    });
    *ON
}

/// With in place on, the page pool is made of committed sections of at
/// most this many pages (256 MiB). A committed section is charged against
/// the system's commit when it is made, and one of the 6.1 GiB the title
/// asks for at once is often refused (error 1450: the kernel's prototype
/// page tables for the whole section are one allocation); sections of
/// this size are not. Each is one heap on the GPU side, so a title range
/// that runs over two of them is copied, not placed (counted there).
const POOL_SECTION_PAGES: usize = 4096;

/// The pages of each section an allocation of `n` pages is made of.
fn pool_sections(n: usize, most: usize) -> Vec<usize> {
    let most = most.max(1);
    (0..n.div_ceil(most)).map(|i| (n - i * most).min(most)).collect()
}

/// A section of physical pages: its handle, and whether it was made
/// committed (`SEC_COMMIT`): its pages need no commit when mapped, and the
/// GPU side can open it as a heap.
struct Section(HANDLE, bool);

// SAFETY: a section handle may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        // Said before the handle is closed, so that its number cannot name
        // another section on the GPU side meanwhile.
        victory_gpu::title_memory_section(self.0 as usize, 0, false);
        // SAFETY: our handle; views keep the section alive on their own.
        unsafe { CloseHandle(self.0) };
    }
}

/// A pagefile-backed section of `size` bytes, committed or reserved.
fn new_section(size: usize, committed: bool) -> Result<Section, std::io::Error> {
    // SAFETY: plain section creation.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE | if committed { SEC_COMMIT } else { SEC_RESERVE },
            (size as u64 >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    victory_gpu::title_memory_section(handle as usize, size as u64, committed);
    Ok(Section(handle, committed))
}

/// The sections for an allocation of `n` pages, with the pages of each.
///
/// In place off: one section, reserved, its pages
/// committed when first mapped or written (`commit`), since the title
/// asks for 6.1 GiB at once, which a busy host often cannot commit (error
/// 1450); under Wine committed whole, since a committed section is a
/// sparse file there, free until touched, and committing a reserved one
/// page by page costs a wineserver request each that scans every range
/// committed so far.
///
/// In place on: committed sections of [`POOL_SECTION_PAGES`] at most, which
/// the GPU side can open as heaps. If Windows refuses one, the sections
/// made so far go and the allocation is made as with in place off (said
/// once per refusal); its memory is then copied, not placed.
fn pool(n: usize) -> Result<Vec<(Arc<Section>, usize)>, std::io::Error> {
    if in_place_requested() {
        let mut made: Vec<(Arc<Section>, usize)> = Vec::new();
        let mut refused = None;
        for pages in pool_sections(n, POOL_SECTION_PAGES) {
            match new_section(pages * PAGE, true) {
                Ok(section) => made.push((Arc::new(section), pages)),
                Err(error) => {
                    refused = Some(error);
                    break;
                }
            }
        }
        let Some(error) = refused else {
            return Ok(made);
        };
        let done: usize = made.iter().map(|m| m.1).sum();
        drop(made);
        victory_core::causes::note(victory_core::causes::Cause::PoolOldForm);
        tracing::warn!(
            "the page pool's openable form refused by Windows ({error}) after {} of {} MB in committed sections of {} MB: \
             this allocation is made reserved, as with VICTORY_INPLACE=0, and its memory is copied, not placed in place; \
             a bigger paging file or fewer other programs make room for it",
            (done * PAGE) >> 20,
            (n * PAGE) >> 20,
            (POOL_SECTION_PAGES * PAGE) >> 20
        );
    }
    Ok(vec![(Arc::new(new_section(n * PAGE, on_wine())?), n)])
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<Section>,
    index: usize,
}

impl Slice {
    /// The key in `Physical::shown` of the page `k` pages after this one.
    fn key(&self, k: usize) -> (usize, usize) {
        (Arc::as_ptr(&self.section) as usize, self.index + k)
    }
}

#[derive(Default)]
struct Physical {
    next_page: usize,
    pages: HashMap<usize, Slice>,
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
    /// Where each page shows: by its section and its index there, the
    /// address of that page in every view that shows it now ([`shown_at`];
    /// as upstream's c75fdba keeps it). The title maps pages where the
    /// card reads them and has them filled afterwards by DirectStorage,
    /// which writes through views of our own.
    shown: HashMap<(usize, usize), Vec<usize>>,
}

static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(|| {
    Mutex::new(Physical {
        // Page numbers are opaque to titles; 0 stays unused.
        next_page: 0x10_0000,
        ..Default::default()
    })
});

fn physical() -> MutexGuard<'static, Physical> {
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    if n.checked_mul(PAGE).is_none_or(|s| s == 0) {
        return 0;
    }
    // Page numbers are opaque to the title: which section a page lies in
    // is ours to choose ([`pool`]).
    let sections = match pool(n) {
        Ok(sections) => sections,
        Err(error) => {
            tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
            // SAFETY: checked non-null.
            unsafe { count.write(0) };
            return 0;
        }
    };
    tracing::debug!(
        n,
        sections = sections.len(),
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    let mut physical = physical();
    let first = physical.next_page;
    physical.next_page += n * FRAMES;
    let mut k = 0;
    for (section, count) in sections {
        for index in 0..count {
            let slice = Slice {
                section: section.clone(),
                index,
            };
            let number = first + k * FRAMES;
            physical.pages.insert(number, slice);
            // SAFETY: the title passes room for `n` page numbers.
            unsafe { pages.add(k).write(number) };
            k += 1;
        }
    }
    1
}

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: the title keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return 0;
    }
    let mut physical = physical();
    for i in 0..n {
        // SAFETY: the title passes `n` page numbers.
        physical.pages.remove(&unsafe { *pages.add(i) });
    }
    1
}

/// A view of our own on consecutive slices of one section, committed and
/// writable, unmapped when dropped.
struct View {
    at: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl View {
    /// Maps `n` slices from `slice` on.
    fn map(slice: &Slice, n: usize) -> Option<View> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let (offset, len) = ((slice.index * PAGE) as u64, n * PAGE);
        // SAFETY: slices of a section we own; the view is ours until dropped.
        let at = unsafe {
            MapViewOfFile(
                slice.section.0,
                FILE_MAP_WRITE,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if at.Value.is_null() {
            return None;
        }
        let view = View { at, len };
        commit(at.Value as usize, len, slice.section.1).then_some(view)
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view holds `len` committed read-write bytes, ours
        // alone while `self` lives (other views of the slices only alias
        // them, as the console's do).
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(self.at) };
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. One look at the page table, which the
/// title's own mapping calls share with every DirectStorage worker. `None`
/// if a page is unknown or `pages` ends early.
fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let physical = physical();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let slice = physical.pages.get(page)?;
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

/// Whether every page under `[offset, offset + len)` is known (a write that
/// fails with them known is a host failure).
pub(crate) fn pages_known(pages: &[usize], offset: usize, len: usize) -> bool {
    runs(pages, offset, len).is_some()
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// One view per run of consecutive pages, not one per page: each view
/// costs a map, a commit and an unmap (which flushes every core's TLB).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let Some(mut view) = View::map(slice, *n) else {
            return false;
        };
        let k = (n * PAGE - within).min(data.len() - done);
        view.bytes()[within..within + k].copy_from_slice(&data[done..done + k]);
        (within, done) = (0, done + k);
    }
    true
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy.
pub(crate) struct PageWindow {
    view: View,
    within: usize,
    len: usize,
}

impl PageWindow {
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        let (within, len) = (self.within, self.len);
        &mut self.view.bytes()[within..within + len]
    }
}

/// A window on bytes `[offset, offset + len)` of `pages` when they are
/// consecutive pages of one allocation (how titles usually hand them out);
/// `None` otherwise (pages of several allocations, out of order, or
/// unknown, or a host failure), which `write_physical` then handles or
/// reports.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    Some(PageWindow {
        view: View::map(slice, *n)?,
        within: offset % PAGE,
        len,
    })
}

/// The title addresses that show bytes `[offset, offset + len)` of `pages`
/// now, byte for byte, as runs (address, bytes) in address order: a write
/// to the pages through a view of our own ([`page_window`],
/// [`write_physical`]) lands there too, and nothing that watches those
/// addresses sees it (the GPU side's guards fault only on a write at the
/// guarded address itself). Empty if no view shows them. Byte for byte,
/// not by whole pages: the title packs several small textures into one
/// page and fills each with a request of its own.
pub(crate) fn shown_at(pages: &[usize], offset: usize, len: usize) -> Vec<(usize, usize)> {
    let Some(end) = offset.checked_add(len) else {
        return Vec::new();
    };
    let first = offset / PAGE;
    let Some(touched) = pages.get(first..end.div_ceil(PAGE)) else {
        return Vec::new();
    };
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    {
        let physical = physical();
        for (i, page) in touched.iter().enumerate() {
            let Some(addresses) = physical.pages.get(page).and_then(|slice| physical.shown.get(&slice.key(0))) else {
                continue;
            };
            // The bytes of this page that the write covers.
            let lo = if i == 0 { offset % PAGE } else { 0 };
            let hi = (end - (first + i) * PAGE).min(PAGE);
            pieces.extend(addresses.iter().map(|&at| (at + lo, hi - lo)));
        }
    }
    pieces.sort_unstable();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (at, bytes) in pieces {
        match runs.last_mut() {
            Some((start, n)) if *start + *n == at => *n += bytes,
            _ => runs.push((at, bytes)),
        }
    }
    runs
}

/// Bytes `[offset, offset + len)` of `pages` have been written through a
/// view of our own: the GPU side is told at every title address that
/// shows them (`victory_gpu::title_memory_written`; what it does with it
/// is `VICTORY_PAGE_WRITES`' to say, and it counts either way). Call when
/// the bytes are there, and without the page table's lock ([`shown_at`]
/// takes and leaves it).
pub(crate) fn pages_written(pages: &[usize], offset: usize, len: usize) {
    for (at, bytes) in shown_at(pages, offset, len) {
        victory_gpu::title_memory_written(at, bytes, true);
    }
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    let mut physical = physical();
    // Until the pages are mapped again: `unmap` takes whole views away and
    // maps what stays of them again, and a mapped view is a reservation
    // until it is committed. A read of title memory in place must not fall
    // into that gap (`victory_gpu::title_memory_remapping`).
    let _changing = victory_gpu::title_memory_remapping();
    unmap(&mut physical, base, n * PAGE);
    if pages.is_null() {
        return address;
    }
    // SAFETY: the title passes `n` page numbers.
    let pages = unsafe { std::slice::from_raw_parts(pages, n) };
    let mut i = 0;
    while i < n {
        let Some(first) = physical.pages.get(&pages[i]).cloned() else {
            victory_core::causes::note(victory_core::causes::Cause::MapPagesFailed);
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return std::ptr::null_mut();
        };
        // One view per run of consecutive slices of one section.
        let run = 1 + pages[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(page, k)| {
                physical.pages.get(page).is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k
                })
            })
            .count();
        if !map(&mut physical, base + i * PAGE, run * PAGE, first) {
            victory_core::causes::note(victory_core::causes::Cause::MapPagesFailed);
            return std::ptr::null_mut();
        }
        i += run;
    }
    address
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (the title passes 0 for addresses to clear).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    if addresses.is_null() {
        return 0;
    }
    for i in 0..n {
        // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
        let (address, page) = unsafe {
            (
                *addresses.add(i),
                if pages.is_null() || *pages.add(i) == 0 {
                    std::ptr::null()
                } else {
                    pages.add(i)
                },
            )
        };
        if XMemMapPhysicalPages(address, 1, page).is_null() {
            return 0;
        }
    }
    1
}

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`victory_gpu`, queue `CopyPageMappingsBatchX`):
/// the title maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Page by page; false if a source page is not mapped physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let (to, from, len) = (to as usize, from as usize, len as usize);
    let mut physical = physical();
    // As in `XMemMapPhysicalPages`: no read in place meets the gap.
    let _changing = victory_gpu::title_memory_remapping();
    unmap(&mut physical, to, len);
    for k in (0..len).step_by(PAGE) {
        if from == 0 {
            break;
        }
        let at = from + k;
        let Some(slice) = physical
            .views
            .range(..=at)
            .next_back()
            .filter(|(base, (size, _))| at < *base + size)
            .map(|(base, (_, s))| Slice {
                index: s.index + (at - base) / PAGE,
                section: s.section.clone(),
            })
        else {
            victory_core::causes::note(victory_core::causes::Cause::MapPagesFailed);
            return false;
        };
        if !map(&mut physical, to + k, PAGE, slice) {
            victory_core::causes::note(victory_core::causes::Cause::MapPagesFailed);
            return false;
        }
    }
    true
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one placeholder.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    // SAFETY: splits the placeholder around the range (failing harmlessly
    // when the range is the whole placeholder), then maps the section
    // there; both act only on placeholders.
    let view = unsafe {
        VirtualFree(
            base as *mut c_void,
            len,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        );
        MapViewOfFile3(
            slice.section.0,
            GetCurrentProcess(),
            base as *const c_void,
            (slice.index * PAGE) as u64,
            len,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value as usize != base {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            error = %std::io::Error::last_os_error(),
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    if !commit(base, len, slice.section.1) {
        // SAFETY: the view just mapped, back to a placeholder.
        unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
        return false;
    }
    // New contents, read-write: the GPU side's copies no longer guard it.
    victory_gpu::title_memory_remapped(base, len);
    // What the range shows now, for the GPU side's in-place census.
    victory_gpu::title_memory_view(base, len, slice.section.0 as usize, (slice.index * PAGE) as u64);
    for k in 0..len.div_ceil(PAGE) {
        physical.shown.entry(slice.key(k)).or_default().push(base + k * PAGE);
    }
    physical.views.insert(base, (len, slice));
    true
}

/// Commits the section pages a view shows (once per page, whichever view);
/// nothing to do for a section made committed.
fn commit(base: usize, len: usize, committed: bool) -> bool {
    if committed || on_wine() {
        return true; // committed whole when made
    }
    // SAFETY: `[base, base + len)` is a view of a reserved section.
    if !unsafe { VirtualAlloc(base as *const c_void, len, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
        return true;
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_COMMITMENT_LIMIT as i32) {
        let mut info = PERFORMANCE_INFORMATION {
            cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
            ..Default::default()
        };
        // SAFETY: the buffer and size describe a writable PERFORMANCE_INFORMATION.
        let queried = unsafe { K32GetPerformanceInfo(&mut info, info.cb) } != 0;
        victory_loader::log_crash(&commit_limit_message(len, &error, queried.then_some(&info)));
    } else {
        tracing::warn!(len, %error, "committing physical pages failed");
    }
    false
}

fn commit_limit_message(
    len: usize,
    error: &std::io::Error,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    let mut message = format!(
        "VIctory: Windows commit limit reached while committing {len} bytes of physical pages: {error}.\n"
    );
    if let Some(info) = info {
        use std::fmt::Write;

        let committed = info.CommitTotal * info.PageSize / MIB;
        let limit = info.CommitLimit * info.PageSize / MIB;
        let _ = writeln!(
            message,
            "System commit: {committed} MiB of {limit} MiB in use."
        );
    }
    message.push_str(
        "Close other applications or increase the Windows paging file size, then retry.\n",
    );
    message
}

/// Returns `[base, base + len)` to placeholders; views reaching past it
/// are mapped again outside it.
fn unmap(physical: &mut Physical, base: usize, len: usize) {
    // No bytes: no view to return. The search below would still find the
    // view holding `base` and cut it in two there, and its range of views
    // starting inside would run backwards (which `BTreeMap` panics on).
    if len == 0 {
        return;
    }
    let end = base + len;
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    for at in hit {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        for k in 0..size.div_ceil(PAGE) {
            let key = slice.key(k);
            if let Some(addresses) = physical.shown.get_mut(&key) {
                addresses.retain(|&a| a != at + k * PAGE);
                if addresses.is_empty() {
                    physical.shown.remove(&key);
                }
            }
        }
        victory_gpu::title_memory_remapped(at, size);
        // SAFETY: a view we mapped; it becomes a placeholder again.
        unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        };
        if at < base {
            map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map(physical, end, at + size - end, Slice { index, ..slice });
        }
    }
}

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget and our process's commit as what it uses. No tools
/// memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0); track
// allocations per class if a title budgets from that field.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, out: *mut WorkingSet) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
    let stats = WorkingSet {
        title_limit: gpu_optimal + standard - TRANSFERRED.load(Ordering::Relaxed),
        title_used: counters.PagefileUsage,
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // What the title is told, logged at the first asking and at every
    // 1,024th: "used" is all the process has committed, the layer's own
    // memory and the card's included (27 GB in run 60's gameplay against a
    // limit of 13.5 GB). A title that budgets its streaming from these two
    // figures believes it is far over its limit.
    // shortcut: the figure is not yet the title's own memory alone (its
    // heaps and the pages it mapped); count those if this line shows the
    // title asking.
    static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let asked = ASKED.fetch_add(1, Ordering::Relaxed);
    if asked.is_multiple_of(1024) {
        tracing::info!(
            asked = asked + 1,
            which = _which,
            limit_mb = stats.title_limit >> 20,
            used_mb = stats.title_used >> 20,
            "the title asks how much memory it uses"
        );
    }
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = victory_gpu::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

/// The title's own calls that free, decommit or protect its memory, and
/// those that commit at an address it names: on the way to the host they
/// first let go of what the GPU side has in place there and wait for the
/// heaps over it to go (`victory_gpu::title_memory_freeing`), since while
/// a heap opened on private memory stands, the host refuses such a call
/// (error 87). A change that still finds a heap there (a list or a slot
/// of ours still holds it) is held and made as soon as the heap is gone;
/// the title is told it was made, which for a free, a decommit or a
/// protection that takes access away is what a correct title can observe.
/// With in place off every call goes straight to the host.
pub(crate) mod title_calls {
    use std::ffi::c_void;
    use std::sync::{Mutex, MutexGuard};

    use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, GetLastError, HANDLE, SetLastError};
    use windows_sys::Win32::System::Memory as sys;
    use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetProcessId};
    use windows_sys::core::BOOL;

    use crate::{HleModule, hle_export};

    /// The host's page.
    const SMALL: usize = 4096;
    /// Protections that let code run: a change to one is never held (code
    /// would run in pages not yet executable), only waited for longer.
    const EXECUTABLE: u32 = 0xf0;

    /// A change the title asked for that waits for a heap.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Change {
        Release,
        Decommit,
        Protect(u32),
    }

    /// A held change: from `at`, `len` bytes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Held {
        pub(crate) at: usize,
        pub(crate) len: usize,
        pub(crate) change: Change,
    }

    impl Held {
        fn meets(&self, at: usize, len: usize) -> bool {
            self.at < at.saturating_add(len) && at < self.at.saturating_add(self.len)
        }
    }

    /// Held changes, in the order the title asked for them.
    static HELD: Mutex<Vec<Held>> = Mutex::new(Vec::new());

    fn held() -> MutexGuard<'static, Vec<Held>> {
        HELD.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What became of a title call.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Outcome {
        /// Made, with what the host answered.
        Made(BOOL),
        /// To be held.
        Held,
    }

    /// The order every call keeps: `freeing` first (it lets go of what is
    /// in place over `[at, at + len)` and waits for the heaps there); the
    /// call only when it says no heap stands; a call the host refuses with
    /// error 87 once more after another round, since a heap may have been
    /// opened over the range between the two; while a heap still stands, a
    /// change that `can_hold` is held, and any other is asked again (at
    /// most three rounds) and then made, failing as the host says.
    pub(crate) fn change(
        at: usize,
        len: usize,
        can_hold: bool,
        mut freeing: impl FnMut(usize, usize) -> bool,
        mut call: impl FnMut() -> (BOOL, u32),
    ) -> Outcome {
        for _ in 0..3 {
            if freeing(at, len) {
                if can_hold {
                    return Outcome::Held;
                }
                continue;
            }
            let (made, error) = call();
            if made != 0 || error != ERROR_INVALID_PARAMETER {
                return Outcome::Made(made);
            }
        }
        Outcome::Made(call().0)
    }

    /// Makes the held changes whose memory no heap covers any more, in
    /// order; a change that meets an earlier one still held waits behind
    /// it. `pinned` says whether a heap still stands over a range.
    pub(crate) fn settle(held: &mut Vec<Held>, mut pinned: impl FnMut(&Held) -> bool, mut make: impl FnMut(&Held)) {
        let mut kept: Vec<Held> = Vec::with_capacity(held.len());
        for h in held.drain(..) {
            if kept.iter().any(|k| k.meets(h.at, h.len)) || pinned(&h) {
                kept.push(h);
            } else {
                make(&h);
            }
        }
        *held = kept;
    }

    /// Takes `[at, at + len)` out of the held decommits (the title commits
    /// it again): the bytes taken out, which must read zero as freshly
    /// committed pages do.
    pub(crate) fn cut(held: &mut Vec<Held>, at: usize, len: usize) -> Vec<(usize, usize)> {
        let end = at.saturating_add(len);
        let mut out = Vec::new();
        let mut kept = Vec::with_capacity(held.len());
        for h in held.drain(..) {
            if h.change != Change::Decommit || !h.meets(at, len) {
                kept.push(h);
                continue;
            }
            let h_end = h.at + h.len;
            let (from, to) = (h.at.max(at), h_end.min(end));
            out.push((from, to - from));
            if h.at < from {
                kept.push(Held { len: from - h.at, ..h });
            }
            if to < h_end {
                kept.push(Held { at: to, len: h_end - to, ..h });
            }
        }
        *held = kept;
        out
    }

    fn query(at: usize) -> Option<sys::MEMORY_BASIC_INFORMATION> {
        // SAFETY: plain data filled in by the query.
        let mut info: sys::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        (unsafe { sys::VirtualQuery(at as *const c_void, &mut info, size_of_val(&info)) } != 0).then_some(info)
    }

    /// The host pages that hold `[at, at + size)`; with `size` 0, from
    /// `at` to the end of its allocation (what a release or a decommit of
    /// size 0 covers).
    fn extent(at: usize, size: usize) -> Option<(usize, usize)> {
        let from = at & !(SMALL - 1);
        if size != 0 {
            let end = at.checked_add(size)?.checked_next_multiple_of(SMALL)?;
            return Some((from, end - from));
        }
        let info = query(at)?;
        let base = info.AllocationBase as usize;
        let mut end = (info.BaseAddress as usize).checked_add(info.RegionSize)?;
        while let Some(next) = query(end).filter(|n| n.AllocationBase as usize == base && n.RegionSize > 0) {
            end = end.checked_add(next.RegionSize)?;
        }
        Some((from, end - from))
    }

    fn ours(process: HANDLE) -> bool {
        // SAFETY: plain queries; -1 is the current-process pseudo handle.
        process as isize == -1 || unsafe { GetProcessId(process) == GetCurrentProcessId() }
    }

    fn hold(h: Held) {
        let mut list = held();
        list.push(h);
        let n = list.len();
        drop(list);
        start_worker();
        victory_core::causes::note(victory_core::causes::Cause::InPlaceHeld);
        static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 16 {
            tracing::warn!(
                at = format_args!("{:#x}", h.at),
                len = h.len,
                change = ?h.change,
                held = n,
                "a title memory change is held until the heap in place over it goes"
            );
        }
    }

    /// Makes one held change on the host.
    fn make(h: &Held) {
        // SAFETY: the title's own change, made later as it asked for it.
        let made = unsafe {
            match h.change {
                Change::Release => sys::VirtualFree(h.at as *mut c_void, 0, sys::MEM_RELEASE),
                Change::Decommit => sys::VirtualFree(h.at as *mut c_void, h.len, sys::MEM_DECOMMIT),
                Change::Protect(protect) => {
                    let mut old = 0;
                    sys::VirtualProtect(h.at as *const c_void, h.len, protect, &mut old)
                }
            }
        };
        if made == 0 {
            tracing::warn!(at = format_args!("{:#x}", h.at), len = h.len, change = ?h.change, error = %std::io::Error::last_os_error(), "a held title memory change failed when made");
        } else {
            tracing::debug!(at = format_args!("{:#x}", h.at), len = h.len, change = ?h.change, "a held title memory change made");
        }
    }

    /// A thread of ours that makes held changes once their heaps are gone.
    fn start_worker() {
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            let started = std::thread::Builder::new().name("victory-held-frees".into()).spawn(|| {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    let mut list = held();
                    if !list.is_empty() {
                        settle(&mut list, |h| victory_gpu::title_memory_still_pinned(h.at, h.len), make);
                    }
                }
            });
            if let Err(error) = started {
                tracing::warn!(%error, "no thread for held title memory changes: they are made at the title's next call there");
            }
        });
    }

    /// Before a title call that commits or resets `[at, at + size)`: held
    /// changes there are made first if their heaps are gone; a held
    /// decommit the title commits again is taken back, its bytes zeroed;
    /// and what is in place there is let go.
    pub(crate) fn prepare(at: usize, size: usize, commit: bool) {
        if !super::in_place_requested() || size == 0 {
            return;
        }
        let Some((at, len)) = extent(at, size) else {
            return;
        };
        {
            let mut list = held();
            if list.iter().any(|h| h.meets(at, len)) {
                settle(&mut list, |h| !h.meets(at, len) || victory_gpu::title_memory_still_pinned(h.at, h.len), make);
                if commit {
                    for (from, bytes) in cut(&mut list, at, len) {
                        let writable = query(from).is_some_and(|i| i.State == sys::MEM_COMMIT && i.Protect == sys::PAGE_READWRITE);
                        if writable {
                            // SAFETY: committed read-write pages the title
                            // gave up and now commits again.
                            unsafe { std::ptr::write_bytes(from as *mut u8, 0, bytes) };
                        }
                    }
                }
            }
        }
        let _ = victory_gpu::title_memory_freeing(at, len);
    }

    /// A free of the title's: see the module.
    pub(crate) fn free(address: *mut c_void, size: usize, kind: u32) -> BOOL {
        // SAFETY: the title's own arguments, as the host takes them.
        let host = || unsafe { (sys::VirtualFree(address, size, kind), GetLastError()) };
        // Placeholder splits and joins (`MEM_COALESCE_PLACEHOLDERS` is 0x1).
        let placeholders = sys::MEM_PRESERVE_PLACEHOLDER | 0x1;
        let change = match kind {
            _ if kind & placeholders != 0 => None,
            _ if kind & sys::MEM_RELEASE != 0 => Some(Change::Release),
            _ if kind & sys::MEM_DECOMMIT != 0 => Some(Change::Decommit),
            _ => None,
        };
        let (Some(change), true, false) = (change, super::in_place_requested(), address.is_null()) else {
            return host().0;
        };
        let Some((at, len)) = extent(address as usize, size) else {
            return host().0;
        };
        // Behind a held change over the same memory, so that the order the
        // title asked for stays.
        if held().iter().any(|h| h.meets(at, len)) {
            hold(Held { at, len, change });
            // SAFETY: plain call.
            unsafe { SetLastError(0) };
            return 1;
        }
        match change_now(at, len, true, host) {
            Outcome::Made(made) => made,
            Outcome::Held => {
                hold(Held { at, len, change });
                // SAFETY: plain call.
                unsafe { SetLastError(0) };
                1
            }
        }
    }

    fn change_now(at: usize, len: usize, can_hold: bool, call: impl FnMut() -> (BOOL, u32)) -> Outcome {
        change(at, len, can_hold, victory_gpu::title_memory_freeing, call)
    }

    /// A protection change of the title's: see the module.
    pub(crate) fn protect(address: *const c_void, size: usize, protect: u32, old: *mut u32) -> BOOL {
        // SAFETY: the title's own arguments, as the host takes them.
        let host = || unsafe { (sys::VirtualProtect(address, size, protect, old), GetLastError()) };
        if !super::in_place_requested() || address.is_null() || size == 0 || old.is_null() {
            return host().0;
        }
        let Some((at, len)) = extent(address as usize, size) else {
            return host().0;
        };
        // What the title is told was the protection: one, of the first page.
        let before = query(at).map(|i| i.Protect);
        let can_hold = protect & EXECUTABLE == 0 && before.is_some() && !held().iter().any(|h| h.meets(at, len));
        match change_now(at, len, can_hold, host) {
            Outcome::Made(made) => made,
            Outcome::Held => {
                hold(Held { at, len, change: Change::Protect(protect) });
                // SAFETY: the title passes room for the old protection.
                unsafe {
                    old.write(before.unwrap_or(sys::PAGE_READWRITE));
                    SetLastError(0);
                }
                1
            }
        }
    }

    pub extern "system" fn VirtualFree(address: *mut c_void, size: usize, kind: u32) -> BOOL {
        free(address, size, kind)
    }

    pub extern "system" fn VirtualFreeEx(process: HANDLE, address: *mut c_void, size: usize, kind: u32) -> BOOL {
        if ours(process) {
            return free(address, size, kind);
        }
        // SAFETY: the title's own arguments for another process.
        unsafe { sys::VirtualFreeEx(process, address, size, kind) }
    }

    pub extern "system" fn VirtualProtect(address: *const c_void, size: usize, new: u32, old: *mut u32) -> BOOL {
        protect(address, size, new, old)
    }

    pub extern "system" fn VirtualProtectEx(process: HANDLE, address: *const c_void, size: usize, new: u32, old: *mut u32) -> BOOL {
        if ours(process) {
            return protect(address, size, new, old);
        }
        // SAFETY: the title's own arguments for another process.
        unsafe { sys::VirtualProtectEx(process, address, size, new, old) }
    }

    pub extern "system" fn VirtualAlloc(address: *const c_void, size: usize, kind: u32, protect: u32) -> *mut c_void {
        if !address.is_null() {
            prepare(address as usize, size, kind & sys::MEM_COMMIT != 0);
        }
        // SAFETY: the title's own arguments.
        unsafe { sys::VirtualAlloc(address, size, kind, protect) }
    }

    pub extern "system" fn VirtualAllocEx(process: HANDLE, address: *const c_void, size: usize, kind: u32, protect: u32) -> *mut c_void {
        if !address.is_null() && ours(process) {
            prepare(address as usize, size, kind & sys::MEM_COMMIT != 0);
        }
        // SAFETY: the title's own arguments.
        unsafe { sys::VirtualAllocEx(process, address, size, kind, protect) }
    }

    pub(crate) fn module() -> HleModule {
        HleModule {
            name: "title memory calls",
            dlls: &["kernel32.dll", "kernelbase.dll", "api-ms-win-core-memory-*"],
            exports: vec![
                hle_export!(VirtualFree),
                hle_export!(VirtualFreeEx),
                hle_export!(VirtualProtect),
                hle_export!(VirtualProtectEx),
                hle_export!(VirtualAlloc),
                hle_export!(VirtualAllocEx),
            ],
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The title's call is made only after the GPU side said no heap
        /// stands over its memory; while one does, a change that can wait
        /// is held and the host is not called.
        #[test]
        fn a_title_call_goes_to_the_host_only_after_the_heaps_went() {
            let steps = std::cell::RefCell::new(Vec::new());
            let outcome = change(
                0x1000,
                0x1000,
                true,
                |_, _| {
                    steps.borrow_mut().push("freeing");
                    false
                },
                || {
                    steps.borrow_mut().push("call");
                    (1, 0)
                },
            );
            assert_eq!(outcome, Outcome::Made(1));
            assert_eq!(*steps.borrow(), ["freeing", "call"]);

            let mut called = false;
            let outcome = change(0x1000, 0x1000, true, |_, _| true, || {
                called = true;
                (1, 0)
            });
            assert_eq!(outcome, Outcome::Held);
            assert!(!called, "never made while a heap stands");

            // A heap opened between the two: refused once, made after the
            // next round.
            let mut answers = vec![(1, 0), (0, ERROR_INVALID_PARAMETER)];
            let mut rounds = 0;
            let outcome = change(0, 1, true, |_, _| {
                rounds += 1;
                false
            }, || answers.pop().unwrap());
            assert_eq!((outcome, rounds), (Outcome::Made(1), 2));

            // An executable protection is never held: made after the
            // rounds, failing as the host says.
            let mut calls = 0;
            let outcome = change(0, 1, false, |_, _| true, || {
                calls += 1;
                (0, ERROR_INVALID_PARAMETER)
            });
            assert_eq!((outcome, calls), (Outcome::Made(0), 1));
        }

        /// Held changes are made in order, each once its heap is gone, and
        /// never ahead of an earlier one over the same memory.
        #[test]
        fn held_changes_keep_their_order() {
            let a = Held { at: 0x10_000, len: 0x10_000, change: Change::Decommit };
            let b = Held { at: 0x10_000, len: 0x40_000, change: Change::Release };
            let c = Held { at: 0x90_000, len: 0x1000, change: Change::Protect(sys::PAGE_READONLY) };
            let mut list = vec![a, b, c];
            let mut made = Vec::new();
            // `a`'s heap still stands: `b` meets it and waits; `c` is free.
            settle(&mut list, |h| *h == a, |h| made.push(*h));
            assert_eq!(made, [c]);
            assert_eq!(list, [a, b]);
            settle(&mut list, |_| false, |h| made.push(*h));
            assert_eq!(made, [c, a, b]);
            assert!(list.is_empty());
        }

        /// A held decommit the title commits again is taken back where it
        /// commits, and kept around it.
        #[test]
        fn a_commit_takes_back_a_held_decommit() {
            let d = Held { at: 0x10_000, len: 0x30_000, change: Change::Decommit };
            let p = Held { at: 0x20_000, len: 0x1000, change: Change::Protect(sys::PAGE_NOACCESS) };
            let mut list = vec![d, p];
            let zeroed = cut(&mut list, 0x20_000, 0x8000);
            assert_eq!(zeroed, [(0x20_000, 0x8000)]);
            // What is left of the decommit keeps its place ahead of the
            // later protection change: the title's order.
            assert_eq!(
                list,
                [
                    Held { at: 0x10_000, len: 0x10_000, change: Change::Decommit },
                    Held { at: 0x28_000, len: 0x18_000, change: Change::Decommit },
                    p,
                ]
            );
            assert!(cut(&mut list, 0x80_000, 0x1000).is_empty(), "nothing held there");
        }

        /// The extent of a size-0 release is its whole allocation, over
        /// regions of different states.
        #[test]
        fn a_release_covers_its_whole_allocation() {
            // SAFETY: an allocation of the test's own.
            unsafe {
                let base = sys::VirtualAlloc(std::ptr::null(), 0x40_000, sys::MEM_RESERVE, sys::PAGE_NOACCESS) as usize;
                assert_ne!(base, 0);
                sys::VirtualAlloc((base + 0x10_000) as *const c_void, 0x10_000, sys::MEM_COMMIT, sys::PAGE_READWRITE);
                assert_eq!(extent(base, 0), Some((base, 0x40_000)));
                assert_eq!(extent(base + 0x10_123, 0x10), Some((base + 0x10_000, 0x1000)));
                assert_eq!(extent(base + 0xfff, 2), Some((base, 0x2000)));
                sys::VirtualFree(base as *mut c_void, 0, sys::MEM_RELEASE);
            }
        }
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll"],
        exports: vec![
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page pool's sections with in place on: whole sections of the
    /// most pages, the rest in the last; the title's 6.1 GiB in 25.
    #[test]
    fn the_pool_is_cut_into_sections_small_enough_to_commit() {
        assert_eq!(pool_sections(1, 4096), [1]);
        assert_eq!(pool_sections(4096, 4096), [4096]);
        assert_eq!(pool_sections(4097, 4096), [4096, 1]);
        let title = (6246 << 20) / PAGE;
        let cut = pool_sections(title, POOL_SECTION_PAGES);
        assert_eq!(cut.len(), 25);
        assert_eq!(cut.iter().sum::<usize>(), title);
        assert!(cut.iter().all(|&n| n > 0 && n <= POOL_SECTION_PAGES));
        assert!(pool_sections(0, 4096).is_empty());
    }

    /// Pages handed out over two sections: numbers go on as one
    /// allocation, mappings split where the sections do, and every page
    /// shows its own bytes.
    #[test]
    fn pages_over_two_sections_map_as_one_allocation() {
        if !in_place_requested() {
            return;
        }
        let n = POOL_SECTION_PAGES + 2;
        let mut count = n;
        let mut pages = vec![0usize; n];
        assert_eq!(XMemAllocatePhysicalPages(0, &mut count, pages.as_mut_ptr()), 1);
        assert!(pages.windows(2).all(|w| w[1] == w[0] + FRAMES));
        let tail = &pages[POOL_SECTION_PAGES - 1..];
        let runs = runs(tail, 0, 3 * PAGE).unwrap();
        assert_eq!(runs.len(), 2, "one run per section");
        assert!(runs.iter().all(|(slice, _)| slice.section.1), "committed sections");
        assert!(write_physical(tail, PAGE - 4, &[7u8; 8]));
        let reservation = XMemVirtualAlloc(std::ptr::null_mut(), 3 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!reservation.is_null());
        assert_eq!(XMemMapPhysicalPages(reservation, 3, tail.as_ptr()), reservation);
        // SAFETY: the three pages just mapped.
        let shown = unsafe { std::slice::from_raw_parts((reservation as *const u8).add(PAGE - 4), 8) };
        assert_eq!(shown, [7u8; 8]);
        assert_eq!(XMemMapPhysicalPages(reservation, 3, std::ptr::null()), reservation);
        assert_eq!(XMemFreePhysicalPages(n, pages.as_ptr()), 1);
    }

    #[test]
    fn commit_notifications_cover_both_blocks_of_an_unaligned_request() {
        let reservation = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE,
            0,
            PAGE_NOACCESS,
        );
        assert!(!reservation.is_null());
        let first = reservation as usize;
        let boundary = first + PAGE;
        let mut notified = None;
        let at = virtual_alloc(
            (boundary - 1) as _,
            2,
            MEM_COMMIT,
            0,
            PAGE_READWRITE,
            |start, size| {
                notified = Some(start..start + size);
            },
        );
        assert!(!at.is_null());
        assert!((at as usize) < boundary - 1);
        assert_eq!(query((boundary - 1) as _).host.State, MEM_COMMIT);
        assert_eq!(query(boundary as _).host.State, MEM_COMMIT);
        // SAFETY: this test owns the entire reservation.
        assert_ne!(unsafe { VirtualFree(reservation, 0, MEM_RELEASE) }, 0);
        let range = notified.expect("a successful commit notifies the GPU watch");
        assert_eq!(
            range.start / PAGE..range.end.div_ceil(PAGE),
            first / PAGE..first / PAGE + 2
        );
    }

    #[test]
    fn commit_notification_uses_the_chosen_address_for_a_null_request() {
        let mut notified = None;
        let at = virtual_alloc(
            std::ptr::null_mut(),
            PAGE,
            MEM_COMMIT,
            0,
            PAGE_READWRITE,
            |start, size| {
                notified = Some((start, size));
            },
        );
        assert!(!at.is_null());
        // SAFETY: this test owns the entire reservation.
        assert_ne!(unsafe { VirtualFree(at, 0, MEM_RELEASE) }, 0);
        assert_eq!(notified, Some((at as usize, PAGE)));
    }

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        let p = XMemAlloc(4096, ALLOC_32BIT);
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        XMemFree(p, ALLOC_32BIT);
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        // Write-watched, for the GPU side's copies.
        let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
        // SAFETY: room for `count` addresses.
        let watched = unsafe {
            windows_sys::Win32::System::Memory::GetWriteWatch(
                0,
                p,
                4 * MIB,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        assert_eq!(watched, 0);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        assert_eq!(query(p).flags, 0);
        // No address field: above the 32-bit range and below 2^40, where
        // the console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn transfers_shrink_the_title_budget() {
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_used > 0);
        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = victory_gpu::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn commit_limit_report_includes_request_and_system_commit() {
        let error = std::io::Error::from_raw_os_error(ERROR_COMMITMENT_LIMIT as i32);
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = commit_limit_message(4 * MIB, &error, Some(&info));
        assert!(report.contains("4194304 bytes"));
        assert!(report.contains("2 MiB of 3 MiB in use"));
        let fallback = commit_limit_message(4 * MIB, &error, None);
        for report in [&report, &fallback] {
            assert!(report.contains("os error 1455"));
            assert!(report.contains("Windows paging file size"));
        }
        assert!(!fallback.contains("System commit:"));
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        // Reservations of 64 KiB pages, as titles make them for mapping.
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(query(b).host.State, MEM_RESERVE);
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    /// A thread that reads a mapped page in place, as the GPU side reads a
    /// texture, never meets the moment in which the runtime has taken the
    /// page's view away to map it again: mapping the same pages at the same
    /// address unmaps the view and maps a new one, and without the lock the
    /// reader faults on a page "reserved, never committed" (the crashes of
    /// runs 71 and 74).
    #[test]
    fn a_read_in_place_never_meets_the_gap_of_a_remapping() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        // SAFETY: mapped read-write.
        unsafe { a.cast::<u8>().write(1) };
        let first = a as usize;
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let _reading = victory_gpu::title_memory_reading();
                    // SAFETY: the page is mapped whenever no change of
                    // mapping is under way, and none is while `_reading`
                    // is held.
                    assert_eq!(unsafe { (first as *const u8).read_volatile() }, 1);
                    reads += 1;
                }
                reads
            })
        };
        for _ in 0..2000 {
            // The same pages at the same address: the view goes and comes
            // back, with the same bytes.
            assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        }
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().expect("the reader never faulted") > 0);
        assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_no_pages_changes_nothing() {
        let pages = allocate(2);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(5) };
        let second = a.wrapping_byte_add(PAGE);

        // A count of 0 at the start of a view, inside it and past it: with
        // pages, without (an unmap), and as the GPU's page-mapping copy.
        for at in [a, second, a.wrapping_byte_add(2 * PAGE)] {
            assert_eq!(XMemMapPhysicalPages(at, 0, pages.as_ptr()), at);
            assert_eq!(XMemMapPhysicalPages(at, 0, std::ptr::null()), at);
            assert!(map_alias(at as u64, a as u64, 0));
            assert!(map_alias(at as u64, 0, 0));
        }

        // The view is still the one view it was.
        let view = physical().views.get(&(a as usize)).map(|v| v.0);
        assert_eq!(view, Some(2 * PAGE));
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(second).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

        // A count of 1 still unmaps that page alone.
        assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        assert_eq!(query(second).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// `n` pages of one new allocation.
    fn allocate(n: usize) -> Vec<usize> {
        let (mut pages, mut given) = (vec![0; n], n);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        assert_eq!(given, n);
        pages
    }

    /// The bytes of one page, through a view of our own.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = physical().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    #[test]
    fn runs_group_consecutive_pages_of_one_allocation() {
        let (a, b) = (allocate(3), allocate(1));
        let lengths = |pages: &[usize], offset, len| {
            runs(pages, offset, len).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
        // Only the pages the bytes reach.
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        // Out of order, or across allocations: one run each.
        assert_eq!(
            lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
            Some(vec![1, 1, 1])
        );
        assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
        // A slice that follows on in another section starts a run.
        assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
        // Too few pages for the bytes, or an unknown one.
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn writes_cross_runs_and_allocations() {
        let (a, b) = (allocate(2), allocate(1));
        let pages = [a[0], a[1], b[0]];
        let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
        let offset = PAGE - 32;
        // Past the third page: the second run is too short.
        assert!(!write_physical(&pages, offset, &data));
        assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
        assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
        assert!(
            !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
            "unknown page"
        );
        assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    /// Pages say where the title has them mapped, byte for byte (after
    /// fork PR #571's test; its runs are whole pages).
    #[test]
    fn pages_say_where_they_are_mapped() {
        let pages = allocate(3);
        let reserve = |n: usize| {
            let at = XMemVirtualAlloc(std::ptr::null_mut(), n * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
            assert!(!at.is_null());
            at
        };
        // `b` keeps a page unmapped at either end, so that no run at `b`
        // ever touches one at `a`, wherever the host puts the two.
        let (a, b) = (reserve(3), reserve(4));
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty(), "mapped nowhere");
        // All three at `a`; the last two at `b`, from its second page on.
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        let second = b.wrapping_byte_add(PAGE);
        assert_eq!(XMemMapPhysicalPages(second, 2, pages[1..].as_ptr()), second);
        let (a, second) = (a as usize, second as usize);
        let sorted = |mut runs: Vec<(usize, usize)>| {
            runs.sort_unstable();
            runs
        };
        assert_eq!(shown_at(&pages, 0, PAGE), vec![(a, PAGE)]);
        // A request for part of a page names those bytes alone.
        assert_eq!(shown_at(&pages, 0x4000, 0x3000), vec![(a + 0x4000, 0x3000)]);
        // Across two pages, shown at two addresses each: one run at each.
        assert_eq!(
            shown_at(&pages, PAGE + 1, PAGE),
            sorted(vec![(a + PAGE + 1, PAGE), (second + 1, PAGE)])
        );
        // A page unmapped in the middle of a view: the rest still shows.
        let middle = (a + PAGE) as *mut c_void;
        assert_eq!(XMemMapPhysicalPages(middle, 1, std::ptr::null()), middle);
        assert_eq!(
            shown_at(&pages, 0, 3 * PAGE),
            sorted(vec![(a, PAGE), (a + 2 * PAGE, PAGE), (second, 2 * PAGE)])
        );
        // Too few pages for the bytes: nothing (the write fails too).
        assert!(shown_at(&pages, 0, 3 * PAGE + 1).is_empty());
        for (at, n) in [(a, 3), (second, 2)] {
            let at = at as *mut c_void;
            assert_eq!(XMemMapPhysicalPages(at, n, std::ptr::null()), at);
        }
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty());
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn page_windows_cover_one_run_in_place() {
        let (a, b) = (allocate(3), allocate(1));
        let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
        assert_eq!(window.bytes().len(), PAGE);
        window.bytes().fill(0x5a);
        drop(window);
        let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
        assert!(second[..3].iter().all(|&b| b == 0));
        assert!(second[3..].iter().all(|&b| b == 0x5a));
        assert!(third[..3].iter().all(|&b| b == 0x5a));
        assert!(third[3..].iter().all(|&b| b == 0));
        // Several runs, or an unknown page: no window.
        assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[0x10], 0, 1).is_none());
        // One page of several is still one run.
        assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }
}
