//! Start the real shell only after the launcher belongs to a non-breakaway job.
//! Assignment after spawning the shell would race a short-lived parent.
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{
        JobObjects::*,
        SystemServices::{JOB_OBJECT_ASSIGN_PROCESS, JOB_OBJECT_QUERY, JOB_OBJECT_SET_ATTRIBUTES},
        Threading::GetCurrentProcess,
    },
};

#[derive(Debug)]
pub struct Job {
    handle: OwnedHandle,
    stop_event: OwnedHandle,
    pub name: String,
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
impl Job {
    #[cfg(test)]
    pub fn for_test_assign(pid: u32) -> Self {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };
        let job = Self::prepare(&mut portable_pty::CommandBuilder::new("cmd.exe")).unwrap();
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        assert!(!process.is_null());
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        assert_ne!(
            unsafe {
                AssignProcessToJobObject(job.handle.as_raw_handle(), process.as_raw_handle())
            },
            0
        );
        job
    }

    pub fn prepare(cmd: &mut portable_pty::CommandBuilder) -> Result<Self, String> {
        let name = format!(
            "Local\\ZaivernWriter-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos()
        );
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), wide(&name).as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Err("PTY job identity collision".into());
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let argv: Vec<Vec<u16>> = cmd
            .get_argv()
            .iter()
            .map(|a| a.encode_wide().collect())
            .collect();
        cmd.env(
            "ZAIVERN_PTY_ARGV",
            serde_json::to_string(&argv).map_err(|e| e.to_string())?,
        );
        cmd.env("ZAIVERN_PTY_JOB", &name);
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        #[cfg(not(test))]
        let launch = format!(
            "start \"\" /B /WAIT \"{}\" --zai-internal-pty-job",
            exe.display()
        );
        #[cfg(test)]
        let launch = format!(
            "start \"\" /B /WAIT \"{}\" --exact terminal::writer_tree::windows::launcher_probe --nocapture >nul 2>nul",
            exe.display()
        );
        cmd.env("ZAIVERN_PTY_LAUNCH", launch);
        // zai's production PE is a GUI executable. A console cmd parent lets
        // the internal launcher attach to this PTY before starting the shell.
        let system =
            std::env::var_os("SystemRoot").ok_or("Windows system directory unavailable")?;
        *cmd.get_argv_mut() = vec![std::path::PathBuf::from(system)
            .join("System32")
            .join("cmd.exe")
            .into_os_string()];
        cmd.args(["/D", "/C", "%ZAIVERN_PTY_LAUNCH%"]);
        let event = unsafe {
            windows_sys::Win32::System::Threading::CreateEventW(
                std::ptr::null(),
                1,
                0,
                std::ptr::null(),
            )
        };
        if event.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(Self {
            handle,
            stop_event: unsafe { OwnedHandle::from_raw_handle(event) },
            name,
        })
    }
    pub fn quiescent(&self) -> Option<bool> {
        quiescent(self.handle.as_raw_handle())
    }
    pub fn request_stop(&self) {
        unsafe {
            windows_sys::Win32::System::Threading::SetEvent(self.stop_event.as_raw_handle());
        }
    }
    pub fn wait_parent_or_stop(&self, process: HANDLE) -> std::io::Result<()> {
        use windows_sys::Win32::System::Threading::{WaitForMultipleObjects, INFINITE};
        let handles = [process, self.stop_event.as_raw_handle()];
        let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        if result == WAIT_FAILED {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub fn stop(&self) {
        unsafe {
            TerminateJobObject(self.handle.as_raw_handle(), 1);
        }
    }
}
fn quiescent(handle: HANDLE) -> Option<bool> {
    if empty(handle) != Some(true) {
        return Some(false);
    }
    // Seal admission before accepting zero writers. Even if the console
    // parent was killed while its launcher was still starting, that late
    // launcher cannot join this completed job and start a new writer.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 0;
    if unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as _,
            std::mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return None;
    }
    empty(handle)
}

