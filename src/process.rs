use anyhow::{bail, Context, Result};
use std::{os::windows::io::AsRawHandle, process::Child};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::JobObjects::*,
};
#[link(name = "ntdll")]
extern "system" {
    fn NtResumeProcess(handle: HANDLE) -> i32;
}
pub(crate) struct ProcessJob(HANDLE);
unsafe impl Send for ProcessJob {}
impl ProcessJob {
    pub(crate) fn assign(child: &Child) -> Result<Self> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error()).context(crate::i18n::ui(
                    "Windows Job oluşturulamadı",
                    "The Windows Job could not be created",
                ));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as _,
                std::mem::size_of_val(&info) as u32,
            ) == 0
            {
                let error = std::io::Error::last_os_error();
                CloseHandle(handle);
                return Err(error).context(crate::i18n::ui(
                    "Windows Job sınırı ayarlanamadı",
                    "The Windows Job limit could not be set",
                ));
            }
            if AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) == 0 {
                let error = std::io::Error::last_os_error();
                CloseHandle(handle);
                return Err(error).context(crate::i18n::ui(
                    "Medya süreci sahip olunan Windows Job nesnesine atanamadı",
                    "The media process could not be assigned to the owned Windows Job object",
                ));
            }
            let resume_status = NtResumeProcess(child.as_raw_handle() as HANDLE);
            if resume_status < 0 {
                TerminateJobObject(handle, 1);
                CloseHandle(handle);
                bail!(
                    "{}",
                    crate::i18n::ui_format!("Medya süreci Windows Job atamasından sonra başlatılamadı (NTSTATUS 0x{0:08x})", "The media process could not be started after the Windows Job assignment (NTSTATUS 0x{0:08x})", resume_status as u32)
                );
            }
            Ok(Self(handle))
        }
    }
    pub(crate) fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }
}
impl Drop for ProcessJob {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
