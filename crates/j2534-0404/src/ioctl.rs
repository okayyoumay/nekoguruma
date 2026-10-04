/// Extension point for vendor-specific or future `PassThruIoctl` commands.
///
/// Implement this trait to describe a custom IOCTL call and pass it to
/// [`J2534Api0404::ioctl`].  The built-in methods on `J2534Api0404` cover all
/// standard J2534 v04.04 IOCTLs; use this trait only for IOCTL IDs that are
/// not exposed through those methods (e.g. adapter-specific extensions).
///
/// # Safety
/// Implementors must guarantee:
/// - [`input_ptr`] returns either `null_mut()` or a pointer to memory that is
///   valid, correctly aligned, and readable by the adapter DLL for the entire
///   duration of the [`J2534Api0404::ioctl`] call.
/// - [`output_ptr`] returns either `null_mut()` or a pointer to memory that is
///   valid, correctly aligned, and writable by the adapter DLL for the entire
///   duration of the call.
/// - The in-memory layout at each pointer matches what the adapter DLL expects
///   for the given [`ioctl_id`].
///
/// [`J2534Api0404::ioctl`]: crate::J2534Api0404::ioctl
/// [`input_ptr`]: IoCtlCommand::input_ptr
/// [`output_ptr`]: IoCtlCommand::output_ptr
/// [`ioctl_id`]: IoCtlCommand::ioctl_id
pub unsafe trait IoCtlCommand {
    /// IOCTL ID passed as the second argument to `PassThruIoctl`.
    fn ioctl_id(&self) -> u32;

    /// Input data pointer (`pInput`).  Returns `null_mut()` by default.
    fn input_ptr(&mut self) -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }

    /// Output data pointer (`pOutput`).  Returns `null_mut()` by default.
    fn output_ptr(&mut self) -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }
}
