// SPDX-License-Identifier: MIT OR Apache-2.0
//! Pure CLI-event folding for one running operation (design §8.3): turns the
//! JSON event stream into progress/aggregate state, slot-map updates, and
//! log lines. No egui dependency, so it's unit-testable without a GUI
//! context.

use std::time::{Duration, Instant};

use sdslot_core::events::{Event, OpKind, SlotState};

use crate::backend::GuiMsg;

/// What the slot map shows for a slot. `Written` and `Busy` are GUI-only
/// states — the engine's event schema never carries them. `Written`: the
/// write succeeded but verification hasn't confirmed it yet. `Busy`: the
/// operation is transferring this slot right now.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewState {
    Core(SlotState),
    Written,
    Busy(OpKind),
}

#[derive(Clone)]
pub struct SlotView {
    pub state: ViewState,
    pub name: Option<String>,
    pub length: Option<u64>,
}

/// Live slot-map update derived from a write/verify/wipe result.
pub enum SlotOutcome {
    Set {
        state: ViewState,
        length: Option<u64>,
    },
    /// The slot was wiped; it is a blank pack again.
    Cleared,
}

/// One slot-map change folded out of the event stream.
pub enum SlotUpdate {
    Status(SlotView),
    Outcome(SlotOutcome),
}

/// Does this slot still need a write? Only a scanned `Matches` says the card
/// already holds the slot's image; every other state — differs, modified,
/// wiped, unknown, or a slot the scan never reported — does not. `Written`
/// also counts as needing one: the data landed but nothing has confirmed it.
pub fn needs_write(state: Option<ViewState>) -> bool {
    !matches!(state, Some(ViewState::Core(SlotState::Matches)))
}

/// "3s", "1m 05s", "2h 03m" — the progress bar's elapsed-time readout.
pub fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    if total < 60 {
        format!("{total}s")
    } else if total < 3600 {
        format!("{}m {:02}s", total / 60, total % 60)
    } else {
        format!("{}h {:02}m", total / 3600, (total % 3600) / 60)
    }
}

/// What `OpFold::apply` produces from one message: log lines to append, and
/// keyed slot-map updates to apply, both in arrival order.
pub type FoldOutput = (Vec<LogMsg>, Vec<((String, u32), SlotUpdate)>);

/// A pending log write. `Complete` appends its suffix to the matching
/// in-progress line ("write rl:0… Ok.") when that line is still the newest,
/// and falls back to a full line otherwise.
pub enum LogMsg {
    New(String),
    Complete {
        start: String,
        suffix: String,
        fallback: String,
    },
}

/// Folds one running operation's CLI event stream into progress/aggregate
/// state. Pure — no egui types — so it is unit-testable without a GUI
/// context.
pub struct OpFold {
    /// (bank, unit, done, total) for the slot currently transferring.
    pub progress: Option<(String, u32, u64, u64)>,
    /// Verb for the progress bar ("write", "reading", …) from the last
    /// SlotStart, since one operation can span phases (write then verify).
    pub verb: String,
    /// Any event has arrived — the elevation prompt has been answered and
    /// the CLI is running.
    pub saw_event: bool,
    /// Total bytes across the current pass, from its `PhaseStart` event: the
    /// bar shows aggregate progress instead of restarting per slot. A
    /// multi-slot write's verify pass gets its own `PhaseStart`, so the
    /// aggregate naturally restarts at the write→verify phase switch.
    pub agg_total: Option<u64>,
    /// Bytes of the already-completed slots in the current pass.
    pub agg_done: u64,
    /// The op kind the aggregate currently measures.
    agg_kind: Option<OpKind>,
    /// When the current pass's `PhaseStart` arrived — the clock the progress
    /// bar's elapsed-time/transfer-rate readout runs from. Resets with the
    /// aggregate at each phase switch (write → its verify pass, etc.).
    pub phase_started_at: Option<Instant>,
    /// The user asked to terminate this operation.
    pub cancel_requested: bool,
    pub finished: bool,
    pub exit_code: Option<i32>,
}

impl OpFold {
    pub fn new(label: &str) -> OpFold {
        OpFold {
            progress: None,
            verb: label.to_string(),
            saw_event: false,
            agg_total: None,
            agg_done: 0,
            agg_kind: None,
            phase_started_at: None,
            cancel_requested: false,
            finished: false,
            exit_code: None,
        }
    }

    pub fn request_cancel(&mut self) {
        self.cancel_requested = true;
    }

