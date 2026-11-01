//! Which application is in front, for per-app profile switching.
//!
//! Returns the app's bundle path (e.g. `/Applications/Zoom.app`), or the
//! executable path for bare binaries that have no bundle. Polled from the UI
//! thread, which AppKit requires.

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)]
pub fn frontmost_app_path() -> Option<String> {
    use cocoa::base::{id, nil};
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::CStr;
    use std::os::raw::c_char;

    objc::rc::autoreleasepool(|| unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let app: id = msg_send![workspace, frontmostApplication];
        if app == nil {
            return None;
        }
        let mut url: id = msg_send![app, bundleURL];
        if url == nil {
            url = msg_send![app, executableURL];
        }
        if url == nil {
            return None;
        }
        let path: id = msg_send![url, path];
        if path == nil {
            return None;
        }
        let utf8: *const c_char = msg_send![path, UTF8String];
        (!utf8.is_null()).then(|| CStr::from_ptr(utf8).to_string_lossy().into_owned())
    })
}

#[cfg(not(target_os = "macos"))]
pub fn frontmost_app_path() -> Option<String> {
    None
}
