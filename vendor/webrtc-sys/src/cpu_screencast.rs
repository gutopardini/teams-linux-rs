//! Blocks on a portal PipeWire fd and delivers mapped BGRA frames.
//! DMA-BUF is never read: a black EGL import must not become a shared screen.

use std::ffi::c_void;
use std::sync::atomic::AtomicBool;

#[repr(C)]
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data_type: u32,
    pub format: u32,
    pub data: *const u8,
    pub size: usize,
}

pub type FrameFn = unsafe extern "C" fn(*mut c_void, *const CpuFrame);

extern "C" {
    fn teams_cpu_screencast(
        fd: i32,
        node_id: u32,
        stop: *const u8,
        callback: FrameFn,
        user: *mut c_void,
    ) -> i32;
}

/// `fd` is consumed. Returns 0 when `stop` is set, and 1 when the stream fails.
///
/// # Safety
/// `callback` may run on the PipeWire thread until this function returns.
/// `user` must stay valid for that whole call, and the callback must not keep
/// the frame pointer.
pub fn consume(
    fd: i32,
    node_id: u32,
    stop: &AtomicBool,
    callback: FrameFn,
    user: *mut c_void,
) -> i32 {
    let stop = stop as *const AtomicBool as *const u8;
    unsafe { teams_cpu_screencast(fd, node_id, stop, callback, user) }
}
