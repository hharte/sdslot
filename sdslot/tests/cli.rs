// SPDX-License-Identifier: MIT OR Apache-2.0
//! CLI integration tests (design §9): run the real binary against
//! file-backed devices, check exit codes (0/1/3), the plan/confirmation
//! gate, and the JSON event contract the GUI depends on.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sdslot")
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let alpha: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.path().join("alpha.img"), alpha).unwrap();
        let beta: Vec<u8> = (0..1u32 << 20).map(|i| (i % 241) as u8).collect();
        std::fs::write(dir.path().join("beta.img"), beta).unwrap();
        std::fs::write(
            dir.path().join("card.toml"),
            r#"
sector_size = 512
toc = "8MiB"

[[bank]]
name = "rl"
base = "16MiB"
slot_size = "1MiB"
units = 4

  [[bank.slot]]
  unit = 0
  name = "alpha"
  image = "alpha.img"

  [[bank.slot]]
  unit = 2
  name = "beta"
  image = "beta.img"
"#,
        )
        .unwrap();
        Fixture { dir }
    }

    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .expect("run sdslot")
    }
}

fn stdout_lines(out: &Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad JSON line {l:?}: {e}")))
        .collect()
}

fn events_of(lines: &[serde_json::Value], kind: &str) -> Vec<serde_json::Value> {
    lines
        .iter()
        .filter(|v| v["event"] == kind)
        .cloned()
        .collect()
}

#[test]
fn json_write_status_read_verify_flow() {
    let f = Fixture::new();
    let card = f.path("card.img");
    let manifest = f.path("card.toml");

    // Write with --json: plan, slot_start/progress/slot_end, done events.
    let out = f.run(&[
        "write",
        "--device",
        &card,
        "--manifest",
        &manifest,
        "--verify",
        "--yes",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines = stdout_lines(&out);

    let plans = events_of(&lines, "plan");
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0]["schema"], 2);
    assert_eq!(plans[0]["sector_size"], 512);
    let ops = plans[0]["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0]["bank"], "rl");
    assert_eq!(ops[0]["offset"], 16 << 20);
    assert_eq!(ops[0]["bytes"], 700_000);

    assert!(!events_of(&lines, "progress").is_empty());
    let ends = events_of(&lines, "slot_end");
    // 2 writes + 2 verifies, all ok.
    assert_eq!(ends.len(), 4);
    assert!(ends.iter().all(|e| e["ok"] == true));
    let done = events_of(&lines, "done");
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["ok"], true);

    // Status --json: slot_status events with states.
    let out = f.run(&[
        "status",
        "--device",
        &card,
        "--manifest",
        &manifest,
        "--json",
    ]);
    assert!(out.status.success());
    let lines = stdout_lines(&out);
    let statuses = events_of(&lines, "slot_status");
    assert_eq!(statuses.len(), 4); // all units of the bank
    let s0 = statuses.iter().find(|s| s["unit"] == 0).unwrap();
    assert_eq!(s0["state"], "matches");
    assert_eq!(s0["name"], "alpha");
    assert_eq!(s0["length"], 700_000);

    // TOC-only status (no manifest).
    let out = f.run(&["status", "--device", &card, "--json"]);
    assert!(out.status.success());
    let statuses = events_of(&stdout_lines(&out), "slot_status");
    assert_eq!(statuses.len(), 2);

    // Read back rl:0 with --length toc and compare bytes.
    let extracted = f.path("out.img");
    let out = f.run(&[
        "read",
        "--device",
        &card,
        "--manifest",
        &manifest,
        "--slot",
        "rl:0",
        "-o",
        &extracted,
        "--length",
        "toc",
        "--json",
    ]);
    assert!(out.status.success());
    assert_eq!(
        std::fs::read(&extracted).unwrap(),
        std::fs::read(f.dir.path().join("alpha.img")).unwrap()
    );

    // Verify OK -> exit 0.
    let out = f.run(&[
        "verify",
        "--device",
        &card,
        "--manifest",
        &manifest,
        "--json",
    ]);
    assert!(out.status.success());

    // Tamper -> verify exit code 3, error event present.
    let mut bytes = std::fs::read(&card).unwrap();
    bytes[(16 << 20) + 5] ^= 0xff;
    std::fs::write(&card, &bytes).unwrap();
    let out = f.run(&[
        "verify",
        "--device",
        &card,
        "--manifest",
        &manifest,
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(3));
    let lines = stdout_lines(&out);
    assert!(!events_of(&lines, "error").is_empty());
    let done = events_of(&lines, "done");
    assert_eq!(done[0]["ok"], false);

    // Wipe rl:0, then TOC status shows only beta.
    let out = f.run(&[
        "wipe",
        "--device",
        &card,
        "--manifest",
        &manifest,
        "--slot",
        "rl:0",
        "--yes",
        "--json",
    ]);
    assert!(out.status.success());
    let out = f.run(&["status", "--device", &card, "--json"]);
    let statuses = events_of(&stdout_lines(&out), "slot_status");
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0]["unit"], 2);
}

