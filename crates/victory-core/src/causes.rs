//! What the layer did not do, and why: one counter per named cause.
//!
//! Every place where a run does less than the title asked (a draw left
//! out, a view left null, a copy not made, a slower path taken, a title
//! call refused) reports its exact cause here with [`note`]. A cause has
//! a stable machine name, a one-line plain description and the part of
//! the picture it affects. The counts are lock-free: a note is one atomic
//! add and two atomic loads in the usual case, so hot paths can afford it.
//!
//! The runtime prints [`changes_line`] with its periodic status (only the
//! causes that changed since the last line) and [`log_summary`] once at
//! the end of a run: every cause that happened, how often, and the first
//! and last frame it happened in. `victory report` reads that summary.
//!
//! Counting only: nothing here changes what the layer does.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The part of the picture (or of the run) a cause affects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// Whole draws, dispatches, passes or command lists missing.
    Geometry,
    /// Textures, render targets or buffers black, zero or out of date.
    Surfaces,
    /// Data the title reads back on the CPU (results of the GPU) out of date.
    TitleData,
    /// A frame not shown.
    Present,
    /// Values a translated shader computes differently from the console.
    Values,
    /// A slower path taken; the picture is the same.
    Speed,
    /// A title call refused or answered with a stand-in.
    TitleCalls,
    /// Assets the title loads (files, DirectStorage) missing or late.
    Loading,
}

impl Area {
    pub fn name(self) -> &'static str {
        match self {
            Area::Geometry => "geometry",
            Area::Surfaces => "surfaces",
            Area::TitleData => "title data",
            Area::Present => "present",
            Area::Values => "shader values",
            Area::Speed => "speed",
            Area::TitleCalls => "title calls",
            Area::Loading => "loading",
        }
    }
}

/// A cause's fixed facts.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    /// Stable machine name (dotted, lower case); logs and tools key on it.
    pub name: &'static str,
    pub area: Area,
    /// One plain line: what was not done.
    pub what: &'static str,
}

macro_rules! causes {
    ($($variant:ident = $name:literal, $area:ident, $what:literal;)*) => {
        /// Every cause the layer counts. The order is the order of the
        /// summary's table when counts are equal; names never change.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u16)]
        pub enum Cause { $($variant,)* }

        impl Cause {
            pub const ALL: &'static [Cause] = &[$(Cause::$variant,)*];
            pub const fn info(self) -> Info {
                const INFO: &[Info] = &[$(Info { name: $name, area: Area::$area, what: $what },)*];
                INFO[self as usize]
            }
        }

        /// The number of causes.
        pub const COUNT: usize = [$(Cause::$variant,)*].len();
    };
}

