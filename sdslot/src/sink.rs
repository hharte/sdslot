// SPDX-License-Identifier: MIT OR Apache-2.0
//! Event sinks: human progress bars (indicatif), line-delimited JSON on
//! stdout (`--json`), or JSON over a localhost TCP connection
//! (`--json-port`, the channel used by the GUI across elevation boundaries).

use std::io::Write;
use std::net::TcpStream;

use indicatif::{ProgressBar, ProgressStyle};
use sdslot_core::events::{Event, EventSink, OpKind, SlotState};
use sdslot_core::units::format_bytes;
use sdslot_core::{Error, Result};

pub enum Sink {
    Human(HumanSink),
    Json(JsonSink),
}

impl Sink {
    /// Build the sink from the command's output flags. `--json-port` wins
    /// over `--json`; both leave human chatter off stdout.
    pub fn new(json: bool, json_port: Option<&str>) -> Result<Sink> {
        if let Some(addr) = json_port {
            let stream = TcpStream::connect(addr).map_err(|e| {
                Error::Validation(format!("cannot connect --json-port listener {addr}: {e}"))
            })?;
            Ok(Sink::Json(JsonSink {
                out: Box::new(stream),
            }))
        } else if json {
            Ok(Sink::Json(JsonSink {
                out: Box::new(std::io::stdout()),
            }))
        } else {
            Ok(Sink::Human(HumanSink {
                bar: None,
                phase_done: 0,
                slot_len: 0,
            }))
        }
    }

    pub fn is_json(&self) -> bool {
        matches!(self, Sink::Json(_))
    }
}

impl EventSink for Sink {
    fn emit(&mut self, ev: &Event) {
        match self {
            Sink::Human(h) => h.emit(ev),
            Sink::Json(j) => j.emit(ev),
        }
    }
}

pub struct JsonSink {
    out: Box<dyn Write>,
}

impl EventSink for JsonSink {
    fn emit(&mut self, ev: &Event) {
        if let Ok(line) = serde_json::to_string(ev) {
            let _ = writeln!(self.out, "{line}");
            let _ = self.out.flush();
        }
    }
}

pub struct HumanSink {
    bar: Option<ProgressBar>,
    /// Bytes already accounted for by slots that finished earlier in the
    /// current phase (write pass, its verify pass, a wipe, a status scan, …).
    phase_done: u64,
    /// `bytes` of the slot currently in progress, folded into `phase_done`
    /// when it ends.
    slot_len: u64,
}

fn state_str(state: SlotState) -> &'static str {
    match state {
        SlotState::Matches => "matches",
        SlotState::Modified => "MODIFIED (extract before overwriting!)",
        SlotState::Differs => "differs from manifest image",
        SlotState::Wiped => "wiped (all zeros)",
        SlotState::Unknown => "-",
    }
}

impl EventSink for HumanSink {
    fn emit(&mut self, ev: &Event) {
        match ev {
            Event::Plan { .. } => {} // the CLI prints its own plan preview
            Event::PhaseStart { bytes, .. } => {
                // One bar per phase (not per slot): its position tracks
                // bytes across every slot in the pass, so indicatif's own
                // {elapsed}/{bytes_per_sec} read as the whole pass's timing
                // instead of restarting at each slot.
                if let Some(old) = self.bar.take() {
                    old.finish_and_clear();
                }
                let bar = ProgressBar::new(*bytes);
                bar.set_style(
                    ProgressStyle::with_template(
                        "{msg:14} [{bar:32}] {bytes}/{total_bytes} {bytes_per_sec} ({elapsed})",
                    )
                    .expect("progress template")
                    .progress_chars("=> "),
                );
                self.bar = Some(bar);
                self.phase_done = 0;
                self.slot_len = 0;
            }
            Event::SlotStart {
                op,
                bank,
                unit,
                bytes,
            } => {
                self.slot_len = *bytes;
                if let Some(bar) = &self.bar {
                    bar.set_message(format!("{} {bank}:{unit}", op.verb()));
                    bar.set_position(self.phase_done);
                }
            }
            Event::Progress { bytes_done, .. } => {
                if let Some(bar) = &self.bar {
                    bar.set_position(self.phase_done + bytes_done);
                }
            }
            Event::SlotEnd {
                op,
                bank,
                unit,
                ok,
                detail,
            } => {
                self.phase_done += self.slot_len;
                let outcome = if *ok { "ok" } else { "FAILED" };
                let line = match detail {
                    Some(d) if !*ok => format!("{op} {bank}:{unit} {outcome}: {d}"),
                    _ => format!("{op} {bank}:{unit} {outcome}"),
                };
                match &self.bar {
                    // Status hashing's outcome is the SlotStatus row that
                    // follows; printing one here too would just be noise.
                    Some(bar) if *op != OpKind::Status => {
                        bar.set_position(self.phase_done);
                        // bar.println() buffers internally and can lose the
                        // line when the bar finishes right after (seen in a
                        // piped/non-tty run); suspend + a real eprintln! is
                        // reliable there.
                        bar.suspend(|| eprintln!("{line}"));
                    }
                    Some(bar) => bar.set_position(self.phase_done),
                    None if *op != OpKind::Status => eprintln!("{line}"),
                    None => {}
                }
            }
            Event::SlotStatus {
                bank,
                unit,
                state,
                name,
                length,
                ..
            } => {
                let name = name.as_deref().unwrap_or("(unnamed)");
                let len = match length {
                    Some(l) => format_bytes(*l),
                    None => String::new(),
                };
                let line = format!(
                    "  [{unit:>2}] {bank}:{unit:<3} {name:<24} {:<40} {len}",
                    state_str(*state)
                );
                // Status rows are stdout output (scripts grep them), unlike
                // the bar itself and every other human message here; hide
                // the bar around the print rather than route through its
                // (stderr) println.
                match &self.bar {
                    Some(bar) => {
                        bar.suspend(|| println!("{line}"));
                    }
                    None => println!("{line}"),
                }
            }
            Event::Device { .. } => {} // `list` prints its own table
            Event::Done { ok, detail } => {
                if let Some(bar) = self.bar.take() {
                    bar.finish_and_clear();
                }
                match (ok, detail) {
                    (true, Some(d)) => eprintln!("done: {d}"),
                    (true, None) => eprintln!("done"),
                    (false, Some(d)) => eprintln!("failed: {d}"),
                    (false, None) => eprintln!("failed"),
                }
            }
            Event::Error { message } => {
                if let Some(bar) = self.bar.take() {
                    bar.finish_and_clear();
                }
                eprintln!("error: {message}");
            }
        }
    }
}
