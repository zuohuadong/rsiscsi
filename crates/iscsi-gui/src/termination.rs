//! Route native termination through the same asynchronous window-close gate.
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

struct QuitGate {
    allowed: AtomicBool,
    requested: UnboundedSender<()>,
}

impl QuitGate {
    fn reply(&self) -> usize {
        if self.allowed.load(Ordering::Acquire) {
            1 // NSTerminateNow
        } else {
            let _ = self.requested.send(());
            0 // NSTerminateCancel, including when the UI receiver has failed.
        }
    }
}

static GATE: OnceLock<QuitGate> = OnceLock::new();

pub fn request() {
    if let Some(gate) = GATE.get() {
        let _ = gate.requested.send(());
    }
}

pub fn allow() {
    if let Some(gate) = GATE.get() {
        gate.allowed.store(true, Ordering::Release);
    }
}

pub fn install() -> Result<UnboundedReceiver<()>, String> {
    let (requested, receiver) = mpsc::unbounded_channel();
    GATE.set(QuitGate {
        allowed: AtomicBool::new(false),
        requested,
    })
    .map_err(|_| "退出保护已安装".to_owned())?;
    #[cfg(target_os = "macos")]
    macos::install()?;
    Ok(receiver)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void};

    #[cfg(target_arch = "x86_64")]
    type ObjcBool = i8;
    #[cfg(not(target_arch = "x86_64"))]
    type ObjcBool = bool;

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn class_addMethod(
            class: *mut c_void,
            selector: *mut c_void,
            implementation: *const c_void,
            types: *const c_char,
        ) -> ObjcBool;
    }

    extern "C" fn should_terminate(_: *mut c_void, _: *mut c_void, _: *mut c_void) -> usize {
        super::GATE.get().map_or(0, super::QuitGate::reply)
    }

    pub(super) fn install() -> Result<(), String> {
        // gpui-pre-macos 0.3.12 only installs applicationWillTerminate:, which
        // cannot veto. Add the earlier delegate method without replacing GPUI's
        // cleanup. Fail closed if a future dependency changes this contract.
        let installed = unsafe {
            let class = objc_getClass(c"GPUIApplicationDelegate".as_ptr());
            if class.is_null() {
                return Err("无法安装 macOS 退出保护：GPUI 委托不存在".into());
            }
            class_addMethod(
                class,
                sel_registerName(c"applicationShouldTerminate:".as_ptr()),
                should_terminate as *const () as *const c_void,
                c"Q@:@".as_ptr(),
            )
        };
        if installed as u8 != 0 {
            Ok(())
        } else {
            Err("无法安装 macOS 退出保护：委托方法已存在".into())
        }
    }

    #[cfg(test)]
    #[test]
    fn native_selector_registration_dispatches_and_preserves_gpui_cleanup() {
        #[link(name = "objc")]
        unsafe extern "C" {
            fn class_getInstanceMethod(class: *mut c_void, selector: *mut c_void) -> *mut c_void;
            fn method_getTypeEncoding(method: *mut c_void) -> *const c_char;
            fn method_getImplementation(method: *mut c_void) -> *const c_void;
        }
        let mut requests = super::install().unwrap();
        // Only the Objective-C runtime is used; no AppKit window or main-thread
        // UI is needed, and these classes live in the isolated test process.
        unsafe {
            let class = objc_getClass(c"GPUIApplicationDelegate".as_ptr());
            let selector = sel_registerName(c"applicationShouldTerminate:".as_ptr());
            let method = class_getInstanceMethod(class, selector);
            assert!(!method.is_null());
            assert_eq!(
                std::ffi::CStr::from_ptr(method_getTypeEncoding(method)),
                c"Q@:@"
            );
            let callback: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> usize =
                std::mem::transmute(method_getImplementation(method));
            assert_eq!(
                callback(std::ptr::null_mut(), selector, std::ptr::null_mut()),
                0
            );
            assert_eq!(requests.try_recv(), Ok(()));
            assert!(
                !class_getInstanceMethod(
                    class,
                    sel_registerName(c"applicationWillTerminate:".as_ptr())
                )
                .is_null()
            );
            super::allow();
            assert_eq!(
                callback(std::ptr::null_mut(), selector, std::ptr::null_mut()),
                1
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_is_vetoed_until_cleanup_success_and_receiver_failure_is_closed() {
        let (requested, mut receiver) = mpsc::unbounded_channel();
        let gate = QuitGate {
            allowed: AtomicBool::new(false),
            requested,
        };
        assert_eq!(gate.reply(), 0);
        assert_eq!(receiver.try_recv(), Ok(()));
        drop(receiver);
        assert_eq!(gate.reply(), 0);
        gate.allowed.store(true, Ordering::Release);
        assert_eq!(gate.reply(), 1);
    }
}