causes! {
    // Draws and dispatches (draw.rs, pipeline, indirect.rs). The two
    // totals count every direct draw or dispatch and every indirect
    // execution left out, whatever the cause; the draw.* rows name the
    // causes, for direct and indirect ones alike.
    DrawDropped = "draw.dropped", Geometry, "direct draws and dispatches left out, all causes together (the draw.* rows name the causes)";
    DrawNoPipelineRefused = "draw.no_pipeline.refused", Geometry, "a draw or dispatch left out: its pipeline has no host pipeline (refused or not translated)";
    DrawNoPipelineLate = "draw.no_pipeline.late", Geometry, "a draw or dispatch left out: its host pipeline was not made within the frame's wait";
    DrawNoPipelineSet = "draw.no_pipeline.none_set", Geometry, "a draw or dispatch left out: no pipeline set (e.g. the title went without one it could not create)";
    DrawWrongPipelineKind = "draw.wrong_pipeline_kind", Geometry, "a draw or dispatch left out: the bound pipeline is of another kind (graphics, mesh, compute)";
    DrawMeshNotSupported = "draw.mesh.list_cannot", Geometry, "a mesh dispatch left out: the host command list cannot dispatch meshes";
    DrawNoDynamicSlots = "draw.no_dynamic_slots", Geometry, "a draw left out: no free run of dynamic descriptor slots for a table or root view it needs";
    DrawNoViewHeap = "draw.no_view_heap", Geometry, "a draw left out: it needs a view heap (raw memory, root views) and none is bound";
    DrawNoRootUav = "draw.no_root_uav", Geometry, "a draw or dispatch left out: no usable root unordered view";
    DrawNoIndexBuffer = "draw.no_index_buffer", Geometry, "an indexed draw left out: its index buffer has no host copy";
    DrawRectanglesNoGeometryStage = "draw.rectangles.no_geometry_stage", Geometry, "indexed rectangles left out: the pipeline has no rectangle geometry stage";
    DrawRectanglesBadVertices = "draw.rectangles.bad_vertices", Geometry, "a rectangle draw left out: its vertices are outside their buffer";
    DrawBuilderOff = "draw.bvh_builder_off", Geometry, "a ray-tracing structure build dispatch left out: the builder is off";
    DrawOther = "draw.other", Geometry, "a draw or dispatch left out for a cause not named here (a diagnostics switch, no scratch memory)";
    DrawRootViewUnset = "draw.root_view_unset", Surfaces, "a root constant or shader view left unset: its title address has no host copy";
    DrawTableNotMade = "draw.table_not_made", Surfaces, "a descriptor table not made for a draw: the shader reads the title's own slots";
    DrawVertexBufferZero = "draw.vertex_buffer_zero", Geometry, "a vertex buffer without a host copy: the draw reads zeros";
    DrawTargetLeftOut = "draw.target_left_out", Surfaces, "a render target or depth view without a record left out of a draw";
    IndirectDropped = "indirect.dropped", Geometry, "indirect executions left out, all causes together (the indirect.* and draw.* rows name the causes)";
    IndirectRootArguments = "indirect.root_arguments", Geometry, "an indirect execution left out: its signature changes root arguments";
    IndirectNotTitleBuffer = "indirect.not_title_buffer", Geometry, "an indirect execution left out: its arguments or count are not in a title buffer";
    IndirectNoSignature = "indirect.no_host_signature", Geometry, "an indirect execution left out: the host refused its command signature";
    IndirectNotBounded = "indirect.not_bounded", Geometry, "an indirect execution left out: its arguments could not be clamped (no clamp pass, or not 4-byte aligned)";
    IndirectMeshX = "indirect.mesh_x", Geometry, "an indirect mesh dispatch (the console's X form) left out";
    MeshGroupsYz = "mesh.groups_yz_ignored", Geometry, "a mesh dispatch's y and z group counts ignored (only x runs)";
    IndirectClampEmptied = "indirect.clamp_emptied", Geometry, "indirect records run empty or cut by the argument clamp";
    MeshRecordsClamped = "indirect.mesh_records_clamped", Geometry, "indirect mesh records clamped on the card (x over 65,535, or y or z over 1): groups beyond run nowhere";
    MeshOutOfTurn = "indirect.mesh_out_of_turn", Geometry, "an indirect mesh draw charged the list's budget out of turn: only the order of the charges changed, so which draws run empty may differ from the clamp pass only where the list's budget ran out";
    MeshClampNoForm = "mesh_launch.clamp_pass.no_form", Speed, "an indirect mesh draw took the clamp pass: its pipeline has no form with the amplification stage";
    MeshClampRecords = "mesh_launch.clamp_pass.records", Speed, "an indirect mesh draw took the clamp pass: a command of other than one record";
    MeshClampLayout = "mesh_launch.clamp_pass.layout", Speed, "an indirect mesh draw took the clamp pass: a record or count the stage cannot read";
    MeshClampNoBudget = "mesh_launch.clamp_pass.no_budget", Speed, "an indirect mesh draw took the clamp pass: no budget words for the list";
    MeshClampNoMeshList = "mesh_launch.clamp_pass.no_mesh_list", Speed, "an indirect mesh draw took the clamp pass: the host list has no mesh dispatch";
    MeshClampMappedBudget = "mesh_launch.clamp_pass.mapped_budget", Speed, "an indirect mesh draw took the clamp pass: the budget words are in memory the CPU maps";
    MeshNoFormTranslate = "mesh_launch.no_form.translate", Speed, "a mesh pipeline without a form with the stage: its mesh shader does not translate for it";
    MeshNoFormRoot = "mesh_launch.no_form.root_signature", Speed, "a mesh pipeline without a form with the stage: its root signature is refused";
    MeshNoFormRefused = "mesh_launch.no_form.refused", Speed, "a mesh pipeline without a form with the stage: the host refused that pipeline";
    MeshNoFormStage = "mesh_launch.no_form.no_stage", Speed, "a mesh pipeline without a form with the stage: the stage itself is missing";
    // Shaders and pipelines (victory-shader, pipeline).
    ShaderNotTranslated = "shader.not_translated", Geometry, "a shader that does not translate: its pipelines' draws are left out";
    ShaderNoWave32 = "shader.refused.no_wave_32", Geometry, "a compute shader refused: it needs waves of 32 lanes, which this card lacks";
    ShaderNoWave64 = "shader.refused.no_wave_64", Geometry, "a compute shader refused: it needs waves of 64 lanes, which this card lacks";
    PipelineRefused = "pipeline.refused", Geometry, "a pipeline refused, every one counted (the host refused it, or it uses a stage not translated): its draws are left out";
    PipelineTitleAmplification = "pipeline.title_amplification", Geometry, "a pipeline with the title's own amplification stage (kind 23) refused: it is not translated";
    ShaderLoopsCapped = "shader.loops_capped", Values, "a translated program whose loops are cut at an iteration cap (pixel 128, vertex and mesh 256, compute 4,096): a lane past it is discarded or ends";
    ShaderJoinZero = "shader.join_zero", Values, "a translated program where a value with no DXIL form comes into a join as zero";
    // Textures and descriptor slots.
    TextureHostCopyRefused = "texture.host_copy_refused", Surfaces, "a texture whose host copy could not be made: every use of it is left out (black)";
    TextureReservedFallback = "texture.reserved_fallback", Speed, "a texture meant to take memory level by level made whole instead";
    TextureNoContentNoLayout = "texture.no_content.no_layout", Surfaces, "a texture shown without contents: no console layout for its format or tiling";
    TextureNoContentXor = "texture.no_content.xor_swizzle", Surfaces, "a texture shown without contents: an XOR-swizzled layout the detiler does not read";
    TextureNoContentStaging = "texture.no_content.no_staging", Surfaces, "a texture shown without contents: no staging buffer could be made";
    TilesNoHeap = "texture.levels_no_heap", Surfaces, "texture levels given on demand left empty: no heap for their tiles";
    TextureDetileFailed = "texture.detile_failed", Surfaces, "a texture level or slice not detiled: it keeps old or zero texels";
    DescriptorViewNotMade = "descriptor.view_not_made", Surfaces, "a descriptor slot made null: its view could not be made (all causes; descriptor.view_refused is one of them)";
    DescriptorViewRefused = "descriptor.view_refused", Surfaces, "a descriptor slot left null: the host would refuse the view (alignment, format, flags)";
    DescriptorPastHostHeap = "descriptor.past_host_heap", Surfaces, "a descriptor slot past the host heap's size: it reads the null slot";
    DescriptorNoHostHeap = "descriptor.no_host_heap", Surfaces, "a descriptor heap without a host heap: none of its slots is shown";
    DescriptorNoDynamicSlots = "descriptor.no_dynamic_slots", Surfaces, "no free run of dynamic descriptor slots for a table (the draw that asked is left out)";
    DescriptorPageUnreadable = "descriptor.page_unreadable", Surfaces, "a changed page of a descriptor heap could not be read: its slots keep their old views";
    DescriptorCopyShort = "descriptor.copy_short", Surfaces, "a descriptor copy stopped early: source and destination ranges do not match";
    DescriptorChangeDeferred = "descriptor.change_deferred", Surfaces, "a slot change put off while a submission was in flight: the slot shows its old view meanwhile";
    DescriptorRemappedOverflow = "descriptor.remapped_overflow", Surfaces, "remapped memory ranges under texture views dropped (too many pending)";
    DepthNoTwin = "depth.no_twin", Surfaces, "a depth texture with unordered access left without its colour twin: shader writes to it are lost";
    PlanePassNotMade = "depth.plane_pass_not_made", Surfaces, "a depth plane not refreshed from its twin: no pass could be made";
    MemoryPageWithoutCopy = "memory.page_without_copy", Surfaces, "a shader reading title memory by address touched pages with no copy: it reads zeros";
    MemoryNoRoom = "memory.no_room", Geometry, "a draw reading title memory by address left out: no view heap or no room for its views";
    // Copies, clears, resolves, read-backs.
    ScaleCopy = "scale.copy_left_out", Surfaces, "a copy between a render-scaled and an unscaled texture left out";
    ScaleRegion = "scale.region_left_out", Surfaces, "a region copy between a render-scaled and an unscaled texture left out";
    ScaleReadBack = "scale.read_back_left_out", TitleData, "a read-back of a render-scaled texture left out";
    ScaleResolveClipped = "scale.resolve_clipped", Surfaces, "a resolve between a render-scaled and an unscaled texture clipped to the smaller";
    ScaleAlias = "scale.alias_left_out", Surfaces, "an alias copy between a render-scaled and an unscaled texture not made";
    CopyNoHostTexture = "copy.no_host_texture", Surfaces, "a texture copy left out: a side has no host texture";
    CopyStandInFormat = "copy.stand_in_format", Surfaces, "a copy or resolve left out: one side is a stand-in format the other is not";
    CopyBufferToTexture = "copy.buffer_to_texture_left_out", Surfaces, "a buffer-to-texture copy left out (no host, unknown footprint, no upload memory)";
    ClearLeftOut = "clear.left_out", Surfaces, "a clear left out: its target has no view record or no host texture";
    ResolveLeftOut = "resolve.left_out", Surfaces, "a resolve left out: a side has no host texture";
    ReadBackRefused = "read_back.refused", TitleData, "a texture read-back into title memory not made: the title reads old data";
    ReadBackNotLanded = "read_back.not_landed", TitleData, "a read-back's bytes not put into title memory (the read-back buffer could not be mapped)";
    TextureBackLost = "texture_back.lost", TitleData, "a texture copied back for the CPU not landed (read or map failed)";
    TitleWriteFailed = "title_memory.write_failed", TitleData, "GPU results not written into title memory: the kernel refused the write";
    TitleReadZeroed = "title_memory.read_zeroed", Surfaces, "title memory that could not be read was copied as zeros";
    TitleCopyNoDestination = "title_memory.copy_no_destination", TitleData, "a copy into title memory left out: the destination is not committed";
    ShadowBackUnreadable = "title_memory.back_unreadable", TitleData, "shader writes not put back: the title memory could not be read to merge them";
    WaitNotKept = "submit.wait_not_kept", TitleData, "a GPU submission not waited for (the wait could not be set): results may be read early";
    // Command lists and submission.
    ListLostNoHostList = "list.lost.no_host_list", Geometry, "a whole command list not submitted: no host command list could be had";
    ListLostAtExecute = "list.lost.at_execute", Geometry, "a title command list without a host list left out of a submission";
    WaitForced = "queue.wait_forced", Surfaces, "a queue's fence or memory wait not met at a present: the work ran anyway";
    PageMappingFailed = "queue.page_mapping_failed", Surfaces, "a page mapping copy failed (reported as success): those pages read zeros";
    BufferNotMade = "buffer.not_made", Geometry, "a host buffer (upload, read-back, shared, unordered) could not be made";
    // Presenting.
    PresentFormat = "present.format", Present, "a frame not shown: the presented format is not one the window takes";
    PresentNoSwapChain = "present.no_swap_chain", Present, "a frame not shown: no swap chain, or it could not be resized or give its buffer";
    PresentNotShown = "present.no_host_texture", Present, "a frame not shown: the presented texture has no host copy";
    // Slower paths.
    InPlaceNotTested = "in_place.before_start_up_test", Speed, "title memory copied: the in-place start-up test had not run yet";
    InPlaceNotWhole = "in_place.refused.not_whole", Speed, "title memory copied instead of used in place: private memory not committed whole and read-write";
    InPlaceNotOwned = "in_place.refused.not_owned", Speed, "title memory copied instead of used in place: private memory the runtime did not make for the title";
    InPlaceOverBudget = "in_place.refused.over_budget", Speed, "title memory copied instead of used in place: the in-place budget (VICTORY_INPLACE_MB) would be passed";
    InPlaceHostRefused = "in_place.refused.by_host", Speed, "title memory copied instead of used in place: the driver refused the opening or the placement";
    InPlaceNoNotice = "in_place.refused.no_notice", Speed, "title memory copied instead of used in place: the heap gives no destruction notice";
    InPlaceDeviceRefused = "in_place.refused.by_start_up_test", Speed, "title memory copied instead of used in place: the start-up test refused this kind of memory";
    InPlaceViewScattered = "in_place.refused.view_scattered", Speed, "page-pool memory copied: its pages come from more than one place of a section";
    InPlaceViewHoles = "in_place.refused.view_holes", Speed, "page-pool memory copied: it has unmapped pages in it";
    InPlaceViewSections = "in_place.refused.view_sections", Speed, "page-pool memory copied: it runs over more than one section";
    InPlaceViewReserved = "in_place.refused.view_reserved", Speed, "page-pool memory copied: its section was made in the old (reserved) form";
    InPlaceStillPinned = "in_place.still_pinned", Speed, "a title memory change came while a heap in place still covered it after the wait";
    InPlaceHeld = "in_place.change_held", Speed, "a title memory change held until a heap in place over it goes (the title was told it succeeded)";
    PoolOldForm = "in_place.pool_old_form", Speed, "the page pool fell back to its old form: its memory is copied, not in place";
    CardCopyRefused = "card.copy_refused", Speed, "a title buffer given no copy in the card's memory: shaders read it over the bus";
    ReusePoolFull = "reuse.pool_full", Speed, "a texture's host copy retired instead of kept for reuse (pool full or card full)";
    PlainCopyFallback = "copy_plain.fallback", Speed, "an in-place copy of title memory fell back to the kernel's copy";
    ShaderCacheNotStored = "shader_cache.not_stored", Speed, "a translated shader not kept in the cache (it is translated again next run)";
    ShaderCacheUnreadable = "shader_cache.unreadable", Speed, "a shader cache entry unreadable: translated again";
    // Title calls.
    CallNotImplementedMethod = "call.method_not_implemented", TitleCalls, "a D3D object method the layer does not implement answered E_NOTIMPL";
    CallUnknownInterface = "call.unknown_interface", TitleCalls, "an interface the layer does not know asked for (E_NOINTERFACE)";
    CallFeatureQuery = "call.feature_query", TitleCalls, "a device feature query the layer does not answer (E_NOTIMPL)";
    CallPipelineMethod = "call.pipeline_method", TitleCalls, "a pipeline creation method not implemented (0x1c8, 0x218): the title goes without that pipeline";
    CallRootSerialize = "call.root_serialize", TitleCalls, "a root signature (de)serialize call refused: no host d3d12.dll";
    CallQueryResults = "call.query_results", TitleCalls, "query results resolved with made-up values (no query runs on the host)";
    CallRuntimeMethod = "call.runtime_method", TitleCalls, "a console runtime method not implemented (E_NOTIMPL)";
    CallRuntimeArea = "call.runtime_area", TitleCalls, "a console runtime API area not implemented";
    CallGetProcAddress = "call.get_proc_address", TitleCalls, "GetProcAddress on a layer module for a name it does not have: null";
    CallHostDllRefused = "call.host_dll_refused", TitleCalls, "a host DLL outside the host policy refused";
    CallImportNotImplemented = "call.import_not_implemented", TitleCalls, "an import the layer does not provide called (answered 0)";
    CallInputUnsupported = "call.input_unsupported", TitleCalls, "a game input method not supported (haptics, motors, raw device)";
    CallTextureComputer = "call.texture_computer", TitleCalls, "a texture computer call refused (no layout, copy failed)";
    CallAudioNotImplemented = "call.audio_not_implemented", TitleCalls, "an audio processing kind not implemented";
    CallSaveRefused = "call.save_refused", TitleCalls, "a game save call failed or was refused";
    // Loading.
    StorageRequestFailed = "storage.request_failed", Loading, "a DirectStorage request failed: its destination keeps old bytes";
    StoragePagesNotWritten = "storage.pages_not_written", Loading, "a DirectStorage read into physical pages gave up after its retries";
    StorageBcpack = "storage.bcpack_unavailable", Loading, "a BCPack request failed: the BCPack tables are not available";
    StorageDropped = "storage.command_dropped", Loading, "a DirectStorage command dropped: its queue has no worker";
    MapPagesFailed = "memory.map_pages_failed", Loading, "physical pages not mapped (unknown page or the host refused): that range reads zeros";
}

