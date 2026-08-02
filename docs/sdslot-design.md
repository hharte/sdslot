# sdslot — Design Document

**A cross-platform utility for writing vintage disk images to SD cards at fixed LBA offsets**

---

## 1. Overview

`sdslot` writes one or more disk images to a raw SD card (or any block device) at
predetermined LBA offsets, so that an FPGA RTL model can present each region as an
independent emulated disk drive. It writes *only* the bytes belonging to the
selected images — never a full-card image — so updating two drives touches a few
hundred megabytes, not the whole card. It also extracts images from the card back
into files, enabling round-trip interchange with software simulators running on
the host.

### Goals

- Write individual disk images to arbitrary, sector-aligned LBA offsets on a raw device.
- Support heterogeneous drive types on one card (e.g., four 10 MiB RL02s alongside
  eight 168 MiB RP06s) with hardware-friendly address math.
- Extract slot contents back into simulator-compatible image files.
- Cross-platform: Linux, Windows, macOS, from a single Rust codebase.
- CLI-first and scriptable; a GUI frontend layered on the same core.
- Declarative card layout shared between software and RTL.
- Strong safety rails: this is a raw-device writer and must not become a `dd` footgun.

### Non-goals

- Partition table awareness (MBR/GPT). The card is a flat block space owned by the FPGA design.
- Filesystem-level operations. Images are opaque byte streams.

---

## 2. Card Layout Model

### 2.1 Banks and slots

The card is divided into **banks**. Each bank is a contiguous region holding a
uniform array of fixed-size **slots**, one per emulated drive unit of that bank's
type. Different banks have different slot sizes, so small and large drive types
coexist without wasting space.

The shipped [examples/pdp11-card.toml](../examples/pdp11-card.toml) lays out an
8 GiB card this way (banks are listed here in address order; the manifest may
declare them in any order):

```
LBA 0x000000  ┌────────────────────────────────┐
    (0)       │ Bank "rl"  — 256 MiB           │  RL02 images (10 MiB each)
              │   16 × 16 MiB slots            │
              │   slot n @ n × 16 MiB          │
LBA 0x080000  ├────────────────────────────────┤
   (256 MiB)  │ Bank "rx"  — 64 MiB            │  RX01/02/50/33/23/26 floppies
              │   16 × 4 MiB slots             │
LBA 0x0a0000  ├────────────────────────────────┤
   (320 MiB)  │ (gap — the TOC sits @ 512 MiB) │  §2.6, 128 KiB
LBA 0x400000  ├────────────────────────────────┤
   (2 GiB)    │ Bank "rp"  — 2 GiB @ 2 GiB     │  RP06 images (166.3 MiB each)
              │   8 × 256 MiB slots            │
LBA 0x800000  ├────────────────────────────────┤
   (4 GiB)    │ Bank "tm"  — 4 GiB @ 4 GiB     │  .tap magtape containers
              │   8 × 512 MiB slots            │  (stream type — variable length)
LBA 0x1000000 └────────────────────────────────┘
   (8 GiB)
```

Rules:

- Slot sizes are powers of two, and each bank's base address is aligned to
  the bank's power-of-two span (`units × slot_size`, rounded up to a power of
  two) — a bank spanning 2 GiB must sit on a 2 GiB boundary. Both are
  enforced by default (`allow_unaligned = true` to override). This keeps the
  FPGA address math to pure concatenation:
  `lba = BANK_BASE | (unit << SLOT_SHIFT) | block_offset`, with no adder.
- Banks must not overlap; slots are wholly contained in their bank.
- A bank may reserve more slots than drives currently attached (the `rl` bank
  above reserves 16 × 16 MiB slots but the controller may only implement units
  0–3). Spare slots cost nothing until written.

### 2.2 Drive type registry

A built-in table maps drive types to canonical image sizes and recommended slot
sizes, used for validation, `--length canonical` extraction (§4), and sizing
suggestions:

| Type | Geometry | Canonical image size | Recommended slot |
|---|---|---|---|
| RX01 | 77c × 1h × 26s × 128B | 256,256 B | 512 KiB |
| RX02 | 77c × 1h × 26s × 256B | 512,512 B | 1 MiB |
| RX50 | 80c × 1h × 10s × 512B | 409,600 B | 512 KiB |
| RX33 | 80c × 2h × 15s × 512B | 1,228,800 B | 2 MiB |
| RX23 | 80c × 2h × 18s × 512B | 1,474,560 B | 2 MiB |
| RX26 | 80c × 2h × 36s × 512B | 2,949,120 B | 4 MiB |
| RL01 | 256c × 2h × 40s × 256B | 5,242,880 B (5 MiB) | 8 MiB |
| RL02 | 512c × 2h × 40s × 256B | 10,485,760 B (10 MiB) | 16 MiB |
| RK05 | 203c × 2h × 12s × 512B | 2,494,464 B | 4 MiB |
| RP04/05 | 411c × 19h × 22s × 512B | 87,960,576 B (83.9 MiB) | 128 MiB |
| RP06 | 815c × 19h × 22s × 512B | 174,423,040 B (166.3 MiB) | 256 MiB |
| RM03 | 823c × 5h × 32s × 512B | 67,420,160 B (64.3 MiB) | 128 MiB |
| RM05 | 823c × 19h × 32s × 512B | 256,196,608 B (244.3 MiB) | 512 MiB |
| RP07 | 630c × 32h × 50s × 512B | 516,096,000 B (492.2 MiB) | 1 GiB |
| TU16 | *stream* — no geometry | *variable* (46,080,000 B nominal) | 64 MiB |
| TU45 | *stream* — no geometry | *variable* (46,080,000 B nominal) | 64 MiB |

