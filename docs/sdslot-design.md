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
coexist without wasting space:

```
LBA 0x000000  ┌────────────────────────────────┐
              │ Bank "rl"  — 512 MiB total     │
              │   32 × 16 MiB slots            │  RL02 images (10 MiB each)
              │   slot n @ n × 16 MiB          │
LBA 0x100000  ├────────────────────────────────┤
              │ (gap — free for the TOC §2.6)  │
LBA 0x400000  ├────────────────────────────────┤
              │ Bank "rp"  — 2 GiB @ 2 GiB     │
              │   8 × 256 MiB slots            │  RP06 images (168 MiB each)
              │                                │
              └────────────────────────────────┘
```

Rules:

- Slot sizes are powers of two, and each bank's base address is aligned to
  the bank's power-of-two span (`units × slot_size`, rounded up to a power of
  two) — a bank spanning 2 GiB must sit on a 2 GiB boundary. Both are
  enforced by default (`allow_unaligned = true` to override). This keeps the
  FPGA address math to pure concatenation:
  `lba = BANK_BASE | (unit << SLOT_SHIFT) | block_offset`, with no adder.
- Banks must not overlap; slots are wholly contained in their bank.
- A bank may reserve more slots than drives currently attached (the RL02 example
  reserves 32 × 16 MiB slots in its 512 MiB region but the controller may only
  implement units 0–3). Spare slots cost nothing until written.

### 2.2 Drive type registry

A built-in table maps drive types to canonical image sizes and recommended slot
sizes, used for validation, `--length canonical` extraction (§4), and sizing
suggestions:

| Type | Geometry | Canonical image size | Recommended slot |
|---|---|---|---|
| RX01 | 77t × 26s × 128B | 256,256 B | 512 KiB |
| RX02 | 77t × 26s × 256B | 512,512 B | 1 MiB |
| RX50 | 80t × 1h × 10s × 512B | 409,600 B | 512 KiB |
| RX33 | 80t × 2h × 15s × 512B | 1,228,800 B | 2 MiB |
| RX23 | 80t × 2h × 18s × 512B | 1,474,560 B | 2 MiB |
| RX26 | 80t × 2h × 36s × 512B | 2,949,120 B | 4 MiB |
| RL01 | 256c × 2h × 40s × 256B | 5,242,880 B (5 MiB) | 8 MiB |
| RL02 | 512c × 2h × 40s × 256B | 10,485,760 B (10 MiB) | 16 MiB |
| RK05 | 203c × 2h × 12s × 512B | 2,494,464 B | 4 MiB |
| RP04/05 | 411c × 19h × 22s × 512B | ~88 MiB | 128 MiB |
| RP06 | 815c × 19h × 22s × 512B | ~176 MiB | 256 MiB |
| RM03 | 823c × 5h × 32s × 512B | ~67 MiB | 128 MiB |
| RM05 | 823c × 19h × 32s × 512B | ~256 MiB | 512 MiB |
| RP07 | 630c × 32h × 50s × 512B | 516,096,000 B | 1 GiB |

RX01/RX02 physical media record track 0 in single density by interchange
convention; flat images are uniform streams including track 0, which is the
canonical size above. Mixed-density RX02 images (509,184 B) exist in
the wild — the non-strict size check warns but still writes them. Note the
later floppies outgrow a 1 MiB slot: RX33 (1.2 MB), RX23 (1.44 MB), and
RX26 (2.88 MB) need 2–4 MiB slots.

Magtape banks carry `.tap` images, which are variable-length — no
canonical size, so tape banks declare no `drive_type` and extraction uses
the byte length recorded in the TOC (`--length toc`). Slot sizing for the
period drives: a 2400 ft reel at 1600 BPI (the TM02/TM03 ceiling — 6250 BPI
GCR needed the TM78) holds at most 46,080,000 bytes, inter-record gaps only
lower that, and `.tap` container overhead (8 B/record) is far smaller than
the gaps it replaces; a TK50 CompacTape holds ~94.5 MB and a TK70
CompacTape II ~296 MB. 512 MiB slots therefore cover every period tape with
power-of-two headroom (64 MiB suffices for 9-track-only layouts).

The registry is extensible via `[drive_types]` entries in the manifest for
anything not built in. (Sizes above are indicative; the built-in registry
carries the exact canonical byte count per type, including bad-block-track
conventions where applicable.)

### 2.3 Manifest file (TOML)

```toml
# pdp11-card.toml
sector_size = 512

[[bank]]
name       = "rl"
base       = "0"           # bytes, KiB/MiB/GiB suffixes, or "NNNs" for LBAs
slot_size  = "16MiB"
units      = 32            # capacity of the bank; controller may use fewer
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
```

Slots are addressed as `bank:unit` on the command line (`--slot rl:1=foo.rl02`,
`--slot rp:7`). With a single bank the prefix may be omitted.

### 2.4 RTL export

`sdslot export-rtl --manifest pdp11-card.toml -o card_layout.vh` emits per-bank
parameters:

```verilog
// Bank "rl": RL02 x 32
localparam RL_BASE_LBA   = 32'h0000_0000;
localparam RL_SLOT_SHIFT = 15;              // 16 MiB / 512 = 2^15 LBAs
localparam RL_UNITS      = 6'd32;

// Bank "rp": RP06 x 8
localparam RP_BASE_LBA   = 32'h0040_0000;   // 2 GiB / 512
localparam RP_SLOT_SHIFT = 19;              // 256 MiB slots
localparam RP_UNITS      = 4'd8;
```

