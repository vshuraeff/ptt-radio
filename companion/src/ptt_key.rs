//! the global push-to-talk key: the only OS-specific part of the companion.
//!
//! global-hotkey reports press and release on macOS (Carbon hotkey), Windows
//! (RegisterHotKey) and Linux X11. each OS needs its events dispatched on the
//! thread that created the manager, which `poll` does on the main thread:
//! - macOS: the NSApplication event queue (implemented)
//! - Windows: a win32 message loop (not yet: the fallback below only sleeps)
//! - Linux: X11 is served by global-hotkey's own thread; Wayland needs the
//!   GlobalShortcuts portal instead (not yet)
//!
//! a Carbon hotkey is consumed system-wide, so the default is a key nobody types.

use std::time::Duration;

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

pub struct PttKey {
    // unregisters the hotkey when dropped
    _manager: GlobalHotKeyManager,
    id: Option<u32>,
    events: EventPump,
}

impl PttKey {
    /// an unparsable key is an error; a key the OS refuses (held by another app) only disables PTT
    pub fn register(spec: &str) -> Result<Self, String> {
        let events = EventPump::new();
        let manager = GlobalHotKeyManager::new()
            .map_err(|err| format!("global hotkeys unavailable: {err}"))?;
        let hotkey: HotKey = spec
            .parse()
            .map_err(|err| format!("--ptt-key {spec}: {err}"))?;
        let id = match manager.register(hotkey) {
            Ok(()) => {
                println!("push-to-talk key: {spec}");
                Some(hotkey.id())
            }
            Err(err) => {
                eprintln!(
                    "warning: push-to-talk key {spec} not registered: {err}; only open mic works"
                );
                None
            }
        };
        Ok(PttKey {
            _manager: manager,
            id,
            events,
        })
    }

    /// dispatches OS events for up to `timeout`; returns the last press (true) or release (false) seen
    pub fn poll(&self, timeout: Duration) -> Option<bool> {
        self.events.wait(timeout);
        GlobalHotKeyEvent::receiver()
            .try_iter()
            .filter(|event| Some(event.id) == self.id)
            .map(|event| event.state == HotKeyState::Pressed)
            .last()
    }
}

#[cfg(target_os = "macos")]
struct EventPump(objc2::rc::Retained<objc2_app_kit::NSApplication>);

#[cfg(target_os = "macos")]
impl EventPump {
    // global-hotkey installs a Carbon handler on the application event target,
    // which only fires while the main thread dispatches app events
    fn new() -> Self {
        use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
        let mtm =
            objc2::MainThreadMarker::new().expect("the hotkey must be set up on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        // no Dock icon and no menu bar
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        EventPump(app)
    }

    fn wait(&self, timeout: Duration) {
        use objc2_app_kit::NSEventMask;
        use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
        let until = NSDate::dateWithTimeIntervalSinceNow(timeout.as_secs_f64());
        // SAFETY: an immutable framework constant
        let mode = unsafe { NSDefaultRunLoopMode };
        while let Some(event) = self.0.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&until),
            mode,
            true,
        ) {
            self.0.sendEvent(&event);
        }
    }
}

#[cfg(not(target_os = "macos"))]
struct EventPump;

#[cfg(not(target_os = "macos"))]
impl EventPump {
    fn new() -> Self {
        EventPump
    }

    fn wait(&self, timeout: Duration) {
        std::thread::sleep(timeout);
    }
}
