//! Windows の CLI は console subsystem、GUI はコンソールなしで起動する。
//! AttachConsole だけでは PowerShell が待機せず、確認入力を取り合ってしまう。

const GUI_MARKER: &str = "--zai-internal-gui";

pub(crate) fn take_gui_marker(mut args: Vec<String>) -> (bool, Vec<String>) {
    let child = args.first().is_some_and(|arg| arg == GUI_MARKER);
    if child {
        args.remove(0);
    }
    (child, args)
}

pub(crate) fn spawn_gui(args: &[String]) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::System::Threading::DETACHED_PROCESS;

    Command::new(std::env::current_exe()?)
        .arg(GUI_MARKER)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS)
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_marker_is_only_consumed_at_the_start() {
        let workspace = "C:\\work with spaces\\日本語".to_string();
        assert_eq!(
            take_gui_marker(vec![GUI_MARKER.into(), workspace.clone()]),
            (true, vec![workspace.clone()])
        );
        let args = vec![workspace, GUI_MARKER.into()];
        assert_eq!(take_gui_marker(args.clone()), (false, args));
        assert_eq!(take_gui_marker(vec![]), (false, vec![]));
        let update = vec!["update".into(), "--check".into()];
        assert_eq!(take_gui_marker(update.clone()), (false, update));
    }
}