Sizes are exact: the byte counts above are what the registry carries and what
`--length canonical` extracts. Beware the unit when comparing against a
drive's advertised capacity — those figures are decimal MB, so the RP06's
174,423,040 B reads as "174 MB" but only 166.3 MiB, and a 256 MiB slot holds
it with room to spare.

RX01/RX02 physical media record track 0 in single density by interchange
convention; flat images are uniform streams including track 0, which is the
canonical size above. Mixed-density RX02 images (509,184 B) exist in
the wild — the non-strict size check warns but still writes them. Note the
later floppies outgrow a 1 MiB slot: RX33 (1.2 MB), RX23 (1.44 MB), and
RX26 (2.88 MB) need 2–4 MiB slots.

Magtape banks carry `.tap` images, which are variable-length — no
canonical size. The registry models them as **stream** drive types
(`stream: true`; builtin TU16/TU45): no C/H/S geometry, an `image_bytes`
that is only the NOMINAL media capacity (display and sizing, never
validation), no canonical-size write warning (any image that fits the slot
is valid), and `--length canonical` refused — extraction defaults to the
byte length recorded in the TOC (`--length toc`), falling back to the full
slot. Manifests may add stream types via `[drive_types]` with
`stream = true` plus `image_size` and/or `recommended_slot` (geometry is
rejected). Slot sizing for the period drives: a 2400 ft reel at 1600 BPI
(the TM02/TM03 ceiling — 6250 BPI GCR needed the TM78) holds at most
46,080,000 bytes (the TU16/TU45 nominal), inter-record gaps only lower
that, and `.tap` container overhead (8 B/record) is far smaller than the
gaps it replaces — though a gapless `.tap` may legally exceed the nominal,
which is why only the slot bounds it; a TK50 CompacTape holds ~94.5 MB and
a TK70 CompacTape II ~296 MB. 512 MiB slots therefore cover every period
tape with power-of-two headroom (64 MiB suffices for 9-track-only
layouts).

The registry is extensible via `[drive_types]` entries in the manifest for
anything not built in. (Sizes above are indicative; the built-in registry
carries the exact canonical byte count per type, including bad-block-track
conventions where applicable.)

### 2.3 Manifest file (TOML)

```toml
# pdp11-card.toml (abridged; the full four-bank layout is in examples/)
sector_size = 512
toc         = "512MiB"     # optional on-card table of contents (§2.6)

[[bank]]
name       = "rl"
base       = "0"           # bytes, KiB/MiB/GiB suffixes, or "NNNs" for LBAs
slot_size  = "16MiB"
units      = 16            # capacity of the bank; controller may use fewer
drive_type = "RL02"        # default type for validation and --length canonical

  [[bank.slot]]
  unit  = 0
  name  = "xxdp-rl02"
  image = "images/xxdp25.rl02"

  [[bank.slot]]
  unit  = 1
  name  = "rt11-work"
  image = "images/rt11work.rl02"

[[bank]]
name       = "rp"
base       = "2GiB"        # aligned to the bank's 2 GiB span
slot_size  = "256MiB"
units      = 8
drive_type = "RP06"

  [[bank.slot]]
  unit  = 0
  name  = "rsx11mplus-v4.6"
  image = "images/rsx11mp46.dsk"

  [[bank.slot]]
  unit  = 7
  name  = "211bsd"
  image = "images/211bsd_rp06.dsk"

# A magtape bank. Omitting drive_type leaves the slots untyped (any image up
# to slot_size is legal); naming a stream type instead — drive_type = "TU45" —
# additionally labels the bank in the UI and RTL comments.
[[bank]]
name       = "tm"
base       = "4GiB"        # aligned to the bank's 8 × 512 MiB = 4 GiB span
slot_size  = "512MiB"
units      = 8

# Registry extensions: anything not built in, or an override of a builtin.
[drive_types.RK07]         # a disk type — full geometry
cylinders        = 815
heads            = 3
sectors          = 22
bytes_per_sector = 512
recommended_slot = "32MiB"

[drive_types.TK50]         # a stream type — no geometry permitted
stream           = true
image_size       = "94500000"  # nominal only; never validated against an image
recommended_slot = "128MiB"
```

Slots are addressed as `bank:unit` on the command line (`--slot rl:1=foo.rl02`,
`--slot rp:7`). With a single bank the prefix may be omitted.

### 2.4 RTL export

`sdslot export-rtl --manifest examples/pdp11-card.toml -o card_layout.vh` emits
per-bank parameters — verbatim output for the shipped example:

```verilog
// Generated by sdslot export-rtl; do not edit.
// Sector size: 512 bytes.

// Bank "rl": RL02 x 16, 16 MiB slots @ base 0 B
localparam RL_BASE_LBA   = 32'h0000_0000;
localparam RL_SLOT_SHIFT = 15;
localparam RL_UNITS      = 5'd16;

// Bank "rx": RX02 x 16, 4 MiB slots @ base 256 MiB
localparam RX_BASE_LBA   = 32'h0008_0000;
localparam RX_SLOT_SHIFT = 13;
localparam RX_UNITS      = 5'd16;

// Bank "tm": 8 units, 512 MiB slots @ base 4 GiB
localparam TM_BASE_LBA   = 32'h0080_0000;
localparam TM_SLOT_SHIFT = 20;
localparam TM_UNITS      = 4'd8;

// Bank "rp": RP06 x 8, 256 MiB slots @ base 2 GiB
localparam RP_BASE_LBA   = 32'h0040_0000;
localparam RP_SLOT_SHIFT = 19;
localparam RP_UNITS      = 4'd8;
```

