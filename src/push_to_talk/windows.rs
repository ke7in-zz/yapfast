use std::{
    mem::size_of,
    ptr,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};

use slint::winit_030::winit::{
    event::{DeviceEvent, DeviceId, ElementState},
    event_loop::ActiveEventLoop,
    platform::scancode::PhysicalKeyExtScancode,
};
use slint::winit_030::{CustomApplicationHandler, EventResult};
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError, LPARAM, LRESULT, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW,
        Power::{RegisterSuspendResumeNotification, UnregisterSuspendResumeNotification},
        RemoteDesktop::{WTSRegisterSessionNotification, WTSUnRegisterSessionNotification},
        Threading::GetCurrentThreadId,
    },
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        Input::{
            GetRegisteredRawInputDevices,
            KeyboardAndMouse::{
                GetAsyncKeyState, MAPVK_VSC_TO_VK_EX, MapVirtualKeyW, VK_LCONTROL, VK_LMENU,
                VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN,
            },
            RAWINPUTDEVICE, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RegisterRawInputDevices,
        },
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            CallNextHookEx, EVENT_SYSTEM_DESKTOPSWITCH, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT,
            LLKHF_EXTENDED, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
            UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_NCDESTROY, WM_QUIT,
            WM_SYSKEYDOWN, WM_SYSKEYUP,
        },
    },
};

use crate::push_to_talk::matcher::{Matcher, ModifierLayout};

use super::{Actions, Binding, Bindings, Reach, StatusReporter};

const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
const HID_USAGE_GENERIC_KEYBOARD: u16 = 0x06;
const WTS_NOTIFY_FOR_THIS_SESSION: u32 = 0;
const WM_POWERBROADCAST_VALUE: u32 = 0x0218;
const WM_WTSSESSION_CHANGE_VALUE: u32 = 0x02b1;
const WTS_CONSOLE_CONNECT_VALUE: u32 = 0x0001;
const WTS_CONSOLE_DISCONNECT_VALUE: u32 = 0x0002;
const WTS_REMOTE_CONNECT_VALUE: u32 = 0x0003;
const WTS_REMOTE_DISCONNECT_VALUE: u32 = 0x0004;
const WTS_SESSION_LOGON_VALUE: u32 = 0x0005;
const WTS_SESSION_LOGOFF_VALUE: u32 = 0x0006;
const WTS_SESSION_LOCK_VALUE: u32 = 0x0007;
const WTS_SESSION_UNLOCK_VALUE: u32 = 0x0008;
const PBT_APMSUSPEND_VALUE: u32 = 0x0004;
const PBT_APMRESUMECRITICAL_VALUE: u32 = 0x0006;
const PBT_APMRESUMESUSPEND_VALUE: u32 = 0x0007;
const PBT_APMRESUMEAUTOMATIC_VALUE: u32 = 0x0012;
const HOTKEY_SUBCLASS_ID: usize = 0x5941_5046;
const WINEVENT_OUTOFCONTEXT_VALUE: u32 = 0x0000;
const WINEVENT_SKIPOWNPROCESS_VALUE: u32 = 0x0002;

#[derive(Clone, Copy)]
struct KeyboardRegistration {
    usage_page: u16,
    usage: u16,
    flags: u32,
    target: isize,
}

struct Input {
    bindings: Bindings,
    matcher: Matcher<DeviceId>,
    actions: Option<Actions>,
    statuses: Option<StatusReporter>,
    original: Option<KeyboardRegistration>,
    target: Option<isize>,
    subclass_installed: bool,
    input_sink: bool,
    wts_registered: bool,
    win_event_hook: Option<usize>,
    power_notification: Option<isize>,
}

impl Input {
    fn new() -> Self {
        Self {
            bindings: Bindings::default(),
            matcher: Matcher::new(Bindings::default(), ModifierLayout::WindowsSet1),
            actions: None,
            statuses: None,
            original: None,
            target: None,
            subclass_installed: false,
            input_sink: false,
            wts_registered: false,
            win_event_hook: None,
            power_notification: None,
        }
    }

    fn has_commands(&self) -> bool {
        self.bindings.toggle_stream.is_some() || self.bindings.leave_voice.is_some()
    }

