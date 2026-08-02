//! Accessibility / input-monitoring permission gating for the global hotkey
//! listener.
//!
//! `rdev::listen()` (the X11/macOS backend behind [`super::RdevBackend`]) has
//! very different permission requirements per OS:
//!
//! * **macOS** — `listen()` *silently no-ops* without Accessibility permission
//!   (System Settings → Privacy & Security → Accessibility). No error is
//!   returned; the callback simply never fires. We must gate startup on
//!   `AXIsProcessTrusted()` and, when not yet trusted, prompt the user via
//!   `AXIsProcessTrustedWithOptions({kAXTrustedCheckOptionPrompt: true})`.
//! * **Windows** — works via a low-level keyboard hook; no special permission.
//! * **Linux (X11)** — no OS-level permission gate (X11 lets clients observe
//!   global key events); native Wayland is a documented follow-up handled by a
//!   different backend.
//!
//! Only the macOS path is non-trivial. The non-macOS implementation returns
//! `true` unconditionally so callers can use a single uniform check.

/// Returns `true` if the process is allowed to observe global keyboard events
/// (i.e. the hotkey listener will actually receive callbacks).
///
/// On macOS this consults the Accessibility trust state and, if the process is
/// not yet trusted, surfaces the system prompt directing the user to grant
/// permission. The call is non-blocking: it returns the *current* trust state
/// immediately (the prompt, if shown, is handled asynchronously by the OS, and
/// the user must re-launch / re-check after granting). On all other platforms
/// it returns `true`.
pub fn ensure_accessibility() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::ensure_accessibility()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// macOS Accessibility (AX) trust check.
///
/// Written-but-unverified on this Windows host; compiled and exercised only on
/// macOS hardware/CI. Implemented against the public `ApplicationServices`
/// C ABI so it needs no extra crate dependency.
#[cfg(target_os = "macos")]
mod macos {
    use std::os::raw::c_void;

    // --- Core Foundation / ApplicationServices FFI -------------------------
    //
    // We link the umbrella `ApplicationServices` framework, which re-exports the
    // HIServices `AXIsProcessTrusted*` symbols and CoreFoundation. We declare
    // the handful of CF symbols we need rather than pulling in a CF crate.

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type Boolean = u8;

    // CFBoolean is a CFTypeRef; kCFBooleanTrue is a global constant symbol.
    #[allow(non_upper_case_globals)]
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFBooleanTrue: CFTypeRef;
        static kCFAllocatorDefault: CFAllocatorRef;

        fn CFStringCreateWithCString(
            alloc: CFAllocatorRef,
            c_str: *const i8,
            encoding: u32,
        ) -> CFStringRef;

        fn CFDictionaryCreate(
            allocator: CFAllocatorRef,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFDictionaryRef;

        fn CFRelease(cf: CFTypeRef);

        // Default callback structs for retained CF object keys/values.
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> Boolean;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> Boolean;

        // The CFStringRef key used to request the system prompt.
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    // kCFStringEncodingUTF8
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    pub fn ensure_accessibility() -> bool {
        // Fast path: already trusted, nothing to prompt.
        if unsafe { AXIsProcessTrusted() } != 0 {
            return true;
        }

        // Not trusted yet — build {kAXTrustedCheckOptionPrompt: true} and call
        // the prompting variant so the user is directed to the settings pane.
        unsafe {
            let key: CFStringRef = kAXTrustedCheckOptionPrompt;
            let value: CFTypeRef = kCFBooleanTrue;

            let keys = [key as *const c_void];
            let values = [value as *const c_void];

            let options = CFDictionaryCreate(
                kCFAllocatorDefault,
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
                &kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
            );

            let trusted = AXIsProcessTrustedWithOptions(options) != 0;

            if !options.is_null() {
                CFRelease(options);
            }

            // Silence the unused-import-style warning for the helper we keep
            // available for callers that want to build CF strings themselves.
            let _ = (
                CFStringCreateWithCString as usize,
                K_CF_STRING_ENCODING_UTF8,
            );

            trusted
        }
    }
}
