// SPDX-License-Identifier: MIT OR Apache-2.0
//! sdslot-gui — GUI frontend for sdslot (design §8). A veneer over the CLI:
//! all raw device I/O happens in `sdslot` subprocesses (elevated
//! per-operation), driven through the versioned JSON event stream.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod devices;
mod ops;
mod settings;
mod theme;

use std::path::PathBuf;
use std::process::ExitCode;

use eframe::egui;

const USAGE: &str = "\
Usage: sdslot-gui [OPTIONS] [LAYOUT.toml]

Open the sdslot GUI, optionally with a card layout manifest preloaded.

Arguments:
  [LAYOUT.toml]          Layout manifest to open on startup

Options:
  -m, --manifest <FILE>  Layout manifest to open on startup (same as the
                         positional argument)
      --theme <NAME>     Visual theme: default (blue) or pdp (PDP-11/70
                         front panel: red LED buttons, magenta accents)
  -h, --help             Print this help
  -V, --version          Print the version
";

/// Hand-rolled argument parsing: the GUI deliberately has no clap
/// dependency, and its whole surface is a manifest path and a theme.
fn parse_args() -> Result<(Option<PathBuf>, theme::ThemeKind), String> {
    let mut manifest: Option<PathBuf> = None;
    let mut kind = theme::ThemeKind::Default;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-h") | Some("--help") => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            Some("-V") | Some("--version") => {
                println!("sdslot-gui {}", sdslot_core::LONG_VERSION);
                std::process::exit(0);
            }
            Some("-m") | Some("--manifest") => {
                let value = args
                    .next()
                    .ok_or_else(|| "--manifest needs a file argument".to_string())?;
                set_manifest(&mut manifest, PathBuf::from(value))?;
            }
            Some("--theme") => {
                let value = args
                    .next()
                    .ok_or_else(|| "--theme needs a name argument".to_string())?;
                kind = value
                    .to_str()
                    .unwrap_or_default()
                    .parse()
                    .map_err(|e: String| format!("{e}\n\n{USAGE}"))?;
            }
            Some(s) if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("unknown option {s:?}\n\n{USAGE}"));
            }
            _ => set_manifest(&mut manifest, PathBuf::from(arg))?,
        }
    }
    Ok((manifest, kind))
}

fn set_manifest(slot: &mut Option<PathBuf>, path: PathBuf) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("more than one layout manifest given\n\n{USAGE}"));
    }
    *slot = Some(path);
    Ok(())
}

/// The window/taskbar icon, drawn in code so no binary asset is committed:
/// an SD card (beveled corner, gold contacts) whose body carries four slot
/// stripes in the app's slot-state colors.
fn app_icon() -> egui::IconData {
    const W: usize = 64;
    const H: usize = 64;
    let mut rgba = vec![0u8; W * H * 4];

    // The classic SD bevel: skip pixels past a diagonal in the top-right.
    // `inset` pulls the diagonal inward so the border shows along it.
    fn cut(x: usize, y: usize, inset: i32) -> bool {
        x as i32 - (40 - inset) > y as i32 - 6
    }
    let mut fill = |x0: usize, y0: usize, x1: usize, y1: usize, inset: i32, c: [u8; 4]| {
        for y in y0..y1 {
            for x in x0..x1 {
                if cut(x, y, inset) {
                    continue;
                }
                let i = (y * W + x) * 4;
                rgba[i..i + 4].copy_from_slice(&c);
            }
        }
    };

    const EDGE: [u8; 4] = [0x51, 0x87, 0xbd, 0xff];
    const BODY: [u8; 4] = [0x22, 0x3a, 0x55, 0xff];
    const GOLD: [u8; 4] = [0xd9, 0xa8, 0x21, 0xff];
    const GREEN: [u8; 4] = [0x38, 0xa0, 0x38, 0xff];
    const BLUE: [u8; 4] = [0x5a, 0x9b, 0xd8, 0xff];
    const ORANGE: [u8; 4] = [0xd0, 0x80, 0x20, 0xff];
    const GRAY: [u8; 4] = [0x6e, 0x78, 0x84, 0xff];

    fill(12, 4, 52, 60, 0, EDGE); // card outline
    fill(15, 7, 49, 57, 4, BODY); // card body
    for i in 0..5 {
        // contact pads along the top
        let x = 18 + i * 6;
        fill(x, 9, x + 3, 16, 4, GOLD);
    }
    for (i, color) in [GREEN, BLUE, ORANGE, GRAY].into_iter().enumerate() {
        // one stripe per slot state: matches / written / modified / empty
        let y = 22 + i * 8;
        fill(18, y, 46, y + 5, 0, color);
    }

    egui::IconData {
        rgba,
        width: W as u32,
        height: H as u32,
    }
}

fn main() -> ExitCode {
    let (manifest, theme_kind) = match parse_args() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::from(1);
        }
    };
    theme::init(theme_kind);
    // Restore the last window size the user chose (persisted in ~/.sdslot);
    // the default is wide enough for the slot table's fixed columns plus
    // card margins without horizontal clipping.
    let saved = settings::Settings::load();
    let size = [
        saved.window_width.clamp(760.0, 8192.0),
        saved.window_height.clamp(460.0, 8192.0),
    ];
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("sdslot {}", sdslot_core::VERSION_FULL))
            .with_inner_size(size)
            .with_min_inner_size([760.0, 460.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    match eframe::run_native(
        "sdslot",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(app::App::new(
                manifest,
                cc.egui_ctx.clone(),
                saved,
            )))
        }),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