    /// Elapsed time since the current pass's `PhaseStart` and its transfer
    /// rate given how many bytes have moved so far — the progress bar's
    /// "elapsed (rate)" readout. `None` before the first `PhaseStart`.
    pub fn phase_elapsed_and_rate(&self, bytes_so_far: u64) -> Option<(Duration, f64)> {
        let elapsed = self.phase_started_at?.elapsed();
        let rate = bytes_so_far as f64 / elapsed.as_secs_f64().max(0.001);
        Some((elapsed, rate))
    }

    /// Fold one message from the CLI subprocess. Returns log lines to
    /// append and slot-map updates to apply, in arrival order.
    pub fn apply(&mut self, msg: GuiMsg) -> FoldOutput {
        let mut lines = Vec::new();
        let mut updates = Vec::new();
        match msg {
            GuiMsg::Event(ev) => {
                self.saw_event = true;
                self.apply_event(ev, &mut lines, &mut updates);
            }
            GuiMsg::Note(n) => lines.push(LogMsg::New(n)),
            GuiMsg::Exited(code) => {
                self.finished = true;
                self.exit_code = code;
                self.progress = None;
                if self.cancel_requested {
                    lines.push(LogMsg::New(
                        "operation canceled — slot contents may be partial; \
                         run Refresh status to re-check"
                            .into(),
                    ));
                } else if code != Some(0) {
                    lines.push(LogMsg::New(format!("CLI exited with {code:?}")));
                }
            }
        }
        (lines, updates)
    }