#[test]
fn json_without_yes_refuses_destructive_ops() {
    let f = Fixture::new();
    let out = f.run(&[
        "write",
        "--device",
        &f.path("card.img"),
        "--manifest",
        &f.path("card.toml"),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(1));
    // The refusal itself must arrive as a JSON error event.
    let lines = stdout_lines(&out);
    assert!(!events_of(&lines, "error").is_empty());
}

#[test]
fn eject_on_file_target_warns_but_write_succeeds() {
    let f = Fixture::new();
    let out = f.run(&[
        "write",
        "--device",
        &f.path("card.img"),
        "--manifest",
        &f.path("card.toml"),
        "--eject",
        "--yes",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines = stdout_lines(&out);
    // The eject is skipped for a regular file, reported as a note event —
    // the write itself still completes and reports done ok.
    let notes = events_of(&lines, "note");
    assert_eq!(notes.len(), 1);
    assert!(notes[0]["message"]
        .as_str()
        .unwrap()
        .contains("--eject ignored"));
    let done = events_of(&lines, "done");
    assert_eq!(done[0]["ok"], true);
}

#[test]
fn standalone_eject_refuses_a_file_target() {
    let f = Fixture::new();
    std::fs::write(f.dir.path().join("card.img"), [0u8; 512]).unwrap();
    // Unlike write's best-effort --eject, the standalone command has
    // nothing else to succeed at: a non-ejectable target is a hard error.
    let out = f.run(&["eject", "--device", &f.path("card.img"), "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let lines = stdout_lines(&out);
    let errors = events_of(&lines, "error");
    assert!(errors[0]["message"]
        .as_str()
        .unwrap()
        .contains("not an ejectable device"));
    let done = events_of(&lines, "done");
    assert_eq!(done[0]["ok"], false);
}

#[test]
fn usage_errors_exit_1() {
    let f = Fixture::new();
    // Unknown slot reference.
    let out = f.run(&[
        "write",
        "--device",
        &f.path("card.img"),
        "--manifest",
        &f.path("card.toml"),
        "--slot",
        "nosuch:0",
        "--yes",
    ]);
    assert_eq!(out.status.code(), Some(1));
    // Bad subcommand usage (clap) also exits 1 per the design.
    let out = f.run(&["write"]);
    assert_eq!(out.status.code(), Some(1));
    // --help exits 0.
    let out = f.run(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn confirmation_prompt_accepts_yes_and_aborts_otherwise() {
    use std::io::Write;
    use std::process::Stdio;
    let f = Fixture::new();
    let card = f.path("card.img");
    let manifest = f.path("card.toml");

    let run_with_stdin = |input: &str| {
        let mut child = Command::new(bin())
            .args(["write", "--device", &card, "--manifest", &manifest])
            .current_dir(f.dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };

    // Typing anything but "yes" aborts with a validation exit.
    let out = run_with_stdin("nah\n");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("aborted"));

    // Typing "yes" proceeds; the plan preview names the slots.
    let out = run_with_stdin("yes\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("write rl:0"), "{stdout}");
}

#[test]
fn device_access_errors_exit_2() {
    let f = Fixture::new();
    // A nonexistent file-backed device cannot be opened for reading.
    let out = f.run(&[
        "status",
        "--device",
        &f.path("does-not-exist.img"),
        "--manifest",
        &f.path("card.toml"),
    ]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn flag_validation_errors() {
    let f = Fixture::new();
    let card = f.path("card.img");
    let manifest = f.path("card.toml");
    // Chunk size must be a sector multiple.
    let out = f.run(&[
        "write",
        "--device",
        &card,
        "--manifest",
        &manifest,
        "--chunk-size",
        "1000",
        "--yes",
    ]);
    assert_eq!(out.status.code(), Some(1));
    // The flat-image assembler refuses raw device paths.
    let out = f.run(&[
        "image",
        "--manifest",
        &manifest,
        "-o",
        "\\\\.\\PhysicalDrive9",
        "--yes",
    ]);
    assert_eq!(out.status.code(), Some(1));
    // Unknown RTL format.
    let out = f.run(&[
        "export-rtl",
        "--manifest",
        &manifest,
        "-o",
        "-",
        "--format",
        "vhdl",
    ]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn export_rtl_alternate_formats() {
    let f = Fixture::new();
    let manifest = f.path("card.toml");
    let text = |fmt: &str| {
        let out = f.run(&[
            "export-rtl",
            "--manifest",
            &manifest,
            "-o",
            "-",
            "--format",
            fmt,
        ]);
        assert!(out.status.success(), "format {fmt}");
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    assert!(text("rs").contains("pub const RL_BASE_LBA: u32"));
    assert!(text("h").contains("#define RL_SLOT_SHIFT"));
    let sv = text("sv");
    assert!(sv.contains("package") && sv.contains("endpackage"));
}

#[test]
fn list_runs_in_both_output_modes() {
    let f = Fixture::new();
    // Real enumeration, metadata-only; the set may be empty on CI but the
    // command must succeed in both human and JSON modes.
    assert!(f.run(&["list"]).status.success());
    let out = f.run(&["list", "--json"]);
    assert!(out.status.success());
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert!(
            v["event"] == "device" || v["event"] == "done",
            "unexpected event {v}"
        );
    }
}

#[test]
fn human_output_flow() {
    let f = Fixture::new();
    let card = f.path("card.img");
    let manifest = f.path("card.toml");
    let out = f.run(&["write", "--device", &card, "--manifest", &manifest, "--yes"]);
    assert!(out.status.success());

    // Human status table renders states and sizes.
    let out = f.run(&["status", "--device", &card, "--manifest", &manifest]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("matches"), "{stdout}");
    // TOC-only human status also works.
    let out = f.run(&["status", "--device", &card]);
    assert!(out.status.success());

    // Human wipe narrates per-slot outcomes.
    let out = f.run(&[
        "wipe",
        "--device",
        &card,
        "--manifest",
        &manifest,
        "--slot",
        "rl:0",
        "--yes",
    ]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("wipe rl:0 ok"));

    // Wiped slot reports as wiped in the human table.
    let out = f.run(&["status", "--device", &card, "--manifest", &manifest]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("wiped"));

    // Human verify of the wiped slot fails with exit 3.
    let out = f.run(&[
        "verify",
        "--device",
        &card,
        "--manifest",
        &manifest,
        "--slot",
        "rl:0",
    ]);
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn multi_slot_extract_to_dir() {
    let f = Fixture::new();
    let card = f.path("card.img");
    let manifest = f.path("card.toml");
    let out = f.run(&["write", "--device", &card, "--manifest", &manifest, "--yes"]);
    assert!(out.status.success());

    let dir = f.path("extracted");
    let out = f.run(&[
        "read",
        "--device",
        &card,
        "--manifest",
        &manifest,
        "--slot",
        "rl:0",
        "--slot",
        "rl:2",
        "--out-dir",
        &dir,
        "--length",
        "toc",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(Path::new(&dir).join("rl0_alpha.img")).unwrap(),
        std::fs::read(f.dir.path().join("alpha.img")).unwrap()
    );
    assert_eq!(
        std::fs::read(Path::new(&dir).join("rl2_beta.img")).unwrap(),
        std::fs::read(f.dir.path().join("beta.img")).unwrap()
    );

    // -o with multiple slots is refused.
    let out = f.run(&[
        "read",
        "--device",
        &card,
        "--manifest",
        &manifest,
        "--slot",
        "rl:0",
        "--slot",
        "rl:2",
        "-o",
        &f.path("x.img"),
    ]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn export_rtl_writes_header() {
    let f = Fixture::new();
    let out_path = f.path("layout.vh");
    let out = f.run(&[
        "export-rtl",
        "--manifest",
        &f.path("card.toml"),
        "-o",
        &out_path,
    ]);
    assert!(out.status.success());
    let text = std::fs::read_to_string(&out_path).unwrap();
    assert!(
        text.contains("localparam RL_BASE_LBA   = 32'h0000_8000;"),
        "{text}"
    );
    assert!(text.contains("localparam RL_SLOT_SHIFT = 11;"), "{text}");
}

#[test]
fn image_assembles_flat_card() {
    let f = Fixture::new();
    let flat = f.path("flat.img");
    let out = f.run(&[
        "image",
        "--manifest",
        &f.path("card.toml"),
        "-o",
        &flat,
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Full span: rl bank ends at 16MiB + 4*1MiB = 20MiB.
    assert_eq!(std::fs::metadata(&flat).unwrap().len(), 20 << 20);
    let bytes = std::fs::read(&flat).unwrap();
    let alpha = std::fs::read(f.dir.path().join("alpha.img")).unwrap();
    assert_eq!(&bytes[16 << 20..(16 << 20) + 700_000], &alpha[..]);
    // TOC magic present at 8MiB.
    assert_eq!(&bytes[8 << 20..(8 << 20) + 8], b"SDSLTOC\x01");

    // Existing output without --yes is refused.
    let out = f.run(&["image", "--manifest", &f.path("card.toml"), "-o", &flat]);
    assert_eq!(out.status.code(), Some(1));

    // A slot override replaces just that slot.
    let out = f.run(&[
        "image",
        "--manifest",
        &f.path("card.toml"),
        "-o",
        &flat,
        "--slot",
        &format!("rl:1={}", f.path("beta.img")),
        "--yes",
    ]);
    assert!(out.status.success());
    let bytes = std::fs::read(&flat).unwrap();
    let beta = std::fs::read(f.dir.path().join("beta.img")).unwrap();
    let slot1 = (16 << 20) + (1 << 20);
    assert_eq!(&bytes[slot1..slot1 + (1 << 20)], &beta[..]);
    // Rebuilt from scratch: alpha is NOT in the new image (only rl:1 named).
    assert!(bytes[16 << 20..(16 << 20) + 1000].iter().all(|&b| b == 0));
}

#[test]
fn relative_paths_resolve_against_manifest_dir() {
    // Run from a different cwd; manifest-relative images must still work.
    let f = Fixture::new();
    let other = tempfile::tempdir().unwrap();
    let card: PathBuf = f.dir.path().join("card.img");
    let out = Command::new(bin())
        .args([
            "write",
            "--device",
            card.to_str().unwrap(),
            "--manifest",
            Path::new(&f.path("card.toml")).to_str().unwrap(),
            "--yes",
        ])
        .current_dir(other.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
