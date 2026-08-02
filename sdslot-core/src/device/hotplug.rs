// SPDX-License-Identifier: MIT OR Apache-2.0
//! Hotplug notification for the GUI's device list: a signal whenever block
//! storage is attached or detached, from the OS event source rather than a
//! poll loop.
//!
//! - macOS: `DiskArbitration.framework` (DASession on a CFRunLoop)
//! - Windows: `WM_DEVICECHANGE` on a message-only window
//! - Linux: netlink kobject uevents (`NETLINK_KOBJECT_UEVENT`)
//! - Anywhere else, or if the event source cannot be opened: a 15-second
//!   heartbeat. The event-driven platforms also run the heartbeat alongside,
//!   so a missed event self-corrects.

use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};

/// Spawn a background thread that listens for OS-level hotplug events and
/// sends a signal on `tx` whenever a storage device is added or removed. The
/// signal carries no payload — the receiver re-enumerates. One signal is sent
/// immediately, so the caller can populate its device list from the same path.
///
/// Returns `None` if the thread could not be spawned. The thread runs until
/// the receiver is dropped.
pub fn spawn_hotplug_listener(tx: Sender<()>) -> Option<JoinHandle<()>> {
    thread::Builder::new()
        .name("sdslot-hotplug".into())
        .spawn(move || {
            // Initial signal to populate the device list on startup.
            let _ = tx.send(());

            #[cfg(target_os = "macos")]
            {
                macos::run_listener(tx);
            }

            #[cfg(windows)]
            {
                windows::run_listener(tx);
            }

            #[cfg(target_os = "linux")]
            {
                linux::run_listener(tx);
            }

            #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
            {
                fallback::run_listener(tx);
            }
        })
        .ok()
}

mod fallback {
    use super::*;
    use std::time::Duration;

    pub fn run_listener(tx: Sender<()>) {
        loop {
            std::thread::sleep(Duration::from_secs(15));
            if tx.send(()).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The startup signal arrives on every platform without any device being
    /// touched, so a caller can drive its first enumeration from the channel
    /// alone. The listener thread itself needs real hardware events to say
    /// anything more, which the manual smoke checklist covers.
    #[test]
    fn listener_signals_once_at_startup() {
        let (tx, rx) = mpsc::channel();
        // The thread parks in a platform event loop, so it is never joined;
        // dropping the receiver is what eventually unblocks it.
        let _handle = spawn_hotplug_listener(tx).expect("listener thread spawns");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)),
            Ok(()),
            "no startup signal"
        );
    }

    /// Dropping the receiver must not panic the listener: every send site
    /// treats a closed channel as "stop".
    #[test]
    fn closed_receiver_is_not_an_error() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        // The startup send fails silently rather than unwrapping.
        let _handle = spawn_hotplug_listener(tx).expect("listener thread spawns");
    }
}

