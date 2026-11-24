//! Overlapped HID handles and the operations queued on them.
//!
//! An [`Op`] is one overlapped read, write or feature control with its own buffer. While the kernel
//! owns it, it lives as a raw pointer: the `OVERLAPPED` pointer handed to the call is the
//! pointer to the whole `Op`. The completion port returns that pointer and
//! [`Op::from_completion`] makes it a `Box` again, so an `Op` is freed only after its
//! completion packet has been dequeued.

#[cfg(test)]
use std::ffi::c_void;
use std::ptr::NonNull;
use std::slice;

use batterysight_core::hid::CollectionId;
use windows::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_IO_PENDING, ERROR_MORE_DATA, GENERIC_READ, GENERIC_WRITE,
    HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    WriteFile,
};
use windows::Win32::System::IO::{DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows::core::PCWSTR;

use crate::hid_enum::OwnedHandle;

// hidclass.h: CTL_CODE(FILE_DEVICE_KEYBOARD, 100, METHOD_*_DIRECT, FILE_ANY_ACCESS).
const IOCTL_HID_SET_FEATURE: u32 = (0x0B << 16) | (100 << 2) | 1; // METHOD_IN_DIRECT
const IOCTL_HID_GET_FEATURE: u32 = (0x0B << 16) | (100 << 2) | 2; // METHOD_OUT_DIRECT

#[cfg(test)]
pub(crate) type FeatureControl = unsafe fn(
    HANDLE,
    u32,
    Option<*const c_void>,
    u32,
    Option<*mut c_void>,
    u32,
    Option<*mut u32>,
    Option<*mut OVERLAPPED>,
) -> windows::core::Result<()>;

#[cfg(test)]
pub(crate) const FEATURE_CONTROL: FeatureControl = DeviceIoControl;

/// Whether Windows holds the collection exclusively: mouse (0x01/0x02) and keyboard
/// (0x01/0x06) collections of the generic desktop page. HID++ uses the vendor collections.
pub(crate) fn is_exclusive(usage_page: u16, usage: u16) -> bool {
    usage_page == 0x01 && matches!(usage, 0x02 | 0x06)
}

/// Opens a collection with the selected access rights, shared read/write, never exclusively.
/// A path that is not NUL-terminated is refused with `ERROR_INVALID_PARAMETER`.
pub(crate) fn open(path: &[u16], read: bool, write: bool) -> windows::core::Result<OwnedHandle> {
    if path.last() != Some(&0) {
        return Err(ERROR_INVALID_PARAMETER.into());
    }
    let read = if read { GENERIC_READ.0 } else { 0 };
    let write = if write { GENERIC_WRITE.0 } else { 0 };
    // SAFETY: `path` is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            read | write,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
    }?;
    Ok(OwnedHandle(handle))
}

/// `bytes` zero-padded to an output report of `len` bytes, or `None` when `bytes` is empty or
/// longer than the report.
pub(crate) fn padded(bytes: &[u8], len: usize) -> Option<Vec<u8>> {
    if bytes.is_empty() || bytes.len() > len {
        return None;
    }
    let mut buffer = Vec::with_capacity(len);
    buffer.extend_from_slice(bytes);
    buffer.resize(len, 0);
    Some(buffer)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Read,
    Write,
    SetFeature,
    GetFeature { report_id: u8 },
}

/// One overlapped operation and the buffer it fills or sends.
#[repr(C)]
pub(crate) struct Op {
    /// First field, so a pointer to it is a pointer to the `Op`.
    overlapped: OVERLAPPED,
    pub(crate) collection: CollectionId,
    pub(crate) kind: Kind,
    pub(crate) buffer: Vec<u8>,
}

impl Op {
    pub(crate) fn new(collection: CollectionId, kind: Kind, buffer: Vec<u8>) -> Box<Op> {
        Box::new(Op {
            overlapped: OVERLAPPED::default(),
            collection,
            kind,
            buffer,
        })
    }

    /// Starts the operation on `handle`, which must be associated with a completion port.
    ///
    /// Reports whether a completion packet will be queued.
    pub(crate) fn start(self: Box<Self>, handle: HANDLE) -> bool {
        self.submit(|kind, buffer, len, overlapped| {
            // SAFETY: `submit` gives Windows the buffer and OVERLAPPED until dequeue.
            unsafe {
                match kind {
                    Kind::Read => ReadFile(
                        handle,
                        Some(slice::from_raw_parts_mut(buffer, len)),
                        None,
                        Some(overlapped),
                    ),
                    Kind::Write => WriteFile(
                        handle,
                        Some(slice::from_raw_parts(buffer, len)),
                        None,
                        Some(overlapped),
                    ),
                    _ => unreachable!("feature controls use start_feature"),
                }
            }
        })
        .is_ok()
    }

