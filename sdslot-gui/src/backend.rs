// SPDX-License-Identifier: MIT OR Apache-2.0
//! GUI backend (design §8.1): drives the `sdslot` CLI as a subprocess. The
//! GUI performs no raw device I/O itself. Operations that touch a raw device
//! run the CLI elevated with `--json-port` pointing at a localhost listener
//! we open (stdout pipes cannot cross the UAC boundary on Windows, and
//! macOS `osascript` returns output only on completion); everything else
//! runs unelevated with `--json` on a stdout pipe.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use sdslot_core::device::is_platform_device_path;
use sdslot_core::events::Event;

#[derive(Debug)]
pub enum GuiMsg {
    Event(Event),
    /// Human-readable diagnostic (spawn failures, stderr summary).
    Note(String),
    Exited(Option<i32>),
}

/// Terminates a running operation's CLI subprocess on request. Covers both
/// spawn paths: a directly spawned child (plain ops, and the pkexec /
/// osascript wrappers on Unix), and the elevated process handle returned by
/// ShellExecuteExW on Windows.
#[derive(Clone, Default)]
pub struct Canceller {
    child: Arc<Mutex<Option<Child>>>,
    #[cfg(windows)]
    elevated: Arc<std::sync::atomic::AtomicUsize>,
}

impl Canceller {
    /// Best-effort kill of whatever this operation is running.
    pub fn cancel(&self) -> Result<(), String> {
        let mut found = false;
        let mut errors = Vec::new();
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                found = true;
                if let Err(e) = child.kill() {
                    errors.push(format!("cannot kill CLI process: {e}"));
                }
            }
        }
        #[cfg(windows)]
        {
            use std::sync::atomic::Ordering;
            let handle = self.elevated.load(Ordering::Acquire);
            if handle != 0 {
                found = true;
                let ok = unsafe {
                    windows_sys::Win32::System::Threading::TerminateProcess(
                        handle as windows_sys::Win32::Foundation::HANDLE,
                        1,
                    )
                };
                if ok == 0 {
                    errors.push(format!(
                        "cannot terminate the elevated CLI: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
        }
        if !found {
            return Err("nothing to cancel".into());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

pub struct RunningOp {
    pub rx: Receiver<GuiMsg>,
    /// The equivalent command line, displayed for learning and bug reports.
    pub equivalent: String,
    pub label: String,
    pub canceller: Canceller,
}

/// The CLI binary: a sibling of the GUI executable, falling back to PATH.
pub fn cli_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let name = if cfg!(windows) {
                "sdslot.exe"
            } else {
                "sdslot"
            };
            let candidate = dir.join(name);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("sdslot")
}

fn shell_quote(arg: &str) -> String {
    if arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || c == '"') {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

pub fn equivalent_command(args: &[String]) -> String {
    let mut parts = vec!["sdslot".to_string()];
    parts.extend(args.iter().map(|a| shell_quote(a)));
    parts.join(" ")
}

/// Does this operation's target need elevated raw-device access?
pub fn needs_elevation(device_arg: Option<&str>) -> bool {
    device_arg.is_some_and(is_platform_device_path)
}

/// Enumerate devices in-process via sdslot-core. Enumeration is
/// metadata-only (no media reads, no elevation), so it does not breach the
/// GUI's no-raw-I/O rule — and it avoids spawning the console-subsystem CLI,
/// whose window would flash on every hotplug poll.
pub fn enumerate_local() -> Result<Vec<sdslot_core::device::DeviceInfo>, String> {
    sdslot_core::device::enumerate_devices().map_err(|e| e.to_string())
}

/// Keep console-subsystem CLI children from flashing a console window when
/// launched from the (windowless) GUI.
#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum CliCommand {
    List,
    Status {
        device: String,
        manifest: Option<PathBuf>,
    },
    Write {
        device: String,
        manifest: PathBuf,
        slots: Vec<String>,
        verify: bool,
        yes: bool,
        force: bool,
    },
    Read {
        device: String,
        manifest: PathBuf,
        slots: Vec<String>,
        out: Option<PathBuf>,
        out_dir: Option<PathBuf>,
        length: Option<String>,
    },
    Wipe {
        device: String,
        manifest: PathBuf,
        slots: Vec<String>,
        yes: bool,
        force: bool,
    },
    Verify {
        device: String,
        manifest: PathBuf,
        slots: Vec<String>,
    },
    Image {
        manifest: PathBuf,
        out: PathBuf,
        slots: Vec<String>,
        verify: bool,
        yes: bool,
    },
}

impl CliCommand {
    pub fn label(&self) -> &'static str {
        match self {
            CliCommand::List => "list",
            CliCommand::Status { .. } => "status",
            CliCommand::Write { .. } => "write",
            CliCommand::Read { .. } => "read",
            CliCommand::Wipe { .. } => "wipe",
            CliCommand::Verify { .. } => "verify",
            CliCommand::Image { .. } => "image",
        }
    }

    pub fn device(&self) -> Option<&str> {
        match self {
            CliCommand::List | CliCommand::Image { .. } => None,
            CliCommand::Status { device, .. }
            | CliCommand::Write { device, .. }
            | CliCommand::Read { device, .. }
            | CliCommand::Wipe { device, .. }
            | CliCommand::Verify { device, .. } => Some(device.as_str()),
        }
    }

    pub fn to_args(&self) -> Vec<String> {
        match self {
            CliCommand::List => vec!["list".into()],
            CliCommand::Status { device, manifest } => {
                let mut args = vec!["status".into(), "--device".into(), device.clone()];
                if let Some(m) = manifest {
                    args.push("--manifest".into());
                    args.push(m.display().to_string());
                }
                args
            }
            CliCommand::Write {
                device,
                manifest,
                slots,
                verify,
                yes,
                force,
            } => {
                let mut args = vec![
                    "write".into(),
                    "--device".into(),
                    device.clone(),
                    "--manifest".into(),
                    manifest.display().to_string(),
                ];
                for s in slots {
                    args.push("--slot".into());
                    args.push(s.clone());
                }
                if *verify {
                    args.push("--verify".into());
                }
                if *yes {
                    args.push("--yes".into());
                }
                if *force {
                    args.push("--force".into());
                }
                args
            }
            CliCommand::Read {
                device,
                manifest,
                slots,
                out,
                out_dir,
                length,
            } => {
                let mut args = vec![
                    "read".into(),
                    "--device".into(),
                    device.clone(),
                    "--manifest".into(),
                    manifest.display().to_string(),
                ];
                for s in slots {
                    args.push("--slot".into());
                    args.push(s.clone());
                }
                if let Some(o) = out {
                    args.push("-o".into());
                    args.push(o.display().to_string());
                }
                if let Some(d) = out_dir {
                    args.push("--out-dir".into());
                    args.push(d.display().to_string());
                }
                if let Some(l) = length {
                    args.push("--length".into());
                    args.push(l.clone());
                }
                args
            }
            CliCommand::Wipe {
                device,
                manifest,
                slots,
                yes,
                force,
            } => {
                let mut args = vec![
                    "wipe".into(),
                    "--device".into(),
                    device.clone(),
                    "--manifest".into(),
                    manifest.display().to_string(),
                ];
                for s in slots {
                    args.push("--slot".into());
                    args.push(s.clone());
                }
                if *yes {
                    args.push("--yes".into());
                }
                if *force {
                    args.push("--force".into());
                }
                args
            }
            CliCommand::Verify {
                device,
                manifest,
                slots,
            } => {
                let mut args = vec![
                    "verify".into(),
                    "--device".into(),
                    device.clone(),
                    "--manifest".into(),
                    manifest.display().to_string(),
                ];
                for s in slots {
                    args.push("--slot".into());
                    args.push(s.clone());
                }
                args
            }
            CliCommand::Image {
                manifest,
                out,
                slots,
                verify,
                yes,
            } => {
                let mut args = vec![
                    "image".into(),
                    "--manifest".into(),
                    manifest.display().to_string(),
                    "-o".into(),
                    out.display().to_string(),
                ];
                for s in slots {
                    args.push("--slot".into());
                    args.push(s.clone());
                }
                if *verify {
                    args.push("--verify".into());
                }
                if *yes {
                    args.push("--yes".into());
                }
                args
            }
        }
    }
}

/// Launch an operation in the background, streaming its events.
pub fn spawn_op(cmd: CliCommand) -> RunningOp {
    let label = cmd.label();
    let elevate = needs_elevation(cmd.device());
    let args = cmd.to_args();
    let (tx, rx) = channel::<GuiMsg>();
    let equivalent = equivalent_command(&args);
    let canceller = Canceller::default();
    if elevate {
        spawn_elevated(args, tx, canceller.clone());
    } else {
        spawn_plain(args, tx, canceller.clone());
    }
    RunningOp {
        rx,
        equivalent,
        label: label.to_string(),
        canceller,
    }
}

fn spawn_plain(mut args: Vec<String>, tx: Sender<GuiMsg>, canceller: Canceller) {
    args.push("--json".into());
    std::thread::spawn(move || {
        let mut cmd = Command::new(cli_path());
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        hide_console(&mut cmd);
        let child = cmd.spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(GuiMsg::Note(format!("cannot run sdslot CLI: {e}")));
                let _ = tx.send(GuiMsg::Exited(None));
                return;
            }
        };
        // Park the child where Canceller::cancel can reach it; reading the
        // pipe below doesn't need the child itself.
        let stdout = child.stdout.take();
        if let Ok(mut guard) = canceller.child.lock() {
            *guard = Some(child);
        }
        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                if let Ok(ev) = serde_json::from_str::<Event>(&line) {
                    let _ = tx.send(GuiMsg::Event(ev));
                }
            }
        }
        let code = canceller
            .child
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .and_then(|mut c| c.wait().ok())
            .and_then(|s| s.code());
        let _ = tx.send(GuiMsg::Exited(code));
    });
}

