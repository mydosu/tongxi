//! Only processes created by this app belong to this kill-on-close job.
#[cfg(windows)]
pub struct ProcessScope(usize);

#[cfg(windows)]
impl ProcessScope {
    pub fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0 as *mut _, 1);
        }
    }
    pub fn attach(child: &std::process::Child) -> Result<Self, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err("无法创建原生进程生命周期".into());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(handle, child.as_raw_handle()) == 0
            {
                CloseHandle(handle);
                return Err("无法绑定原生进程生命周期".into());
            }
            Ok(Self(handle as usize))
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessScope {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0 as *mut _);
        }
    }
}

#[cfg(not(windows))]
pub struct ProcessScope;
#[cfg(not(windows))]
impl ProcessScope {
    pub fn terminate(&self) {}
    pub fn attach(_: &std::process::Child) -> Result<Self, String> {
        Ok(Self)
    }
}
