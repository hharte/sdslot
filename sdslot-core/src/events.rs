// SPDX-License-Identifier: MIT OR Apache-2.0
//! Structured progress events. One stream feeds three consumers: the CLI's
//! human progress bars, `--json` line-delimited output, and the GUI over a
//! localhost socket (design §3, §8.1). The serialized form is the versioned
//! contract between the CLI and GUI.

use serde::{Deserialize, Serialize};

/// Bump when the serialized event schema changes incompatibly.
/// v2: `phase_start` events announce each pass's byte total, and every
/// slot — status hashing included — gets a uniform `slot_end`.
pub const EVENT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Write,
    Read,
    Wipe,
    Verify,
    /// Re-hashing a slot's on-card content for `status`.
    Status,
}

impl OpKind {
    /// Human display verb, shared by every frontend ("status" hashing reads
    /// as "reading" to the user).
    pub fn verb(self) -> &'static str {
        match self {
            OpKind::Write => "write",
            OpKind::Read => "read",
            OpKind::Wipe => "wipe",
            OpKind::Verify => "verify",
            OpKind::Status => "reading",
        }
    }
}

impl std::fmt::Display for OpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            OpKind::Write => "write",
            OpKind::Read => "read",
            OpKind::Wipe => "wipe",
            OpKind::Verify => "verify",
            OpKind::Status => "status",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanOp {
    pub op: OpKind,
    pub bank: String,
    pub unit: u32,
    /// Absolute device byte offset of the slot.
    pub offset: u64,
    /// Bytes that will be transferred.
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

/// On-card state of a slot as reported by `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotState {
    /// Nothing known about this slot (no TOC record, no manifest image).
    Unknown,
    /// Content hash matches the TOC record / manifest image.
    Matches,
    /// TOC record exists but the on-card hash differs: the FPGA (or someone)
    /// wrote to the media since the last host write. Extract before overwriting.
    Modified,
    /// No TOC; content differs from the manifest image.
    Differs,
    /// The slot's probed content is all zeros — a blank pack, distinct from
    /// merely differing content.
    Wiped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// First event of every operation.
    Plan {
        schema: u32,
        device: String,
        sector_size: u32,
        ops: Vec<PlanOp>,
    },
    /// A pass over some set of slots is starting; `bytes` is the total the
    /// whole pass will transfer. A multi-slot write emits one for the write
    /// pass and another for its verify pass, so frontends can render
    /// aggregate progress without inferring phase changes.
    PhaseStart {
        op: OpKind,
        bytes: u64,
    },
    SlotStart {
        op: OpKind,
        bank: String,
        unit: u32,
        bytes: u64,
    },
    Progress {
        bank: String,
        unit: u32,
        bytes_done: u64,
        bytes_total: u64,
    },
    SlotEnd {
        op: OpKind,
        bank: String,
        unit: u32,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    SlotStatus {
        bank: String,
        unit: u32,
        state: SlotState,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        length: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
    },
    Device {
        path: String,
        model: String,
        bus: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        size_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        removable: Option<bool>,
        system: bool,
    },
    Done {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Error {
        message: String,
    },
    /// A human-readable side note that is not a slot outcome or an error —
    /// e.g. the post-write eject result. Additive in schema v2: an older
    /// consumer drops the unknown line harmlessly.
    Note {
        message: String,
    },
}

/// Receives events as an operation proceeds.
pub trait EventSink {
    fn emit(&mut self, ev: &Event);
}

/// Discards everything; for library callers that don't care.
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _ev: &Event) {}
}

impl<F: FnMut(&Event)> EventSink for F {
    fn emit(&mut self, ev: &Event) {
        self(ev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_kind_display_names() {
        let expect = [
            (OpKind::Write, "write"),
            (OpKind::Read, "read"),
            (OpKind::Wipe, "wipe"),
            (OpKind::Verify, "verify"),
            (OpKind::Status, "status"),
        ];
        for (kind, name) in expect {
            assert_eq!(kind.to_string(), name);
        }
    }

    #[test]
    fn slot_state_wire_names_are_stable() {
        // The GUI and scripts match on these strings; changing them breaks
        // the event contract.
        let expect = [
            (SlotState::Unknown, "\"unknown\""),
            (SlotState::Matches, "\"matches\""),
            (SlotState::Modified, "\"modified\""),
            (SlotState::Differs, "\"differs\""),
            (SlotState::Wiped, "\"wiped\""),
        ];
        for (state, wire) in expect {
            assert_eq!(serde_json::to_string(&state).unwrap(), wire);
        }
        assert_eq!(EVENT_SCHEMA_VERSION, 2);
    }

    #[test]
    fn display_verbs() {
        assert_eq!(OpKind::Status.verb(), "reading");
        assert_eq!(OpKind::Write.verb(), "write");
        assert_eq!(OpKind::Read.verb(), "read");
        assert_eq!(OpKind::Wipe.verb(), "wipe");
        assert_eq!(OpKind::Verify.verb(), "verify");
        let ev = Event::PhaseStart {
            op: OpKind::Write,
            bytes: 42,
        };
        assert!(serde_json::to_string(&ev)
            .unwrap()
            .contains("\"event\":\"phase_start\""));
    }

    #[test]
    fn null_and_closure_sinks() {
        let ev = Event::Done {
            ok: true,
            detail: None,
        };
        NullSink.emit(&ev);
        let mut count = 0;
        let mut counting = |_e: &Event| count += 1;
        counting.emit(&ev);
        counting.emit(&ev);
        assert_eq!(count, 2);
    }

    #[test]
    fn note_event_round_trips() {
        let ev = Event::Note {
            message: "ejected".into(),
        };
        let line = serde_json::to_string(&ev).unwrap();
        assert!(line.contains("\"event\":\"note\""));
        let back: Event = serde_json::from_str(&line).unwrap();
        match back {
            Event::Note { message } => assert_eq!(message, "ejected"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn events_round_trip_as_json() {
        let ev = Event::Progress {
            bank: "rl".into(),
            unit: 1,
            bytes_done: 1024,
            bytes_total: 4096,
        };
        let line = serde_json::to_string(&ev).unwrap();
        assert!(line.contains("\"event\":\"progress\""));
        let back: Event = serde_json::from_str(&line).unwrap();
        match back {
            Event::Progress { bytes_done, .. } => assert_eq!(bytes_done, 1024),
            _ => panic!("wrong variant"),
        }
    }
}
