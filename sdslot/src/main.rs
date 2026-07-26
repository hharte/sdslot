// SPDX-License-Identifier: MIT OR Apache-2.0
//! sdslot CLI (design §3): list, status, write, read, wipe, verify,
//! export-rtl, plus `image` for assembling a dd/balenaEtcher-writable flat
//! card image. Exit codes: 0 success, 1 usage/validation, 2 device access,
//! 3 verify mismatch.

mod sink;

use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use sdslot_core::device::{
    enumerate_devices, is_platform_device_path, open_device, AccessMode, DeviceInfo,
};
use sdslot_core::engine::{self, EngineOpts, LengthMode, WriteJob};
use sdslot_core::events::{Event, EventSink, OpKind};
use sdslot_core::layout::{Layout, SlotAssign, SlotRef};
use sdslot_core::rtl::{self, RtlFormat};
use sdslot_core::units::{format_bytes, parse_size};
use sdslot_core::{Error, Result};
use sink::Sink;

#[derive(Parser)]
#[command(
    name = "sdslot",
    version = sdslot_core::VERSION_FULL,
    long_version = sdslot_core::LONG_VERSION,
    about = "Write vintage disk images to SD cards at fixed LBA offsets"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct OutputArgs {
    /// Emit line-delimited JSON events on stdout instead of human output
    #[arg(long)]
    json: bool,
    /// Send JSON events to a localhost TCP listener, e.g. 127.0.0.1:7070
    /// (used by the GUI when stdout cannot cross an elevation boundary)
    #[arg(long, value_name = "ADDR")]
    json_port: Option<String>,
}

#[derive(Args)]
struct EngineArgs {
    /// Transfer chunk size (default 8MiB); accepts KiB/MiB suffixes
    #[arg(long, value_name = "SIZE")]
    chunk_size: Option<String>,
    /// Treat an image size that differs from the drive type's canonical
    /// size as an error instead of a warning
    #[arg(long)]
    strict_size: bool,
}

