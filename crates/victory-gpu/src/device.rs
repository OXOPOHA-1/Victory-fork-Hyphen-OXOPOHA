//! The device titles get from device creation, laid out as their inlined
//! code expects: the object header, then device fields. Its table starts
//! like the PC device interface (IUnknown, the object methods, then the
//! device methods in PC order); console additions follow. Methods we have
//! not written log their table offset and fail, so a run shows what to
//! write next.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;

use crate::object::{
    self, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, HRESULT, Iid, Object, S_FALSE, S_OK, Table, This,
    guid, guid_text,
};
use crate::{
    descriptor, dxgi, fence, frame, list, pipeline, query, queue, resource, root, signature,
};

/// What D3D returns when none of the requested feature levels is supported.
const UNSUPPORTED: HRESULT = 0x887A_0004_u32 as i32;

/// The device interface id (the same as on PC). Titles also import it as
/// data.
pub static IID: Iid = guid(
    0x189819f1,
    0x1db6,
    0x4b57,
    [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
);

/// The levels the device answers to, as on PC: `ID3D12Device1` to
/// `ID3D12Device8`, and `ID3D12Device11`, which the title creates
/// its device as. Each level's table is the one before plus its methods.
const LEVEL_IIDS: [Iid; 9] = [
    guid(
        0x77acce80,
        0x638e,
        0x4e65,
        [0x88, 0x95, 0xc1, 0xf2, 0x33, 0x86, 0x86, 0x3e],
    ),
    guid(
        0x30baa41e,
        0xb15b,
        0x475c,
        [0xa0, 0xbb, 0x1a, 0xf5, 0xc5, 0xb6, 0x43, 0x28],
    ),
    guid(
        0x81dadc15,
        0x2bad,
        0x4392,
        [0x93, 0xc5, 0x10, 0x13, 0x45, 0xc4, 0xaa, 0x98],
    ),
    guid(
        0xe865df17,
        0xa9ee,
        0x46f9,
        [0xa4, 0x63, 0x30, 0x98, 0x31, 0x5a, 0xa2, 0xe5],
    ),
    guid(
        0x8b4f173b,
        0x2fea,
        0x4b80,
        [0x8f, 0x58, 0x43, 0x07, 0x19, 0x1a, 0xb9, 0x5d],
    ),
    guid(
        0xc70b221b,
        0x40e4,
        0x4a17,
        [0x89, 0xaf, 0x02, 0x5a, 0x07, 0x27, 0xa6, 0xdc],
    ),
    guid(
        0x5c014b53,
        0x68a1,
        0x4b9b,
        [0x8b, 0xd1, 0xdd, 0x60, 0x46, 0xb9, 0x35, 0x8b],
    ),
    guid(
        0x728f1fa2,
        0xea11,
        0x4a9f,
        [0x9b, 0x52, 0x9b, 0x42, 0xa4, 0x84, 0xd5, 0xc9],
    ),
    guid(
        0x5405c344,
        0xd457,
        0x444e,
        [0xb4, 0xdd, 0x23, 0x66, 0xe4, 0x5a, 0xee, 0x39],
    ),
];

/// Whether `iid` names the device: its first interface or a newer level.
pub(crate) fn is_device(iid: *const Iid) -> bool {
    object::is(iid, &[IID]) || object::is(iid, &LEVEL_IIDS)
}

/// The interface version these layouts were checked against (2.4). Both
/// Minecraft (March 2022) and the title (April 2026) pass it.
pub const CHECKED_VERSION: u32 = 0x0002_0004;

/// Feature level 12_2, what the console hardware supports.
const HIGHEST_FEATURE_LEVEL: u32 = 0xc200;

#[repr(C)]
pub struct Device {
    object: Object,
    /// GPU address of a null constant buffer; none yet.
    null_constant_buffer: u64,
    /// Byte 0 non-zero: calls the title would otherwise inline come to the
    /// table. The rest is reserved.
    out_of_line: [u32; 16],
}
const _: () = assert!(size_of::<Device>() == 0x58);

/// One device per process, as on the console.
pub(crate) static DEVICE: Device = Device {
    object: Object::new(&FUNCTIONS),
    null_constant_buffer: 0,
    out_of_line: [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
};

pub(crate) fn device() -> This {
    (&raw const DEVICE).cast()
}

/// Device creation: `(adapter, parameters, iid, out)`. The parameters start
/// with the interface version; the rest (ring and scratch sizes, features
/// to leave out) does not matter to us yet.
pub extern "system" fn create(
    _adapter: *const c_void,
    parameters: *const u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if parameters.is_null() || iid.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes its parameters and an id.
    let (version, iid) = unsafe { (parameters.read(), &*iid) };
    tracing::info!(version = format_args!("{version:#x}"), "D3D12.X device");
    if version != CHECKED_VERSION {
        tracing::warn!(
            version = format_args!("{version:#x}"),
            "device layouts were checked against {CHECKED_VERSION:#x} only"
        );
    }
    if !is_device(iid) {
        tracing::warn!(iid = %guid_text(iid), "device created as an unknown interface");
        return E_NOINTERFACE;
    }
    if out.is_null() {
        return S_FALSE;
    }
    if *iid == LEVEL_IIDS[8] {
        NEWER.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    object::answer(object::DEVICE, iid, Some(device()), out)
}

/// Set when the title creates the device as `ID3D12Device11`. Its lists and
/// queues then have the newer layouts (two more methods each; see
/// `list` and `queue`), although the interface version stays 2.4.
/// shortcut: the creation interface is the only sign of the newer layouts
/// we have (the title, April 2026, versus Minecraft, March 2022);
/// look for a better one if a title breaks the rule.
static NEWER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn newer() -> bool {
    NEWER.load(std::sync::atomic::Ordering::Relaxed)
}

/// The device, or its DXGI side.
extern "system" fn query_interface(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = if is_device(iid) {
        Some(this)
    } else if object::is(iid, &dxgi::DEVICE_IIDS) {
        Some(dxgi::device())
    } else {
        None
    };
    object::answer(object::DEVICE, iid, found, out)
}

/// What `D3D12ConfigureVirtualMemoryX` takes and fills in.
#[repr(C)]
pub struct VirtualMemory {
    flags: u32,
    /// GPU page table size in 4 MiB pages.
    page_table_pages: u32,
    /// Memory kept across title processes for the frame buffer.
    preserved_bytes: u32,
    /// Out: the range graphics addresses fall in.
    lowest: u64,
    highest: u64,
}
const _: () = assert!(size_of::<VirtualMemory>() == 0x20);

/// `d3d12_xs!D3D12ConfigureVirtualMemoryX`, called before any graphics
/// allocation. The inputs size console structures we do not have; the
/// range out is all of user space, since GPU addresses are CPU addresses.
pub extern "system" fn configure_virtual_memory(config: *mut VirtualMemory) -> HRESULT {
    // SAFETY: the title passes its configuration (null is tolerated).
    let Some(config) = (unsafe { config.as_mut() }) else {
        return E_INVALIDARG;
    };
    tracing::info!(
        flags = config.flags,
        page_table_pages = config.page_table_pages,
        preserved_bytes = config.preserved_bytes,
        "graphics virtual memory"
    );
    (config.lowest, config.highest) = user_space();
    S_OK
}

fn user_space() -> (u64, u64) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        // SAFETY: plain data, filled in by the call.
        let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer.
        unsafe { GetSystemInfo(&mut info) };
        (
            info.lpMinimumApplicationAddress as u64,
            info.lpMaximumApplicationAddress as u64,
        )
    }
    #[cfg(not(windows))]
    (0x1_0000, 0x7fff_fffe_ffff)
}

extern "system" fn node_count(_: This) -> u32 {
    1
}

/// Feature query 2: the highest of the requested feature levels we support.
#[repr(C)]
struct FeatureLevels {
    count: u32,
    requested: *const u32,
    highest: u32,
}

/// Feature query 0, what an RDNA 2 GPU reports. The console's layout has
/// one field more than PC's (index 11).
const OPTIONS: [u32; 16] = [
    1,  // double-precision shader operations
    1,  // output-merger logic operations
    2,  // minimum precision: 16-bit
    3,  // tiled resources tier
    3,  // resource binding tier
    1,  // stencil reference from pixel shaders
    1,  // typed UAV loads of more formats
    1,  // rasterizer-ordered views
    3,  // conservative rasterization tier
    40, // GPU address bits per resource
    0,  // 64 KiB standard swizzle
    0,  // console-only field
    0,  // cross-node sharing tier
    0,  // cross-adapter row-major textures
    1,  // viewport and render target index from any stage
    2,  // resource heap tier
];

extern "system" fn check_feature_support(
    _: This,
    feature: u32,
    data: *mut c_void,
    size: u32,
) -> HRESULT {
    const OPTIONS_QUERY: u32 = 0;
    const ARCHITECTURE: u32 = 1;
    const FEATURE_LEVELS: u32 = 2;
    const FORMAT_SUPPORT: u32 = 3;
    const MULTISAMPLE: u32 = 4;
    let expected = match feature {
        OPTIONS_QUERY => size_of_val(&OPTIONS),
        ARCHITECTURE => 16,
        FEATURE_LEVELS => size_of::<FeatureLevels>(),
        FORMAT_SUPPORT => 12,
        MULTISAMPLE => 16,
        _ => {
            victory_core::causes::note(victory_core::causes::Cause::CallFeatureQuery);
            tracing::warn!(feature, size, "device feature query not implemented");
            return E_NOTIMPL;
        }
    };
    if data.is_null() || size as usize != expected {
        return E_INVALIDARG;
    }
    match feature {
        OPTIONS_QUERY => {
            // SAFETY: checked size and non-null.
            unsafe { data.cast::<[u32; 16]>().write_unaligned(OPTIONS) };
            S_OK
        }
        ARCHITECTURE => {
            // SAFETY: checked size and non-null.
            let a = unsafe { &mut *data.cast::<[u32; 4]>() };
            if a[0] != 0 {
                return E_INVALIDARG; // one node only
            }
            // Not tile-based; unified, cache-coherent memory.
            a[1..].copy_from_slice(&[0, 1, 1]);
            S_OK
        }
        FORMAT_SUPPORT => {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<[u32; 3]>() };
            // shortcut: every use of every format; answer from the host
            // device once there is one.
            q[1] = 0x7fff_ffff;
            q[2] = 0xffff;
            S_OK
        }
        MULTISAMPLE => {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<[u32; 4]>() };
            // Format, sample count, flags in; one quality level out for the
            // counts the hardware has.
            q[3] = u32::from(matches!(q[1], 1 | 2 | 4 | 8));
            S_OK
        }
        _ => feature_levels(data),
    }
}