    fn command_status(&self, reach: Reach) {
        if let Some(statuses) = &self.statuses {
            for binding in [Binding::ToggleStream, Binding::LeaveVoice] {
                if binding.configured(&self.bindings) {
                    statuses.set(binding, reach.clone());
                }
            }
        }
    }

    fn reset_matcher(&mut self) {
        let _ = self.matcher.clear();
        yapfast_core::push_to_talk::hold(false);
    }

    fn synchronize_keys(&mut self) {
        self.reset_matcher();
        if !self.has_commands() {
            return;
        }
        for (scancode, key) in [
            (0x001d, VK_LCONTROL),
            (0xe01d, VK_RCONTROL),
            (0x0038, VK_LMENU),
            (0xe038, VK_RMENU),
            (0x002a, VK_LSHIFT),
            (0x0036, VK_RSHIFT),
            (0xe05b, VK_LWIN),
            (0xe05c, VK_RWIN),
        ] {
            // SAFETY: Query state only. Seeding never generates a command edge.
            if unsafe { GetAsyncKeyState(key as i32) } < 0 {
                self.matcher.seed_down([scancode]);
            }
        }
        for chord in [&self.bindings.toggle_stream, &self.bindings.leave_voice]
            .into_iter()
            .flatten()
        {
            // SAFETY: Map this configured physical key, without injecting input.
            let key = unsafe { MapVirtualKeyW(chord.key.scancode, MAPVK_VSC_TO_VK_EX) };
            if key != 0 && unsafe { GetAsyncKeyState(key as i32) } < 0 {
                self.matcher.seed_down([chord.key.scancode]);
            }
        }
    }
}

struct Shared {
    handler_installed: AtomicBool,
    input: Mutex<Input>,
}

impl Shared {
    fn new() -> Self {
        Self {
            handler_installed: AtomicBool::new(false),
            input: Mutex::new(Input::new()),
        }
    }

    fn configure(&self, bindings: Bindings, statuses: StatusReporter, actions: Actions) {
        let mut input = self
            .input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        input.reset_matcher();
        input.matcher = Matcher::new(command_bindings(&bindings), ModifierLayout::WindowsSet1);
        input.bindings = bindings;
        input.actions = Some(actions);
        input.statuses = Some(statuses);
        if !input.has_commands() {
            restore_input_sink(&mut input);
            unregister_notifications(&mut input);
            return;
        }
        if !self.handler_installed.load(Ordering::Acquire) {
            input.command_status(Reach::Unavailable(
                "Yapfast's Windows global-input handler isn't installed",
            ));
            return;
        }
        if !discover_receiver(&mut input) {
            input.command_status(Reach::Unavailable(
                "Winit did not register a keyboard raw-input receiver",
            ));
            return;
        }
        activate(&mut input);
    }

    fn key(&self, device: DeviceId, scancode: u32, pressed: bool) {
        let (edges, actions) = {
            let mut input = self
                .input
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // A failed restoration can leave the sink registered after a
            // notification failure. Unavailable input must not execute commands.
            if !input.input_sink
                || !input.subclass_installed
                || !input.wts_registered
                || input.win_event_hook.is_none()
                || input.power_notification.is_none()
            {
                return;
            }
            let edges = input.matcher.event(device, scancode, pressed);
            if edges.commands().next().is_none() {
                return;
            }
            (edges, input.actions.clone())
        };
        if let Some(actions) = actions {
            for command in edges.commands() {
                actions.emit(command);
            }
        }
    }

    fn remove_device(&self, device: &DeviceId) {
        let mut input = self
            .input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = input.matcher.remove_device(device);
    }

    fn clear_stale(&self) {
        let mut input = self
            .input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        input.synchronize_keys();
    }
    fn receiver_destroyed(&self, target: isize) {
        let mut input = self
            .input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if input.target != Some(target) {
            return;
        }
        input.reset_matcher();
        let restored = restore_input_sink(&mut input);
        let notifications_unregistered = unregister_notifications(&mut input);
        input.command_status(Reach::Unavailable(
            "Windows' global-input receiver was destroyed",
        ));
        if restored && notifications_unregistered {
            input.original = None;
            input.target = None;
        }
    }

