# sdslot

A cross-platform utility for writing vintage disk images to SD cards at fixed
LBA offsets, so an FPGA RTL model can present each region as an independent
emulated disk drive (RL02, RP06, …). It writes *only* the bytes belonging to
the selected images — never a full-card image — and extracts slot contents
back into files for round-trip interchange with software simulators.

The full design is in [docs/sdslot-design.md](docs/sdslot-design.md).

## Workspace

| Crate | What it is |
|---|---|
| `sdslot-core` | Library: manifest/layout model, drive-type registry, streaming write/read engine, on-card TOC, RTL export, raw device access (Linux/Windows/macOS + file-backed) |
| `sdslot` | CLI over the core; also the privileged backend for the GUI |
| `sdslot-gui` | egui frontend; performs no raw I/O itself — it drives the CLI as a (per-operation elevated) subprocess over a versioned JSON event stream |

## Quick start

Describe the card once in a TOML manifest (see
[examples/pdp11-card.toml](examples/pdp11-card.toml)):

```toml
sector_size = 512
toc = "512MiB"                # optional on-card table of contents

[[bank]]
name       = "rl"
base       = "0"
slot_size  = "16MiB"
units      = 16
drive_type = "RL02"

  [[bank.slot]]
  unit  = 0
  name  = "xxdp-rl02"
  image = "images/xxdp25.rl02"
```

Then:

```console
$ sdslot list                                   # find the card (size, model, removable)
$ sdslot write  --device \\.\PhysicalDrive2 --manifest card.toml --verify
$ sdslot status --device \\.\PhysicalDrive2 --manifest card.toml
$ sdslot read   --device \\.\PhysicalDrive2 --manifest card.toml \
                --slot rp:7 -o 211bsd.dsk --length canonical     # attach in a simulator
$ sdslot read   --device \\.\PhysicalDrive2 --manifest card.toml \
                --slot rl:0 --slot rp:7 --out-dir extracted\     # batch extract
$ sdslot wipe   --device \\.\PhysicalDrive2 --manifest card.toml --slot rl:3
$ sdslot verify --device \\.\PhysicalDrive2 --manifest card.toml
$ sdslot export-rtl --manifest card.toml -o card_layout.vh       # or .sv/.rs/.h
```

On Linux/macOS the device is `/dev/sdX` / `/dev/rdiskN` and the commands need
`sudo`. Any command also accepts a **regular file** as `--device` (relaxed
alignment, grows on demand) — used by the tests and useful for building card
images offline.

### Flat images for dd / balenaEtcher

`sdslot image` assembles the entire card layout into one flat file that any
raw-image writer can flash in a single pass:

```console
$ sdslot image --manifest card.toml -o card.img
$ dd if=card.img of=/dev/sdX bs=4M conv=fsync     # or balenaEtcher, etc.
```

The GUI's **Export flat image…** button does the same thing.

## Safety rails

This is a raw-device writer and is deliberately not a `dd` footgun:

- Non-removable devices require `--force`; the system/boot disk is refused
  even with `--force`.
- Destructive commands print the full plan (device, model, exact byte ranges)
  and require typed confirmation unless `--yes`.
- Exclusive access during writes: `O_EXCL` on Linux, volume lock + dismount on
  Windows, `diskutil unmountDisk` on macOS; reads take a shared lock.
- Bounds, overlap, and sector-size validation before any byte is written.
- Exit codes: 0 success, 1 usage/validation, 2 device access, 3 verify
  mismatch.

## Simulator interchange

Simulator disk images are flat C×H×S byte streams — exactly what a slot
holds, so no conversion happens in either direction. Extraction lengths:

- `--length canonical` (default with a `drive_type`) — the registry's exact
  canonical byte count, directly attachable.
- `--length toc` — the byte length recorded at write time (needs the optional
  on-card TOC; best for images with nonstandard sizes).
- `--length slot` — the full slot extent, for forensics.
- `--length <bytes>` — explicit.

`sdslot status` re-hashes slots against the TOC and flags slots the FPGA has
written since the last host write as **modified** — the cue to extract before
overwriting.

## GUI

`sdslot-gui` is an egui app over the same core. Highlights:

- **Card map** — the whole card drawn to scale: banks at their offsets,
  per-slot occupancy cells, the TOC tick, and a live op-colored highlight on
  the slot currently transferring. With no card inserted, a generic 8 GB
  reference shows what size card the layout needs; overflowing a real card
  paints the excess red.
- **Slot table** — spreadsheet-style rows with selection checkboxes
  (bank-master and All/None controls honor "Hide empty slots"), status
  pills (matches / written / modified / differs / wiped, live-updated from
  operation results), and per-slot Write/Extract/Wipe. Loading a manifest
  auto-ticks every slot whose image exists on disk; a filesystem watcher
  enables/disables Write buttons as image files appear or vanish.
- **Batch actions** — Write Selected, Extract Selected (into a folder, via
  `read --out-dir`), and Wipe Selected, each behind a plan-preview
  confirmation; missing images are skipped only after an explicit warning.
  **Export flat image…** assembles the dd/Etcher file, and a **Cancel**
  button terminates a running operation.
- **Devices** — the list refreshes automatically on hotplug, hides empty
  card readers, and grays out non-removable disks; the system/boot disk is
  never selectable. A regular file can be targeted instead ("File
  image…").
- **Settings** (persisted to `~/.sdslot`) — Show all devices, Advanced
  (warning-gated — an internal disk write can destroy the machine's OS),
  Select first removable disk at startup (warning-gated), Verify after
  write (default on), Hide empty slots, Hide log, and Developer mode
  (shows the equivalent CLI command for every operation), plus a
  reset-to-safe-defaults button. Window size and the log pane height are
  persisted too.
- **Log** — an 80's phosphor CRT (the bundled VT323 DEC-terminal font in
  P1 green), drag-resizable; the GUI logs its version/copyright sign-on at
  startup, and the version (with git hash) is also in the window title bar.
- **Command line** — `sdslot-gui card.toml` (or `--manifest`) preloads a
  layout; `--theme pdp` switches to a PDP-11/70 front-panel look (red LED
  buttons, magenta accents, white bezel; blue theme is the default).

Elevation is requested per-operation (UAC / Polkit / macOS authorization),
with events streamed back over a localhost socket (`--json-port`) because
stdout pipes cannot cross the Windows UAC boundary.

## Building and testing

```console
$ cargo build --workspace --release
$ cargo test  --workspace
```

Minimum supported Rust version: **1.88** (set by the GUI's eframe/image
dependency stack and verified by toolchain check).

Tests run entirely against file-backed devices; no hardware or elevation
needed. The device layers and elevation paths are `cfg`-gated, so a host-only
clippy run sees just one platform's code; add the other targets and clippy
them from any host:

```console
$ rustup target add x86_64-unknown-linux-gnu aarch64-apple-darwin \
                    x86_64-pc-windows-msvc
$ cargo clippy --workspace --all-targets --target aarch64-apple-darwin -- -D warnings
```

A pre-commit gate (fmt + clippy on the host and on every installed cross
target + tests) is provided:

```console
$ git config core.hooksPath .githooks
```

CI (`.github/workflows/release.yml`) runs the same gate on every push and
builds release archives for Linux, macOS (x86_64 + aarch64), and Windows;
publishing a GitHub release attaches them as assets.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option. The GUI bundles the [VT323](https://fonts.google.com/specimen/VT323)
terminal font (SIL Open Font License 1.1; see
`sdslot-gui/assets/fonts/OFL.txt`).
