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
| `sdslot-core` | Library: manifest/layout model, drive-type registry, streaming write/read engine, on-card TOC, RTL export, raw device access, enumeration/hotplug/eject (Linux/Windows/macOS + file-backed) |
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
$ sdslot write  --device \\.\PhysicalDrive2 --manifest card.toml --verify --eject
$ sdslot status --device \\.\PhysicalDrive2 --manifest card.toml
$ sdslot read   --device \\.\PhysicalDrive2 --manifest card.toml \
                --slot rp:7 -o 211bsd.dsk --length canonical     # attach in a simulator
$ sdslot read   --device \\.\PhysicalDrive2 --manifest card.toml \
                --slot rl:0 --slot rp:7 --out-dir extracted\     # batch extract
$ sdslot wipe   --device \\.\PhysicalDrive2 --manifest card.toml --slot rl:3
$ sdslot verify --device \\.\PhysicalDrive2 --manifest card.toml
$ sdslot eject  --device \\.\PhysicalDrive2                      # pull the card safely
$ sdslot export-rtl --manifest card.toml -o card_layout.vh       # or .sv/.rs/.h
```

On Linux/macOS the device is `/dev/sdX` / `/dev/rdiskN` and the commands need
`sudo` — on macOS, **also Full Disk Access** (see
[macOS: Full Disk Access](#macos-full-disk-access) below; `sudo` alone is not
enough). Any command also accepts a **regular file** as `--device` (relaxed
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

## Drive types

A bank's `drive_type` supplies the canonical image size used for validation,
`--length canonical` extraction, and slot-size suggestions. Built in:

- **Disk** — RX01, RX02, RX50, RX33, RX23, RX26 (floppies); RL01, RL02;
  RK05; RP04, RP05, RP06, RP07; RM03, RM05.
- **Magtape** (stream media — variable-length `.tap` containers, no geometry
  and no canonical size) — TU16, TU45.

Names are case-insensitive. Anything else — or an override of a builtin — goes
in the manifest, and the exact canonical byte counts are tabulated in
[the design doc §2.2](docs/sdslot-design.md#22-drive-type-registry):

```toml
[drive_types.RK07]             # a disk type: full geometry
cylinders = 815
heads = 3
sectors = 22
bytes_per_sector = 512
recommended_slot = "32MiB"