/// Elevated launch: open a localhost listener, pass `--json-port`, and read
/// the event stream from the connect-back socket.
fn spawn_elevated(mut args: Vec<String>, tx: Sender<GuiMsg>, canceller: Canceller) {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            let _ = tx.send(GuiMsg::Note(format!("cannot open event listener: {e}")));
            let _ = tx.send(GuiMsg::Exited(None));
            return;
        }
    };
    let addr = listener.local_addr().expect("listener addr");
    args.push("--json-port".into());
    args.push(addr.to_string());

    // Reader thread: one connection from the elevated CLI.
    let tx_reader = tx.clone();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            for line in BufReader::new(stream).lines().map_while(|l| l.ok()) {
                if let Ok(ev) = serde_json::from_str::<Event>(&line) {
                    let _ = tx_reader.send(GuiMsg::Event(ev));
                }
            }
        }
    });

    // Launcher thread: elevation prompt + wait for exit.
    std::thread::spawn(move || match elevated_wait(&args, &canceller) {
        Ok(code) => {
            let _ = tx.send(GuiMsg::Exited(code));
        }
        Err(e) => {
            let _ = tx.send(GuiMsg::Note(e));
            let _ = tx.send(GuiMsg::Exited(None));
        }
    });
}

#[cfg(windows)]
fn elevated_wait(args: &[String], canceller: &Canceller) -> Result<Option<i32>, String> {
    use std::iter::once;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(once(0)).collect()
    }

    let exe = cli_path().display().to_string();
    let params = args
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let verb = wide("runas");
    let file = wide(&exe);
    let parameters = wide(&params);

    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_HIDE;

    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 || info.hProcess.is_null() {
        return Err(
            "elevation was refused (UAC prompt declined?) or the CLI could not be launched".into(),
        );
    }
    unsafe {
        use std::sync::atomic::Ordering;
        // Expose the handle to Canceller::cancel for the wait's duration;
        // clear it before closing so a late cancel can't hit a stale handle.
        canceller
            .elevated
            .store(info.hProcess as usize, Ordering::Release);
        WaitForSingleObject(info.hProcess, INFINITE);
        canceller.elevated.store(0, Ordering::Release);
        let mut code: u32 = 1;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        Ok(Some(code as i32))
    }
}

