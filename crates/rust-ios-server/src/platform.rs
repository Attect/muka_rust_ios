use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;

#[cfg(windows)]
pub fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 两个 UTF-16 buffer 都以 NUL 结尾，并在调用期间保持有效。
    let success = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(windows)]
#[derive(Debug)]
pub struct DetachedChild {
    process: windows_sys::Win32::Foundation::HANDLE,
    pub pid: u32,
}

#[cfg(windows)]
impl DetachedChild {
    pub fn try_wait(&mut self) -> io::Result<Option<u32>> {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        // SAFETY: process 是 CreateProcessW 返回且由本对象独占的有效 handle。
        let wait = unsafe { WaitForSingleObject(self.process, 0) };
        if wait != WAIT_OBJECT_0 {
            return Ok(None);
        }
        let mut code = 0_u32;
        // SAFETY: code 指向有效可写内存，process 在 Drop 前保持有效。
        if unsafe { GetExitCodeProcess(self.process, &mut code) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Some(code))
        }
    }
}

#[cfg(windows)]
impl Drop for DetachedChild {
    fn drop(&mut self) {
        // SAFETY: handle 仅在此处关闭一次；关闭 handle 不会终止 child。
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.process) };
    }
}

#[cfg(windows)]
pub fn spawn_detached(executable: &Path, arguments: &[OsString]) -> io::Result<DetachedChild> {
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
    };

    let application: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut command_line = quote_windows_argument(executable.as_os_str());
    for argument in arguments {
        command_line.push(' ' as u16);
        command_line.extend(quote_windows_argument(argument));
    }
    command_line.push(0);

    // SAFETY: Windows structs may be zero-initialized; cb is set as required by CreateProcessW.
    let mut startup: STARTUPINFOW = unsafe { zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    // SAFETY: PROCESS_INFORMATION is an output-only POD struct.
    let mut process: PROCESS_INFORMATION = unsafe { zeroed() };
    // bInheritHandles=FALSE is the important invariant: the daemon cannot keep the client host's
    // stdout/stderr/PTY pipes alive. The daemon opens its own log file after startup.
    let success = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            DETACHED_PROCESS | CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    // Thread handle is never needed.
    unsafe { CloseHandle(process.hThread) };
    Ok(DetachedChild {
        process: process.hProcess,
        pid: process.dwProcessId,
    })
}

#[cfg(windows)]
fn quote_windows_argument(value: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let units: Vec<u16> = value.encode_wide().collect();
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|unit| matches!(*unit, 0x09 | 0x0a | 0x0d | 0x20 | 0x22));
    if !needs_quotes {
        return units;
    }

    let mut output = Vec::with_capacity(units.len() + 2);
    output.push('"' as u16);
    let mut backslashes = 0_usize;
    for unit in units {
        if unit == '\\' as u16 {
            backslashes += 1;
            continue;
        }
        if unit == '"' as u16 {
            output.extend(std::iter::repeat_n('\\' as u16, backslashes * 2 + 1));
            output.push(unit);
        } else {
            output.extend(std::iter::repeat_n('\\' as u16, backslashes));
            output.push(unit);
        }
        backslashes = 0;
    }
    output.extend(std::iter::repeat_n('\\' as u16, backslashes * 2));
    output.push('"' as u16);
    output
}

#[cfg(not(windows))]
#[derive(Debug)]
pub struct DetachedChild {
    child: std::process::Child,
    pub pid: u32,
}

#[cfg(not(windows))]
impl DetachedChild {
    pub fn try_wait(&mut self) -> io::Result<Option<u32>> {
        Ok(self
            .child
            .try_wait()?
            .and_then(|status| status.code())
            .map(|code| code as u32))
    }
}

#[cfg(not(windows))]
pub fn spawn_detached(executable: &Path, arguments: &[OsString]) -> io::Result<DetachedChild> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn()?;
    let pid = child.id();
    Ok(DetachedChild { child, pid })
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn windows_quoting_preserves_spaces_quotes_and_trailing_slashes() {
        use super::quote_windows_argument;
        use std::ffi::OsStr;
        let quoted =
            String::from_utf16(&quote_windows_argument(OsStr::new("C:\\A B\\\"x\\"))).unwrap();
        assert!(quoted.starts_with('"'));
        assert!(quoted.ends_with('"'));
        assert!(quoted.contains("A B"));
        assert!(quoted.contains("\\\""));
    }
}
