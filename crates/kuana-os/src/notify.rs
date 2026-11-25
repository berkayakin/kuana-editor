//! System notifications: something finished or needs the user while they're
//! elsewhere, as Claude's desktop app tells. macOS: the User Notifications
//! framework (only an app bundle may post; the system asks the user once,
//! `prepare`), a newer one with the same id replacing the last, and a click
//! comes back with its id (`on_click`); the Dock icon also bounces once
//! (`request_attention`). Linux: the desktop's notification server
//! (freedesktop's D-Bus interface; a click isn't heard). Windows: the window's
//! taskbar button flashes until it's brought forward (`request_attention`); a
//! toast needs an app identity registered by the installer, which Kuana's
//! doesn't do yet.

use std::sync::OnceLock;

type Handler = Box<dyn Fn(String) + Send + Sync>;

static ON_CLICK: OnceLock<Handler> = OnceLock::new();

/// What a click on one of Kuana's notifications does: it's given the
/// notification's id (on any thread). Set once, at startup.
pub fn on_click(handler: impl Fn(String) + Send + Sync + 'static) {
    let _ = ON_CLICK.set(Box::new(handler));
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn clicked(id: String) {
    if let Some(handler) = ON_CLICK.get() {
        handler(id);
    }
}

/// Asks the system's permission to notify where it asks (macOS, once ever):
/// call it when the user is there to answer, before the first `show`.
pub fn prepare() {
    imp::prepare();
}

/// Shows a notification. One with the same `id` replaces it (a session's
/// newest news); a click on it gives `id` to the `on_click` handler.
pub fn show(title: &str, body: &str, id: &str) {
    imp::show(title, body, id);
}

/// Asks once for the user's attention to Kuana while it isn't in front: the
/// Dock icon bounces (macOS, from the main thread), the taskbar button of
/// `window` (its `HWND`) flashes until it's brought forward (Windows).
pub fn request_attention(window: Option<isize>) {
    imp::request_attention(window);
}

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::OnceLock;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{AllocAnyThread, MainThreadMarker, define_class, msg_send};
    use objc2_app_kit::{NSApplication, NSRequestUserAttentionType};
    use objc2_foundation::{NSBundle, NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
        UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and `Delegate`
        // doesn't implement `Drop`.
        #[unsafe(super(NSObject))]
        #[name = "KuanaNotificationDelegate"]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            // Shown even with Kuana in front: it's posted for a window or a
            // session the user isn't looking at.
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(&self, _center: &UNUserNotificationCenter, _notification: &UNNotification, completion: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>) {
                completion.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List | UNNotificationPresentationOptions::Sound,));
            }

            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(&self, _center: &UNUserNotificationCenter, response: &UNNotificationResponse, completion: &block2::DynBlock<dyn Fn()>) {
                super::clicked(response.notification().request().identifier().to_string());
                completion.call(());
            }
        }
    );

    impl Delegate {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            // SAFETY: NSObject's `init` on a freshly allocated instance.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// The notification center with Kuana's delegate (which it keeps weakly).
    struct Center {
        center: Retained<UNUserNotificationCenter>,
        _delegate: Retained<Delegate>,
    }

    // SAFETY: UNUserNotificationCenter may be used from any thread, and the
    // delegate holds no state.
    unsafe impl Send for Center {}
    unsafe impl Sync for Center {}

    static CENTER: OnceLock<Option<Center>> = OnceLock::new();

    /// None outside an app bundle (a development build run from a terminal),
    /// where the framework would throw.
    fn center() -> Option<&'static UNUserNotificationCenter> {
        CENTER
            .get_or_init(|| {
                NSBundle::mainBundle().bundleIdentifier()?;
                let center = UNUserNotificationCenter::currentNotificationCenter();
                let delegate = Delegate::new();
                center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
                Some(Center { center, _delegate: delegate })
            })
            .as_ref()
            .map(|c| &*c.center)
    }

    pub fn prepare() {
        let Some(center) = center() else { return };
        let answered = RcBlock::new(|_granted: Bool, _error: *mut NSError| {});
        center.requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound, &answered);
    }

    pub fn show(title: &str, body: &str, id: &str) {
        let Some(center) = center() else { return };
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        // Grouped together in Notification Center.
        content.setThreadIdentifier(&NSString::from_str("kuana"));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str(id), &content, None);
        center.addNotificationRequest_withCompletionHandler(&request, None);
    }

    pub fn request_attention(_: Option<isize>) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        // A no-op while Kuana is the active app.
        NSApplication::sharedApplication(mtm).requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx};

    pub fn prepare() {}

    pub fn show(_: &str, _: &str, _: &str) {}

    pub fn request_attention(window: Option<isize>) {
        let Some(hwnd) = window else { return };
        let info = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd: HWND(hwnd as *mut core::ffi::c_void),
            // The taskbar button, until the window comes to the foreground.
            dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        // SAFETY: `info` is filled in; a stale handle only makes it fail.
        unsafe {
            let _ = FlashWindowEx(&info);
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use zbus::blocking::Connection;
    use zbus::zvariant::Value;

    /// The server's numbers for the notifications shown, by id: a newer one
    /// replaces the one before.
    static SHOWN: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

    pub fn prepare() {}

    pub fn show(title: &str, body: &str, id: &str) {
        let (title, body, id) = (title.to_string(), body.to_string(), id.to_string());
        // A D-Bus round trip: not on the caller's (UI) thread.
        std::thread::spawn(move || {
            let Ok(connection) = Connection::session() else { return };
            let replaces = SHOWN.lock().ok().and_then(|shown| shown.as_ref().and_then(|s| s.get(&id).copied())).unwrap_or(0);
            let hints: HashMap<&str, Value> = HashMap::from([("desktop-entry", Value::from("dev.kuana.editor"))]);
            let actions: Vec<&str> = Vec::new();
            let reply = connection.call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &("Kuana", replaces, "dev.kuana.editor", title.as_str(), body.as_str(), actions, hints, -1i32),
            );
            if let Ok(number) = reply.and_then(|r| r.body().deserialize::<u32>())
                && let Ok(mut shown) = SHOWN.lock()
            {
                shown.get_or_insert_with(HashMap::new).insert(id, number);
            }
        });
    }

    pub fn request_attention(_: Option<isize>) {}
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod imp {
    pub fn prepare() {}

    pub fn show(_: &str, _: &str, _: &str) {}

    pub fn request_attention(_: Option<isize>) {}
}