Formats: Verilog header (`.vh`), SystemVerilog package, Rust, C header.

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
               [--verify] [--yes] [--force] [--json]
    Write the images named in the manifest (or per-slot overrides). Only named
    slots are touched.

sdslot read    --device <dev> --manifest layout.toml --slot rp:7 -o out.dsk
               [--length <bytes|canonical|toc|slot>] [--json]
sdslot read    --device <dev> --manifest layout.toml --slot B:N... --out-dir DIR
    Extract one slot into a file, or any number of slots into a directory
    (files named <bank><unit>_<name>.img); see §4.

sdslot wipe    --device <dev> --manifest layout.toml --slot rl:3 [--yes]
    Zero a slot's full extent (present a "blank pack" to the FPGA).

sdslot verify  --device <dev> --manifest layout.toml [--slot B:N]... [--json]
    Re-read slots and compare against manifest images (SHA-256).

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
  └── device:      RawDevice trait + file-backed device
        ├── linux.rs    (/dev/sdX, ioctls)
        ├── windows.rs  (\\.\PhysicalDriveN)
        └── macos.rs    (/dev/rdiskN, diskutil)

sdslot     (bin) — CLI over sdslot-core; also the privileged backend for the GUI
sdslot-gui (bin) — GUI frontend (§8); performs no raw I/O itself
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

All platforms require elevation; EACCES-class errors produce a task-appropriate
message ("re-run with sudo" / "run from an elevated prompt").

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
| `notify` | GUI watch of image directories (live Write-button state) |

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
- **Loose coupling.** The JSON event schema (plan, progress {bytes, total,
  slot}, result, error) is the only contract; GUI and CLI can evolve
  independently and the schema is versioned.

One wrinkle: elevated subprocesses on Windows can't inherit stdout pipes across
the UAC boundary. The CLI therefore also supports `--json-port 127.0.0.1:<n>`
(connect back to a localhost TCP listener the GUI opened) as the event channel
when pipe inheritance is unavailable. macOS `osascript` returns output only on
completion, so the port channel is used there for live progress as well.

### 8.2 Toolkit choice

**egui/eframe** is the recommended toolkit: pure Rust, statically linked, one
~5 MB binary per platform, no webview or system toolkit dependencies, trivially
cross-compiled in the same CI matrix as the CLI. The GUI's needs are modest
(lists, buttons, progress bars, a slot map), well within immediate-mode comfort.

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
card's real capacity — refreshed automatically by the hotplug poller when a
card is inserted or swapped — or a generic 8 GB card when none is selected,
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
The batch actions operate on the selection: "Write Selected…" writes the
ticked slots' images in one confirmed `write` invocation (one elevation
prompt, per-slot progress, verified) with explicit `--slot` arguments, so
the CLI validates only the checked slots; missing image files are flagged
in the preview and skipped on confirmation with their slots untouched,
while ticked slots with no image are noted and ignored. "Extract
Selected…" pulls the ticked slots into a chosen folder via `read
--out-dir` (one invocation, generated filenames), and "Wipe Selected…"
zeroes them via a multi-slot `wipe` after a confirmation listing every
byte range.

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
(on by default), "Hide empty slots", "Hide log", and "Developer mode" —
plus a "Reset all settings" button restoring the safe defaults.

A Cancel button beside the progress bar (enabled only while an operation
runs) terminates the CLI subprocess — the direct child for unelevated
operations and the pkexec/osascript wrappers, or the elevated process via
its handle on Windows. Rows still marked busy when a canceled operation
ends resolve honestly: an interrupted write/wipe shows differing, an
interrupted verify falls back to written; the log advises re-running
status.

Both binaries embed the git revision at build time: `--version` reports
e.g. `0.1.0 (git 4a1322b9aeaa, dirty)` plus the copyright and repository
link, and the GUI logs the same sign-on at startup. The GUI's window icon
is drawn in code (an SD card with slot-state stripes) — no binary asset. Settings live in a TOML file `.sdslot` in the user's home
directory and are written back on every change; the two dangerous options
only take effect after their warning dialog is confirmed.

The device list follows hotplug: a background poller re-enumerates every
few seconds and updates the list when the device set changes (selection is
tracked by device path, so it survives reordering and vanishes with an
unplugged device). Enumeration happens in-process via `sdslot-core` — it is
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

---

## 9. Testing Strategy

- **Loopback devices.** Linux CI runs full write/read/verify round-trips against
  `losetup` loop devices.
- **File-backed mode.** `--device file.img` (regular file, relaxed alignment) for
  unit tests and for building full card images when that *is* desired.
- **Golden layouts.** Manifest validation tested against good and deliberately
  broken (overlapping banks, oversized images, non-power-of-two) layouts.
- **JSON schema tests.** GUI event contract validated against recorded CLI output.
- **Windows/macOS smoke checklist** on real SD cards: write rl:0 and rp:7,
  verify, extract rp:7 with `--length canonical`, attach in a simulator, boot.

## 10. Future Work

- `sdslot sync` bidirectional freshness reporting (hash diff card vs. host).
- GUI: drag-and-drop of an image file onto a slot as a `write` invocation;
  double-click extraction.
- `--trim`/`BLKDISCARD` for wiped slots; sparse-aware writes (skip all-zero
  source chunks).
- Read-only "attach card slot directly in simulator" via a small block-device
  shim, skipping extraction entirely for quick boots.
- GUI: manifest editor (create banks/slots graphically, export TOML + RTL).