`SLOT_SHIFT` is `log2(slot_size / sector_size)`, so `lba = BASE_LBA | (unit <<
SLOT_SHIFT) | block_offset` needs no adder; `UNITS` is emitted at its minimum
width (`5'd16` — five bits hold 16). Banks appear in manifest order, not
address order. Only addressing constants are generated: which slots currently
hold an image is host-side state that changes on every write, so the RTL is
never regenerated for it.

Formats: Verilog header (`.vh`/`.v`), SystemVerilog package (`.sv`/`.svh`,
wrapped in `package <stem>; … endpackage`), Rust (`.rs`, `pub const`), C header
(`.h`, `#define` with an `<STEM>_H` include guard). The format follows the
output extension, unrecognized extensions default to `.vh`, and `--format
vh|sv|rs|h` overrides both. The package/guard stem is the output filename's
stem, sanitized to a legal identifier (a leading digit gets an `_` prefix).
`-o -` writes to stdout.

### 2.5 Image size conventions

Images are opaque; the only hard rule is `image_size <= slot_size`. When a bank
declares a `drive_type`, the tool warns (not errors) if an image doesn't match
the canonical size — catching truncated downloads while still permitting images
with metadata trailers. `--strict-size` upgrades the warning to an error.

### 2.6 Optional on-card table of contents (TOC)

Writing an image records only bytes; the card itself doesn't know how large the
*meaningful* content of a slot is, which matters for extraction (§4). An optional
TOC solves this:

- One reserved region of fixed size (128 KiB) at a byte offset declared in
  the manifest: `toc = "512MiB"` (e.g., in the gap between the RL and RP
  banks — the region must not overlap any bank).
- Contents: a small versioned structure (magic, layout hash, then per-slot
  records: bank, unit, image byte length, SHA-256, name, timestamp).
- Updated atomically after each successful write/wipe; ignored by the FPGA.
- Entirely optional: without a TOC, extraction falls back to drive-type canonical
  size or full slot length.

The TOC also lets `sdslot status --device X` show what's on a card with no
host-side records, and lets the GUI display card contents on insertion.

---

## 3. Command-Line Interface

```
sdslot list
    Enumerate candidate block devices (size, model, bus, removable flag).

sdslot status  --device <dev> [--manifest layout.toml]
    Show card contents from the TOC (or slot-by-slot hash probe with manifest).

sdslot write   --device <dev> --manifest layout.toml [--slot rl:1=rt11.rl02]...
               [--verify] [--yes] [--force] [--eject] [--json]
    Write the images named in the manifest (or per-slot overrides). Only named
    slots are touched. --eject ejects the device after a successful write
    (and verify), so the card can be pulled safely; an eject failure is a
    warning, not a command failure, and a file-backed target skips it with
    a note.

sdslot read    --device <dev> --manifest layout.toml --slot rp:7 -o out.dsk
               [--length <bytes|canonical|toc|slot>] [--json]
sdslot read    --device <dev> --manifest layout.toml --slot B:N... --out-dir DIR
    Extract one slot into a file, or any number of slots into a directory
    (files named <bank><unit>_<name>.img); see §4.

sdslot wipe    --device <dev> --manifest layout.toml --slot rl:3 [--yes]
    Zero a slot's full extent (present a "blank pack" to the FPGA).

sdslot verify  --device <dev> --manifest layout.toml [--slot B:N]... [--json]
    Re-read slots and compare against manifest images (SHA-256).

sdslot eject   --device <dev> [--json]
    Eject removable media so the card can be pulled safely (what the GUI's
    Eject button runs). Unlike write's best-effort --eject, a failure here
    is a command failure; a regular-file target is refused.

sdslot export-rtl --manifest layout.toml -o card_layout.vh [--format vh|sv|rs|h]

sdslot image   --manifest layout.toml -o card.img [--slot B:N[=IMG]]...
               [--verify] [--yes] [--json]
    Assemble the full card layout (all named images plus the TOC) into one
    flat image file that dd or balenaEtcher can write to a card in a single
    pass — the complement of per-slot writes, for when a full-card image
    *is* what you want.
```

Conventions:

- Destructive commands print a full plan (device, model, capacity, exact byte
  ranges per slot) and require interactive confirmation unless `--yes`.
- `--json` switches stdout to line-delimited JSON events (plan, progress,
  result) for consumption by the GUI (§8) and scripts.
- Exit codes: 0 success, 1 usage/validation, 2 device access, 3 verify mismatch.

---

## 4. Simulator Interchange (Extraction)

The FPGA and a host-side software simulator share media by moving flat
images through the card:

**PC → FPGA:** `sdslot write` an image produced/used by the simulator. These
images are flat C×H×S byte streams, exactly what the slot holds, so no
conversion is needed.

**FPGA → PC:** `sdslot read --slot rp:7 -o 211bsd.dsk --length canonical`
extracts the slot trimmed to the drive type's canonical size — directly
attachable in a simulator. Length resolution order:

1. `--length <bytes>` — explicit.
2. `--length toc` — byte length recorded at write time (requires TOC, §2.6).
   Best when the original image had a nonstandard size.
3. `--length canonical` (default when the bank has a `drive_type`) — registry size.
4. `--length slot` — the full slot extent, for forensics.

Notes:

