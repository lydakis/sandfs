//! Handing memory the process freed back to the system.
//!
//! Freeing memory gives it back to the allocator, which keeps it for the next
//! allocation instead of returning it: after a mount of a large folder ends, macOS's
//! allocator still holds hundreds of megabytes of freed pages for the process, and
//! glibc's holds some too. Only the allocator can let them go, when asked to.
//!
//! This is the one place sandfs calls code Rust can't check: each call below takes no
//! pointer of ours (a null zone means every zone) and only returns unused pages to the
//! system, so it can't touch memory in use.

/// Ask the allocator to return the memory it holds unused, after a trim or once a mount
/// is gone. Returns the bytes given back, where the allocator says.
#[allow(unsafe_code)]
pub fn give_back() -> Option<usize> {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            /// `<malloc/malloc.h>`: returns the free pages of `zone` (every zone when
            /// null) to the system, until `goal` bytes are freed (every one when 0).
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone and a goal of 0 are documented to mean every zone and
        // every free page; nothing else is passed, and only unused pages are released.
        let released = unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
        Some(released)
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: `malloc_trim` takes no pointer; 0 keeps no padding at the heap's top.
        unsafe {
            libc::malloc_trim(0);
        }
        None
    }
    // musl's allocator returns freed memory on its own.
    #[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    None
}