/// One cause's counts. `first` and `last` hold frame + 1 (0: never).
struct Slot {
    count: AtomicU64,
    first: AtomicU64,
    last: AtomicU64,
    shown: AtomicU64,
}

impl Slot {
    const fn new() -> Slot {
        Slot {
            count: AtomicU64::new(0),
            first: AtomicU64::new(0),
            last: AtomicU64::new(0),
            shown: AtomicU64::new(0),
        }
    }
}

/// A set of counters, one per cause. The runtime uses [`REGISTRY`]; tests
/// make their own.
pub struct Registry {
    slots: [Slot; COUNT],
    frame: AtomicU64,
    changes_shown: Mutex<()>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// One line of the summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub cause: Cause,
    pub count: u64,
    pub first_frame: u64,
    pub last_frame: u64,
}

impl Registry {
    pub const fn new() -> Registry {
        Registry {
            slots: [const { Slot::new() }; COUNT],
            frame: AtomicU64::new(0),
            changes_shown: Mutex::new(()),
        }
    }

    /// The frame number later notes are stamped with (frames presented).
    pub fn set_frame(&self, frame: u64) {
        self.frame.store(frame, Ordering::Relaxed);
    }

    pub fn frame(&self) -> u64 {
        self.frame.load(Ordering::Relaxed)
    }

