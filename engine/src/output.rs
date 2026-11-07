//! The Mac's default output device, by UID (see output_shim.c).

use std::ffi::{CStr, CString};

/// Our device's UID (kDeviceUID in the driver).
pub const OUR_UID: &str = "LS50Stream_UID";

extern "C" {
    fn ls50stream_default_output_uid(buf: *mut libc::c_char, len: libc::c_int) -> libc::c_int;
    fn ls50stream_set_default_output_uid(uid: *const libc::c_char) -> libc::c_int;
}

pub fn default_output() -> Option<String> {
    let mut buf = [0 as libc::c_char; 512];
    if unsafe { ls50stream_default_output_uid(buf.as_mut_ptr(), buf.len() as libc::c_int) } != 0 {
        return None;
    }
    Some(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}

pub fn set_default_output(uid: &str) -> bool {
    let Ok(uid) = CString::new(uid) else { return false };
    unsafe { ls50stream_set_default_output_uid(uid.as_ptr()) == 0 }
}