- Extraction is read-only and skips the write-safety gauntlet (no confirmation
  needed), but still takes a shared/read lock so a concurrent write can't tear it.
- If the FPGA design modifies media (it will — that's the point), the TOC hash
  will no longer match; `sdslot status` flags such slots as *modified*, which is
  the cue to extract before overwriting. A slot whose probed content reads as
  all zeros is flagged *wiped* (a blank pack) rather than modified. The GUI
  surfaces both prominently.
- A convenience `sdslot sync` (future) could diff manifest images against card
  state by hash and report which side is newer — deferred until real usage
  patterns emerge.

---

## 5. Architecture

The project is a workspace with the logic in a library crate and two thin frontends:

```
sdslot-core (lib)
  ├── layout:      manifest parse, bank/slot resolution, overlap/bounds
  │                validation, slot references
  ├── drive_types: drive-type registry (canonical sizes, geometries)
  ├── rtl:         layout parameter export (vh/sv/rs/h)
  ├── engine:      chunked streaming copy, padding, verify, status,
  │                aligned buffers, progress events
  ├── toc:         on-card table of contents (§2.6)
  ├── events:      the versioned progress-event schema (CLI/GUI contract)
  ├── units:       size expression parsing ("16MiB", "2048s")
  ├── error:       error type; variants map to the CLI exit codes
  └── device:      RawDevice trait, enumeration, eject
        ├── file.rs     (regular-file target: growable, relaxed alignment)
        ├── hotplug.rs  (OS hotplug notifications for the GUI)
        ├── linux.rs    (/dev/sdX, ioctls)
        ├── windows.rs  (\\.\PhysicalDriveN)
        └── macos.rs    (/dev/rdiskN, diskutil)

sdslot     (bin) — CLI over sdslot-core; also the privileged backend for the GUI
  ├── main.rs      argument parsing, per-command dispatch, plan confirmation
  └── sink.rs      renders events as indicatif bars or line-delimited JSON

sdslot-gui (bin) — GUI frontend (§8); performs no raw I/O itself
  ├── main.rs      eframe bootstrap, CLI args, code-drawn window icon
  ├── app.rs       the egui application: card map, slot table, log
  ├── backend.rs   spawns the CLI (elevated when needed), parses its events
  ├── ops.rs       in-flight operation state and aggregate progress
  ├── devices.rs   device list, hotplug integration, safety filtering
  └── theme.rs     the single home of everything visual (§8.2)
```

### 5.1 The `RawDevice` trait

```rust
pub trait RawDevice {
    fn sector_size(&self) -> u32;      // logical sector size (verified vs manifest)
    fn capacity_bytes(&self) -> u64;   // current length for file-backed devices

    /// True for file-backed devices, which grow on write and zero-fill
    /// reads past EOF; the engine skips fixed-capacity bounds checks.
    fn growable(&self) -> bool { false }

    /// offset and buf.len() are guaranteed sector-aligned by the engine,
    /// and buf is 4 KiB-aligned in memory.
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()>;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;

    fn flush(&mut self) -> Result<()>; // durable before "done" is reported

    /// Grow a file-backed device so a flat image spans the full layout
    /// (`sdslot image`); no-op on real block devices.
    fn ensure_len(&mut self, bytes: u64) -> Result<()> { Ok(()) }
}

/// Open with exclusive (write) or shared (read) access — fails if the device
/// is in use (mounted) and cannot be safely claimed. Dispatches to the
/// platform device type for `\\.\…` / `/dev/…` paths, and to the file-backed
/// device otherwise.
pub fn open_device(path: &str, mode: AccessMode, expected_sector: u32)
    -> Result<Box<dyn RawDevice>>;

pub fn enumerate_devices() -> Result<Vec<DeviceInfo>>;  // per-platform

/// Eject removable media (the writing handle must be closed first):
/// media-removal IOCTLs on Windows, CDROMEJECT on Linux (translated to
/// SCSI START STOP UNIT for USB readers), `diskutil eject` on macOS.
pub fn eject_device(path: &str) -> Result<()>;
```

The engine performs all alignment, chunking, and padding so platform
implementations stay minimal and identical in contract.

### 5.2 Write/read engine

1. Resolve slots: manifest + CLI overrides → list of `(bank, unit, extent, image)`.
2. Validate: images open; `image_size <= slot_size`; extents within capacity;
   no overlaps; device sector size matches manifest.
3. Plan + confirm (§6).
4. Stream in **8 MiB aligned buffers** (`--chunk-size` tunable); final chunk
   zero-padded to a sector boundary. Large sequential writes keep the SD card's
   FTL happy and throughput near the card's rating.
5. Update TOC record (if configured), then `flush()`.
6. Optional `--verify`: re-read and compare SHA-256 against the source.

Buffers use explicit 4 KiB alignment (`std::alloc`) because Windows raw I/O and
Linux `O_DIRECT` both require it; one aligned code path everywhere.

Progress is reported through a callback trait so the CLI renders `indicatif`
bars while `--json` mode and the GUI receive structured events from the same hook.

### 5.3 Platform notes

**Linux** (`/dev/sdX`, `/dev/mmcblkN`)
- Open with `O_RDWR | O_EXCL` — the kernel refuses `O_EXCL` opens of a block
  device with mounted partitions: a free, race-free safety check.
- `O_DIRECT` on by default.
- ioctls: `BLKGETSIZE64` (capacity), `BLKSSZGET` (sector size).
- Enumeration: walk `/sys/block` (`size`, `removable`, `device/model`).

**Windows** (`\\.\PhysicalDriveN`)
- `CreateFileW` with `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`.
- All raw-disk I/O must be sector-aligned in offset **and** length.
- Before writing: enumerate volumes on the target disk
  (`IOCTL_STORAGE_GET_DEVICE_NUMBER` per volume, match disk number), then per
  volume `FSCTL_LOCK_VOLUME` + `FSCTL_DISMOUNT_VOLUME`, holding the handles
  (and thus the locks) for the duration.
- `IOCTL_DISK_GET_LENGTH_INFO`, `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`.
- Crate: `windows-sys`.

**macOS** (`/dev/rdiskN`)
- Use the **raw character device**; buffered `diskN` is dramatically slower.
- Unmount first via `diskutil unmountDisk /dev/diskN` (the approach used by
  balenaEtcher et al.).
- ioctls: `DKIOCGETBLOCKCOUNT`, `DKIOCGETBLOCKSIZE`.
- Enumeration: parse `diskutil list -plist`.
- **Elevation is necessary but not sufficient.** Raw disk devices are also
  gated by TCC's Full Disk Access, which root does not bypass, so an
  authenticated `open()` can still fail. The two denials are distinguishable
  by errno and must not be reported the same way:
  - `EACCES` (13) — the device node is `root:operator 0640`, so this is the
    ordinary file-mode denial: the caller is not elevated. Hint: re-run with
    `sudo`.
  - `EPERM` (1) — the file-mode check already passed and TCC denied it above
    the filesystem. Elevating again cannot help. Hint: grant Full Disk Access
    to the app that launched sdslot, then relaunch that app.

  TCC attributes the request to the *responsible* process, which for a bare
  binary is whatever terminal launched it — hence the `.app` bundle
  (§8.4), which gives the GUI a bundle identifier and code signature that a
  grant can attach to instead.

All platforms require elevation; permission errors produce a task-appropriate
message ("re-run with sudo" / "run from an elevated prompt" / on macOS the
Full Disk Access instructions above).

---

## 6. Safety Design

1. **Enumeration-first UX.** `sdslot list` shows model, size, removable flag.
2. **Removable-only by default.** Non-removable devices need `--force`; the
   system/boot disk is refused even with `--force`.
3. **Plan preview + confirmation.** Exact byte ranges printed; typed
   confirmation or `--yes`.
4. **Exclusive access.** `O_EXCL` / volume locks / unmount ensure no live
   filesystem during writes.
5. **Bounds validation.** Extent vs. capacity, overlap detection, sector-size
   mismatch is a hard error.
6. **Deterministic failure reporting.** Interrupted writes report the suspect
   byte range; `verify` confirms state.

---

## 7. Crate Dependencies

| Crate | Purpose |
|---|---|
| `clap` (derive) | CLI |
| `serde`, `toml` | Manifest |
| `serde_json` | `--json` event stream + TOC payload |
| `windows-sys` | Win32 raw disk + volume ioctls; GUI elevation (`ShellExecuteExW`) |
| `libc` | Unix ioctls, `O_DIRECT`/`O_EXCL`, `flock` |
| `indicatif` | CLI progress |
| `sha2` | Verify/TOC hashing |
| `thiserror` | Errors (three variants mapping to the exit codes) |
| `eframe`/`egui`, `rfd` | GUI toolkit + native file dialogs |
| `egui_extras` | GUI slot table (resizable columns) |
| `notify` | GUI watch of image directories (live Write-button state) |
| `tempfile` (dev) | File-backed device fixtures in the test suites |

No existing crate cleanly wraps the Windows lock/dismount dance; the ~300-line
platform layer is owned in-tree.

---

## 8. GUI Frontend

### 8.1 Architecture: veneer over the CLI

The GUI performs **no raw device I/O itself**. It drives the `sdslot` CLI as a
subprocess using `--json` mode, which yields three properties for the price of one:

- **Privilege separation.** Only the small CLI process runs elevated; the GUI
  runs as the normal user. Elevation is requested per-operation:
  - Windows: relaunch the CLI via `ShellExecuteExW` with the `runas` verb
    (UAC prompt), keeping the process handle to await completion.
  - macOS: `osascript -e 'do shell script ... with administrator privileges'`
    wrapping the CLI invocation (native auth dialog).
  - Linux: `pkexec sdslot ...` (Polkit prompt), falling back to a terminal
    `sudo` hint if Polkit is absent.
- **Guaranteed parity.** Every GUI action is by construction expressible as a CLI
  command; the GUI can display the equivalent command line (shown when the
  "Developer mode" setting is enabled — good for learning and bug reports).
- **Loose coupling.** The JSON event schema is the only contract; GUI and CLI
  can evolve independently. It is versioned (§8.1.1) and additive.

One wrinkle: elevated subprocesses on Windows can't inherit stdout pipes across
the UAC boundary. The CLI therefore also supports `--json-port 127.0.0.1:<n>`
(connect back to a localhost TCP listener the GUI opened) as the event channel
when pipe inheritance is unavailable. macOS `osascript` returns output only on
completion, so the port channel is used there for live progress as well.

#### 8.1.1 The event schema

One line of JSON per event, tagged by an `"event"` field (snake_case). The
current version is **`EVENT_SCHEMA_VERSION = 2`**, carried in the `plan`
event's `schema` field — the first event of every operation, so a consumer
learns the version before anything else arrives.

| `event` | Fields | Meaning |
|---|---|---|
| `plan` | `schema`, `device`, `sector_size`, `ops[]` | First event of every operation. Each `ops[]` entry is `{op, bank, unit, offset, bytes, image?}` — the exact byte ranges the plan will touch. |
| `phase_start` | `op`, `bytes` | A pass is starting and will transfer `bytes` in total. A verified multi-slot write emits one for the write pass and a second for the verify pass. |
| `slot_start` | `op`, `bank`, `unit`, `bytes` | One slot's transfer begins. |
| `progress` | `bank`, `unit`, `bytes_done`, `bytes_total` | Periodic within a slot. |
| `slot_end` | `op`, `bank`, `unit`, `ok`, `detail?` | One slot finished; emitted uniformly for every slot, status hashing included. |
| `slot_status` | `bank`, `unit`, `state`, `name?`, `length?`, `sha256?` | A `status` scan's finding. `state` is `unknown` \| `matches` \| `modified` \| `differs` \| `wiped` (§4). |
| `device` | `path`, `model`, `bus`, `size_bytes?`, `removable?`, `system` | One entry of `sdslot list`. |
| `note` | `message` | A human-readable side note that is neither a slot outcome nor an error — e.g. the post-write eject result. |
| `error` | `message` | Operation failed. |
| `done` | `ok`, `detail?` | Last event of every operation. |

`op` is `write` \| `read` \| `wipe` \| `verify` \| `status`. Optional fields are
omitted entirely rather than serialized as `null`.

Versioning rules: the constant is bumped only for an *incompatible* change.
Adding a variant is additive — `note` was added under v2, because a consumer
that does not recognize the tag drops the line harmlessly. v2 itself was a
break: it introduced `phase_start` and made `slot_end` uniform across every
operation, so a v1 consumer's per-slot progress accounting would misreport.

### 8.2 Toolkit choice

**egui/eframe**: pure Rust, statically linked, one ~5 MB binary per platform,
no webview or system toolkit dependencies, trivially cross-compiled in the same
CI matrix as the CLI. The GUI's needs are modest (lists, buttons, progress
bars, a slot map), well within immediate-mode comfort.