/// The device + manifest pair every destructive/extractive command targets.
#[derive(Args)]
struct TargetArgs {
    #[arg(long, value_name = "DEV")]
    device: String,
    #[arg(long, value_name = "FILE")]
    manifest: PathBuf,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enumerate candidate block devices (size, model, bus, removable flag)
    List {
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Show card contents from the TOC (or slot-by-slot hash probe with a manifest)
    Status {
        #[arg(long, value_name = "DEV")]
        device: String,
        #[arg(long, value_name = "FILE")]
        manifest: Option<PathBuf>,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Write the images named in the manifest (or per-slot overrides); only
    /// named slots are touched
    Write {
        #[command(flatten)]
        target: TargetArgs,
        /// Slot to write: bank:unit[=image], repeatable
        #[arg(long, value_name = "B:N[=IMG]")]
        slot: Vec<String>,
        /// Re-read and compare after writing
        #[arg(long)]
        verify: bool,
        /// Skip the interactive confirmation
        #[arg(long)]
        yes: bool,
        /// Allow non-removable devices (the system disk is always refused)
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        engine: EngineArgs,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Extract slots into simulator-compatible image files
    Read {
        #[command(flatten)]
        target: TargetArgs,
        /// Slot to read: bank:unit, repeatable with --out-dir
        #[arg(long, value_name = "B:N", required = true)]
        slot: Vec<String>,
        /// Output file (single slot)
        #[arg(short, long, value_name = "FILE", conflicts_with = "out_dir")]
        out: Option<PathBuf>,
        /// Output directory (any number of slots; files are named
        /// BANKUNIT_NAME.img, e.g. rl0_xxdp.img)
        #[arg(long, value_name = "DIR")]
        out_dir: Option<PathBuf>,
        /// Extraction length: a byte count, or one of canonical | toc | slot
        #[arg(long, value_name = "LEN")]
        length: Option<String>,
        #[command(flatten)]
        engine: EngineArgs,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Zero a slot's full extent (present a "blank pack" to the FPGA)
    Wipe {
        #[command(flatten)]
        target: TargetArgs,
        /// Slot to wipe: bank:unit, repeatable
        #[arg(long, value_name = "B:N", required = true)]
        slot: Vec<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        engine: EngineArgs,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Re-read slots and compare against manifest images (SHA-256)
    Verify {
        #[command(flatten)]
        target: TargetArgs,
        /// Slot to verify: bank:unit, repeatable (default: all manifest slots)
        #[arg(long, value_name = "B:N")]
        slot: Vec<String>,
        #[command(flatten)]
        engine: EngineArgs,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Emit per-bank layout parameters for RTL (or Rust/C) consumption
    ExportRtl {
        #[arg(long, value_name = "FILE")]
        manifest: PathBuf,
        /// Output file; "-" for stdout
        #[arg(short, long, value_name = "FILE")]
        out: PathBuf,
        /// vh | sv | rs | h (default: inferred from the output extension, else vh)
        #[arg(long, value_name = "FMT")]
        format: Option<String>,
    },
    /// Assemble a full flat card image file that dd or balenaEtcher can
    /// write to a card in one pass
    Image {
        #[arg(long, value_name = "FILE")]
        manifest: PathBuf,
        /// Output image file
        #[arg(short, long, value_name = "FILE")]
        out: PathBuf,
        /// Slot override: bank:unit[=image], repeatable
        #[arg(long, value_name = "B:N[=IMG]")]
        slot: Vec<String>,
        /// Re-read and compare after writing
        #[arg(long)]
        verify: bool,
        /// Overwrite an existing output file without confirmation
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        engine: EngineArgs,
        #[command(flatten)]
        output: OutputArgs,
    },
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind;
            let code = match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
                _ => 1,
            };
            let _ = e.print();
            std::process::exit(code);
        }
    };
    match run(cli.cmd) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(e.exit_code());
        }
    }
}

/// Run a command, mirroring any error into the JSON event stream so GUI and
/// script consumers see structured failures.
fn run(cmd: Cmd) -> Result<()> {
    let (json, json_port) = match &cmd {
        Cmd::List { output } => (output.json, output.json_port.clone()),
        Cmd::Status { output, .. }
        | Cmd::Write { output, .. }
        | Cmd::Read { output, .. }
        | Cmd::Wipe { output, .. }
        | Cmd::Verify { output, .. }
        | Cmd::Image { output, .. } => (output.json, output.json_port.clone()),
        Cmd::ExportRtl { .. } => (false, None),
    };
    let quiet_done = matches!(cmd, Cmd::List { .. } | Cmd::ExportRtl { .. });
    let mut sink = Sink::new(json, json_port.as_deref())?;
    let result = dispatch(cmd, &mut sink);
    match &result {
        Ok(()) => {
            if !quiet_done || sink.is_json() {
                sink.emit(&Event::Done {
                    ok: true,
                    detail: None,
                });
            }
        }
        Err(e) => {
            sink.emit(&Event::Error {
                message: e.to_string(),
            });
            sink.emit(&Event::Done {
                ok: false,
                detail: Some(e.to_string()),
            });
        }
    }
    result
}

fn dispatch(cmd: Cmd, sink: &mut Sink) -> Result<()> {
    match cmd {
        Cmd::List { .. } => cmd_list(sink),
        Cmd::Status {
            device, manifest, ..
        } => cmd_status(&device, manifest.as_deref(), sink),
        Cmd::Write {
            target,
            slot,
            verify,
            yes,
            force,
            engine,
            ..
        } => cmd_write(
            &target.device,
            &target.manifest,
            &slot,
            verify,
            yes,
            force,
            &engine,
            sink,
        ),
        Cmd::Read {
            target,
            slot,
            out,
            out_dir,
            length,
            engine,
            ..
        } => cmd_read(
            &target.device,
            &target.manifest,
            &slot,
            out.as_deref(),
            out_dir.as_deref(),
            length.as_deref(),
            &engine,
            sink,
        ),
        Cmd::Wipe {
            target,
            slot,
            yes,
            force,
            engine,
            ..
        } => cmd_wipe(
            &target.device,
            &target.manifest,
            &slot,
            yes,
            force,
            &engine,
            sink,
        ),
        Cmd::Verify {
            target,
            slot,
            engine,
            ..
        } => cmd_verify(&target.device, &target.manifest, &slot, &engine, sink),
        Cmd::ExportRtl {
            manifest,
            out,
            format,
        } => cmd_export_rtl(&manifest, &out, format.as_deref()),
        Cmd::Image {
            manifest,
            out,
            slot,
            verify,
            yes,
            engine,
            ..
        } => cmd_image(&manifest, &out, &slot, verify, yes, &engine, sink),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn engine_opts(args: &EngineArgs, verify: bool, sector_size: u32) -> Result<EngineOpts> {
    let mut opts = EngineOpts {
        verify,
        strict_size: args.strict_size,
        ..EngineOpts::default()
    };
    if let Some(cs) = &args.chunk_size {
        let n = parse_size(cs, sector_size)?;
        if n == 0 || n % u64::from(sector_size) != 0 || n > (512 << 20) {
            return Err(Error::Validation(format!(
                "--chunk-size must be a nonzero multiple of the {sector_size}-byte sector, at most 512 MiB"
            )));
        }
        opts.chunk_size = n as usize;
    }
    Ok(opts)
}

/// Load the manifest and derive engine options together — the identical
/// preamble every write/read/wipe/verify/image command needs before it can
/// open its device and build a `Session`.
fn load_target(
    manifest: &Path,
    engine_args: &EngineArgs,
    verify: bool,
) -> Result<(Layout, EngineOpts)> {
    let layout = Layout::load(manifest)?;
    let opts = engine_opts(engine_args, verify, layout.sector_size)?;
    Ok((layout, opts))
}

fn parse_assigns(slots: &[String]) -> Result<Vec<SlotAssign>> {
    slots.iter().map(|s| SlotAssign::parse(s)).collect()
}

/// Safety rails for destructive commands (design §6): removable-only by
/// default, the system disk refused even with --force. Returns enumeration
/// info for the plan preview when available.
fn device_guard(path: &str, force: bool) -> Result<Option<DeviceInfo>> {
    if !is_platform_device_path(path) {
        return Ok(None); // file-backed target: rails not applicable
    }
    let devices = enumerate_devices().unwrap_or_default();
    let info = devices
        .into_iter()
        .find(|d| d.path.eq_ignore_ascii_case(path));
    match info {
        Some(d) if d.system => Err(Error::Validation(format!(
            "{path} is the system/boot disk; refusing to write it"
        ))),
        Some(d) if d.removable == Some(true) => Ok(Some(d)),
        Some(d) => {
            if force {
                Ok(Some(d))
            } else {
                Err(Error::Validation(format!(
                    "{path} is not a removable device (model {:?}); pass --force if you are sure",
                    d.model
                )))
            }
        }
        None => {
            if force {
                Ok(None)
            } else {
                Err(Error::Validation(format!(
                    "{path} was not found by device enumeration; pass --force if you are sure"
                )))
            }
        }
    }
}

/// Print the full plan (device, model, capacity, exact byte ranges) and
/// require typed confirmation unless --yes (design §6, "plan preview").
fn confirm_plan(
    verb: &str,
    device: &str,
    info: Option<&DeviceInfo>,
    layout: &Layout,
    ops: &[(String, u32, u64, u64, Option<String>)],
    yes: bool,
    sink: &Sink,
) -> Result<()> {
    if sink.is_json() && !yes {
        return Err(Error::Validation(
            "--json/--json-port runs are non-interactive; pass --yes to confirm the plan".into(),
        ));
    }
    if !sink.is_json() {
        match info {
            Some(d) => {
                let size = d
                    .size_bytes
                    .map(format_bytes)
                    .unwrap_or_else(|| "unknown size".into());
                let removable = match d.removable {
                    Some(true) => "removable",
                    Some(false) => "NON-REMOVABLE",
                    None => "removability unknown",
                };
                println!("About to {verb} on {device}");
                println!("  {} ({}, {removable}), {size}", d.model, d.bus);
            }
            None => println!("About to {verb} on {device}"),
        }
        println!("  sector size {} bytes", layout.sector_size);
        for (bank, unit, offset, bytes, image) in ops {
            let src = image
                .as_ref()
                .map(|i| format!("  <- {i}"))
                .unwrap_or_default();
            println!(
                "  {verb} {bank}:{unit}  bytes 0x{offset:010x}..0x{:010x} ({bytes} bytes){src}",
                offset + bytes
            );
        }
        if let Some(t) = layout.toc_extent() {
            println!("  TOC update at 0x{:010x}", t.offset);
        }
    }
    if yes {
        return Ok(());
    }
    eprint!("Type 'yes' to proceed: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| Error::Validation(format!("cannot read confirmation: {e}")))?;
    if line.trim() != "yes" {
        return Err(Error::Validation("aborted: confirmation not given".into()));
    }
    Ok(())
}

fn write_plan_tuples(jobs: &[WriteJob]) -> Vec<(String, u32, u64, u64, Option<String>)> {
    jobs.iter()
        .map(|j| {
            (
                j.bank.clone(),
                j.unit,
                j.extent.offset,
                j.image_len,
                Some(j.image.display().to_string()),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_list(sink: &mut Sink) -> Result<()> {
    let devices = enumerate_devices()?;
    if sink.is_json() {
        for d in &devices {
            sink.emit(&Event::Device {
                path: d.path.clone(),
                model: d.model.clone(),
                bus: d.bus.clone(),
                size_bytes: d.size_bytes,
                removable: d.removable,
                system: d.system,
            });
        }
        return Ok(());
    }
    if devices.is_empty() {
        println!("no block devices found (try an elevated prompt / sudo)");
        return Ok(());
    }
    println!(
        "{:<24} {:>10} {:<8} {:<10} MODEL",
        "DEVICE", "SIZE", "BUS", "REMOVABLE"
    );
    for d in &devices {
        let size = d.size_bytes.map(format_bytes).unwrap_or_else(|| "?".into());
        let removable = match d.removable {
            Some(true) => "yes",
            Some(false) => "no",
            None => "?",
        };
        let system = if d.system { "  [SYSTEM DISK]" } else { "" };
        println!(
            "{:<24} {:>10} {:<8} {:<10} {}{system}",
            d.path, size, d.bus, removable, d.model
        );
    }
    Ok(())
}

fn cmd_status(device: &str, manifest: Option<&Path>, sink: &mut Sink) -> Result<()> {
    match manifest {
        Some(m) => {
            let layout = Layout::load(m)?;
            let opts = EngineOpts::default();
            let mut dev = open_device(device, AccessMode::Read, layout.sector_size)?;
            if !sink.is_json() {
                println!("Card status for {device} (manifest {}):", m.display());
            }
            let mut session = engine::Session::new(dev.as_mut(), &layout)?;
            session.status(&opts, sink)?;
        }
        None => {
            let opts = EngineOpts::default();
            let mut dev = open_device(device, AccessMode::Read, 512)?;
            if !sink.is_json() {
                println!("Card status for {device} (from on-card TOC):");
            }
            match engine::status_from_card(dev.as_mut(), &opts, sink)? {
                Some(_) => {}
                None => {
                    return Err(Error::Validation(format!(
                        "no TOC found on {device}; pass --manifest for a slot-by-slot hash probe"
                    )))
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_write(
    device: &str,
    manifest: &Path,
    slots: &[String],
    verify: bool,
    yes: bool,
    force: bool,
    engine_args: &EngineArgs,
    sink: &mut Sink,
) -> Result<()> {
    let (layout, opts) = load_target(manifest, engine_args, verify)?;
    let overrides = parse_assigns(slots)?;
    let jobs = engine::plan_writes(&layout, &overrides)?;
    let info = device_guard(device, force)?;

    sink.emit(&engine::plan_ops(device, &layout, &jobs, OpKind::Write));
    confirm_plan(
        "write",
        device,
        info.as_ref(),
        &layout,
        &write_plan_tuples(&jobs),
        yes,
        sink,
    )?;

    let mut dev = open_device(device, AccessMode::Write, layout.sector_size)?;
    let mut session = engine::Session::new(dev.as_mut(), &layout)?;
    for w in session.validate_writes(&jobs, &opts)? {
        eprintln!("warning: {w}");
    }
    session.write_slots(&jobs, &opts, sink)
}

#[allow(clippy::too_many_arguments)]
fn cmd_read(
    device: &str,
    manifest: &Path,
    slots: &[String],
    out: Option<&Path>,
    out_dir: Option<&Path>,
    length: Option<&str>,
    engine_args: &EngineArgs,
    sink: &mut Sink,
) -> Result<()> {
    let (layout, opts) = load_target(manifest, engine_args, false)?;
    let mode = length
        .map(|s| LengthMode::parse(s, layout.sector_size))
        .transpose()?;
    let mut resolved = Vec::new();
    for s in slots {
        resolved.push(layout.resolve_slot(&SlotRef::parse(s)?)?);
    }
    // Output rules: -o names the one file for a single slot; --out-dir
    // takes any number of slots with generated names.
    let targets: Vec<PathBuf> = match (out, out_dir) {
        (Some(_), _) if resolved.len() != 1 => {
            return Err(Error::Validation(
                "-o names a single output file; use --out-dir with multiple slots".into(),
            ))
        }
        (Some(o), _) => vec![o.to_path_buf()],
        (None, Some(dir)) => {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::Validation(format!("cannot create {}: {e}", dir.display())))?;
            resolved
                .iter()
                .map(|&(bank_idx, unit)| {
                    let bank = &layout.banks[bank_idx];
                    let name = bank.slots.get(&unit).and_then(|s| s.name.as_deref());
                    dir.join(engine::extract_filename(&bank.name, unit, name))
                })
                .collect()
        }
        (None, None) => {
            return Err(Error::Validation(
                "give -o <file> for one slot or --out-dir <dir>".into(),
            ))
        }
    };
    // Extraction is read-only: no confirmation gauntlet, but a shared lock
    // (taken by the platform open) so a concurrent write can't tear it.
    let mut dev = open_device(device, AccessMode::Read, layout.sector_size)?;
    let mut session = engine::Session::new(dev.as_mut(), &layout)?;
    let mut jobs = Vec::with_capacity(resolved.len());
    for (&(bank_idx, unit), target) in resolved.iter().zip(&targets) {
        let length = session.resolve_length(bank_idx, unit, mode.clone())?;
        jobs.push(engine::ReadJob {
            bank_idx,
            unit,
            length,
            out_path: target.clone(),
        });
    }
    session.read_slots(&jobs, &opts, sink)?;
    if !sink.is_json() {
        for job in &jobs {
            eprintln!(
                "extracted {} bytes to {}",
                job.length,
                job.out_path.display()
            );
        }
    }
    Ok(())
}

fn cmd_wipe(
    device: &str,
    manifest: &Path,
    slots: &[String],
    yes: bool,
    force: bool,
    engine_args: &EngineArgs,
    sink: &mut Sink,
) -> Result<()> {
    let (layout, opts) = load_target(manifest, engine_args, false)?;
    let mut resolved = Vec::new();
    for s in slots {
        resolved.push(layout.resolve_slot(&SlotRef::parse(s)?)?);
    }
    let info = device_guard(device, force)?;

    let ops: Vec<_> = resolved
        .iter()
        .map(|&(bank_idx, unit)| {
            let e = layout.slot_extent(bank_idx, unit);
            (
                layout.banks[bank_idx].name.clone(),
                unit,
                e.offset,
                e.len,
                None,
            )
        })
        .collect();
    confirm_plan("wipe", device, info.as_ref(), &layout, &ops, yes, sink)?;

    let mut dev = open_device(device, AccessMode::Write, layout.sector_size)?;
    let mut session = engine::Session::new(dev.as_mut(), &layout)?;
    session.wipe_slots(&resolved, &opts, sink)
}

fn cmd_verify(
    device: &str,
    manifest: &Path,
    slots: &[String],
    engine_args: &EngineArgs,
    sink: &mut Sink,
) -> Result<()> {
    let (layout, opts) = load_target(manifest, engine_args, false)?;
    let overrides = parse_assigns(slots)?;
    let jobs = engine::plan_writes(&layout, &overrides)?;
    let mut dev = open_device(device, AccessMode::Read, layout.sector_size)?;
    let mut session = engine::Session::new(dev.as_mut(), &layout)?;
    let mismatches = session.verify_slots(&jobs, &opts, sink)?;
    if !mismatches.is_empty() {
        return Err(Error::VerifyMismatch(format!(
            "{} slot(s) differ from their manifest images: {}",
            mismatches.len(),
            mismatches.join(", ")
        )));
    }
    Ok(())
}

fn cmd_export_rtl(manifest: &Path, out: &Path, format: Option<&str>) -> Result<()> {
    let layout = Layout::load(manifest)?;
    let fmt = match format {
        Some(f) => f.parse::<RtlFormat>()?,
        None => RtlFormat::from_path(out).unwrap_or(RtlFormat::Vh),
    };
    let stem = out
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("card_layout");
    let text = rtl::export(&layout, fmt, stem)?;
    if out == Path::new("-") {
        print!("{text}");
    } else {
        std::fs::write(out, &text)
            .map_err(|e| Error::Validation(format!("cannot write {}: {e}", out.display())))?;
        eprintln!("wrote {}", out.display());
    }
    Ok(())
}

fn cmd_image(
    manifest: &Path,
    out: &Path,
    slots: &[String],
    verify: bool,
    yes: bool,
    engine_args: &EngineArgs,
    sink: &mut Sink,
) -> Result<()> {
    let (layout, opts) = load_target(manifest, engine_args, verify)?;
    let overrides = parse_assigns(slots)?;
    let jobs = engine::plan_writes(&layout, &overrides)?;

    if is_platform_device_path(&out.display().to_string()) {
        return Err(Error::Validation(
            "image writes a regular file; use `sdslot write` for raw devices".into(),
        ));
    }
    if out.exists() && !yes {
        return Err(Error::Validation(format!(
            "{} already exists; pass --yes to overwrite it",
            out.display()
        )));
    }
    // Start from an empty file so stale content can't leak into the image.
    std::fs::File::create(out)
        .map_err(|e| Error::Validation(format!("cannot create {}: {e}", out.display())))?;

    sink.emit(&engine::plan_ops(
        &out.display().to_string(),
        &layout,
        &jobs,
        OpKind::Write,
    ));
    let mut dev = open_device(
        &out.display().to_string(),
        AccessMode::Write,
        layout.sector_size,
    )?;
    let mut session = engine::Session::new(dev.as_mut(), &layout)?;
    for w in session.validate_writes(&jobs, &opts)? {
        eprintln!("warning: {w}");
    }
    session.write_slots(&jobs, &opts, sink)?;
    session.extend_to_full_layout()?;
    dev.flush()?;
    if !sink.is_json() {
        eprintln!(
            "assembled {} ({}); write it to a card with dd or balenaEtcher",
            out.display(),
            format_bytes(std::fs::metadata(out).map(|m| m.len()).unwrap_or(0))
        );
    }
    Ok(())
}
