//! Overlapped HID handles and the operations queued on them.
//!
//! An [`Op`] is one overlapped `ReadFile` or `WriteFile` with its own buffer. While the kernel
//! owns it, it lives as a raw pointer: the `OVERLAPPED` pointer handed to the call is the
//! pointer to the whole `Op`. The completion port returns that pointer and
//! [`Op::from_completion`] makes it a `Box` again, so an `Op` is freed only after its
//! completion packet has been dequeued.

use std::slice;

use batterysight_core::hid::CollectionId;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, GENERIC_READ, GENERIC_WRITE,
    HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    WriteFile,
};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows::core::PCWSTR;

use crate::hid_enum::OwnedHandle;

/// Whether Windows holds the collection exclusively: mouse (0x01/0x02) and keyboard
/// (0x01/0x06) collections of the generic desktop page. HID++ uses the vendor collections.
pub(crate) fn is_exclusive(usage_page: u16, usage: u16) -> bool {
    usage_page == 0x01 && matches!(usage, 0x02 | 0x06)
}

/// Opens a collection for overlapped I/O, shared read/write, never exclusively.
///
/// Asks for read and write access, and for read access alone when that is denied. Returns the
/// handle and whether it is writable. `path` must be NUL-terminated.
pub(crate) fn open(path: &[u16]) -> windows::core::Result<(OwnedHandle, bool)> {
    debug_assert_eq!(path.last(), Some(&0), "path must be NUL-terminated");
    match create(path, GENERIC_READ.0 | GENERIC_WRITE.0) {
        Ok(handle) => Ok((handle, true)),
        Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            Ok((create(path, GENERIC_READ.0)?, false))
        }
        Err(error) => Err(error),
    }
}

fn create(path: &[u16], access: u32) -> windows::core::Result<OwnedHandle> {
    // SAFETY: `path` is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            access,
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
}

/// One overlapped read or write and the buffer it fills or sends.
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
    /// Returns `true` when the operation is pending or already complete: either way a
    /// completion packet will be queued, and the `Op` belongs to the kernel until
    /// [`Op::from_completion`] takes it back. Returns `false` when the call failed at once; no
    /// packet is queued then and the `Op` is freed here.
    pub(crate) fn start(mut self: Box<Self>, handle: HANDLE) -> bool {
        self.overlapped = OVERLAPPED::default();
        let len = self.buffer.len();
        let op = Box::into_raw(self);
        // SAFETY: `op` stays allocated until its completion is dequeued, or until it is freed
        // below when no completion will come. Nothing else touches its buffer meanwhile.
        let result = unsafe {
            let overlapped = &raw mut (*op).overlapped;
            let buffer = (*op).buffer.as_mut_ptr();
            match (*op).kind {
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
            }
        };
        match result {
            Ok(()) => true,
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => true,
            Err(_) => {
                // SAFETY: the call failed synchronously, so the kernel kept no reference.
                drop(unsafe { Box::from_raw(op) });
                false
            }
        }
    }

    /// Takes back an `Op` whose completion packet was dequeued.
    ///
    /// # Safety
    /// `overlapped` must come from a completion packet for an `Op` passed to [`Op::start`],
    /// and each packet may be taken back only once.
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

/// Whether `error` reports an operation cancelled by `CancelIoEx`.
pub(crate) fn is_cancelled(error: &windows::core::Error) -> bool {
    error.code() == ERROR_OPERATION_ABORTED.to_hresult()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