    fn resumed(&self) {
        if !self.handler_installed.load(Ordering::Acquire) {
            return;
        }
        let mut input = self
            .input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !input.has_commands() {
            return;
        }
        if !discover_receiver(&mut input) {
            input.command_status(Reach::Unavailable(
                "Winit did not register a keyboard raw-input receiver",
            ));
            return;
        }
        activate(&mut input);
    }

    fn clear_watcher(&self) -> bool {
        let mut input = self
            .input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        input.reset_matcher();
        let restored = restore_input_sink(&mut input);
        let notifications_unregistered = unregister_notifications(&mut input);
        input.bindings = Bindings::default();
        input.matcher = Matcher::new(Bindings::default(), ModifierLayout::WindowsSet1);
        input.actions = None;
        input.statuses = None;
        restored && notifications_unregistered
    }

    fn teardown(&self) -> bool {
        if !self.clear_watcher() {
            return false;
        }
        let mut input = self
            .input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        input.original = None;
        input.target = None;
        true
    }
}

fn shared() -> Arc<Shared> {
    static SHARED: LazyLock<Arc<Shared>> = LazyLock::new(|| Arc::new(Shared::new()));
    SHARED.clone()
}

fn command_bindings(bindings: &Bindings) -> Bindings {
    Bindings {
        talk: None,
        toggle_stream: bindings.toggle_stream.clone(),
        leave_voice: bindings.leave_voice.clone(),
    }
}