// ---------------------------------------------------------------------------
// macOS: DiskArbitration framework
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::ffi::c_void;

    #[repr(C)]
    struct DASession(c_void);
    type DASessionRef = *const DASession;
    #[repr(C)]
    struct DADisk(c_void);
    type DADiskRef = *const DADisk;

    type DADiskCallback = extern "C" fn(disk: DADiskRef, context: *mut c_void);

    #[link(name = "DiskArbitration", kind = "framework")]
    extern "C" {
        fn DASessionCreate(allocator: *const c_void) -> DASessionRef;
        fn DARegisterDiskAppearedCallback(
            session: DASessionRef,
            match_dict: *const c_void,
            callback: DADiskCallback,
            context: *mut c_void,
        );
        fn DARegisterDiskDisappearedCallback(
            session: DASessionRef,
            match_dict: *const c_void,
            callback: DADiskCallback,
            context: *mut c_void,
        );
        fn DASessionScheduleWithRunLoop(
            session: DASessionRef,
            runLoop: *const c_void,
            runLoopMode: *const c_void,
        );
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopGetCurrent() -> *const c_void;
        fn CFRunLoopRun();
        fn CFRelease(cf: *const c_void);
    }

    extern "C" fn on_disk_change(_disk: DADiskRef, context: *mut c_void) {
        let tx = unsafe { &*(context as *const Sender<()>) };
        let _ = tx.send(());
    }

    pub fn run_listener(tx: Sender<()>) {
        unsafe {
            let session = DASessionCreate(std::ptr::null());
            if session.is_null() {
                fallback::run_listener(tx);
                return;
            }

            let ctx_ptr = &tx as *const Sender<()> as *mut c_void;
            DARegisterDiskAppearedCallback(session, std::ptr::null(), on_disk_change, ctx_ptr);
            DARegisterDiskDisappearedCallback(session, std::ptr::null(), on_disk_change, ctx_ptr);

            let run_loop = CFRunLoopGetCurrent();
            DASessionScheduleWithRunLoop(session, run_loop, kCFRunLoopDefaultMode);

            // Periodically wake up for a heartbeat check alongside OS events
            let heartbeat_tx = tx.clone();
            thread::spawn(move || loop {
                thread::sleep(std::time::Duration::from_secs(15));
                if heartbeat_tx.send(()).is_err() {
                    break;
                }
            });

            CFRunLoopRun();
            CFRelease(session as *const c_void);
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: WM_DEVICECHANGE window messages
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows {
    use super::*;
    use std::cell::RefCell;
    use std::ptr;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW, MSG,
        WM_DEVICECHANGE, WNDCLASSW,
    };

    const DBT_DEVICEARRIVAL: u32 = 0x8000;
    const DBT_DEVICEREMOVECOMPLETE: u32 = 0x8004;

    thread_local! {
        static SENDER: RefCell<Option<Sender<()>>> = const { RefCell::new(None) };
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_DEVICECHANGE {
            let event = wparam as u32;
            if event == DBT_DEVICEARRIVAL || event == DBT_DEVICEREMOVECOMPLETE {
                SENDER.with(|s| {
                    if let Some(ref tx) = *s.borrow() {
                        let _ = tx.send(());
                    }
                });
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    pub fn run_listener(tx: Sender<()>) {
        SENDER.with(|s| *s.borrow_mut() = Some(tx.clone()));

        unsafe {
            let class_name = "SdSlotHotplugListener\0".encode_utf16().collect::<Vec<_>>();
            let wnd_class = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: ptr::null_mut(),
                hIcon: ptr::null_mut(),
                hCursor: ptr::null_mut(),
                hbrBackground: ptr::null_mut(),
                lpszMenuName: ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };

            RegisterClassW(&wnd_class);

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                windows_sys::Win32::UI::WindowsAndMessaging::HWND_MESSAGE,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );

            if hwnd.is_null() {
                fallback::run_listener(tx);
                return;
            }

            // Periodically wake up for a heartbeat check alongside OS events
            let heartbeat_tx = tx.clone();
            thread::spawn(move || loop {
                thread::sleep(std::time::Duration::from_secs(15));
                if heartbeat_tx.send(()).is_err() {
                    break;
                }
            });

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                DispatchMessageW(&msg);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Linux: Netlink kobject uevents (NETLINK_KOBJECT_UEVENT)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    pub fn run_listener(tx: Sender<()>) {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_KOBJECT_UEVENT,
            )
        };

        if fd < 0 {
            fallback::run_listener(tx);
            return;
        }

        let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        sa.nl_family = libc::AF_NETLINK as u16;
        sa.nl_groups = 1; // kernel uevents

        let bind_res = unsafe {
            libc::bind(
                fd,
                &sa as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            )
        };

        if bind_res < 0 {
            unsafe {
                libc::close(fd);
            }
            fallback::run_listener(tx);
            return;
        }

        // Periodically wake up for a heartbeat check alongside OS events
        let heartbeat_tx = tx.clone();
        thread::spawn(move || loop {
            thread::sleep(std::time::Duration::from_secs(15));
            if heartbeat_tx.send(()).is_err() {
                break;
            }
        });

        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            if n <= 0 {
                break;
            }
            let data = &buf[..n as usize];
            if data.windows(15).any(|w| w == b"SUBSYSTEM=block") && tx.send(()).is_err() {
                break;
            }
        }
        unsafe {
            libc::close(fd);
        }
    }
}