fn feature_levels(data: *mut c_void) -> HRESULT {
    // SAFETY: the caller checked size and non-null.
    let query = unsafe { &mut *data.cast::<FeatureLevels>() };
    if query.requested.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes `count` levels.
    let requested = unsafe { std::slice::from_raw_parts(query.requested, query.count as usize) };
    match requested
        .iter()
        .copied()
        .filter(|&l| l <= HIGHEST_FEATURE_LEVEL)
        .max()
    {
        Some(level) => {
            query.highest = level;
            S_OK
        }
        None => UNSUPPORTED,
    }
}

/// The directory `VICTORY_RAW_SAVE` names, if it may be used.
///
/// The files are the title's own data (docs/asset-policy.md), so a
/// directory inside a VIctory source tree is refused (the same check as the
/// caches); the refusal is logged once and nothing is saved.
#[cfg(windows)]
fn raw_save_dir() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("VICTORY_RAW_SAVE")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())?;
    match victory_core::ensure_outside_repo(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::error!(error = %e, "VICTORY_RAW_SAVE refused: nothing is saved");
            None
        }
    }
}

/// For the pipeline code: each shader container the title passes through
/// the normal creation methods, once per distinct container, so that the
/// capture can tie raw code from 0x1c8/0x218 to programs whose records are
/// understood. Does nothing unless `VICTORY_RAW_SAVE` is set.
#[cfg(windows)]
pub(crate) fn raw_save_container(stage: &'static str, bytes: &[u8]) {
    if let Some(capture) = capture::get() {
        capture.container(stage, bytes);
    }
}

/// Waits (at most a few seconds) until the capture's files are written and
/// logs its final counts. For the end of a run; does nothing unless
/// `VICTORY_RAW_SAVE` is set.
pub fn raw_save_finish() {
    #[cfg(windows)]
    if let Some(capture) = capture::get() {
        capture.finish();
    }
}

/// Device methods 0x1c8 and 0x218 (`SLOT`). The title calls both
/// (title+0x2477578 and title+0x24800d5) as `(this, description, interface
/// id, out)`, asks for `ID3D12PipelineState`, and on success keeps the
/// object `out` names and draws with it; on failure it goes without that
/// pipeline, silently. What the descriptions hold is not known: the calls
/// fail as they always did, with `out` null. The first calls of each
/// method, then every power of two, log the description's words, each
/// word's class by content ([`capture::classify`]) and what the existing
/// readers see in each address-and-length blob. With `VICTORY_RAW_SAVE`
/// every call is saved ([`capture`]).
/// shortcut: not implemented. Answering `S_OK` here without making the
/// pipeline would hand the title whatever its stack held as an object.
extern "system" fn unknown_pipeline<const SLOT: usize>(
    _: This,
    desc: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    use std::sync::atomic::{AtomicU32, Ordering};
    static CALLS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];
    victory_core::causes::note(victory_core::causes::Cause::CallPipelineMethod);
    let calls = CALLS[usize::from(SLOT == 0x218)].fetch_add(1, Ordering::Relaxed) + 1;
    if !out.is_null() {
        // SAFETY: the title's out pointer (a local of its own).
        unsafe { *out = std::ptr::null() };
    }
    let logged = calls <= 4 || calls.is_power_of_two();
    #[cfg(windows)]
    {
        let capture = capture::get();
        if logged || capture.is_some() {
            // The readers meet arbitrary title memory here, inside a
            // callback from the title: a panic must not unwind into it
            // (that aborts the game).
            let looked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                capture::look(SLOT, calls, desc, iid, out, logged, capture)
            }));
            if looked.is_err() {
                capture::note_panic();
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (desc, iid, logged);
    E_NOTIMPL
}

/// `VICTORY_RAW_SAVE=<dir>`: what the title passes to the two pipeline
/// methods we cannot implement yet, saved whole, so that a reader for them
/// can be written from evidence (`victory raw <dir>` reads it).
///
/// Nothing here assumes a layout. Every call saves the first [`WINDOW`]
/// bytes at the description and one line in `calls.tsv` that says, for
/// each 64-bit word, what it is by content: zero, a small integer, one of
/// our objects (a root signature named by its `RTS0` hash), a readable
/// address followed by a plausible length (those bytes are saved), a bare
/// readable address (a bounded window there is saved) or another value.
/// Every root signature the title creates is saved (`roots.tsv`), and so is
/// every shader container passed through the normal methods
/// (`containers.tsv`, fed by [`raw_save_container`]). Files are named by
/// content hash, so each distinct content is written once. The format is
/// described in the header of each table.
///
/// Reading only, through the guarded copy (`host::read_into`): an
/// unmapped page fails the read instead of faulting. Writing happens on a
/// thread of its own behind a bounded queue; what does not fit, or comes
/// after the size cap (`VICTORY_RAW_SAVE_MB`, default 2048), is counted and
/// not saved. No pipeline is made from any of it and nothing is handed to
/// the title.
///
/// [`WINDOW`]: capture::WINDOW
pub(crate) mod capture {
    #[cfg(windows)]
    use crate::object::{Iid, This};

    /// Description bytes read and classified per call. One 0x218
    /// description is 0x60 bytes and they lie in an array; 0x1c8 ones run
    /// past 0x80 bytes (logged words 13-15 of later calls are not zero), so
    /// 0x200 covers each with room to spare. A read that meets an unmapped
    /// page stops there.
    pub(crate) const WINDOW: usize = 0x200;
    /// Bytes saved at a bare address (one whose next word is no length).
    pub(crate) const BARE: usize = 0x200;
    /// The largest word taken as the length of the address before it.
    pub(crate) const MAX_LEN: u64 = 16 << 20;
    /// The most bytes saved for one address and length (the largest logged
    /// length is 0x1f9c0).
    pub(crate) const BLOB_CAP: usize = 1 << 20;
    /// Values tried as addresses: user space without its first 64 KiB.
    pub(crate) const USER: std::ops::Range<u64> = 0x1_0000..0x7fff_ffff_0000;
    const PAGE: u64 = 0x1000;