[drive_types.TK50]             # a stream type: geometry is rejected
stream = true
recommended_slot = "128MiB"
```

`drive_type` is optional — a bank without one accepts any image up to
`slot_size`, which is the simplest way to carry tapes or unrecognized media.

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
holds, so no conversion happens in either direction. Magtape slots hold
SimH `.tap` containers verbatim (stream drive types, e.g. TU45: any length
up to the slot; extraction defaults to the TOC-recorded length).
Extraction lengths:

- `--length canonical` (default with a fixed-size `drive_type`; refused
  for stream types) — the registry's exact canonical byte count, directly
  attachable.
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
  (bank-master and All/None/Needed controls honor "Hide empty slots"),
  status pills (matches / written / modified / differs / wiped,
  live-updated from operation results), and per-slot Write/Extract/Wipe.
  Loading a manifest auto-ticks every slot whose image exists on disk; a
  filesystem watcher enables/disables Write buttons as image files appear
  or vanish. **Needed** ticks only the slots the last status scan did not
  report as matching — a completed Refresh status does it automatically
  (switchable off in Settings), so Write Selected writes just the slots
  the card is missing.
- **Batch actions** — Write Selected, Extract Selected (into a folder, via
  `read --out-dir`), and Wipe Selected, each behind a plan-preview
  confirmation; missing images are skipped only after an explicit warning.
  A successful Write Selected ejects the card (via the write invocation's
  `--eject`, so no second elevation prompt) — switchable off in Settings.
  **Export flat image…** assembles the dd/Etcher file, and a **Cancel**
  button terminates a running operation.
- **Devices** — the list refreshes automatically on hotplug, hides empty
  card readers, and grays out non-removable disks; the system/boot disk is
  never selectable. A regular file can be targeted instead ("File
  image…"). An **Eject** button (enabled while a device is selected) ejects
  the card on demand. Slot statuses stay visible after an eject as the last
  known state, and are cleared when a different target is selected so a
  fresh card is never shown with the previous card's statuses.
- **Settings** (persisted to `~/.sdslot`) — Show all devices, Advanced
  (warning-gated — an internal disk write can destroy the machine's OS),
  Select first removable disk at startup (warning-gated), Verify after
  write (default on), Eject disk after writing (default on; ejects the
  card after a successful Write Selected), Select only what needs writing
  (default on; after a status refresh), Hide empty slots, Hide log,
  and Developer mode
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
stdout pipes cannot cross the Windows UAC boundary. On macOS, elevation alone
is not enough — see the next section.

## macOS: Full Disk Access

macOS gates the raw disk devices sdslot writes to (`/dev/rdiskN`) behind
**Full Disk Access**, and **root does not bypass it**. So an operation can
fail even after you authenticate at the elevation prompt:

```console
$ sdslot write --device /dev/rdisk4 --manifest card.toml
error: cannot open /dev/rdisk4: Operation not permitted (os error 1)
```

The errno tells you which of the two denials you hit:

| Error | errno | Meaning | Fix |
|---|---|---|---|
| `Permission denied` | 13 (EACCES) | Not elevated — the device node is `root:operator 0640` | Re-run with `sudo` |
| `Operation not permitted` | 1 (EPERM) | Elevated, but TCC denied it | Grant Full Disk Access |

To grant it: **System Settings → Privacy & Security → Full Disk Access**, add
the app that launches sdslot, then **quit and relaunch that app** — TCC
changes do not apply to already-running processes.

*Which* app to add depends on how you run sdslot:

- **CLI** — the terminal you type `sudo sdslot …` into (Terminal, iTerm2, …).
- **GUI** — `sdslot-gui.app`. A bare `sdslot-gui` binary has no bundle
  identifier and no code signature, so TCC has nothing to attach a grant to
  and falls back to the launching terminal; build the bundle below and run
  that instead.

Quick check that a grant took effect, without writing anything:

```console
$ sudo dd if=/dev/rdisk4 of=/dev/null bs=1m count=1
```

### Building the .app bundle

```console
$ cargo build --workspace --release
$ sdslot-gui/macos/bundle.sh          # -> target/release/sdslot-gui.app
```

The bundle carries both binaries (the GUI finds the CLI as a sibling inside
`Contents/MacOS`), so it is self-contained. Release archives for macOS ship it
alongside the bare binaries.

By default it is **ad-hoc signed**, which is enough for TCC to hold a grant but
changes on every rebuild — after rebuilding, remove and re-add the app in Full
Disk Access. Set `CODESIGN_IDENTITY` to a Developer ID Application certificate
for a signature (and therefore a grant) that survives rebuilds:

```console
$ CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
    sdslot-gui/macos/bundle.sh
```

The icon is checked in; `sdslot-gui/macos/make-icon.py` (Pillow + `iconutil`)
regenerates it if the artwork changes.

## Building and testing

```console
$ cargo build --workspace --release
$ cargo test  --workspace
```

Minimum supported Rust version: **1.88** (set by the GUI's eframe/image
dependency stack). It is declared as `rust-version` in the workspace
`Cargo.toml`, so cargo refuses an older toolchain with a clear error rather
than failing somewhere in the dependency tree.

Tests run entirely against file-backed devices; no hardware or elevation
needed. Coverage, if you want it:

```console
$ cargo install cargo-llvm-cov --locked && rustup component add llvm-tools-preview
$ cargo llvm-cov --workspace --summary-only
```

The core library sits near 90–100% per module; the workspace total is much
lower because the egui view code in `sdslot-gui` and the platform device
layers are not reachable without a display or real hardware.

The device layers and elevation paths are `cfg`-gated, so a host-only
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
