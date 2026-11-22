//! Windows: the server puts itself in a job object that kills its members when the last handle to
//! it closes (ADR-491), so the object server, which inherits the membership, never outlives the
//! server, however the server ends. The handle is held for the life of the process and never
//! closed: closing it would kill the server itself.

use std::io;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Puts this process in a kill-on-close job object, once per process; later calls return the
/// first call's outcome.
pub(super) fn ensure() -> Result<(), io::ErrorKind> {
    static JOB: OnceLock<Result<(), io::ErrorKind>> = OnceLock::new();
    *JOB.get_or_init(|| create().map_err(|error| error.kind()))
}

#[expect(
    unsafe_code,
    reason = "creates a kill-on-close Win32 job object for the server process and assigns the process to it (ADR-502)"
)]
fn create() -> io::Result<()> {
    // SAFETY: plain Win32 calls with valid arguments. `info` is a zeroed plain-data structure of
    // the size the call is told; the job handle is checked and then deliberately leaked.
    unsafe {
        let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set == 0 || AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
            // The handle is not leaked on failure, and nothing is in the job yet.
            let error = io::Error::last_os_error();
            windows_sys::Win32::Foundation::CloseHandle(job);
            return Err(error);
        }
    }
    Ok(())
}
