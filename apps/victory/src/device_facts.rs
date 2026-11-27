//! `victory device`: what each hardware adapter's D3D12 device says about
//! itself, without a title. The layer makes its device with
//! `D3D12CreateDevice(None, ..)`, which is the first adapter DXGI lists;
//! that one is marked. Everything made here is released before the next
//! adapter is asked.

use anyhow::{Context, Result};
use std::ffi::c_void;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Memory::*;
use windows::core::Interface;

const K64: usize = 64 * 1024;

/// `MEM_WRITE_WATCH`, which the bindings leave out.
const MEM_WRITE_WATCH: VIRTUAL_ALLOCATION_TYPE = VIRTUAL_ALLOCATION_TYPE(0x0020_0000);

/// The view heap size the layer tries first on start-up (descriptor/host.rs,
/// `UNCAPPED_VIEWS`); a host that refuses it gets the portable 1,000,000.
const UNCAPPED_VIEWS: u32 = 1 << 21;
const PORTABLE_VIEWS: u32 = 1_000_000;

pub fn run() -> Result<()> {
    // SAFETY: plain DXGI enumeration.
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }.context("no DXGI factory")?;
    let mut hardware = 0;
    for index in 0.. {
        // SAFETY: as above; the end of the list is an error.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        // SAFETY: a plain query of a live adapter.
        let desc = unsafe { adapter.GetDesc1() }?;
        let len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..len])
            .trim()
            .to_owned();
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            println!("adapter {index}: {name} (software adapter, skipped)");
            continue;
        }
        hardware += 1;
        println!();
        println!(
            "adapter {index}: {name}{}",
            if index == 0 {
                "   <-- the layer uses this one (the system's default adapter)"
            } else {
                ""
            }
        );
        println!(
            "  vendor id: {:#06x}, device id: {:#06x}",
            desc.VendorId, desc.DeviceId
        );
        println!(
            "  dedicated video memory: {} MiB",
            desc.DedicatedVideoMemory >> 20
        );
        // SAFETY: as above.
        let driver = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }.map_or_else(
            |e| format!("unknown ({e})"),
            |v| {
                format!(
                    "{}.{}.{}.{}",
                    (v >> 48) & 0xffff,
                    (v >> 32) & 0xffff,
                    (v >> 16) & 0xffff,
                    v & 0xffff
                )
            },
        );
        println!("  driver version: {driver}");
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: plain device creation on a live adapter.
        match unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device) } {
            Ok(()) => match device {
                Some(device) => facts(&device),
                None => println!("  D3D12 device: none returned"),
            },
            Err(e) => println!(
                "  D3D12 device: REFUSED ({:#010x} {})",
                e.code().0 as u32,
                e.message()
            ),
        }
    }
    if hardware == 0 {
        println!("no hardware adapter found");
    }
    Ok(())
}

fn feature<T>(device: &ID3D12Device, which: D3D12_FEATURE, data: &mut T) -> bool {
    // SAFETY: a plain query that fills a structure of the asked size.
    unsafe { device.CheckFeatureSupport(which, (data as *mut T).cast(), size_of::<T>() as u32) }
        .is_ok()
}