    fn apply_event(
        &mut self,
        ev: Event,
        lines: &mut Vec<LogMsg>,
        updates: &mut Vec<((String, u32), SlotUpdate)>,
    ) {
        match ev {
            // Announces the byte total for a whole pass (write, its verify
            // pass, wipe, or status scan): switch the bar to aggregate
            // progress over the pass instead of restarting per slot.
            Event::PhaseStart { op, bytes } => {
                self.agg_total = Some(bytes);
                self.agg_done = 0;
                self.agg_kind = Some(op);
                self.phase_started_at = Some(Instant::now());
            }
            Event::Progress {
                bank,
                unit,
                bytes_done,
                bytes_total,
            } => self.progress = Some((bank, unit, bytes_done, bytes_total)),
            Event::SlotStart {
                op: kind,
                bank,
                unit,
                bytes,
            } => {
                self.progress = Some((bank.clone(), unit, 0, bytes));
                self.verb = kind.verb().to_string();
                // The slot row shows what is happening to it right now.
                // Write/wipe/verify always end in a SlotEnd that replaces
                // this; status rows are handled by the scan's own
                // reset-then-fill flow, and reads don't change slot state.
                if matches!(kind, OpKind::Write | OpKind::Wipe | OpKind::Verify) {
                    updates.push((
                        (bank.clone(), unit),
                        SlotUpdate::Outcome(SlotOutcome::Set {
                            state: ViewState::Busy(kind),
                            length: None,
                        }),
                    ));
                }
                // Status announces every slot; the bar shows it, so don't
                // also spam the log pane.
                if kind != OpKind::Status {
                    lines.push(LogMsg::New(format!("{kind} {bank}:{unit}…")));
                }
            }
            Event::SlotEnd {
                op: kind,
                bank,
                unit,
                ok,
                detail,
            } => {
                // The slot's transfer length, from the progress state being
                // cleared (SlotEnd itself carries no byte count).
                let length = self
                    .progress
                    .as_ref()
                    .filter(|(b, u, _, _)| b == &bank && *u == unit)
                    .map(|(_, _, _, total)| *total);
                // Fold the finished slot into the aggregate for the current
                // pass.
                if self.agg_kind == Some(kind) {
                    self.agg_done += length.unwrap_or(0);
                }
                self.progress = None;
                let key = (bank.clone(), unit);
                match kind {
                    // A successful write shows "written"; only a successful
                    // verify upgrades it to "matches". A failure of either
                    // marks the slot differing.
                    OpKind::Write => {
                        let state = if ok {
                            ViewState::Written
                        } else {
                            ViewState::Core(SlotState::Differs)
                        };
                        updates
                            .push((key, SlotUpdate::Outcome(SlotOutcome::Set { state, length })));
                    }
                    OpKind::Verify => {
                        let state = ViewState::Core(if ok {
                            SlotState::Matches
                        } else {
                            SlotState::Differs
                        });
                        updates
                            .push((key, SlotUpdate::Outcome(SlotOutcome::Set { state, length })));
                    }
                    OpKind::Wipe if ok => {
                        updates.push((key, SlotUpdate::Outcome(SlotOutcome::Cleared)))
                    }
                    _ => {}
                }
                // Status's outcome is the SlotStatus row that follows;
                // completing an in-progress log line here would be noise.
                if kind != OpKind::Status {
                    let outcome = if ok {
                        "Ok.".to_string()
                    } else {
                        match detail {
                            Some(d) => format!("FAILED: {d}"),
                            None => "FAILED".to_string(),
                        }
                    };
                    lines.push(LogMsg::Complete {
                        start: format!("{kind} {bank}:{unit}…"),
                        suffix: format!(" {outcome}"),
                        fallback: format!("{kind} {bank}:{unit} {outcome}"),
                    });
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
                updates.push((
                    (bank, unit),
                    SlotUpdate::Status(SlotView {
                        state: ViewState::Core(state),
                        name,
                        length,
                    }),
                ));
            }
            Event::Error { message } => lines.push(LogMsg::New(format!("error: {message}"))),
            // Side notes (e.g. the post-write eject result) go straight to
            // the log.
            Event::Note { message } => lines.push(LogMsg::New(message)),
            Event::Done { ok, detail } => {
                let mut line = (if ok { "done" } else { "failed" }).to_string();
                if let Some(d) = detail {
                    line.push_str(&format!(": {d}"));
                }
                lines.push(LogMsg::New(line));
            }
            // The CLI prints its own plan preview; the GUI renders the plan
            // through its confirmation modals instead.
            Event::Plan { .. } | Event::Device { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot_view(u: &((String, u32), SlotUpdate)) -> Option<(ViewState, Option<u64>)> {
        match &u.1 {
            SlotUpdate::Status(v) => Some((v.state, v.length)),
            SlotUpdate::Outcome(SlotOutcome::Set { state, length }) => Some((*state, *length)),
            SlotUpdate::Outcome(SlotOutcome::Cleared) => {
                Some((ViewState::Core(SlotState::Wiped), None))
            }
        }
    }

    #[test]
    fn only_a_matching_slot_needs_no_write() {
        assert!(!needs_write(Some(ViewState::Core(SlotState::Matches))));
        for state in [
            SlotState::Differs,
            SlotState::Modified,
            SlotState::Wiped,
            SlotState::Unknown,
        ] {
            assert!(needs_write(Some(ViewState::Core(state))));
        }
        // Written is unconfirmed data, and an unscanned slot says nothing.
        assert!(needs_write(Some(ViewState::Written)));
        assert!(needs_write(None));
    }

    #[test]
    fn format_elapsed_picks_the_right_units() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m 00s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_elapsed(Duration::from_secs(3599)), "59m 59s");
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1h 00m");
        assert_eq!(format_elapsed(Duration::from_secs(3725)), "1h 02m");
    }

    #[test]
    fn phase_start_resets_the_aggregate() {
        let mut fold = OpFold::new("write");
        fold.agg_done = 999;
        let (_, _) = fold.apply(GuiMsg::Event(Event::PhaseStart {
            op: OpKind::Write,
            bytes: 4096,
        }));
        assert_eq!(fold.agg_total, Some(4096));
        assert_eq!(fold.agg_done, 0);
    }

    #[test]
    fn phase_elapsed_and_rate_is_none_before_any_phase() {
        let fold = OpFold::new("write");
        assert!(fold.phase_elapsed_and_rate(0).is_none());
    }

    #[test]
    fn phase_elapsed_and_rate_tracks_from_phase_start() {
        let mut fold = OpFold::new("write");
        fold.apply(GuiMsg::Event(Event::PhaseStart {
            op: OpKind::Write,
            bytes: 1_000_000,
        }));
        let (elapsed, rate) = fold.phase_elapsed_and_rate(500_000).expect("phase started");
        // A few microseconds into the pass: some time has passed, and the
        // rate is a large-but-finite number, not NaN/inf from a div-by-zero.
        assert!(elapsed.as_secs_f64() >= 0.0);
        assert!(rate.is_finite() && rate > 0.0);
    }

    #[test]
    fn write_then_verify_phases_each_get_their_own_aggregate() {
        let mut fold = OpFold::new("write");
        fold.apply(GuiMsg::Event(Event::PhaseStart {
            op: OpKind::Write,
            bytes: 1000,
        }));
        fold.apply(GuiMsg::Event(Event::SlotStart {
            op: OpKind::Write,
            bank: "rl".into(),
            unit: 0,
            bytes: 1000,
        }));
        fold.apply(GuiMsg::Event(Event::SlotEnd {
            op: OpKind::Write,
            bank: "rl".into(),
            unit: 0,
            ok: true,
            detail: None,
        }));
        assert_eq!(fold.agg_done, 1000);

        // The verify pass gets its own PhaseStart and restarts the
        // aggregate rather than continuing to accumulate onto the write
        // pass's total.
        fold.apply(GuiMsg::Event(Event::PhaseStart {
            op: OpKind::Verify,
            bytes: 1000,
        }));
        assert_eq!(fold.agg_done, 0);
        assert_eq!(fold.agg_total, Some(1000));
    }

    #[test]
    fn slot_end_folds_bytes_into_the_aggregate_for_status_too() {
        // Schema v2: status hashing gets a uniform SlotEnd like every other
        // op, so its bytes fold into the aggregate the same way.
        let mut fold = OpFold::new("status");
        fold.apply(GuiMsg::Event(Event::PhaseStart {
            op: OpKind::Status,
            bytes: 2048,
        }));
        fold.apply(GuiMsg::Event(Event::SlotStart {
            op: OpKind::Status,
            bank: "rl".into(),
            unit: 0,
            bytes: 2048,
        }));
        let (lines, _) = fold.apply(GuiMsg::Event(Event::SlotEnd {
            op: OpKind::Status,
            bank: "rl".into(),
            unit: 0,
            ok: true,
            detail: None,
        }));
        assert_eq!(fold.agg_done, 2048);
        assert_eq!(fold.progress, None);
        // No log line for status's SlotEnd — the SlotStatus row is the
        // user-visible outcome.
        assert!(lines.is_empty());
    }

    #[test]
    fn write_slot_end_produces_written_outcome() {
        let mut fold = OpFold::new("write");
        fold.apply(GuiMsg::Event(Event::SlotStart {
            op: OpKind::Write,
            bank: "rl".into(),
            unit: 1,
            bytes: 512,
        }));
        let (lines, updates) = fold.apply(GuiMsg::Event(Event::SlotEnd {
            op: OpKind::Write,
            bank: "rl".into(),
            unit: 1,
            ok: true,
            detail: None,
        }));
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            slot_view(&updates[0]),
            Some((ViewState::Written, Some(512)))
        ));
        assert!(matches!(&lines[0], LogMsg::Complete { suffix, .. } if suffix == " Ok."));
    }