/// Park a spawned wrapper child in the canceller and wait for it, keeping
/// the lock free so `cancel()` can kill it mid-run.
#[cfg(unix)]
fn wait_cancellable(child: Child, canceller: &Canceller) -> Result<Option<i32>, String> {
    if let Ok(mut guard) = canceller.child.lock() {
        *guard = Some(child);
    }
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let Ok(mut guard) = canceller.child.lock() else {
            return Ok(None);
        };
        match guard.as_mut() {
            None => return Ok(None),
            Some(c) => match c.try_wait() {
                Ok(Some(status)) => {
                    guard.take();
                    return Ok(status.code());
                }
                Ok(None) => {}
                Err(e) => {
                    guard.take();
                    return Err(format!("cannot wait for the CLI: {e}"));
                }
            },
        }
    }
}

#[cfg(target_os = "macos")]
fn elevated_wait(args: &[String], canceller: &Canceller) -> Result<Option<i32>, String> {
    // `do shell script ... with administrator privileges` shows the native
    // auth dialog; live progress arrives over the --json-port socket.
    let exe = cli_path().display().to_string();
    let shell_cmd = std::iter::once(exe.as_str())
        .chain(args.iter().map(|s| s.as_str()))
        .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        shell_cmd.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let child = std::process::Command::new("osascript")
        .args(["-e", &script])
        .spawn()
        .map_err(|e| format!("cannot run osascript: {e}"))?;
    wait_cancellable(child, canceller)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn elevated_wait(args: &[String], canceller: &Canceller) -> Result<Option<i32>, String> {
    // Polkit prompt via pkexec; if absent, surface a sudo hint.
    let exe = cli_path();
    match std::process::Command::new("pkexec")
        .arg(&exe)
        .args(args)
        .spawn()
    {
        Ok(child) => wait_cancellable(child, canceller),
        Err(_) => Err(format!(
            "pkexec is not available; run this from a terminal instead: sudo {}",
            equivalent_command(args)
        )),
    }
}
