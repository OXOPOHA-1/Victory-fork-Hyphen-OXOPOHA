# Victory-fork-Hyphen-OXOPOHA

A fork of [Hyphen's VIctory fork](https://github.com/fxceless/victory-fork),
which is a fork of [VIctory](https://github.com/Marvisak/VIctory) by Marvisak.
VIctory runs an Xbox Series title natively on Windows through Direct3D 12,
from files the user supplies.

**This repository contains no game files, code or assets.** You need your own
copy of the game. The project is not affiliated with or endorsed by the game's
developer or publisher.

## Goal

The console's 30 frames per second with a correct picture on an Intel Arc
B580, by handing the title's memory and draw calls to the PC's card as
directly as the driver allows.

## Status (2026-11-27)

- Builds in release; `clippy` is clean with warnings as errors.
- 462 tests pass in the workspace and 241 in `victory-gpu`. The GPU tests ran
  on an Intel Arc B580 (driver 32.0.101.9150).
- **Not yet run in the game.** Nothing below is a measured frame-rate or
  picture claim for the title itself.

## What this fork changes

- **Title memory in place.** Where the driver accepts it, the card reads the
  title's own memory instead of a copy (`VICTORY_INPLACE`, default `2`). A
  self-test at start-up falls back to copies if the driver refuses.
- **Mesh draws through an amplification stage.** Indirect mesh draws are
  launched by a generated amplification shader instead of a compute pass in
  front of every draw (`VICTORY_MESH_AMPLIFY`, default on). On the B580 the
  output is byte-identical in 11 GPU tests, and GPU time per draw fell from
  6.0 µs to 0.6 µs.
- **Wave size from the card's real range.** Translated shaders are pinned to
  a lane count the card supports (the B580 reports 16 to 32; the console
  uses 64). A defect where pixel quad reads returned zero at triangle edges
  on the B580 is fixed and covered by a test on the card.
- **Exact scheduling of parallel work.** Steps of a submission run on worker
  threads in groups cut exactly where one step reads what another writes.
- **Caches kept longer, textures reused by address** (`VICTORY_IDLE_SCALE`,
  `VICTORY_REUSE`, bounded by `VICTORY_REUSE_MB`).
- **Lower internal resolution** with viewports and scissors scaled to match
  (`VICTORY_RES=2k|1080p|720p`).
- **Every run names what did not work.** 130 named causes are counted during
  a run; `victory report` turns a log into a short text report that starts
  with what failed and why.
- **Capture for the pipelines not implemented yet** (`VICTORY_RAW_SAVE`) and
  `victory raw` to read the capture.
- **`victory device`** prints what each adapter's Direct3D 12 device supports.

## Build and run

Windows 11 and the Rust toolchain named in `rust-toolchain.toml`.

```powershell
cargo build --release -p victory
.\run-improved.ps1 -GameExe '<path to the title exe>'
```

With no other option the script runs the code's defaults at 1920x1080
internal resolution. The log and a report of it are written to
`%LOCALAPPDATA%\victory\runs\`. Useful options:

| Option | Meaning |
|---|---|
| `-Res 2k \| 1080p \| 720p` | Internal render size. |
| `-FrameLog` | Frame times every five seconds. |
| `-RawSave <dir>` | Save the blobs of pipeline calls not implemented. The directory must be outside the source tree. |
| `-InPlace 0 \| 1 \| 2` | Title memory in place: off, windows only, windows and buffers. |
| `-MeshAmplify 0 \| 1` | `0` restores the compute pass in front of indirect mesh draws. |
| `-Console` | Show the log in the window and keep no files. |

`collect-ab.ps1` runs two variants one after another and compares their
frame logs. Every switch can also be set as an environment variable.

## Known limits

- Two pipeline-creation calls of the title (methods `0x1c8` and `0x218`)
  are not implemented and return `E_NOTIMPL`. The capture above exists to
  write them from real data.
- A texture that a shader picks by index can stay black until its content
  arrives.
- Ray-traced light and shadows are not implemented, so shadowed areas are
  dark.
- With a lower internal resolution only the targets, viewports and scissors
  are scaled; the title's shaders still compute for 2560x1440.
- In-place memory still copies buffers above 1 MiB and does not cover
  memory calls the title makes directly into `ntdll`.
- Checked on one card only.

## Credits

- [VIctory](https://github.com/Marvisak/VIctory) by Marvisak and its
  contributors is the base of everything here.
- [Hyphen's fork](https://github.com/fxceless/victory-fork) is the tree this
  fork started from; its history is kept below the single commit that
  carries this fork's changes.
- Hyphen's fork in turn carries work credited in its own README:
  [XCL](https://github.com/ivall/xcl) by ivall (GPL-2.0-only, used as a
  reference) and [victory-amd](https://github.com/myokex/victory-amd) by
  myokex, among others.
- This fork's changes were developed with AI coding tools.

## Licence

GPL-3.0-or-later, the same as the upstream projects. See `LICENSE`.