fn keyboard_registration() -> Option<KeyboardRegistration> {
    let mut count = 0;
    // SAFETY: The first query asks for the number of process registrations.
    let result = unsafe {
        GetRegisteredRawInputDevices(
            ptr::null_mut(),
            &mut count,
            size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    // A null-buffer size query normally returns -1/INSUFFICIENT_BUFFER.
    if count == 0 || (result == u32::MAX && unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER)
    {
        return None;
    }
    let mut devices = Vec::<RAWINPUTDEVICE>::new();
    devices.try_reserve_exact(count as usize).ok()?;
    // SAFETY: `devices` has capacity for the reported registrations.
    let result = unsafe {
        GetRegisteredRawInputDevices(
            devices.as_mut_ptr(),
            &mut count,
            size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    if result == u32::MAX || result as usize > devices.capacity() {
        return None;
    }
    // SAFETY: The successful API return is the number of initialized records,
    // which we verified fits the vector's allocated capacity.
    unsafe { devices.set_len(result as usize) };
    devices
        .into_iter()
        .find(|device| {
            device.usUsagePage == HID_USAGE_PAGE_GENERIC
                && device.usUsage == HID_USAGE_GENERIC_KEYBOARD
                && !device.hwndTarget.is_null()
        })
        .map(|device| KeyboardRegistration {
            usage_page: device.usUsagePage,
            usage: device.usUsage,
            flags: device.dwFlags,
            target: device.hwndTarget as isize,
        })
}

fn discover_receiver(input: &mut Input) -> bool {
    if input.original.is_some() {
        return true;
    }
    let Some(registration) = keyboard_registration() else {
        return false;
    };
    input.target = Some(registration.target);
    input.original = Some(registration);
    true
}

fn activate(input: &mut Input) {
    if let Some(target) = input.target {
        if !input.subclass_installed {
            // SAFETY: This is Winit's live receiver HWND on the event-loop thread.
            input.subclass_installed = unsafe {
                SetWindowSubclass(target as _, Some(session_messages), HOTKEY_SUBCLASS_ID, 0) != 0
            };
        }
        if !input.wts_registered {
            // SAFETY: `target` is the existing Winit raw-input receiver HWND.
            input.wts_registered = unsafe {
                WTSRegisterSessionNotification(target as _, WTS_NOTIFY_FOR_THIS_SESSION) != 0
            };
        }
        if input.power_notification.is_none() {
            // SAFETY: Deliver resume messages to the same live Winit receiver.
            let notification = unsafe { RegisterSuspendResumeNotification(target as _, 0) };
            if notification != 0 {
                input.power_notification = Some(notification);
            }
        }
        if input.win_event_hook.is_none() {
            // SAFETY: The callback is process-static and does not retain any
            // message data. Winit's event loop pumps callbacks on this thread.
            let hook = unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_DESKTOPSWITCH,
                    EVENT_SYSTEM_DESKTOPSWITCH,
                    ptr::null_mut(),
                    Some(desktop_switch),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT_VALUE | WINEVENT_SKIPOWNPROCESS_VALUE,
                )
            };
            if !hook.is_null() {
                input.win_event_hook = Some(hook as usize);
            }
        }
    }
    if input.has_commands()
        && (!input.subclass_installed
            || !input.wts_registered
            || input.win_event_hook.is_none()
            || input.power_notification.is_none())
    {
        restore_input_sink(input);
        input.command_status(Reach::Unavailable(
            "Windows could not monitor session, desktop, and power transitions safely",
        ));
        return;
    }

    if !input.has_commands() {
        restore_input_sink(input);
        return;
    }
    let Some(original) = input.original else {
        input.command_status(Reach::Unavailable(
            "Winit did not register a keyboard raw-input receiver",
        ));
        return;
    };
    if !input.input_sink {
        let keyboard = RAWINPUTDEVICE {
            usUsagePage: original.usage_page,
            usUsage: original.usage,
            dwFlags: RIDEV_DEVNOTIFY | RIDEV_INPUTSINK,
            hwndTarget: original.target as _,
        };
        // SAFETY: This updates only the existing keyboard TLC registration;
        // the Winit HWND and its single receiver remain unchanged.
        let registered = unsafe {
            RegisterRawInputDevices(&keyboard, 1, size_of::<RAWINPUTDEVICE>() as u32) != 0
        };
        input.input_sink = registered;
    }
    if input.input_sink {
        input.synchronize_keys();
        input.command_status(Reach::Everywhere(None));
    } else {
        input.command_status(Reach::Unavailable(
            "Windows did not enable background keyboard raw input",
        ));
    }
}

fn restore_input_sink(input: &mut Input) -> bool {
    if !input.input_sink {
        return true;
    }
    let Some(original) = input.original else {
        tracing::error!("couldn't restore raw input because Winit's registration was lost");
        return false;
    };
    let keyboard = RAWINPUTDEVICE {
        usUsagePage: original.usage_page,
        usUsage: original.usage,
        dwFlags: original.flags,
        hwndTarget: original.target as _,
    };
    // SAFETY: Restores the single keyboard registration exactly as it was
    // before this watcher changed it; mouse registration is untouched.
    let restored =
        unsafe { RegisterRawInputDevices(&keyboard, 1, size_of::<RAWINPUTDEVICE>() as u32) != 0 };
    if restored {
        input.input_sink = false;
    } else {
        tracing::error!("Windows rejected restoration of the keyboard raw-input registration");
    }
    restored
}

fn unregister_notifications(input: &mut Input) -> bool {
    let mut clean = true;
    if input.wts_registered {
        if let Some(target) = input.target {
            // SAFETY: This unregisters the session notification on the HWND
            // registered during this listener's lifetime.
            let unregistered = unsafe { WTSUnRegisterSessionNotification(target as _) != 0 };
            if unregistered {
                input.wts_registered = false;
            } else {
                clean = false;
                tracing::error!("Windows rejected unregistering session notifications");
            }
        } else {
            clean = false;
        }
    }
    if let Some(hook) = input.win_event_hook {
        // SAFETY: The handle is returned by SetWinEventHook above.
        let unhooked = unsafe { UnhookWinEvent(hook as HWINEVENTHOOK) != 0 };
        if unhooked {
            input.win_event_hook = None;
        } else {
            clean = false;
            tracing::error!("Windows rejected removing the desktop-switch event hook");
        }
    }
    if let Some(notification) = input.power_notification {
        // SAFETY: This handle belongs to this listener's registration.
        if unsafe { UnregisterSuspendResumeNotification(notification) } != 0 {
            input.power_notification = None;
        } else {
            clean = false;
            tracing::error!("Windows rejected removing power notifications");
        }
    }
    if input.subclass_installed {
        if let Some(target) = input.target {
            // SAFETY: This removes the subclass installed on the same HWND.
            if unsafe {
                RemoveWindowSubclass(target as _, Some(session_messages), HOTKEY_SUBCLASS_ID) != 0
            } {
                input.subclass_installed = false;
            } else {
                clean = false;
                tracing::error!("Windows rejected removing the raw-input receiver subclass");
            }
        } else {
            clean = false;
        }
    }
    clean
}

unsafe extern "system" fn desktop_switch(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _window: windows_sys::Win32::Foundation::HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    shared().clear_stale();
}
unsafe extern "system" fn session_messages(
    hwnd: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    if subclass_id == HOTKEY_SUBCLASS_ID {
        match message {
            WM_WTSSESSION_CHANGE_VALUE => match wparam as u32 {
                WTS_CONSOLE_CONNECT_VALUE
                | WTS_CONSOLE_DISCONNECT_VALUE
                | WTS_REMOTE_CONNECT_VALUE
                | WTS_REMOTE_DISCONNECT_VALUE
                | WTS_SESSION_LOGON_VALUE
                | WTS_SESSION_LOGOFF_VALUE
                | WTS_SESSION_LOCK_VALUE
                | WTS_SESSION_UNLOCK_VALUE => shared().clear_stale(),
                _ => {}
            },
            WM_POWERBROADCAST_VALUE => match wparam as u32 {
                PBT_APMSUSPEND_VALUE
                | PBT_APMRESUMECRITICAL_VALUE
                | PBT_APMRESUMESUSPEND_VALUE
                | PBT_APMRESUMEAUTOMATIC_VALUE => shared().clear_stale(),
                _ => {}
            },
            WM_NCDESTROY => shared().receiver_destroyed(hwnd as isize),
            _ => {}
        }
    }
    // SAFETY: The subclass always forwards the message to Winit's procedure.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

pub(super) struct Global {
    _hook: Option<Hook>,
}

impl Global {
    pub fn start(bindings: &Bindings, statuses: StatusReporter, actions: Actions) -> Self {
        let hook = bindings
            .talk
            .as_ref()
            .and_then(|key| Hook::start(key.scancode, statuses.clone()));
        shared().configure(bindings.clone(), statuses, actions);
        Self { _hook: hook }
    }
}

impl Drop for Global {
    fn drop(&mut self) {
        shared().clear_watcher();
    }
}

pub(super) fn custom_application_handler() -> impl CustomApplicationHandler + 'static {
    let shared = shared();
    shared.handler_installed.store(true, Ordering::Release);
    InputHandler { shared }
}

pub(super) fn teardown() -> bool {
    shared().teardown()
}

struct InputHandler {
    shared: Arc<Shared>,
}

impl CustomApplicationHandler for InputHandler {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) -> EventResult {
        self.shared.resumed();
        EventResult::Propagate
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        device_id: DeviceId,
        event: DeviceEvent,
    ) -> EventResult {
        match event {
            DeviceEvent::Key(key) => {
                if let Some(scancode) = key.physical_key.to_scancode() {
                    self.shared
                        .key(device_id, scancode, key.state == ElementState::Pressed);
                }
            }
            DeviceEvent::Added => {
                let mut input = self
                    .shared
                    .input
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if input.input_sink {
                    input.matcher.add_device(device_id);
                }
            }
            DeviceEvent::Removed => self.shared.remove_device(&device_id),
            _ => {}
        }
        EventResult::Propagate
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) -> EventResult {
        self.shared.teardown();
        EventResult::Propagate
    }
}

/// The low-level keyboard hook is retained solely for the single PTT key; all
/// command chords use Winit's process-level Raw Input receiver above.
struct Hook {
    thread: u32,
    join: Option<JoinHandle<()>>,
}

impl Hook {
    fn start(scancode: u32, statuses: StatusReporter) -> Option<Self> {
        PTT_KEY.store(scancode, Ordering::Relaxed);
        let (started, ready) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("yapfast-talk-key".into())
            .spawn(move || {
                // SAFETY: The hook procedure reads only the event data passed
                // by Windows, and this message loop keeps the hook thread alive.
                unsafe {
                    let hook = SetWindowsHookExW(
                        WH_KEYBOARD_LL,
                        Some(keyboard_hook),
                        GetModuleHandleW(ptr::null()),
                        0,
                    );
                    if hook.is_null() {
                        let _ = started.send(None);
                        return;
                    }
                    let mut message: MSG = std::mem::zeroed();
                    PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_NOREMOVE);
                    let _ = started.send(Some(GetCurrentThreadId()));
                    while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {}
                    UnhookWindowsHookEx(hook);
                }
            })
            .ok()?;
        match ready.recv().ok().flatten() {
            Some(thread) => {
                statuses.set(Binding::Talk, Reach::Everywhere(None));
                Some(Self {
                    thread,
                    join: Some(join),
                })
            }
            None => {
                let _ = join.join();
                statuses.set(
                    Binding::Talk,
                    Reach::Focused("Windows didn't let Yapfast watch keys in other apps"),
                );
                None
            }
        }
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        PTT_KEY.store(u32::MAX, Ordering::Relaxed);
        // SAFETY: Posting WM_QUIT to the hook thread ends its message loop.
        unsafe {
            PostThreadMessageW(self.thread, WM_QUIT, 0, 0);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

static PTT_KEY: AtomicU32 = AtomicU32::new(u32::MAX);

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: For HC_ACTION, lparam points at the keyboard hook's event.
        let key = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let extended = if key.flags & LLKHF_EXTENDED != 0 {
            0xe000
        } else {
            0
        };
        if key.scanCode | extended == PTT_KEY.load(Ordering::Relaxed) {
            match wparam as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => yapfast_core::push_to_talk::hold(true),
                WM_KEYUP | WM_SYSKEYUP => yapfast_core::push_to_talk::hold(false),
                _ => {}
            }
        }
    }
    // SAFETY: Forward every key event so this hook never consumes input.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::winit_030::winit::{
        event_loop::EventLoop, platform::windows::EventLoopBuilderExtWindows,
    };
    use std::time::{Duration, Instant};
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
            SendInput,
        },
        WindowsAndMessaging::SendMessageW,
    };

    #[test]
    fn a_remaining_input_sink_cannot_execute_without_session_notifications() {
        let count = Arc::new(AtomicU32::new(0));
        let heard = count.clone();
        let actions = Actions::new(move |_| {
            heard.fetch_add(1, Ordering::Relaxed);
        });
        actions.set_enabled(true);
        let bindings = Bindings {
            leave_voice: Some(super::super::Chord {
                key: super::super::Keybind {
                    scancode: 0x26,
                    name: "L".into(),
                    trigger: None,
                },
                modifiers: super::super::ModifierMask::default(),
            }),
            ..Bindings::default()
        };
        let mut input = Input::new();
        input.matcher = Matcher::new(bindings.clone(), ModifierLayout::WindowsSet1);
        input.bindings = bindings;
        input.actions = Some(actions);
        // Registration restoration can fail after notification setup fails.
        input.input_sink = true;
        let shared = Shared {
            handler_installed: AtomicBool::new(true),
            input: Mutex::new(input),
        };
        shared.key(DeviceId::dummy(), 0x26, true);
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    /// Exercises the real two-call Win32 query; a null-buffer size query
    /// returns INSUFFICIENT_BUFFER, not a successful element count.
    #[test]
    #[ignore = "needs a native Windows desktop"]
    fn winit_keyboard_receiver_can_be_promoted_and_restored() {
        let mut builder = EventLoop::builder();
        builder.with_any_thread(true);
        let _events = builder.build().unwrap();
        let before =
            keyboard_registration().expect("Winit's keyboard receiver must be discoverable");
        let mut input = Input::new();
        input.original = Some(before);
        input.target = Some(before.target);
        input.bindings.leave_voice = Some(super::super::Chord {
            key: super::super::Keybind {
                scancode: 0x26,
                name: "L".into(),
                trigger: None,
            },
            modifiers: super::super::ModifierMask::CTRL,
        });
        struct Restore(Input);
        impl Drop for Restore {
            fn drop(&mut self) {
                restore_input_sink(&mut self.0);
                unregister_notifications(&mut self.0);
            }
        }
        let mut guard = Restore(input);
        activate(&mut guard.0);
        assert!(guard.0.input_sink, "background registration must succeed");
        let active = keyboard_registration().unwrap();
        assert_eq!(active.target, before.target);
        assert_ne!(active.flags & RIDEV_INPUTSINK, 0);
        assert!(restore_input_sink(&mut guard.0));
        let restored = keyboard_registration().unwrap();
        assert_eq!(
            (restored.target, restored.flags),
            (before.target, before.flags)
        );
    }
    #[test]
    #[ignore = "injects keys into the interactive Windows desktop"]
    fn the_ptt_hook_observes_down_up_and_ignores_other_keys() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                yapfast_core::push_to_talk::hold(false);
                yapfast_core::push_to_talk::set_on(false);
            }
        }
        let _reset = Reset;
        yapfast_core::push_to_talk::set_on(true);
        let bindings = Bindings {
            talk: Some(super::super::Keybind {
                scancode: 0x2f,
                name: "V".into(),
                trigger: None,
            }),
            ..Bindings::default()
        };
        let hook = Hook::start(0x2f, StatusReporter::new(&bindings, |_, _| {}))
            .expect("Windows did not install the PTT hook");
        send_key(0x2f, false);
        wait_held(true);
        send_key(0x2f, true);
        wait_held(false);
        send_key(0x30, false);
        send_key(0x30, true);
        thread::sleep(Duration::from_millis(100));
        assert!(!yapfast_core::push_to_talk::held());
        drop(hook);
    }

    fn send_key(scancode: u16, up: bool) {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: scancode,
                    dwFlags: KEYEVENTF_SCANCODE | if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // SAFETY: One correctly sized synthetic keyboard input.
        let sent = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
        assert_eq!(sent, 1);
    }

    fn wait_held(held: bool) {
        let until = Instant::now() + Duration::from_secs(2);
        while yapfast_core::push_to_talk::held() != held {
            assert!(Instant::now() < until, "PTT state did not become {held}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "needs a native Windows desktop and sends messages to Winit's input HWND"]
    fn sent_session_and_power_messages_clear_modifier_state() {
        let _handler = custom_application_handler();
        let mut builder = EventLoop::builder();
        builder.with_any_thread(true);
        let _events = builder.build().unwrap();
        let count = Arc::new(AtomicU32::new(0));
        let heard = count.clone();
        let actions = Actions::new(move |_| {
            heard.fetch_add(1, Ordering::Relaxed);
        });
        actions.set_enabled(true);
        let bindings = Bindings {
            leave_voice: Some(super::super::Chord {
                key: super::super::Keybind {
                    scancode: 0x26,
                    name: "L".into(),
                    trigger: None,
                },
                modifiers: super::super::ModifierMask::CTRL | super::super::ModifierMask::SHIFT,
            }),
            ..Bindings::default()
        };
        let statuses = StatusReporter::new(&bindings, |_, _| {});
        let global = Global::start(&bindings, statuses, actions);
        let target = shared()
            .input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .target
            .expect("Winit's keyboard receiver is missing");
        assert!(
            shared()
                .input
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .subclass_installed
        );

        let key = |scancode, pressed| shared().key(DeviceId::dummy(), scancode, pressed);
        key(0x1d, true);
        // SendMessage exercises the WndProc path; winit's queue hook cannot
        // observe this direct delivery.
        unsafe {
            SendMessageW(
                target as _,
                WM_POWERBROADCAST_VALUE,
                PBT_APMRESUMEAUTOMATIC_VALUE as usize,
                0,
            );
        }
        key(0x2a, true);
        key(0x26, true);
        assert_eq!(count.load(Ordering::Relaxed), 0);
        key(0x26, false);
        key(0x2a, false);

        key(0x1d, true);
        unsafe {
            SendMessageW(
                target as _,
                WM_WTSSESSION_CHANGE_VALUE,
                WTS_SESSION_UNLOCK_VALUE as usize,
                0,
            );
        }
        key(0x2a, true);
        key(0x26, true);
        assert_eq!(count.load(Ordering::Relaxed), 0);
        key(0x26, false);
        key(0x2a, false);

        key(0x1d, true);
        key(0x2a, true);
        key(0x26, true);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        drop(global);
    }
}