The visual design (a `theme` module, the single home of everything visual) is
an iOS-inspired dark language: card-based sections with rounded corners and
soft shadows, the iOS system palette (blue accent, green/orange/red status
colors), capsule status badges in the slot map, animated toggle switches in
Settings, and a typographic hierarchy with generous spacing. A second
palette, `--theme pdp`, styles the app as a PDP-11/70 front panel: near-black
panel behind an off-white bezel, the 11/70 magenta as the accent, and red
"LED" push buttons; semantic status colors are identical across themes.

Alternatives considered:
- **Tauri**: better native look and richer widgets, but adds a webview
  dependency (WebView2 installer question on Windows) and a JS layer — heavier
  than warranted for this scope.
- **Slint / iced**: viable; egui wins on maturity-for-effort for tool UIs.

### 8.3 UI sketch

```
┌──────────────────────────────────────────────────────────┐
│ Device: [SanDisk Ultra 32GB — E: (removable) ▾]  ⟳       │
│ Layout: [pdp11-card.toml ▾]  [Open…]                     │
├──────────────────────────────────────────────────────────┤
│ Bank rl — RL02, 16 MiB slots                             │
│  [0] xxdp-rl02    ✔ matches     [Write] [Extract] [Wipe] │
│  [1] rt11-work    ⚠ modified    [Write] [Extract] [Wipe] │
│  [2] (empty)                    [Write…]                 │
│  [3] (empty)                    [Write…]                 │
│ Bank rp — RP06, 256 MiB slots                            │
│  [0] rsx11mplus   ✔ matches     [Write] [Extract] [Wipe] │
│  [7] 211bsd       ⚠ modified    [Write] [Extract] [Wipe] │
├──────────────────────────────────────────────────────────┤
│ Writing rp:0 … ██████████░░░░░░ 62%  41 MB/s             │
│ Equivalent: sdslot write --device \\.\PhysicalDrive2 …   │
└──────────────────────────────────────────────────────────┘
```

