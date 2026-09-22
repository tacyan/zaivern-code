//! Classify create responses before generic user-facing error conversion.
use super::ResourceKind;
use crate::features::cloud_execution::{model::CollectSink, transport::run_child};
use std::{process::Command, time::Duration};

pub(crate) fn run(
    command: Command,
    timeout: Duration,
    kind: ResourceKind,
    name: &str,
) -> CreateOutcome {
    let mut sink = CollectSink::with_limit(64 * 1024);
    let result = run_child(command, timeout, "docker", &mut sink);
    create_outcome(
        result.as_ref().ok().and_then(|r| r.exit_code),
        result.is_ok(),
        &sink,
        kind,
        name,
    )
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CreateOutcome {
    Created,
    DefinitelyNotCreated,
    Unknown,
}

fn create_outcome(
    code: Option<i32>,
    completed: bool,
    output: &CollectSink,
    kind: ResourceKind,
    name: &str,
) -> CreateOutcome {
    if !completed || output.truncated {
        return CreateOutcome::Unknown;
    }
    match code {
        Some(0) => CreateOutcome::Created,
        // Even a complete daemon error can follow partial filesystem creation.
        // Only recognize rejections known to precede resource creation.
        Some(code)
            if code > 0
                && output.stdout.is_empty()
                && pre_create_rejection(kind, name, &output.stderr) =>
        {
            CreateOutcome::DefinitelyNotCreated
        }
        _ => CreateOutcome::Unknown,
    }
}

fn pre_create_rejection(kind: ResourceKind, name: &str, stderr: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(stderr) else {
        return false;
    };
    let Some(message) = text.trim_end().strip_prefix("Error response from daemon: ") else {
        return false;
    };
    // Moby v28.1.1: daemon/create.go resolves the image before newContainer;
    // volume/local/local_unix.go validates options before Root.Create's mkdir.
    // Do not broaden to filesystem/backend/context errors: an unlisted partial
    // volume directory can become visible after daemon restart (local.go New).
    match kind {
        ResourceKind::Container => message
            .strip_prefix("No such image: ")
            .is_some_and(|image| !image.is_empty() && !image.chars().any(char::is_whitespace)),
        ResourceKind::Volume => {
            let Some(reason) = message.strip_prefix(&format!("create {name}: ")) else {
                return false;
            };
            let option = reason
                .strip_prefix("invalid option: ")
                .or_else(|| reason.strip_prefix("missing required option: "));
            option
                .and_then(|value| serde_json::from_str::<String>(value).ok())
                .is_some_and(|value| {
                    !value.is_empty()
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                })
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn only_completed_daemon_rejection_is_definite() {
        for (script, expected) in [
            ("exit 0", CreateOutcome::Created),
            (
                "echo 'Error response from daemon: No such image: sha256:0000' >&2; exit 1",
                CreateOutcome::DefinitelyNotCreated,
            ),
            (
                "echo 'error during connect: EOF' >&2; exit 1",
                CreateOutcome::Unknown,
            ),
            ("exit 1", CreateOutcome::Unknown),
            (
                "echo 'Error response from daemon: truncated' >&2; kill -KILL $$",
                CreateOutcome::Unknown,
            ),
            (
                "echo 'Error response from daemon: delayed' >&2; exec sleep 30",
                CreateOutcome::Unknown,
            ),
        ] {
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", script]).env_clear().process_group(0);
            let timeout = if script.ends_with("sleep 30") {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(5)
            };
            assert_eq!(
                run(cmd, timeout, ResourceKind::Container, "fixture"),
                expected,
                "{script}"
            );
        }
    }

    #[test]
    fn incomplete_or_truncated_output_never_proves_rejection() {
        let mut output = CollectSink::default();
        output.stderr = b"Error response from daemon: No such image: sha256:0000\n".to_vec();
        assert_eq!(
            create_outcome(Some(1), false, &output, ResourceKind::Container, "fixture"),
            CreateOutcome::Unknown
        );
        assert_eq!(
            create_outcome(None, true, &output, ResourceKind::Container, "fixture"),
            CreateOutcome::Unknown
        );
        output.truncated = true;
        assert_eq!(
            create_outcome(Some(1), true, &output, ResourceKind::Container, "fixture"),
            CreateOutcome::Unknown
        );
        output.truncated = false;
        output.stdout = b"possibly-created-id".to_vec();
        assert_eq!(
            create_outcome(Some(1), true, &output, ResourceKind::Container, "fixture"),
            CreateOutcome::Unknown
        );
    }

    #[test]
    fn daemon_system_errors_are_unknown_even_with_complete_nonzero_exit() {
        for message in [
            "create fixture: error while creating volume data path '/var/lib/docker/volumes/fixture/_data': no space left on device",
            "create fixture: error while creating volume root path '/var/lib/docker/volumes/fixture': permission denied",
            "context deadline exceeded",
            "rpc error: code = Unavailable desc = transport is closing",
            "create fixture: error while persisting volume options: input/output error",
            "create fixture: quota size requested but no quota support",
            "create other: invalid option: \"bad-option\"",
        ] {
            let mut output = CollectSink::default();
            output.stderr = format!("Error response from daemon: {message}\n").into_bytes();
            for kind in [ResourceKind::Volume, ResourceKind::Container] {
                assert_eq!(create_outcome(Some(1), true, &output, kind, "fixture"), CreateOutcome::Unknown, "{message}");
            }
        }
        for reason in [
            "invalid option: \"bad-option\"",
            "missing required option: \"device\"",
        ] {
            let mut output = CollectSink::default();
            output.stderr =
                format!("Error response from daemon: create fixture: {reason}\n").into_bytes();
            assert_eq!(
                create_outcome(Some(1), true, &output, ResourceKind::Volume, "fixture"),
                CreateOutcome::DefinitelyNotCreated
            );
        }
    }
}