fn empty(handle: HANDLE) -> Option<bool> {
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        QueryInformationJobObject(
            handle,
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as _,
            std::mem::size_of_val(&info) as u32,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(info.ActiveProcesses == 0)
}
pub fn persisted_alive(name: &str) -> bool {
    let handle = unsafe {
        OpenJobObjectW(
            JOB_OBJECT_QUERY | JOB_OBJECT_SET_ATTRIBUTES,
            0,
            wide(name).as_ptr(),
        )
    };
    if handle.is_null() {
        return unsafe { GetLastError() } != ERROR_FILE_NOT_FOUND;
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    quiescent(handle.as_raw_handle()) != Some(true)
}

fn join(name: &str) -> Result<(), String> {
    let handle = unsafe { OpenJobObjectW(JOB_OBJECT_ASSIGN_PROCESS, 0, wide(name).as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    if unsafe { AssignProcessToJobObject(handle.as_raw_handle(), GetCurrentProcess()) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // Only Zaivern retains a job handle: its death must kill the entire job.
    drop(handle);
    Ok(())
}

unsafe extern "system" fn launcher_control(event: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    i32::from(matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT))
}

fn launch() -> Result<i32, String> {
    use windows_sys::Win32::System::Console::{
        AttachConsole, SetConsoleCtrlHandler, ATTACH_PARENT_PROCESS,
    };
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0
        && unsafe { GetLastError() } != ERROR_ACCESS_DENIED
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // START /B can inherit an ignored Ctrl-C flag. Clear it for the real CLI,
    // then protect only this launcher with a non-inherited handler.
    if unsafe { SetConsoleCtrlHandler(None, 0) } == 0
        || unsafe { SetConsoleCtrlHandler(Some(launcher_control), 1) } == 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let name = std::env::var("ZAIVERN_PTY_JOB").map_err(|e| e.to_string())?;
    let argv: Vec<Vec<u16>> =
        serde_json::from_str(&std::env::var("ZAIVERN_PTY_ARGV").map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let argv: Vec<_> = argv
        .iter()
        .map(|a| std::ffi::OsString::from_wide(a))
        .collect();
    let program = argv.first().ok_or("PTY command missing")?;
    join(&name)?;
    // Open this attached console explicitly. The production launcher is a GUI
    // executable; test launchers also suppress libtest's own banner on stdout.
    let input = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONIN$")
        .map_err(|e| e.to_string())?;
    let output = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONOUT$")
        .map_err(|e| e.to_string())?;
    let error = output.try_clone().map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new(program)
        .args(&argv[1..])
        .stdin(input)
        .stdout(output)
        .stderr(error)
        .env_remove("ZAIVERN_PTY_JOB")
        .env_remove("ZAIVERN_PTY_ARGV")
        .env_remove("ZAIVERN_PTY_LAUNCH")
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(child.wait().map_err(|e| e.to_string())?.code().unwrap_or(1))
}
pub fn entry() -> ! {
    let code = match launch() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("PTY writer containment failed: {e}");
            1
        }
    };
    std::process::exit(code)
}
#[test]
fn launcher_probe() {
    if std::env::var_os("ZAIVERN_PTY_JOB").is_some() {
        entry();
    }
}

#[test]
fn completed_job_refuses_a_late_launcher() {
    let mut cmd = portable_pty::CommandBuilder::new("cmd.exe");
    cmd.args(["/D", "/C", "exit 0"]);
    let job = Job::prepare(&mut cmd).unwrap();
    assert_eq!(job.quiescent(), Some(true));
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "terminal::writer_tree::windows::admission_probe",
            "--nocapture",
        ])
        .env("ZAIVERN_PTY_JOB", &job.name)
        .env("ZAIVERN_PTY_ARGV", cmd.get_env("ZAIVERN_PTY_ARGV").unwrap())
        .status()
        .unwrap();
    assert!(!status.success(), "a sealed job admitted another writer");
    assert_eq!(job.quiescent(), Some(true));
}

#[test]
fn admission_probe() {
    if let Ok(name) = std::env::var("ZAIVERN_PTY_JOB") {
        std::process::exit(if join(&name).is_ok() { 0 } else { 1 });
    }
}