A card map at the top of the slot area draws the whole card to scale: each
bank at its offset with per-slot cells (dark = occupied, light = free; a
fill-fraction bar when slots are subpixel), the TOC as an orange tick, and
hover tooltips naming the region. The reference length is the selected
card's real capacity — refreshed automatically on hotplug when a card is
inserted or swapped — or a generic 8 GB card when none is selected,
so the user can judge what size card the layout needs. Overflowing a real
card paints the excess red with an error caption; outgrowing the generic
reference shows orange sizing guidance ("use a 16 GB card or larger").
While an operation runs, the slot being transferred is highlighted on the
map — an op-colored progress fill (teal write, green verify, orange wipe,
blue read/status) with a pulsing outline — so long scans and writes show
where on the card they currently are.

Slot states come from `sdslot status --json` (TOC-based when present, hash probe
otherwise), and update live from operation results: while a slot transfers,
its row shows the in-progress verb (*writing…*, *verifying…*, *wiping…*);
a successful write then shows *written* (a GUI-only state — data landed but
is unconfirmed), upgraded to *matches* only when its verify pass succeeds; a
failure shows differing, and a wipe marks the slot wiped — no manual status
refresh needed after an operation. A persisted "Hide empty slots" option (off by default) collapses
rows with no content, name, or image; while active, a notice above the slot
map says how many are hidden and where to un-hide them. A status scan
first resets every row to a pending marker, then fills states in as slots are
hashed; the scan emits a plan with per-slot byte counts, so the progress bar
advances over the whole scan (byte-weighted) instead of restarting per slot.
Multi-slot writes aggregate the same way: the write pass measures the whole
operation and the verify pass restarts the aggregate over the same total;
bars show percent complete alongside the byte counts. "Modified" slots — where the FPGA has written to the media since the
last host write — are highlighted as needing extraction before overwrite, which
is the main workflow hazard in simulator↔FPGA interchange.

