//! Disposable C adapters; all expectations are owned by the Rust integration tests.
#![allow(dead_code)]
use std::{
    ffi::{CStr, CString},
    path::Path,
    process::Command,
};
pub struct Guest {
    handle: *mut libc::c_void,
    call: unsafe extern "C" fn(
        *const libc::c_char,
        *const libc::c_char,
        *const libc::c_char,
        i64,
    ) -> i64,
    pub root: tempfile::TempDir,
}
impl Guest {
    pub fn new() -> Self {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let binary = root.path().join("guest-library");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/guest_bridge.c");
        let mut command = Command::new("cc");
        command.args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Wno-unused-function",
            "-fPIC",
        ]);
        command.arg(if cfg!(target_os = "macos") {
            "-dynamiclib"
        } else {
            "-shared"
        });
        let output = command
            .arg(source)
            .arg("-I")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guest"))
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = CString::new(binary.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the adapter is compiled above and retained until dlclose.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        assert!(
            !handle.is_null(),
            "{}",
            unsafe { CStr::from_ptr(libc::dlerror()) }.to_string_lossy()
        );
        let mut guest = Self {
            handle,
            call: placeholder,
            root,
        };
        guest.call = unsafe { guest.symbol(c"sb_call") };
        guest
    }
    pub unsafe fn symbol<T: Copy>(&self, name: &CStr) -> T {
        // SAFETY: callers provide the exact signature exported by guest_bridge.c.
        let pointer = unsafe { libc::dlsym(self.handle, name.as_ptr()) };
        assert!(!pointer.is_null(), "missing C adapter symbol {name:?}");
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&pointer));
        unsafe { std::mem::transmute_copy(&pointer) }
    }
    pub fn call(&self, operation: &str, a: Option<&str>, b: Option<&str>, value: i64) -> i64 {
        let operation = CString::new(operation).unwrap();
        let a = a.map(|a| CString::new(a).unwrap());
        let b = b.map(|b| CString::new(b).unwrap());
        unsafe {
            (self.call)(
                operation.as_ptr(),
                a.as_ref().map_or(std::ptr::null(), |a| a.as_ptr()),
                b.as_ref().map_or(std::ptr::null(), |b| b.as_ptr()),
                value,
            )
        }
    }
    pub fn get(&self, operation: &str) -> i64 {
        self.call(operation, None, None, 0)
    }
    pub fn set(&self, operation: &str, value: i64) -> i64 {
        self.call(operation, None, None, value)
    }
    pub fn text(&self, operation: &str, text: &str) -> i64 {
        self.call(operation, Some(text), None, 0)
    }
    #[cfg(target_os = "linux")]
    pub fn child(&self) -> std::path::PathBuf {
        let binary = self.root.path().join("child");
        let output = Command::new("cc")
            .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/supervised_child.c"))
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        binary
    }
}
unsafe extern "C" fn placeholder(
    _: *const libc::c_char,
    _: *const libc::c_char,
    _: *const libc::c_char,
    _: i64,
) -> i64 {
    -1
}
impl Drop for Guest {
    fn drop(&mut self) {
        self.get("cleanup");
        unsafe {
            libc::dlclose(self.handle);
        }
    }
}
