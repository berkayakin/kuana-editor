//! Keeping the computer from idle-sleeping while something works (an agent
//! session), as Claude's desktop app does. Idle sleep only: the display may
//! still sleep, and a closed lid or the user choosing Sleep still sleeps.
//! macOS: an IOKit power assertion (`pmset -g assertions` lists it by its
//! reason); Windows: a power request (`powercfg /requests`); Linux: the
//! desktop portal's idle inhibitor, else logind's.

/// Held, the computer stays awake; dropped, it may sleep again.
pub struct Awake(#[allow(dead_code)] imp::Hold);

/// Keeps the computer awake until the value is dropped. `reason` is what the
/// system shows where it lists what keeps it awake. None when it refused or
/// has no way to.
pub fn keep_awake(reason: &str) -> Option<Awake> {
    imp::hold(reason).map(Awake)
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2_core_foundation::CFString;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(kind: &CFString, level: u32, name: &CFString, id: *mut u32) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    /// `kIOPMAssertionLevelOn`.
    const LEVEL_ON: u32 = 255;

    pub struct Hold(u32);

    pub fn hold(reason: &str) -> Option<Hold> {
        // `kIOPMAssertionTypePreventUserIdleSystemSleep`: the system, not the display.
        let kind = CFString::from_static_str("PreventUserIdleSystemSleep");
        let name = CFString::from_str(reason);
        let mut id = 0;
        // SAFETY: both strings live through the call; `id` is written on success.
        let status = unsafe { IOPMAssertionCreateWithName(&kind, LEVEL_ON, &name, &mut id) };
        (status == 0).then_some(Hold(id))
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            // SAFETY: the assertion was created by `hold` and is released once.
            unsafe { IOPMAssertionRelease(self.0) };
        }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Power::{PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest};
    use windows::Win32::System::Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0};
    use windows::core::PWSTR;

    /// `POWER_REQUEST_CONTEXT_VERSION`.
    const CONTEXT_VERSION: u32 = 0;

    pub struct Hold(HANDLE);

    // A kernel handle: any thread may clear and close it.
    unsafe impl Send for Hold {}

    pub fn hold(reason: &str) -> Option<Hold> {
        let mut wide: Vec<u16> = reason.encode_utf16().chain(Some(0)).collect();
        let context = REASON_CONTEXT {
            Version: CONTEXT_VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 { SimpleReasonString: PWSTR(wide.as_mut_ptr()) },
        };
        // SAFETY: `context` and its string live through the call, which copies them.
        let handle = unsafe { PowerCreateRequest(&context) }.ok()?;
        // SAFETY: a valid power request handle, closed once on failure or drop.
        if unsafe { PowerSetRequest(handle, PowerRequestSystemRequired) }.is_err() {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        }
        Some(Hold(handle))
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            // SAFETY: the request `hold` set, cleared and closed once.
            unsafe {
                let _ = PowerClearRequest(self.0, PowerRequestSystemRequired);
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;

    use zbus::blocking::Connection;
    use zbus::zvariant::{OwnedFd, OwnedObjectPath, Value};

    /// The portal's `Inhibit` flag for the session going idle.
    const IDLE: u32 = 8;

    pub enum Hold {
        /// The portal's request, closed to let go; the connection must live
        /// as long (the inhibition goes with it).
        Portal(Connection, OwnedObjectPath),
        /// logind's inhibitor lasts while its file descriptor is open.
        Logind(#[allow(dead_code)] OwnedFd),
    }

    pub fn hold(reason: &str) -> Option<Hold> {
        portal(reason).or_else(|| logind(reason))
    }

    fn portal(reason: &str) -> Option<Hold> {
        let connection = Connection::session().ok()?;
        let options: HashMap<&str, Value> = HashMap::from([("reason", Value::from(reason))]);
        let reply = connection
            .call_method(Some("org.freedesktop.portal.Desktop"), "/org/freedesktop/portal/desktop", Some("org.freedesktop.portal.Inhibit"), "Inhibit", &("", IDLE, options))
            .ok()?;
        let request: OwnedObjectPath = reply.body().deserialize().ok()?;
        Some(Hold::Portal(connection, request))
    }

    fn logind(reason: &str) -> Option<Hold> {
        let connection = Connection::system().ok()?;
        let reply = connection
            .call_method(Some("org.freedesktop.login1"), "/org/freedesktop/login1", Some("org.freedesktop.login1.Manager"), "Inhibit", &("idle", "Kuana", reason, "block"))
            .ok()?;
        let fd: OwnedFd = reply.body().deserialize().ok()?;
        Some(Hold::Logind(fd))
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            if let Hold::Portal(connection, request) = self {
                let _ = connection.call_method(Some("org.freedesktop.portal.Desktop"), request.as_ref(), Some("org.freedesktop.portal.Request"), "Close", &());
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod imp {
    pub struct Hold;

    pub fn hold(_: &str) -> Option<Hold> {
        None
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn an_assertion_is_held_and_let_go() {
        let reason = format!("kuana-os test {}", std::process::id());
        let listed = || {
            let out = std::process::Command::new("pmset").args(["-g", "assertions"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
            out.contains(&reason)
        };
        let awake = super::keep_awake(&reason).expect("macOS grants idle-sleep assertions");
        assert!(listed(), "pmset lists the assertion while it's held");
        drop(awake);
        assert!(!listed(), "and not after");
    }
}