    /// What an address is, if it is one of our objects.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Own {
        /// A root signature, with the hash it is saved under (`RTS0` part,
        /// else the whole blob), if it is registered.
        Root(Option<[u8; 32]>),
        Device,
        /// Its first word points into our own image: most likely another of
        /// our objects, of a kind not told apart here.
        Ours,
    }

    /// Memory as the classifier sees it.
    pub(crate) trait Memory {
        /// Fills `out` from `at`, or returns false if any of it is not
        /// readable.
        fn read(&self, at: u64, out: &mut [u8]) -> bool;
        fn object(&self, at: u64) -> Option<Own>;
    }

    /// Up to `len` bytes from `at`, stopping at the first page that is not
    /// readable; empty for a value outside [`USER`].
    pub(crate) fn read_prefix(mem: &impl Memory, at: u64, len: usize) -> Vec<u8> {
        if !USER.contains(&at) || len == 0 {
            return Vec::new();
        }
        let len = len.min((USER.end - at) as usize);
        let mut out = vec![0u8; len];
        if mem.read(at, &mut out) {
            return out;
        }
        let mut done = 0;
        while done < len {
            let here = at + done as u64;
            let step = ((PAGE - here % PAGE) as usize).min(len - done);
            if !mem.read(here, &mut out[done..done + step]) {
                break;
            }
            done += step;
        }
        out.truncate(done);
        out
    }

    /// The first 16 hex digits of a BLAKE3, as file names and tokens use.
    pub(crate) fn short(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex()[..16].to_owned()
    }

    pub(crate) fn short_of(hash: &[u8; 32]) -> String {
        blake3::Hash::from_bytes(*hash).to_hex()[..16].to_owned()
    }

    /// What one word of a description is, by content.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Word {
        Null,
        /// Below 2^32 and not a readable address.
        Small,
        /// The length of the [`Word::Pair`] before it.
        Len,
        /// Anything else: above 2^32 and not readable.
        Other,
        Root(Option<String>),
        /// One of our other objects; for [`Own::Ours`] the window saved.
        Object(&'static str, Option<(String, usize)>),
        /// A readable address followed by a length.
        Pair { hash: String, len: u64, saved: usize },
        /// A readable address with no length after it.
        Bare { hash: String, saved: usize },
    }

    impl Word {
        /// The word's token in `calls.tsv` (see [`CALLS_HEADER`]).
        pub(crate) fn token(&self) -> String {
            match self {
                Word::Null => "0".into(),
                Word::Small => "i".into(),
                Word::Len => "L".into(),
                Word::Other => "x".into(),
                Word::Root(Some(h)) => format!("R:{h}"),
                Word::Root(None) => "R:?".into(),
                Word::Object(kind, None) => format!("O:{kind}"),
                Word::Object(kind, Some((h, n))) => format!("O:{kind}:{h}:{n:x}"),
                Word::Pair { hash, len, saved } if *saved as u64 == *len => {
                    format!("P:{hash}:{len:x}")
                }
                Word::Pair { hash, len, saved } => format!("P:{hash}:{len:x}:{saved:x}"),
                Word::Bare { hash, saved } => format!("B:{hash}:{saved:x}"),
            }
        }
    }

    /// The classes of a description's words, and the bytes read for them by
    /// file name (`blob_<hash>.bin`, `win_<hash>.bin`).
    #[derive(Debug, Default)]
    pub(crate) struct Classified {
        pub(crate) words: Vec<Word>,
        pub(crate) files: Vec<(String, Vec<u8>)>,
    }

    /// Classifies each word of a description by what it holds or points at,
    /// never by its position. A readable address followed by a word in
    /// `1..=MAX_LEN` is taken as an address and a length; the bytes saved
    /// tell later whether that was right.
    pub(crate) fn classify(words: &[u64], mem: &impl Memory) -> Classified {
        let mut out = Classified {
            words: Vec::with_capacity(words.len()),
            files: Vec::new(),
        };
        let small_or_other = |v: u64| if v < 1 << 32 { Word::Small } else { Word::Other };
        let mut i = 0;
        while i < words.len() {
            let v = words[i];
            i += 1;
            if v == 0 {
                out.words.push(Word::Null);
                continue;
            }
            if let Some(own) = mem.object(v) {
                let word = match own {
                    Own::Root(hash) => Word::Root(hash.as_ref().map(short_of)),
                    Own::Device => Word::Object("device", None),
                    Own::Ours => {
                        let window = read_prefix(mem, v, BARE);
                        let hash = short(&window);
                        let n = window.len();
                        out.files.push((format!("win_{hash}.bin"), window));
                        Word::Object("ours", Some((hash, n)))
                    }
                };
                out.words.push(word);
                continue;
            }
            if !USER.contains(&v) {
                out.words.push(small_or_other(v));
                continue;
            }
            if let Some(&len) = words.get(i)
                && (1..=MAX_LEN).contains(&len)
            {
                let bytes = read_prefix(mem, v, (len as usize).min(BLOB_CAP));
                if !bytes.is_empty() {
                    let hash = short(&bytes);
                    out.words.push(Word::Pair {
                        hash: hash.clone(),
                        len,
                        saved: bytes.len(),
                    });
                    out.words.push(Word::Len);
                    out.files.push((format!("blob_{hash}.bin"), bytes));
                    i += 1;
                    continue;
                }
            }
            let window = read_prefix(mem, v, BARE);
            if window.is_empty() {
                out.words.push(small_or_other(v));
                continue;
            }
            let hash = short(&window);
            out.words.push(Word::Bare {
                hash: hash.clone(),
                saved: window.len(),
            });
            out.files.push((format!("win_{hash}.bin"), window));
        }
        out
    }

    /// Header of `calls.tsv`; the last line names the columns.
    pub(crate) const CALLS_HEADER: &str = "\
# VIctory raw capture, format 2 (VICTORY_RAW_SAVE): one line per call of device method 0x1c8 or 0x218.
# Both methods failed every call (E_NOTIMPL, out set to null); nothing was made for the title.
# Columns, tab-separated, in this order:
#   seq            order among all captured calls of this process, from 0
#   method         device method table offset, 0x1c8 or 0x218
#   call           the call's number for its method, from 1
#   frame          frames begun when it was called
#   thread         the calling thread's id
#   run            the process id
#   desc           the description's address
#   desc_hash      the bytes read at desc are desc_<desc_hash>.bin
#   desc_len       bytes read at desc (hex): 0x200, fewer if a later page was not readable
#   new            1 if these description bytes were first seen in this call
#   iid            the interface id asked for, or - if it could not be read
#   out            the address the title passed for the object
#   caller         return address in the calling code (hex), or -
#   caller_module  <module base>+<offset> of the caller (hex), or -
#   words          one token per 64-bit word of the bytes read, in order, separated by spaces:
#     0                      zero
#     i                      below 2^32 and not a readable address
#     x                      above 2^32 and not a readable address (a hash, flags, an unmapped address)
#     L                      the length of the P token just before
#     R:<h>                  one of our root signature objects; <h> is its hash in roots.tsv; R:? not registered
#     O:device               our device object
#     O:ours:<h>:<n>         its first word points into our own image (likely another object of ours); n bytes in win_<h>.bin
#     P:<h>:<len>[:<saved>]  a readable address followed by a word in 1..=0x1000000: the bytes there, saved as blob_<h>.bin
#                            (at most 0x100000; <saved> is given when fewer than <len> were saved)
#     B:<h>:<n>              a readable address not followed by a length: n bytes from it (at most 0x200) in win_<h>.bin
# Hashes are the first 16 hex digits of BLAKE3 of the saved bytes; sizes are hex. A named file can be
# missing when it was dropped (queue full or size cap): summary.txt counts those.
seq\tmethod\tcall\tframe\tthread\trun\tdesc\tdesc_hash\tdesc_len\tnew\tiid\tout\tcaller\tcaller_module\twords";

    /// Header of `roots.tsv`.
    pub(crate) const ROOTS_HEADER: &str = "\
# VIctory raw capture, format 2: one line per distinct root signature the title created (CreateRootSignature).
# Columns, tab-separated, in this order:
#   hash   what R:<h> tokens in calls.tsv name: the BLAKE3 of its RTS0 part (16 hex digits), else of the whole blob
#   rts0   the BLAKE3 of its RTS0 part (16 hex digits), or - if the blob has none
#   blob   the BLAKE3 of the whole blob (16 hex digits); the blob is root_<hash>.bin
#   size   blob bytes (hex)
#   frame  frames begun when it was first created
hash\trts0\tblob\tsize\tframe";

    /// Header of `containers.tsv`.
    pub(crate) const CONTAINERS_HEADER: &str = "\
# VIctory raw capture, format 2: one line per distinct shader container passed through the normal pipeline methods.
# Columns, tab-separated, in this order:
#   hash      the first 16 hex digits of BLAKE3 of the container, as shader logs print it; the bytes are cont_<hash>.bin
#   stage     the stage the pipeline description gave it
#   size      container bytes (hex)
#   xdat_hash the bytes of its XDAT kind 0x10 entry (the shader hash), hex in memory order, or -
#   xdat      each XDAT entry as kind:offset:size (hex), comma-separated, or - if the DXIL part has no XDAT
#   programs  each native program as stage:wave:vgprs:code_hash:code_size (code_hash as above, of its code bytes),
#             comma-separated; - if none; error:<text> if it could not be read
hash\tstage\tsize\txdat_hash\txdat\tprograms";

    /// The XDAT entries of a `DXIL` part's data: `(kind, offset, size)`.
    pub(crate) fn xdat_entries(part: &[u8]) -> Vec<(u32, u32, u32)> {
        let word = |at: usize| {
            part.get(at..at + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        if part.get(0x18..0x1c) != Some(b"XDAT") {
            return Vec::new();
        }
        let count = word(0x1c).unwrap_or(0).min(64) as usize;
        (0..count)
            .map_while(|i| {
                let at = 0x24 + 12 * i;
                Some((word(at)?, word(at + 4)?, word(at + 8)?))
            })
            .collect()
    }

    /// The `containers.tsv` line of a container.
    pub(crate) fn container_line(stage: &str, bytes: &[u8]) -> String {
        use std::fmt::Write;
        use victory_shader::{Container, FourCC};
        let hash = short(bytes);
        let (mut xdat_hash, mut xdat, mut programs) =
            ("-".to_owned(), "-".to_owned(), "-".to_owned());
        match Container::parse(bytes) {
            Err(e) => programs = format!("error:{e}"),
            Ok(c) => {
                if let Some(part) = c.part(FourCC::DXIL) {
                    let entries = xdat_entries(part.data);
                    if !entries.is_empty() {
                        xdat = entries
                            .iter()
                            .map(|(k, o, s)| format!("{k:x}:{o:x}:{s:x}"))
                            .collect::<Vec<_>>()
                            .join(",");
                    }
                    if let Some(&(_, offset, size)) = entries.iter().find(|e| e.0 == 0x10)
                        && let Some(h) = part
                            .data
                            .get(offset as usize..(offset as usize).saturating_add(size as usize))
                    {
                        xdat_hash = h.iter().fold(String::new(), |mut s, b| {
                            let _ = write!(s, "{b:02x}");
                            s
                        });
                    }
                    match victory_shader::native_programs(part.data) {
                        Ok(native) if !native.is_empty() => {
                            programs = native
                                .iter()
                                .map(|p| {
                                    format!(
                                        "{:?}:{}:{}:{}:{:x}",
                                        p.stage,
                                        if p.wave64 { 64 } else { 32 },
                                        p.vgprs,
                                        short(&p.code),
                                        p.code.len()
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(",");
                        }
                        Ok(_) => {}
                        Err(e) => programs = format!("error:{e}"),
                    }
                }
            }
        }
        format!(
            "{hash}\t{stage}\t{:x}\t{xdat_hash}\t{xdat}\t{programs}",
            bytes.len()
        )
    }

    #[cfg(windows)]
    pub(crate) use live::{get, look, note_panic};

    #[cfg(windows)]
    mod live {
        use std::collections::{HashMap, HashSet};
        use std::io::Write;
        use std::path::Path;
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
        use std::sync::{Mutex, OnceLock, mpsc};
        use std::time::{Duration, Instant};

        use super::*;
        use crate::object::guid_text;

        const QUEUE_ITEMS: usize = 8192;
        /// File bytes waiting for the writer at most; more is dropped.
        const QUEUE_BYTES: usize = 64 << 20;
        const DEFAULT_CAP_MIB: u64 = 2048;
        /// After the cap, lines still go out up to this much more.
        const LINE_RESERVE: u64 = 256 << 20;

        /// Counters, by the indices below.
        static COUNT: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];
        const CALLS: usize = 0; // and 1
        const DESCS: usize = 2; // and 3
        const BLOBS: usize = 4;
        const WINDOWS: usize = 5;
        const ROOTS: usize = 6;
        const CONTAINERS: usize = 7;
        const WRITTEN_FILES: usize = 8;
        const WRITTEN_BYTES: usize = 9;
        const DROPPED_QUEUE: usize = 10;
        const DROPPED_CAP: usize = 11;
        const DROPPED_BYTES: usize = 12;
        const LINES_DROPPED: usize = 13;
        const WRITE_ERRORS: usize = 14;
        const PANICS: usize = 15;
        /// File bytes in the queue.
        static QUEUED: AtomicUsize = AtomicUsize::new(0);

        fn count(at: usize) {
            COUNT[at].fetch_add(1, Relaxed);
        }

        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        enum Table {
            Calls,
            Roots,
            Containers,
        }

        impl Table {
            fn file(self) -> (&'static str, &'static str) {
                match self {
                    Table::Calls => ("calls.tsv", CALLS_HEADER),
                    Table::Roots => ("roots.tsv", ROOTS_HEADER),
                    Table::Containers => ("containers.tsv", CONTAINERS_HEADER),
                }
            }
        }

        enum Item {
            File(String, Vec<u8>),
            Line(Table, String),
            Container(&'static str, Vec<u8>),
            Flush(mpsc::Sender<()>),
        }

        #[derive(PartialEq, Eq)]
        enum Saved {
            New,
            Seen,
            Dropped,
        }

        pub(crate) struct Capture {
            tx: mpsc::SyncSender<Item>,
            cap: u64,
            /// Bytes handed to the writer (files and lines).
            accepted: AtomicU64,
            /// Names of the files handed to the writer.
            seen: Mutex<HashSet<String>>,
            seq: AtomicU64,
            run: u32,
            cap_said: std::sync::Once,
            lines_said: std::sync::Once,
            queue_said: std::sync::Once,
        }

        /// The capture, if `VICTORY_RAW_SAVE` is set (and usable). Unset,
        /// this is one load and a branch.
        pub(crate) fn get() -> Option<&'static Capture> {
            static CAPTURE: OnceLock<Option<Capture>> = OnceLock::new();
            CAPTURE.get_or_init(start).as_ref()
        }

        fn start() -> Option<Capture> {
            let dir = super::super::raw_save_dir()?;
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::error!(dir = %dir.display(), error = %e, "VICTORY_RAW_SAVE: the directory cannot be made; nothing is saved");
                return None;
            }
            let cap_mib = std::env::var("VICTORY_RAW_SAVE_MB")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|&v| v > 0)
                .unwrap_or(DEFAULT_CAP_MIB);
            let (tx, rx) = mpsc::sync_channel(QUEUE_ITEMS);
            let to = dir.clone();
            let cap = cap_mib << 20;
            if let Err(e) = std::thread::Builder::new()
                .name("raw capture".into())
                .spawn(move || write(&to, &rx, cap))
            {
                tracing::error!(error = %e, "VICTORY_RAW_SAVE: no writer thread; nothing is saved");
                return None;
            }
            tracing::info!(
                dir = %dir.display(),
                cap_mib,
                "VICTORY_RAW_SAVE: every call of device methods 0x1c8 and 0x218 is saved: {WINDOW:#x} description bytes \
                 (one file per distinct content) and a calls.tsv line classifying each word by content; the bytes each \
                 address-and-length names (up to {BLOB_CAP:#x}) and {BARE:#x} bytes at each bare address. Also every root \
                 signature created (roots.tsv) and every shader container of the normal methods (containers.tsv). Both \
                 methods still fail; nothing is made for the title. Size cap {cap_mib} MiB (VICTORY_RAW_SAVE_MB). Read it \
                 with `victory raw <dir>`."
            );
            Some(Capture {
                tx,
                cap,
                accepted: AtomicU64::new(0),
                seen: Mutex::new(HashSet::new()),
                seq: AtomicU64::new(0),
                run: std::process::id(),
                cap_said: std::sync::Once::new(),
                lines_said: std::sync::Once::new(),
                queue_said: std::sync::Once::new(),
            })
        }

        impl Capture {
            fn seen(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
                self.seen.lock().unwrap_or_else(|e| e.into_inner())
            }

            fn behind(&self) {
                self.queue_said.call_once(|| {
                    tracing::warn!(
                        "VICTORY_RAW_SAVE: the writer fell behind; what does not fit its queue is counted, not saved (said once)"
                    );
                });
            }

            fn dropped(&self, at: usize, bytes: usize) {
                count(at);
                COUNT[DROPPED_BYTES].fetch_add(bytes as u64, Relaxed);
                if at == DROPPED_CAP {
                    self.cap_said.call_once(|| {
                        tracing::warn!(
                            cap_mib = self.cap >> 20,
                            "VICTORY_RAW_SAVE size cap reached: later files are counted, not saved; lines go on for 256 MiB more (VICTORY_RAW_SAVE_MB raises the cap)"
                        );
                    });
                } else {
                    self.behind();
                }
            }

            /// Hands bytes to the writer unless a file of that name was:
            /// `make` turns them into the queue item.
            fn queue(&self, name: String, bytes: Vec<u8>, make: impl FnOnce(String, Vec<u8>) -> Item) -> Saved {
                if !self.seen().insert(name.clone()) {
                    return Saved::Seen;
                }
                let len = bytes.len();
                let why = if self.accepted.load(Relaxed) + len as u64 > self.cap {
                    Some(DROPPED_CAP)
                } else if QUEUED.fetch_add(len, Relaxed) + len > QUEUE_BYTES
                    || self.tx.try_send(make(name.clone(), bytes)).is_err()
                {
                    QUEUED.fetch_sub(len, Relaxed);
                    Some(DROPPED_QUEUE)
                } else {
                    None
                };
                match why {
                    None => {
                        self.accepted.fetch_add(len as u64, Relaxed);
                        Saved::New
                    }
                    Some(why) => {
                        // Not saved: a later call may still save it.
                        self.seen().remove(&name);
                        self.dropped(why, len);
                        Saved::Dropped
                    }
                }
            }

            fn file(&self, name: String, bytes: Vec<u8>) -> Saved {
                self.queue(name, bytes, Item::File)
            }

            fn line(&self, table: Table, text: String) {
                if self.accepted.load(Relaxed) > self.cap + LINE_RESERVE {
                    count(LINES_DROPPED);
                    self.lines_said.call_once(|| {
                        tracing::warn!(
                            "VICTORY_RAW_SAVE: the size cap and its reserve for lines are used up; later lines are counted, not saved"
                        );
                    });
                    return;
                }
                let len = text.len() as u64 + 1;
                if self.tx.try_send(Item::Line(table, text)).is_ok() {
                    self.accepted.fetch_add(len, Relaxed);
                } else {
                    count(LINES_DROPPED);
                    self.behind();
                }
            }

            /// A root signature the title created: saved once per distinct
            /// blob. Returns the BLAKE3 of its `RTS0` part, if it has one,
            /// and of the whole blob.
            pub(crate) fn root(&self, blob: &[u8]) -> (Option<[u8; 32]>, [u8; 32]) {
                let rts0 = crate::root::rts0_hash(blob);
                let whole = *blake3::hash(blob).as_bytes();
                let named = rts0.unwrap_or(whole);
                let hash = short_of(&named);
                if self.file(format!("root_{hash}.bin"), blob.to_vec()) == Saved::New {
                    count(ROOTS);
                    let rts0_text = rts0.as_ref().map_or("-".to_owned(), short_of);
                    self.line(
                        Table::Roots,
                        format!(
                            "{hash}\t{rts0_text}\t{}\t{:x}\t{}",
                            short_of(&whole),
                            blob.len(),
                            crate::frame::current()
                        ),
                    );
                }
                (rts0, whole)
            }

            /// A shader container of the normal methods; the writer reads
            /// it and writes its `containers.tsv` line.
            pub(crate) fn container(&self, stage: &'static str, bytes: &[u8]) {
                let name = format!("cont_{}.bin", short(bytes));
                self.queue(name, bytes.to_vec(), |_, bytes| Item::Container(stage, bytes));
            }

            #[allow(clippy::too_many_arguments)]
            fn call(
                &self,
                slot: usize,
                call: u32,
                desc: u64,
                window: Vec<u8>,
                classified: Classified,
                iid: *const Iid,
                out: *mut This,
            ) {
                let method = usize::from(slot == 0x218);
                count(CALLS + method);
                let seq = self.seq.fetch_add(1, Relaxed);
                let desc_hash = short(&window);
                let desc_len = window.len();
                let new = self.file(format!("desc_{desc_hash}.bin"), window) == Saved::New;
                if new {
                    count(DESCS + method);
                }
                let words = classified
                    .words
                    .iter()
                    .map(Word::token)
                    .collect::<Vec<_>>()
                    .join(" ");
                for (name, bytes) in classified.files {
                    let blob = name.starts_with("blob_");
                    if self.file(name, bytes) == Saved::New {
                        count(if blob { BLOBS } else { WINDOWS });
                    }
                }
                let iid = iid_text(iid);
                let (caller, module) = caller();
                // SAFETY: no arguments; the calling thread's id.
                let thread = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
                self.line(
                    Table::Calls,
                    format!(
                        "{seq}\t{slot:#x}\t{call}\t{}\t{thread}\t{}\t{desc:#x}\t{desc_hash}\t{desc_len:#x}\t{}\t{iid}\t{:#x}\t{caller}\t{module}\t{words}",
                        crate::frame::current(),
                        self.run,
                        u8::from(new),
                        out as usize,
                    ),
                );
            }

            /// Waits until the writer has written what it was handed, at
            /// most a few seconds, and logs the counts.
            pub(crate) fn finish(&self) {
                let (tx, rx) = mpsc::channel();
                let mut item = Item::Flush(tx);
                let until = Instant::now() + Duration::from_secs(3);
                loop {
                    match self.tx.try_send(item) {
                        Ok(()) => {
                            let _ = rx.recv_timeout(Duration::from_secs(5));
                            break;
                        }
                        Err(mpsc::TrySendError::Full(back)) if Instant::now() < until => {
                            item = back;
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                tracing::info!("VICTORY_RAW_SAVE at the end: {}", summary(self.cap));
            }
        }

        fn iid_text(iid: *const Iid) -> String {
            <[u8; 16]>::try_from(read_prefix(&Title, iid as u64, 16).as_slice())
                .map_or("-".to_owned(), |g| guid_text(&g))
        }

        /// One line of counts.
        fn summary(cap: u64) -> String {
            let c = |at: usize| COUNT[at].load(Relaxed);
            format!(
                "calls_1c8={} calls_218={} descriptions_1c8={} descriptions_218={} blobs={} windows={} roots={} \
                 containers={} files_written={} bytes_written={} dropped_queue_full={} dropped_size_cap={} \
                 dropped_bytes={} lines_dropped={} write_errors={} probe_panics={} cap_mib={}",
                c(CALLS),
                c(CALLS + 1),
                c(DESCS),
                c(DESCS + 1),
                c(BLOBS),
                c(WINDOWS),
                c(ROOTS),
                c(CONTAINERS),
                c(WRITTEN_FILES),
                c(WRITTEN_BYTES),
                c(DROPPED_QUEUE),
                c(DROPPED_CAP),
                c(DROPPED_BYTES),
                c(LINES_DROPPED),
                c(WRITE_ERRORS),
                c(PANICS),
                cap >> 20,
            )
        }

        /// The writer thread: files, table lines, and `summary.txt` at most
        /// every ten seconds while counts change; a log line at most every
        /// minute.
        fn write(dir: &Path, rx: &mpsc::Receiver<Item>, cap: u64) {
            let mut tables: HashMap<Table, std::io::BufWriter<std::fs::File>> = HashMap::new();
            let total = || COUNT.iter().map(|c| c.load(Relaxed)).sum::<u64>();
            let mut summarized = (Instant::now(), u64::MAX);
            let mut logged = (Instant::now(), 0u64);
            loop {
                let first = match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(item) => Some(item),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                };
                let mut flushes = Vec::new();
                let items = first
                    .into_iter()
                    .chain(std::iter::from_fn(|| rx.try_recv().ok()).take(4096));
                for item in items {
                    match item {
                        Item::File(name, bytes) => {
                            QUEUED.fetch_sub(bytes.len(), Relaxed);
                            save(&dir.join(name), &bytes);
                        }
                        Item::Line(table, text) => line(dir, &mut tables, table, &text),
                        Item::Container(stage, bytes) => {
                            QUEUED.fetch_sub(bytes.len(), Relaxed);
                            let text = std::panic::catch_unwind(|| container_line(stage, &bytes))
                                .unwrap_or_else(|_| {
                                    format!(
                                        "{}\t{stage}\t{:x}\t-\t-\terror:the reader panicked",
                                        short(&bytes),
                                        bytes.len()
                                    )
                                });
                            save(&dir.join(format!("cont_{}.bin", short(&bytes))), &bytes);
                            count(CONTAINERS);
                            line(dir, &mut tables, Table::Containers, &text);
                        }
                        Item::Flush(done) => flushes.push(done),
                    }
                }
                for table in tables.values_mut() {
                    if table.flush().is_err() {
                        count(WRITE_ERRORS);
                    }
                }
                let now = total();
                if !flushes.is_empty()
                    || (now != summarized.1 && summarized.0.elapsed() >= Duration::from_secs(10))
                {
                    let text = summary(cap).replace(' ', "\n");
                    let _ = std::fs::write(dir.join("summary.txt"), format!("{text}\n"));
                    summarized = (Instant::now(), now);
                }
                if now != logged.1 && logged.0.elapsed() >= Duration::from_secs(60) {
                    tracing::info!("VICTORY_RAW_SAVE so far: {}", summary(cap));
                    logged = (Instant::now(), now);
                }
                for done in flushes {
                    let _ = done.send(());
                }
            }
        }

        fn save(path: &Path, bytes: &[u8]) {
            if path.exists() {
                return; // named by content: the same bytes
            }
            match std::fs::write(path, bytes) {
                Ok(()) => {
                    count(WRITTEN_FILES);
                    COUNT[WRITTEN_BYTES].fetch_add(bytes.len() as u64, Relaxed);
                }
                Err(_) => count(WRITE_ERRORS),
            }
        }

        fn line(
            dir: &Path,
            tables: &mut HashMap<Table, std::io::BufWriter<std::fs::File>>,
            table: Table,
            text: &str,
        ) {
            if let std::collections::hash_map::Entry::Vacant(entry) = tables.entry(table) {
                let (name, header) = table.file();
                let path = dir.join(name);
                let fresh = std::fs::metadata(&path).map_or(true, |m| m.len() == 0);
                let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
                    count(WRITE_ERRORS);
                    return;
                };
                let mut file = std::io::BufWriter::new(file);
                if fresh && writeln!(file, "{header}").is_err() {
                    count(WRITE_ERRORS);
                }
                entry.insert(file);
            }
            if let Some(file) = tables.get_mut(&table) {
                match writeln!(file, "{text}") {
                    Ok(()) => {
                        COUNT[WRITTEN_BYTES].fetch_add(text.len() as u64 + 1, Relaxed);
                    }
                    Err(_) => count(WRITE_ERRORS),
                }
            }
        }

        /// Title memory, read through the guarded copy.
        struct Title;

        impl Memory for Title {
            fn read(&self, at: u64, out: &mut [u8]) -> bool {
                crate::host::read_into(at as usize as *const u8, out)
            }

            fn object(&self, at: u64) -> Option<Own> {
                if at == super::super::device() as u64 {
                    return Some(Own::Device);
                }
                if let Some(facts) = crate::root::registered(at as usize) {
                    return Some(Own::Root(Some(facts.rts0.unwrap_or(facts.blob))));
                }
                if !USER.contains(&at) {
                    return None;
                }
                let mut first = [0u8; 8];
                if !self.read(at, &mut first) {
                    return None;
                }
                let table = u64::from_le_bytes(first);
                if crate::root::is_table(table) {
                    return Some(Own::Root(None));
                }
                own_image().contains(&table).then_some(Own::Ours)
            }
        }

        /// The address range of the module holding `at`, from its PE
        /// header.
        fn image_of(at: u64) -> Option<std::ops::Range<u64>> {
            use windows_sys::Win32::System::LibraryLoader::{
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                GetModuleHandleExW,
            };
            let mut module = std::ptr::null_mut();
            // SAFETY: looks up the module holding the address, no reference.
            let found = unsafe {
                GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                        | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                    at as usize as *const u16,
                    &mut module,
                )
            };
            if found == 0 {
                return None;
            }
            let base = module as u64;
            let u32_at = |at: u64| {
                let mut b = [0u8; 4];
                crate::host::read_into(at as usize as *const u8, &mut b)
                    .then(|| u32::from_le_bytes(b))
            };
            // e_lfanew, then SizeOfImage in the optional header.
            let nt = u32_at(base + 0x3c)?;
            let size = u32_at(base + u64::from(nt) + 0x50)?;
            Some(base..base + u64::from(size))
        }

        /// Our own image (the module this code is in).
        fn own_image() -> std::ops::Range<u64> {
            static RANGE: OnceLock<std::ops::Range<u64>> = OnceLock::new();
            RANGE
                .get_or_init(|| image_of(own_image as fn() -> std::ops::Range<u64> as usize as u64).unwrap_or(0..0))
                .clone()
        }

        /// The first return address on the stack outside our own image, and
        /// its module and offset.
        #[inline(never)]
        fn caller() -> (String, String) {
            use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
            static MODULES: Mutex<Option<HashMap<u64, String>>> = Mutex::new(None);
            let own = own_image();
            if own.is_empty() {
                return ("-".into(), "-".into());
            }
            let mut frames = [std::ptr::null_mut(); 8];
            // SAFETY: room for that many frames.
            let n = unsafe {
                RtlCaptureStackBackTrace(
                    0,
                    frames.len() as u32,
                    frames.as_mut_ptr(),
                    std::ptr::null_mut(),
                )
            } as usize;
            let Some(at) = frames[..n.min(frames.len())]
                .iter()
                .map(|&f| f as u64)
                .find(|f| !own.contains(f))
            else {
                return ("-".into(), "-".into());
            };
            // By return address, so that a caller outside any module is not
            // looked up again; a run has a few callers.
            let mut modules = MODULES.lock().unwrap_or_else(|e| e.into_inner());
            let modules = modules.get_or_insert_with(HashMap::new);
            let module = match modules.get(&at) {
                Some(m) => m.clone(),
                None => {
                    let m = image_of(at).map_or("-".to_owned(), |m| {
                        format!("{:#x}+{:#x}", m.start, at - m.start)
                    });
                    if modules.len() < 256 {
                        modules.insert(at, m.clone());
                    }
                    m
                }
            };
            (format!("{at:#x}"), module)
        }

        pub(crate) fn note_panic() {
            count(PANICS);
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| {
                tracing::warn!(
                    "reading a missing pipeline's description panicked; that call is not captured (said once)"
                );
            });
        }

        /// What the existing readers see in a blob: magic, native programs
        /// in its `DXIL` part, how many of those translate (and why not),
        /// and whether it is a root signature (`root::rts0_in`).
        type Probe = (String, usize, usize, Vec<String>, Option<(usize, usize, bool)>);

        fn probe(bytes: &[u8]) -> Probe {
            use victory_shader::{Container, FourCC, HwStage};
            let magic = String::from_utf8_lossy(&bytes[..bytes.len().min(4)]).into_owned();
            let native = Container::parse(bytes)
                .ok()
                .and_then(|c| {
                    c.part(FourCC::DXIL)
                        .map(|p| victory_shader::native_programs(p.data).unwrap_or_default())
                })
                .unwrap_or_default();
            let count = native.len();
            let mut translated = 0;
            let mut reasons = Vec::new();
            for p in native {
                if !matches!(p.stage, HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms) {
                    continue;
                }
                match victory_shader::emit::translate_native_program(p.clone()) {
                    Ok(_) => translated += 1,
                    Err(e) => {
                        let text = format!("{:?}: {e}", p.stage);
                        if !reasons.contains(&text) {
                            reasons.push(text);
                        }
                    }
                }
            }
            (magic, count, translated, reasons, crate::root::rts0_in(bytes))
        }

        /// One call: read and classify, log it if `logged`, save it if
        /// capturing.
        pub(crate) fn look(
            slot: usize,
            call: u32,
            desc: *const u8,
            iid: *const Iid,
            out: *mut This,
            logged: bool,
            capture: Option<&Capture>,
        ) {
            let window = read_prefix(&Title, desc as u64, WINDOW);
            let words: Vec<u64> = window
                .as_chunks::<8>()
                .0
                .iter()
                .map(|w| u64::from_le_bytes(*w))
                .collect();
            let classified = classify(&words, &Title);
            if logged {
                let first = words.first().map(|&w| {
                    read_prefix(&Title, w, 128)
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|w| u64::from_le_bytes(*w))
                        .collect::<Vec<_>>()
                });
                // What the existing readers see in each address-and-length
                // blob, by word index: a format we must learn, or pipelines
                // we already make, handed over a new way.
                let shader_pairs: Vec<(usize, u64, Probe)> = classified
                    .words
                    .iter()
                    .enumerate()
                    .filter_map(|(i, w)| match w {
                        Word::Pair { hash, len, .. } => {
                            let name = format!("blob_{hash}.bin");
                            let bytes = classified.files.iter().find(|f| f.0 == name)?;
                            Some((i, *len, probe(&bytes.1)))
                        }
                        _ => None,
                    })
                    .collect();
                let description = (!words.is_empty()).then(|| &words[..words.len().min(16)]);
                let tokens = classified
                    .words
                    .iter()
                    .map(Word::token)
                    .collect::<Vec<_>>()
                    .join(" ");
                tracing::warn!(
                    slot = format_args!("{:#x}", slot),
                    calls = call,
                    iid = %iid_text(iid),
                    description = format_args!("{description:x?}"),
                    words = %tokens,
                    first_points_at = format_args!("{first:x?}"),
                    ?shader_pairs,
                    "a pipeline creation method that is not implemented: the title goes without this pipeline"
                );
            }
            if let Some(capture) = capture {
                capture.call(slot, call, desc as u64, window, classified, iid, out);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::HashMap;

        /// Memory made of regions; a read succeeds only inside one region.
        #[derive(Default)]
        struct Fake {
            regions: Vec<(u64, Vec<u8>)>,
            objects: HashMap<u64, Own>,
        }

        impl Fake {
            fn region(&mut self, at: u64, len: usize, fill: u8) {
                self.regions
                    .push((at, (0..len).map(|i| fill ^ i as u8).collect()));
            }
        }

        impl Memory for Fake {
            fn read(&self, at: u64, out: &mut [u8]) -> bool {
                for (start, bytes) in &self.regions {
                    let end = start + bytes.len() as u64;
                    if *start <= at && at + out.len() as u64 <= end {
                        let from = (at - start) as usize;
                        out.copy_from_slice(&bytes[from..from + out.len()]);
                        return true;
                    }
                }
                false
            }
            fn object(&self, at: u64) -> Option<Own> {
                self.objects.get(&at).copied()
            }
        }

        const ROOT: u64 = 0x1d3_6867_4fc0;

        /// The first 16 words of a logged 0x218 call (run 301), over memory
        /// shaped to match, then words of other kinds.
        fn logged_0x218() -> (Vec<u64>, Fake) {
            let mut mem = Fake::default();
            mem.objects.insert(ROOT, Own::Root(Some([7; 32])));
            mem.region(0xd_04d4_a000, 0x2000, 1); // the small blobs, words 1-8
            mem.region(0xd_4942_0000, 0x1000, 2); // the code, word 5
            mem.region(0xd_074d_2000, 0x1000, 3); // word 9: a whole window
            mem.region(0xd_074a_1d00, 0x300, 4); // word 10: 0x100 bytes to the end
            let words = vec![
                ROOT,
                0xd_04d4_a6e0,
                0x1c,
                0xd_04d4_a6fc,
                0x14,
                0xd_4942_0000,
                0x180,
                0xd_04d4_a710,
                0x118,
                0xd_074d_26e0,
                0xd_074a_1f00,
                0xd7e7_b1a2_2f0c_3802,
                0,
                0xd_04d4_a6e0,
                0x1c,
                0xd_0bad_0000, // not readable
                0x30,
                0x7ff6_558b_7428, // not readable, above 2^32
                0x8000_0000,      // not readable, below 2^32
            ];
            (words, mem)
        }

        fn kinds(c: &Classified) -> String {
            c.words
                .iter()
                .map(|w| match w {
                    Word::Null => '0',
                    Word::Small => 'i',
                    Word::Len => 'L',
                    Word::Other => 'x',
                    Word::Root(_) => 'R',
                    Word::Object(..) => 'O',
                    Word::Pair { .. } => 'P',
                    Word::Bare { .. } => 'B',
                })
                .collect()
        }

        #[test]
        fn raw_capture_classifies_the_logged_layout_by_content() {
            let (words, mem) = logged_0x218();
            let c = classify(&words, &mem);
            assert_eq!(kinds(&c), "RPLPLPLPLBBx0PLxixi");
            assert_eq!(c.words.len(), words.len());
            assert_eq!(c.words[0], Word::Root(Some(short_of(&[7; 32]))));
            match &c.words[5] {
                Word::Pair { len, saved, hash } => {
                    assert_eq!((*len, *saved), (0x180, 0x180));
                    let name = format!("blob_{hash}.bin");
                    assert!(c.files.iter().any(|f| f.0 == name && f.1.len() == 0x180));
                }
                w => panic!("{w:?}"),
            }
            // A bare address gets a whole window, or what is readable of it.
            assert!(matches!(&c.words[9], Word::Bare { saved: BARE, .. }));
            assert!(matches!(&c.words[10], Word::Bare { saved: 0x100, .. }));
            // Words 1 and 13 name the same bytes: one hash.
            assert_eq!(c.words[1], c.words[13]);
            assert_eq!(c.words[0].token(), format!("R:{}", short_of(&[7; 32])));
            assert_eq!(c.words[2].token(), "L");
            assert!(c.words[5].token().ends_with(":180"));
        }

        #[test]
        fn raw_capture_keeps_partial_reads_and_our_objects() {
            let mut mem = Fake::default();
            mem.region(0x2_0000_0f00, 0x100, 5); // 0x100 bytes, then nothing
            mem.region(0x3_0000_0000, 0x1000, 6);
            mem.objects.insert(0x3_0000_0000, Own::Ours);
            mem.objects.insert(0x4_0000_0000, Own::Device);
            mem.objects.insert(0x5_0000_0000, Own::Root(None));
            let words = [
                0x2_0000_0f00,
                0x400,
                0x3_0000_0000,
                0x4_0000_0000,
                0x5_0000_0000,
                0xffff,
            ];
            let c = classify(&words, &mem);
            assert_eq!(kinds(&c), "PLOORi");
            assert!(matches!(&c.words[0], Word::Pair { len: 0x400, saved: 0x100, .. }));
            assert!(c.words[0].token().ends_with(":400:100"));
            assert!(matches!(&c.words[2], Word::Object("ours", Some((_, BARE)))));
            assert_eq!(c.words[3].token(), "O:device");
            assert_eq!(c.words[4].token(), "R:?");
            // A length too large, or an address after an address, is no length.
            let mut mem = Fake::default();
            mem.region(0x2_0000_0000, 0x1000, 7);
            let words = [0x2_0000_0000, MAX_LEN + 1, 0x2_0000_0010, 0x2_0000_0020];
            let c = classify(&words, &mem);
            assert_eq!(kinds(&c), "BiBB");
        }

        #[test]
        fn raw_capture_reads_stop_at_the_first_missing_page() {
            let mut mem = Fake::default();
            mem.region(0x7_0000_0000, 0x1800, 8);
            assert_eq!(read_prefix(&mem, 0x7_0000_0800, 0x2000).len(), 0x800);
            assert_eq!(read_prefix(&mem, 0x7_0000_0000, 0x800).len(), 0x800);
            assert!(read_prefix(&mem, 0x7_0000_2000, 8).is_empty());
            assert!(read_prefix(&mem, 0x10, 8).is_empty());
            assert!(read_prefix(&mem, u64::MAX - 4, 8).is_empty());
        }

        #[test]
        fn raw_capture_reads_xdat_entries_in_bounds() {
            let mut part = vec![0u8; 0x18];
            part.extend_from_slice(b"XDAT");
            part.extend_from_slice(&2u32.to_le_bytes());
            part.extend_from_slice(&0x24u32.to_le_bytes());
            for (k, o, s) in [(0x10u32, 0x40u32, 0x10u32), (0x0d, 0x50, 0x20)] {
                for v in [k, o, s] {
                    part.extend_from_slice(&v.to_le_bytes());
                }
            }
            assert_eq!(xdat_entries(&part), [(0x10, 0x40, 0x10), (0x0d, 0x50, 0x20)]);
            assert_eq!(xdat_entries(&part[..part.len() - 4]), [(0x10, 0x40, 0x10)]);
            assert!(xdat_entries(b"short").is_empty());
            let line = container_line("compute", b"not a container");
            assert!(line.contains("\tcompute\tf\t-\t-\terror:"), "{line}");
        }
    }
}

/// IUnknown 3, object methods 4, device methods 76 (0x18-0x290), then the
/// methods `ID3D12Device1` to `ID3D12Device8` add, in order: 3, 1, 3, 6, 9,
/// 1, 2, 9 (0x298-0x3a0, the console's own among them), then those of
/// `ID3D12Device9` to `ID3D12Device11`, whose console layout is unknown.
/// Every slot exists so that a call to one we have not written logs it.
static FUNCTIONS: Table<160> = {
    let mut f = object::d3d12_object(object::missing_table::<{ object::DEVICE }, 160>());
    f[0] = query_interface as *const (); // 0x00
    f[7] = node_count as *const (); // 0x38
    f[8] = queue::create as *const (); // 0x40
    f[9] = list::create_allocator as *const (); // 0x48
    f[12] = list::create_list as *const (); // 0x60
    f[10] = pipeline::create_graphics as *const (); // 0x50
    f[11] = pipeline::create_compute as *const (); // 0x58
    f[13] = check_feature_support as *const (); // 0x68
    f[14] = descriptor::create_heap as *const (); // 0x70
    f[15] = descriptor::increment_size as *const (); // 0x78
    f[16] = root::create as *const (); // 0x80
    f[17] = descriptor::create_cbv as *const (); // 0x88
    f[18] = descriptor::create_srv as *const (); // 0x90
    f[19] = descriptor::create_uav as *const (); // 0x98
    f[20] = descriptor::create_rtv as *const (); // 0xa0
    f[21] = descriptor::create_dsv as *const (); // 0xa8
    f[22] = descriptor::create_sampler as *const (); // 0xb0
    f[23] = descriptor::copy as *const (); // 0xb8
    f[24] = descriptor::copy_simple as *const (); // 0xc0
    f[25] = resource::allocation_info as *const (); // 0xc8
    f[26] = resource::custom_heap_properties as *const (); // 0xd0
    f[27] = resource::create_committed as *const (); // 0xd8
    f[28] = resource::create_heap as *const (); // 0xe0
    f[29] = resource::create_placed as *const (); // 0xe8
    f[36] = fence::create as *const (); // 0x120
    f[38] = resource::copyable_footprints as *const (); // 0x130
    f[39] = query::create as *const (); // 0x138
    f[41] = signature::create as *const (); // 0x148
    f[48] = queue::create_x as *const (); // 0x180
    f[57] = unknown_pipeline::<0x1c8> as *const (); // 0x1c8
    f[61] = descriptor::create_sampler_x as *const (); // 0x1e8
    f[62] = pipeline::create_compute_x as *const (); // 0x1f0
    f[67] = unknown_pipeline::<0x218> as *const (); // 0x218
    f[69] = object::ignore as *const (); // 0x228 debug error filter
    f[70] = descriptor::create_placed_srv as *const (); // 0x230
    f[71] = descriptor::create_placed_uav as *const (); // 0x238
    f[73] = list::create_list_x as *const (); // 0x248
    f[74] = resource::register_page_pool as *const (); // 0x250
    f[75] = resource::unregister_page_pool as *const (); // 0x258
    // CreateCommittedResourceX: its description is PC's plus DCC options,
    // which we have no use for (the AMD upscaler's, on Series S).
    f[76] = resource::create_committed as *const (); // 0x260
    f[78] = frame::set_interval as *const (); // 0x270
    f[79] = frame::schedule as *const (); // 0x278
    f[80] = frame::wait as *const (); // 0x280
    f[81] = frame::statistics as *const (); // 0x288
    f[86] = pipeline::create_stream as *const (); // 0x2b0
    f[108] = resource::allocation_info2 as *const (); // 0x360
    f[127] = resource::create_committed3 as *const (); // 0x3f8
    f[133] = resource::create_at_address as *const (); // 0x428
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    fn create_as(iid: &Iid) -> (HRESULT, This) {
        let version = CHECKED_VERSION;
        let mut device = std::ptr::null();
        let hr = create(std::ptr::null(), &version, iid, &mut device);
        (hr, device)
    }

    #[test]
    fn creation_hands_out_the_device() {
        let (hr, device) = create_as(&IID);
        assert_eq!(hr, S_OK);
        assert_eq!(device, super::device());
        // The title tests byte 0x18 before inlining a device call.
        // SAFETY: the device is 0x58 bytes.
        assert_ne!(unsafe { device.cast::<u8>().add(0x18).read() }, 0);
        assert_eq!(create_as(&[0; 16]).0, E_NOINTERFACE);
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(device, 0x38)(device),
            1
        );
        let missing = slot::<extern "system" fn() -> HRESULT>(device, 0xf0);
        assert_eq!(missing(), E_NOTIMPL);
    }

    #[test]
    fn newer_device_levels_are_the_same_device() {
        // ID3D12Device11, as the title asks for it.
        let (hr, device) = create_as(&LEVEL_IIDS[8]);
        assert_eq!((hr, device), (S_OK, super::device()));
        let mut same = std::ptr::null();
        assert_eq!(query_interface(device, &LEVEL_IIDS[4], &mut same), S_OK);
        assert_eq!(same, device);
        // A Device8 method we have not written fails without crashing.
        let missing = slot::<extern "system" fn() -> HRESULT>(device, 0x3a0);
        assert_eq!(missing(), E_NOTIMPL);
    }

    /// A device child, and the DXGI device, hand out the device as any
    /// interface the device itself answers to.
    #[test]
    fn the_device_is_found_at_every_level_through_others() {
        type Get = extern "system" fn(This, *const Iid, *mut This) -> HRESULT;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let mut fence = std::ptr::null();
        assert_eq!(
            crate::fence::create(std::ptr::null(), 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let get_device = slot::<Get>(fence, 0x38);
        let dxgi = dxgi::device();
        let query = slot::<Get>(dxgi, 0);
        for iid in std::iter::once(&IID).chain(&LEVEL_IIDS) {
            let mut found = std::ptr::null();
            assert_eq!(get_device(fence, iid, &mut found), S_OK, "{iid:02x?}");
            assert_eq!(found, device());
            let mut found = std::ptr::null();
            assert_eq!(query(dxgi, iid, &mut found), S_OK, "{iid:02x?}");
            assert_eq!(found, device());
        }
        // What the device is not, it is not through them either.
        let mut found = device();
        assert_eq!(get_device(fence, fence_iid, &mut found), E_NOINTERFACE);
        assert!(found.is_null());
        slot::<extern "system" fn(This) -> u32>(fence, 0x10)(fence);
    }

    /// The host's: a fence's `GetDevice` for `ID3D12Device5`.
    #[cfg(windows)]
    #[test]
    fn the_hosts_children_hand_out_newer_device_levels() {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_FENCE_FLAG_NONE, ID3D12Device5, ID3D12Fence,
        };
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        // SAFETY: plain calls on the host device and its fence.
        unsafe {
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let mut device: Option<ID3D12Device5> = None;
            assert!(fence.GetDevice(&mut device).is_ok());
            assert!(device.is_some());
        }
    }

    /// Both methods fail and hand out nothing, whatever the description
    /// holds, also when it is looked into (the first calls are logged).
    #[test]
    fn missing_pipeline_methods_fail_and_hand_out_nothing() {
        type Make = extern "system" fn(This, *const u8, *const Iid, *mut This) -> HRESULT;
        let blob = [0xab_u8; 0x40];
        let words: [u64; 4] = [device() as u64, blob.as_ptr() as u64, 0x40, 0xd7e7_b1a2_2f0c_3802];
        for offset in [0x1c8, 0x218] {
            let make = slot::<Make>(device(), offset);
            for _ in 0..5 {
                let mut out = device();
                assert_eq!(make(device(), words.as_ptr().cast(), &IID, &mut out), E_NOTIMPL);
                assert!(out.is_null());
            }
            let null = std::ptr::null_mut();
            assert_eq!(make(device(), std::ptr::null(), std::ptr::null(), null), E_NOTIMPL);
        }
    }

    #[test]
    fn virtual_memory_configuration_reports_user_space() {
        let mut config = VirtualMemory {
            flags: 0,
            page_table_pages: 7,
            preserved_bytes: 0,
            lowest: 0,
            highest: 0,
        };
        assert_eq!(configure_virtual_memory(&mut config), S_OK);
        assert!(0 < config.lowest && config.lowest < config.highest);
        let heap = Box::new(0u8);
        assert!((config.lowest..=config.highest).contains(&(&raw const *heap as u64)));
        assert_eq!(configure_virtual_memory(std::ptr::null_mut()), E_INVALIDARG);
    }

    #[test]
    fn feature_levels_pick_the_highest_supported() {
        type Check = extern "system" fn(This, u32, *mut c_void, u32) -> HRESULT;
        let device = device();
        let check = slot::<Check>(device, 0x68);
        let query = |levels: &[u32], size| {
            let mut query = FeatureLevels {
                count: levels.len() as u32,
                requested: levels.as_ptr(),
                highest: 0,
            };
            let hr = check(device, 2, (&raw mut query).cast(), size);
            (hr, query.highest)
        };
        let levels = [0xb000, 0xb100, 0xc000, 0xc100, 0xd000];
        assert_eq!(query(&levels, 0x18), (S_OK, 0xc100));
        assert_eq!(query(&levels, 0x10).0, E_INVALIDARG);
        assert_eq!(query(&levels[4..], 0x18).0, UNSUPPORTED);
    }
}