fn facts(device: &ID3D12Device) {
    let asked = [
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_12_0,
        D3D_FEATURE_LEVEL_12_1,
        D3D_FEATURE_LEVEL_12_2,
    ];
    let mut levels = D3D12_FEATURE_DATA_FEATURE_LEVELS {
        NumFeatureLevels: asked.len() as u32,
        pFeatureLevelsRequested: asked.as_ptr(),
        MaxSupportedFeatureLevel: D3D_FEATURE_LEVEL_11_0,
    };
    if feature(device, D3D12_FEATURE_FEATURE_LEVELS, &mut levels) {
        let v = levels.MaxSupportedFeatureLevel.0;
        println!(
            "  max feature level: {}_{}",
            (v >> 12) & 0xf,
            (v >> 8) & 0xf
        );
    } else {
        println!("  max feature level: unknown (query refused)");
    }
    // A runtime that does not know the model asked for refuses the query,
    // so ask from 6.9 down (as the layer does).
    let mut model = None;
    for asked in (D3D_SHADER_MODEL_6_0.0..=D3D_SHADER_MODEL_6_9.0).rev() {
        let mut data = D3D12_FEATURE_DATA_SHADER_MODEL {
            HighestShaderModel: D3D_SHADER_MODEL(asked),
        };
        if feature(device, D3D12_FEATURE_SHADER_MODEL, &mut data) {
            model = Some(data.HighestShaderModel.0);
            break;
        }
    }
    match model {
        Some(m) => println!("  highest shader model: {}.{}", m >> 4, m & 0xf),
        None => println!("  highest shader model: unknown (query refused)"),
    }

    let mut o1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    if feature(device, D3D12_FEATURE_D3D12_OPTIONS1, &mut o1) {
        println!(
            "  wave lane count: min {}, max {}",
            o1.WaveLaneCountMin, o1.WaveLaneCountMax
        );
        let ok = |n: u32| {
            if o1.WaveLaneCountMin <= n && n <= o1.WaveLaneCountMax {
                "OK"
            } else {
                "NOT OK"
            }
        };
        println!(
            "  translated shaders need a 32- or 64-lane wave: 32 {}, 64 {}",
            ok(32),
            ok(64)
        );
    } else {
        println!("  wave lane count: unknown (OPTIONS1 refused)");
    }

    let mut o7 = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
    if feature(device, D3D12_FEATURE_D3D12_OPTIONS7, &mut o7) {
        // The enumeration counts tiers in tens (D3D12_MESH_SHADER_TIER_1 is 10).
        let mesh = o7.MeshShaderTier.0;
        if mesh == 0 {
            println!("  mesh shader tier: 0 (not supported)");
        } else {
            println!(
                "  mesh shader tier: {}.{} (raw value {mesh})",
                mesh / 10,
                mesh % 10
            );
        }
    } else {
        println!("  mesh shader tier: unknown (OPTIONS7 refused)");
    }

    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    if feature(device, D3D12_FEATURE_D3D12_OPTIONS, &mut o) {
        println!("  resource binding tier: {}", o.ResourceBindingTier.0);
        println!("  tiled resources tier: {}", tier(o.TiledResourcesTier.0));
        println!("  resource heap tier: {}", o.ResourceHeapTier.0);
        println!(
            "  conservative rasterisation tier: {}",
            tier(o.ConservativeRasterizationTier.0)
        );
    } else {
        println!("  binding/tiled/heap/conservative tiers: unknown (OPTIONS refused)");
    }

    let mut o16 = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
    if feature(device, D3D12_FEATURE_D3D12_OPTIONS16, &mut o16) {
        println!(
            "  GPU upload heap supported: {} (needs Resizable BAR enabled in the BIOS and used by the driver)",
            yes(o16.GPUUploadHeapSupported.as_bool())
        );
    } else {
        println!(
            "  GPU upload heap supported: unknown (OPTIONS16 refused: the runtime is too old)"
        );
    }

    let mut arch = D3D12_FEATURE_DATA_ARCHITECTURE1::default();
    if feature(device, D3D12_FEATURE_ARCHITECTURE1, &mut arch) {
        println!(
            "  unified memory (UMA): {}, cache-coherent UMA: {}",
            yes(arch.UMA.as_bool()),
            yes(arch.CacheCoherentUMA.as_bool())
        );
    } else {
        println!("  unified memory: unknown (ARCHITECTURE1 refused)");
    }

    // The layer's own test (descriptor/host.rs `probe_view_limit`): one
    // heap of the uncapped size, released at once.
    // SAFETY: plain heap creation; the heap is dropped at the end of the statement.
    let fits = unsafe {
        device.CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: UNCAPPED_VIEWS,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })
    }
    .is_ok();
    println!(
        "  shader-visible view heap of {UNCAPPED_VIEWS} views: {} (the layer would use {} views); the true maximum is not queried",
        if fits { "ACCEPTED" } else { "REFUSED" },
        if fits { UNCAPPED_VIEWS } else { PORTABLE_VIEWS }
    );

    let mut existing = D3D12_FEATURE_DATA_EXISTING_HEAPS::default();
    if feature(device, D3D12_FEATURE_EXISTING_HEAPS, &mut existing) {
        println!(
            "  existing-address heaps reported supported: {}",
            yes(existing.Supported.as_bool())
        );
    } else {
        println!("  existing-address heaps reported supported: unknown (query refused)");
    }
    println!(
        "  interfaces: ID3D12Device3 {}, ID3D12Device13 {}",
        yes(device.cast::<ID3D12Device3>().is_ok()),
        yes(device.cast::<ID3D12Device13>().is_ok())
    );
    in_place(device, true);
    in_place(device, false);
}