A slot whose manifest-named image file is missing on disk has its Write
button disabled (tooltip names the missing file). The images' directories
are watched (`notify`: ReadDirectoryChangesW / inotify / FSEvents), so
creating or deleting an image file enables/disables the button in near real
time, with a slow periodic re-stat as fallback for filesystems where
watching is unreliable. Enabled interactive widgets carry an accent
fill/stroke so disabled ones (flat, faded) are visually unmistakable.

Each slot row carries a selection checkbox (a bank-master checkbox in the
table header ticks/unticks a whole bank; global All/None buttons cover the
card). Loading a manifest auto-ticks every slot whose image exists on disk.
A third "Needed" button — and, with "Select only what needs writing"
enabled (the default), a completed status scan itself —
narrows the selection to the slots with an image that the scan did *not*
report as matching, so the following "Write Selected…" writes only what the
card is missing. A canceled or failed scan leaves it alone: its unscanned
rows would all read as needing a write. What "matches" measures is the scan's
comparison, and for a slot the TOC records that is the hash of what was last
written there, not the current image file — restaging different content under
the same image path still reads as matching, and such a slot must be ticked
by hand.
The batch actions operate on the selection: "Write Selected…" writes the
ticked slots' images in one confirmed `write` invocation (one elevation
prompt, per-slot progress, verified) with explicit `--slot` arguments, so
the CLI validates only the checked slots; missing image files are flagged
in the preview and skipped on confirmation with their slots untouched,
while ticked slots with no image are noted and ignored. When "Eject disk
after writing" is enabled (the default) and the target is a real removable
device, the GUI passes `--eject` so the same elevated CLI invocation ejects
the card after the write (and its verify pass) succeeds — no second
elevation prompt — and reports the result as a `note` event in the log.
"Extract
Selected…" pulls the ticked slots into a chosen folder via `read
--out-dir` (one invocation, generated filenames), and "Wipe Selected…"
zeroes them via a multi-slot `wipe` after a confirmation listing every
byte range.

An Eject button next to the device selector (enabled whenever a real device
is selected and no operation is running) runs `sdslot eject` so the card can
be pulled without a write. Slot statuses are kept when the selection is lost
(eject, unplug) — the last known state of the card that just left — but are
cleared whenever a *different* target is selected, so a freshly inserted
card is never displayed with the previous card's statuses.

The device list mirrors the CLI's safety rails (§6): devices without media
(empty card readers) are hidden unless "Show all devices" is enabled, and
non-removable disks appear grayed out and unselectable unless "Advanced" is
enabled. Enabling Advanced first raises a warning dialog — writing an
internal disk can destroy the machine's operating system — that must be
explicitly confirmed; on confirm, non-removable targets become selectable
and the GUI passes the CLI's `--force` for them. The system/boot disk is
never selectable, matching the CLI's refusal even with `--force`.

A Settings window collects the persisted options — "Show all devices",
"Advanced", "Select first removable disk at startup" (warning-gated: the
first removable disk may not be the intended card), "Verify after write"
(on by default), "Eject disk after writing" (on by default; ejects the
card after a successful Write Selected), "Select only what needs writing"
(on by default; after a status refresh), "Hide empty slots", "Hide log",
and "Developer mode" — plus a "Reset all settings" button restoring the
safe defaults.

A Cancel button beside the progress bar (enabled only while an operation
runs) terminates the CLI subprocess — the direct child for unelevated
operations and the pkexec/osascript wrappers, or the elevated process via
its handle on Windows. Rows still marked busy when a canceled operation
ends resolve honestly: an interrupted write/wipe shows differing, an
interrupted verify falls back to written; the log advises re-running
status.

Beneath the progress area sits a **log pane**, styled as an 80's phosphor CRT:
the bundled VT323 face (a faithful DEC VT320 terminal font, OFL-licensed, in
`assets/fonts/`) rendered in classic P1 green, installed as a named egui font
family with the stock fonts behind it so glyphs VT323 does not cover still
render. The pane is drag-resizable and its height is persisted; "Hide log"
collapses it entirely. Semantic status colors are shared with the rest of the
UI, so the log's severity coloring matches the slot table's.

Both binaries embed the git revision at build time: `--version` reports
e.g. `0.1.0 (git 4a1322b9aeaa, dirty)` plus the copyright and repository
link, and the GUI logs the same sign-on at startup. The GUI's *window* icon is
drawn in code (an SD card with slot-state stripes) rather than loaded from a
file; the only binary assets the crate commits are the VT323 font and the
macOS bundle icon (`assets/sdslot.icns`, §8.4). Settings live in a TOML file
`.sdslot` in the user's home directory and are written back on every change;
the two dangerous options only take effect after their warning dialog is
confirmed. Window size is persisted alongside them.

The device list follows hotplug: a background thread waits on the OS event
source — `DiskArbitration` on macOS, `WM_DEVICECHANGE` on Windows, netlink
kobject uevents on Linux, with a 15-second heartbeat alongside (and as the
sole fallback where none of those can be opened) — and re-enumerates when
it fires (selection is tracked by device path, so it survives reordering
and vanishes with an unplugged device). Enumeration happens in-process via
`sdslot-core` — it is
metadata-only (no media reads, no elevation), so it does not breach the
GUI's no-raw-I/O rule, and it avoids spawning the console-subsystem CLI,
whose window would flash on every poll. All data-path operations still run
through the CLI subprocess. With startup auto-select enabled, a newly hotplugged
removable disk is selected automatically when no target is currently
chosen — an explicit selection is never overridden.
An "Export flat image…" action maps to `sdslot image` (no elevation needed —
it writes a regular file). The layout manifest may be given on the command
line (`sdslot-gui card.toml`, or `-m/--manifest card.toml`), so a layout can
be associated with the GUI via shell shortcuts or file-manager "open with".

### 8.4 macOS packaging: the `.app` bundle

Distributing `sdslot-gui` as a bare executable makes the Full Disk Access
requirement (§5.3) effectively unsatisfiable: TCC cannot hold a grant for
something with no bundle identifier and no code signature, so authorization
falls to whichever terminal launched the binary — invisible to the user, and
wrong the moment they launch it another way.

`sdslot-gui/macos/bundle.sh` therefore assembles a real bundle from the
already-built binaries:

```
sdslot-gui.app/Contents/
  Info.plist          # from Info.plist.in; @VERSION@ substituted
  PkgInfo
  MacOS/sdslot-gui    # the GUI
  MacOS/sdslot        # the CLI — found as a sibling by backend.rs::cli_path
  Resources/sdslot.icns