    /// Counts `n` occurrences of `cause` in the current frame.
    #[inline]
    pub fn note_n(&self, cause: Cause, n: u64) {
        if n == 0 {
            return;
        }
        let slot = &self.slots[cause as usize];
        slot.count.fetch_add(n, Ordering::Relaxed);
        let stamp = self.frame.load(Ordering::Relaxed) + 1;
        if slot.first.load(Ordering::Relaxed) == 0 {
            // Only the first stamp is kept; a lost race keeps an equal one.
            let _ = slot
                .first
                .compare_exchange(0, stamp, Ordering::Relaxed, Ordering::Relaxed);
        }
        if slot.last.load(Ordering::Relaxed) < stamp {
            slot.last.fetch_max(stamp, Ordering::Relaxed);
        }
    }

    /// For a cause counted by a cumulative counter elsewhere: raises the
    /// count to `total` (never lowers it), stamping the current frame.
    pub fn observe(&self, cause: Cause, total: u64) {
        let now = self.slots[cause as usize].count.load(Ordering::Relaxed);
        if total > now {
            self.note_n(cause, total - now);
        }
    }

    pub fn count(&self, cause: Cause) -> u64 {
        self.slots[cause as usize].count.load(Ordering::Relaxed)
    }

    /// Every cause that happened, most frequent first.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = Cause::ALL
            .iter()
            .filter_map(|&cause| {
                let s = &self.slots[cause as usize];
                let count = s.count.load(Ordering::Relaxed);
                (count > 0).then(|| Row {
                    cause,
                    count,
                    first_frame: s.first.load(Ordering::Relaxed).saturating_sub(1),
                    last_frame: s.last.load(Ordering::Relaxed).saturating_sub(1),
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then((a.cause as u16).cmp(&(b.cause as u16)))
        });
        rows
    }

    /// The causes that changed since the last call, as one line; `None`
    /// when nothing changed.
    pub fn changes_line(&self, seconds: f64) -> Option<String> {
        let _one = self.changes_shown.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed: Vec<(u64, u64, Cause)> = Vec::new();
        for &cause in Cause::ALL {
            let s = &self.slots[cause as usize];
            let count = s.count.load(Ordering::Relaxed);
            let shown = s.shown.swap(count, Ordering::Relaxed);
            if count > shown {
                changed.push((count - shown, count, cause));
            }
        }
        if changed.is_empty() {
            return None;
        }
        changed.sort_by(|a, b| b.0.cmp(&a.0).then((a.2 as u16).cmp(&(b.2 as u16))));
        let list: Vec<String> = changed
            .iter()
            .map(|(d, t, c)| format!("{} +{d} (total {t})", c.info().name))
            .collect();
        Some(format!(
            "not done in the last {seconds:.0} s, by cause (frame {}): {}",
            self.frame(),
            list.join(", ")
        ))
    }

    /// The end-of-run summary: a header line, then one line per cause that
    /// happened. Empty when nothing happened.
    pub fn summary_lines(&self) -> Vec<String> {
        let rows = self.rows();
        if rows.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(rows.len() + 1);
        out.push(format!(
            "{SUMMARY_HEAD} {} causes over {} frames, most frequent first",
            rows.len(),
            self.frame()
        ));
        for r in &rows {
            out.push(summary_row(r));
        }
        out
    }
}

/// The summary header's text; `victory report` looks for it.
pub const SUMMARY_HEAD: &str = "what the layer did not do in this run:";
/// Each summary row starts with this text; `victory report` looks for it.
pub const ROW_HEAD: &str = "not done:";
/// The summary when nothing was counted.
pub const SUMMARY_NOTHING: &str = "what the layer did not do in this run: nothing counted";

/// `not done: <name> count=N first_frame=F last_frame=L area="..." what="..."`.
pub fn summary_row(r: &Row) -> String {
    let i = r.cause.info();
    format!(
        "{ROW_HEAD} {} count={} first_frame={} last_frame={} area=\"{}\" what=\"{}\"",
        i.name,
        r.count,
        r.first_frame,
        r.last_frame,
        i.area.name(),
        i.what
    )
}

/// The run's registry.
pub static REGISTRY: Registry = Registry::new();

/// Counts one occurrence of `cause`.
#[inline]
pub fn note(cause: Cause) {
    REGISTRY.note_n(cause, 1);
}

/// Counts `n` occurrences of `cause`.
#[inline]
pub fn note_n(cause: Cause, n: u64) {
    REGISTRY.note_n(cause, n);
}

/// See [`Registry::observe`].
pub fn observe(cause: Cause, total: u64) {
    REGISTRY.observe(cause, total);
}

/// Sets the frame later notes are stamped with.
pub fn set_frame(frame: u64) {
    REGISTRY.set_frame(frame);
}

/// See [`Registry::changes_line`].
pub fn changes_line(seconds: f64) -> Option<String> {
    REGISTRY.changes_line(seconds)
}

/// Functions run just before the summary, for causes counted by a
/// cumulative counter in a crate that cannot note them itself (they call
/// [`observe`]).
static BEFORE_SUMMARY: Mutex<Vec<fn()>> = Mutex::new(Vec::new());

/// Runs `f` just before the end-of-run summary is made.
pub fn before_summary(f: fn()) {
    BEFORE_SUMMARY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(f);
}

/// Logs the end-of-run summary once (later calls do nothing).
pub fn log_summary() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let hooks: Vec<fn()> = BEFORE_SUMMARY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    for hook in hooks {
        hook();
    }
    let lines = REGISTRY.summary_lines();
    if lines.is_empty() {
        tracing::info!(target: "victory_core::causes", "{SUMMARY_NOTHING}");
    }
    for line in lines {
        tracing::info!(target: "victory_core::causes", "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_plain() {
        let mut names: Vec<&str> = Cause::ALL.iter().map(|c| c.info().name).collect();
        assert_eq!(names.len(), COUNT);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), COUNT, "two causes share a name");
        for c in Cause::ALL {
            let i = c.info();
            assert!(
                i.name.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'.'
                    || b == b'_'),
                "{}",
                i.name
            );
            assert!(!i.what.contains('"') && !i.what.is_empty(), "{}", i.name);
        }
    }

    #[test]
    fn notes_are_counted_with_their_first_and_last_frame() {
        let r = Registry::new();
        r.set_frame(7);
        r.note_n(Cause::ClearLeftOut, 1);
        r.set_frame(9);
        r.note_n(Cause::ClearLeftOut, 2);
        r.note_n(Cause::PresentFormat, 0);
        r.set_frame(30);
        r.note_n(Cause::ClearLeftOut, 1);
        assert_eq!(r.count(Cause::ClearLeftOut), 4);
        assert_eq!(r.count(Cause::PresentFormat), 0);
        assert_eq!(
            r.rows(),
            vec![Row {
                cause: Cause::ClearLeftOut,
                count: 4,
                first_frame: 7,
                last_frame: 30
            }]
        );
    }

    #[test]
    fn a_note_before_the_first_frame_is_frame_zero() {
        let r = Registry::new();
        r.note_n(Cause::PoolOldForm, 1);
        assert_eq!(r.rows()[0].first_frame, 0);
        assert_eq!(r.rows()[0].last_frame, 0);
    }

    #[test]
    fn observe_raises_to_a_total_and_never_lowers() {
        let r = Registry::new();
        r.set_frame(3);
        r.observe(Cause::ShaderNoWave64, 5);
        r.set_frame(4);
        r.observe(Cause::ShaderNoWave64, 2);
        assert_eq!(
            r.rows(),
            vec![Row {
                cause: Cause::ShaderNoWave64,
                count: 5,
                first_frame: 3,
                last_frame: 3
            }]
        );
        r.set_frame(8);
        r.observe(Cause::ShaderNoWave64, 6);
        assert_eq!(r.rows()[0].last_frame, 8);
    }

    #[test]
    fn nothing_happened_means_no_output() {
        let r = Registry::new();
        assert!(r.rows().is_empty());
        assert!(r.summary_lines().is_empty());
        assert_eq!(r.changes_line(5.0), None);
    }

    #[test]
    fn the_periodic_line_holds_only_what_changed() {
        let r = Registry::new();
        r.set_frame(100);
        r.note_n(Cause::ClearLeftOut, 3);
        r.note_n(Cause::PresentFormat, 1);
        assert_eq!(
            r.changes_line(5.0).as_deref(),
            Some(
                "not done in the last 5 s, by cause (frame 100): clear.left_out +3 (total 3), present.format +1 (total 1)"
            )
        );
        assert_eq!(r.changes_line(5.0), None);
        r.note_n(Cause::PresentFormat, 4);
        assert_eq!(
            r.changes_line(5.0).as_deref(),
            Some("not done in the last 5 s, by cause (frame 100): present.format +4 (total 5)")
        );
    }

    #[test]
    fn the_summary_is_a_table_most_frequent_first() {
        let r = Registry::new();
        r.set_frame(2);
        r.note_n(Cause::PresentFormat, 1);
        r.set_frame(5);
        r.note_n(Cause::ScaleCopy, 12);
        r.set_frame(640);
        let lines = r.summary_lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            "what the layer did not do in this run: 2 causes over 640 frames, most frequent first"
        );
        assert_eq!(
            lines[1],
            "not done: scale.copy_left_out count=12 first_frame=5 last_frame=5 area=\"surfaces\" \
             what=\"a copy between a render-scaled and an unscaled texture left out\""
        );
        assert!(lines[2].starts_with(
            "not done: present.format count=1 first_frame=2 last_frame=2 area=\"present\""
        ));
    }

    #[test]
    fn notes_from_many_threads_add_up() {
        static R: Registry = Registry::new();
        let threads: Vec<_> = (0..8)
            .map(|t| {
                std::thread::spawn(move || {
                    for i in 0..10_000u64 {
                        if t == 0 && i % 1000 == 0 {
                            R.set_frame(i);
                        }
                        R.note_n(Cause::DrawOther, 1);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(R.count(Cause::DrawOther), 80_000);
        let row = &R.rows()[0];
        assert!(row.first_frame <= row.last_frame);
    }
}