    /// Starts a GET or SET control and returns its OVERLAPPED for targeted cancellation.
    pub(crate) fn start_feature(
        self: Box<Self>,
        handle: HANDLE,
        #[cfg(test)] control: FeatureControl,
    ) -> windows::core::Result<NonNull<OVERLAPPED>> {
        #[cfg(not(test))]
        let control = DeviceIoControl;
        self.submit(|kind, buffer, len, overlapped| {
            let len = len as u32; // HID report widths come from the u16 descriptor lengths.
            let (code, input, input_len, output, output_len) = match kind {
                Kind::SetFeature => (
                    IOCTL_HID_SET_FEATURE,
                    Some(buffer.cast_const().cast()),
                    len,
                    None,
                    0,
                ),
                Kind::GetFeature { .. } => {
                    (IOCTL_HID_GET_FEATURE, None, 0, Some(buffer.cast()), len)
                }
                _ => unreachable!("read and write use start"),
            };
            // SAFETY: `submit` gives Windows the buffer and OVERLAPPED until dequeue.
            unsafe {
                control(
                    handle,
                    code,
                    input,
                    input_len,
                    output,
                    output_len,
                    None,
                    Some(overlapped),
                )
            }
        })
    }

    /// Hands off this allocation until dequeue, or reclaims it if submission fails with
    /// an error status and therefore will not produce an IOCP packet.
    fn submit(
        mut self: Box<Self>,
        issue: impl FnOnce(Kind, *mut u8, usize, *mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> windows::core::Result<NonNull<OVERLAPPED>> {
        self.overlapped = OVERLAPPED::default();
        let op = Box::into_raw(self);
        let overlapped = op.cast::<OVERLAPPED>();
        // SAFETY: the allocation is stable. No one else accesses it before submission;
        // afterward the kernel owns it while a completion packet is expected.
        let result =
            unsafe { issue((*op).kind, (*op).buffer.as_mut_ptr(), (*op).buffer.len(), overlapped) };
        if queues_completion(&result) {
            return Ok(NonNull::new(overlapped).expect("Box yields a non-null pointer"));
        }
        // SAFETY: the request failed without queueing a packet; the kernel kept no reference.
        drop(unsafe { Box::from_raw(op) });
        Err(result.expect_err("a successful overlapped request queues a completion"))
    }

    /// Takes back an `Op` whose completion packet was dequeued.
    ///
    /// # Safety
    /// `overlapped` must come from a completion packet for a submitted `Op`, and each packet
    /// may be taken back only once.
    pub(crate) unsafe fn from_completion(overlapped: *mut OVERLAPPED) -> Box<Op> {
        // SAFETY: per the contract, the pointer is a leaked `Box<Op>`; `overlapped` is its
        // first field of a `repr(C)` struct.
        unsafe { Box::from_raw(overlapped.cast::<Op>()) }
    }

    /// Bytes transferred by the completed operation, or its error. `handle` is the handle the
    /// operation was started on.
    pub(crate) fn result(&self, handle: HANDLE) -> windows::core::Result<usize> {
        let mut transferred = 0;
        // SAFETY: the operation has completed, so this only reads its status; no wait happens.
        unsafe { GetOverlappedResult(handle, &self.overlapped, &mut transferred, false) }?;
        Ok(transferred as usize)
    }
}

/// Whether a just-issued overlapped request will queue a completion packet.
///
/// The I/O manager queues one unless the request failed with an error status. Success,
/// pending, and a warning status all queue one; for example, `STATUS_BUFFER_OVERFLOW`
/// surfaces as `ERROR_MORE_DATA`.
fn queues_completion(result: &windows::core::Result<()>) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => {
            error.code() == ERROR_IO_PENDING.to_hresult()
                || error.code() == ERROR_MORE_DATA.to_hresult()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_OPERATION_ABORTED};

    #[test]
    fn only_mouse_and_keyboard_are_exclusive() {
        assert!(is_exclusive(0x01, 0x02));
        assert!(is_exclusive(0x01, 0x06));
        assert!(!is_exclusive(0x01, 0x05));
        assert!(!is_exclusive(0x0C, 0x01));
        assert!(!is_exclusive(0xFF00, 0x02));
    }

    #[test]
    fn writes_are_zero_padded_to_the_report() {
        assert_eq!(padded(&[0x10, 1], 4), Some(vec![0x10, 1, 0, 0]));
        assert_eq!(padded(&[1, 2, 3], 3), Some(vec![1, 2, 3]));
        assert_eq!(padded(&[1, 2, 3], 2), None);
        assert_eq!(padded(&[], 7), None);
    }

    #[test]
    fn warnings_and_pending_queue_a_completion() {
        assert!(queues_completion(&Ok(())));
        assert!(queues_completion(&Err(ERROR_IO_PENDING.into())));
        assert!(queues_completion(&Err(ERROR_MORE_DATA.into())));
        assert!(!queues_completion(&Err(ERROR_ACCESS_DENIED.into())));
        assert!(!queues_completion(&Err(ERROR_OPERATION_ABORTED.into())));
    }
}