fn tier(v: i32) -> String {
    if v == 0 {
        "0 (not supported)".to_owned()
    } else {
        v.to_string()
    }
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn hr<T>(r: &windows::core::Result<T>) -> String {
    match r {
        Ok(_) => "ACCEPTED".to_owned(),
        Err(e) => format!("REFUSED ({:#010x} {})", e.code().0 as u32, e.message()),
    }
}

/// Opens heaps on 1 MiB of committed private memory (write-watched, as the
/// title's memory is made, or plain), places a 64 KiB buffer in each the
/// way the layer does (inplace.rs `place`), and releases it all.
fn in_place(device: &ID3D12Device, watched: bool) {
    const SIZE: usize = 16 * K64;
    let kind = if watched {
        "write-watched memory, as title memory is made"
    } else {
        "plain memory"
    };
    let mut flags = MEM_RESERVE | MEM_COMMIT;
    if watched {
        flags |= MEM_WRITE_WATCH;
    }
    // SAFETY: a fresh allocation at an address of the system's choice
    // (always 64 KiB aligned).
    let base = unsafe { VirtualAlloc(None, SIZE, flags, PAGE_READWRITE) };
    if base.is_null() {
        println!("  existing-address heaps ({kind}): not tried, VirtualAlloc failed");
        return;
    }
    {
        // SAFETY: the address is the base of the allocation above.
        let whole: windows::core::Result<ID3D12Heap> = device
            .cast::<ID3D12Device3>()
            .and_then(|d| unsafe { d.OpenExistingHeapFromAddress(base as *const c_void) });
        println!(
            "  existing-address heaps, whole allocation, OpenExistingHeapFromAddress ({kind}): {}",
            hr(&whole)
        );
        if let Ok(heap) = &whole {
            println!(
                "    placed 64 KiB buffer in it: {}",
                hr(&place(device, heap))
            );
        }
        // SAFETY: 4 committed pages from the 4th on, inside the allocation.
        let part: windows::core::Result<ID3D12Heap> =
            device.cast::<ID3D12Device13>().and_then(|d| unsafe {
                d.OpenExistingHeapFromAddress1(
                    base.cast::<u8>().add(4 * K64) as *const c_void,
                    4 * K64,
                )
            });
        println!(
            "  existing-address heaps, part of an allocation, OpenExistingHeapFromAddress1 ({kind}): {}",
            hr(&part)
        );
        if let Ok(heap) = &part {
            println!(
                "    placed 64 KiB buffer in it: {}",
                hr(&place(device, heap))
            );
        }
        // Heaps and buffers are released here, before the memory.
    }
    // SAFETY: the allocation above, released whole; nothing refers to it now.
    let _ = unsafe { VirtualFree(base, 0, MEM_RELEASE) };
}

fn place(device: &ID3D12Device, heap: &ID3D12Heap) -> windows::core::Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: K64 as u64,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        // The heap is cross-adapter shared, so the buffer must say so too.
        Flags: D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut placed: Option<ID3D12Resource> = None;
    // SAFETY: plain creation in a live heap.
    unsafe {
        device.CreatePlacedResource(
            heap,
            0,
            &desc,
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut placed,
        )?
    };
    placed.ok_or_else(windows::core::Error::empty)
}