    #[test]
    fn failed_verify_marks_the_slot_differing() {
        let mut fold = OpFold::new("verify");
        fold.apply(GuiMsg::Event(Event::SlotStart {
            op: OpKind::Verify,
            bank: "rp".into(),
            unit: 0,
            bytes: 256,
        }));
        let (_, updates) = fold.apply(GuiMsg::Event(Event::SlotEnd {
            op: OpKind::Verify,
            bank: "rp".into(),
            unit: 0,
            ok: false,
            detail: Some("mismatch".into()),
        }));
        assert!(matches!(
            slot_view(&updates[0]),
            Some((ViewState::Core(SlotState::Differs), Some(256)))
        ));
    }

    #[test]
    fn successful_wipe_clears_the_slot() {
        let mut fold = OpFold::new("wipe");
        let (_, updates) = fold.apply(GuiMsg::Event(Event::SlotEnd {
            op: OpKind::Wipe,
            bank: "rl".into(),
            unit: 2,
            ok: true,
            detail: None,
        }));
        assert!(matches!(
            &updates[0].1,
            SlotUpdate::Outcome(SlotOutcome::Cleared)
        ));
    }

    #[test]
    fn note_event_logs_its_message() {
        let mut fold = OpFold::new("write");
        let (lines, updates) = fold.apply(GuiMsg::Event(Event::Note {
            message: "\\\\.\\PhysicalDrive2 ejected — safe to remove".into(),
        }));
        assert!(updates.is_empty());
        assert!(matches!(&lines[0], LogMsg::New(l) if l.contains("ejected")));
    }

    #[test]
    fn exited_marks_finished_and_notes_a_nonzero_code() {
        let mut fold = OpFold::new("write");
        fold.progress = Some(("rl".into(), 0, 10, 100));
        let (lines, _) = fold.apply(GuiMsg::Exited(Some(1)));
        assert!(fold.finished);
        assert_eq!(fold.exit_code, Some(1));
        assert!(fold.progress.is_none());
        assert!(matches!(&lines[0], LogMsg::New(l) if l.contains("CLI exited")));
    }

    #[test]
    fn canceled_exit_gets_its_own_note_not_the_exit_code_line() {
        let mut fold = OpFold::new("write");
        fold.request_cancel();
        let (lines, _) = fold.apply(GuiMsg::Exited(Some(1)));
        assert_eq!(lines.len(), 1);
        assert!(matches!(&lines[0], LogMsg::New(l) if l.contains("canceled")));
    }
}