```

Design points:

- **The CLI ships inside the bundle.** `cli_path()` looks for `sdslot` next to
  the running executable, so `Contents/MacOS/` satisfies it with no code
  change, and the elevated backend is always the matching build.
- **`CFBundleIdentifier` is stable across releases.** TCC keys the grant by
  identifier plus signature; changing it discards every user's authorization.
- **Signing is nested-first.** `Contents/MacOS/sdslot` is signed before the
  bundle (`codesign --deep` is deprecated and mis-signs inner Mach-O
  binaries). The default is an ad-hoc signature, which is enough for TCC to
  hold a grant but changes on every rebuild — so the grant must be re-added
  after each one. `CODESIGN_IDENTITY` selects a Developer ID certificate for a
  grant that survives rebuilds and updates.
- **Numeric `CFBundleVersion`.** Release archives are also built from untagged
  commits, whose "version" is a git hash; the hash goes in
  `CFBundleShortVersionString` and `CFBundleVersion` falls back to `0.0.0`,
  which macOS requires to be numeric-dotted.

The bare binaries stay in the release archive alongside the bundle for CLI and
terminal use.

---

## 9. Testing Strategy

The whole suite runs on any host with no hardware, no elevation, and no root:
every data-path test targets a **file-backed device** (`--device file.img` —
a regular file, growable, with relaxed alignment), which exercises the same
engine code paths a real card does. `cargo test --workspace` is currently 111
tests:

| Suite | Tests | Covers |
|---|---|---|
| `sdslot-core` unit | 33 | Size parsing, the drive-type registry (including the stream types' lack of geometry), event round-tripping and the schema version, RTL identifier sanitizing, TOC encoding, the hotplug listener's startup signal |
| `sdslot-core/tests/layout_tests.rs` | 19 | Manifest resolution against good and deliberately broken layouts: overlapping banks, oversized images, non-power-of-two slots, misaligned bases, stream types declared with geometry |
| `sdslot-core/tests/engine_roundtrip.rs` | 12 | Write → read → verify round-trips, padding and chunk boundaries, wipe, status states, extraction lengths |
| `sdslot-core/tests/device_tests.rs` | 4 | `RawDevice` contract on the file backend; eject refuses regular files |
| `sdslot/tests/cli.rs` | 15 | Argument parsing, exit codes, and the `--json` event stream as the GUI actually consumes it |
| `sdslot-gui` unit | 20 | Theme/palette invariants, settings persistence, operation-progress accounting |
| `sdslot-core` doctests | 8 | The public library API as documented: manifest → slot extent → RTL export, size expressions, the registry, slot references, device-path classification |

Gates: `.githooks/pre-commit` (opt in with `git config core.hooksPath
.githooks`) runs fmt, clippy on the host, clippy on every installed cross
target, then the tests. CI (`.github/workflows/release.yml`) runs the same
fmt/clippy/test gate plus a clippy matrix over the Linux, macOS, and Windows
targets — necessary because the device and elevation layers are `cfg`-gated,
so a host-only lint sees just one platform's code.

Not automated, and done by hand before a release:

- **Real-hardware smoke checklist** on Windows/macOS/Linux with an actual SD
  card: write rl:0 and rp:7, verify, extract rp:7 with `--length canonical`,
  attach the result in a simulator, boot it. This is the only way to cover
  elevation, volume locking/dismount, `O_EXCL`, macOS Full Disk Access, and
  eject — none of which a file-backed target reaches.
- **Loopback devices.** Running the round-trips against `losetup` devices in
  Linux CI would automate the block-device path (though not elevation or the
  Windows/macOS layers); it is not wired up today.

## 10. Future Work

- `sdslot sync` bidirectional freshness reporting (hash diff card vs. host).
- GUI: drag-and-drop of an image file onto a slot as a `write` invocation;
  double-click extraction.
- `--trim`/`BLKDISCARD` for wiped slots; sparse-aware writes (skip all-zero
  source chunks).
- Read-only "attach card slot directly in simulator" via a small block-device
  shim, skipping extraction entirely for quick boots.
- GUI: manifest editor (create banks/slots graphically, export TOML + RTL).
- Loopback (`losetup`) round-trips in Linux CI, to cover the real block-device
  path automatically instead of only in the manual smoke checklist (§9).
