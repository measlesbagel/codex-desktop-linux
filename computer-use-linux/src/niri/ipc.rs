use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    os::unix::{ffi::OsStringExt, net::UnixStream},
    path::Path,
    process::{Command, Output as CommandOutput},
};

const NIRI_SOCKET_ENV: &str = "NIRI_SOCKET";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(super) enum OutputTransform {
    Normal,
    #[serde(rename = "90")]
    Rotate90,
    #[serde(rename = "180")]
    Rotate180,
    #[serde(rename = "270")]
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Output {
    pub(crate) name: String,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) scale: f64,
    pub(crate) transform: OutputTransform,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowOutput {
    pub(crate) output_name: String,
    pub(crate) workspace_active: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Window {
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) app_id: Option<String>,
    #[serde(default)]
    pub(crate) pid: Option<u64>,
    #[serde(default)]
    pub(crate) workspace_id: Option<u64>,
    #[serde(default)]
    pub(crate) is_focused: bool,
    #[serde(default)]
    pub(crate) layout: Option<WindowLayout>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WindowLayout {
    #[serde(default)]
    pub(crate) window_size: Option<[u32; 2]>,
}

pub(crate) fn is_session() -> bool {
    let desktop_is_niri = std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .is_some_and(|value| {
            value
                .split([':', ';'])
                .any(|desktop| desktop.trim().eq_ignore_ascii_case("niri"))
        });
    desktop_is_niri
        || (std::env::var_os(NIRI_SOCKET_ENV).is_some_and(|value| !value.is_empty())
            && probe_windows().is_ok())
}

pub(crate) fn probe_windows() -> Result<()> {
    let _: Vec<serde_json::Value> = niri_json(&["windows"], "windows")?;
    Ok(())
}

pub(crate) fn windows() -> Result<Vec<Window>> {
    niri_json(&["windows"], "windows")
}

pub(super) fn outputs() -> Result<Vec<Output>> {
    let outputs: HashMap<String, OutputWire> = niri_json(&["outputs"], "outputs")?;
    parse_outputs(outputs)
}

pub(crate) fn output_for_window(window_id: u64) -> Result<WindowOutput> {
    let windows = windows()?;
    let workspaces: Vec<Workspace> = niri_json(&["workspaces"], "workspaces")?;
    resolve_window_output(windows, workspaces, window_id)
}

pub(crate) fn activate_window(window_id: u64) -> Result<()> {
    let window_id = window_id.to_string();
    let args = ["action", "focus-window", "--id", &window_id];
    let output = niri_output(&args)
        .with_context(|| format!("failed to run niri msg action focus-window --id {window_id}"))?;
    ensure_success(&output, &format!("action focus-window --id {window_id}"))
}

fn niri_json<T: DeserializeOwned>(args: &[&str], operation: &str) -> Result<T> {
    let output =
        niri_output(args).with_context(|| format!("failed to run niri msg --json {operation}"))?;
    ensure_success(&output, operation)?;
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("failed to parse niri msg --json {operation} output"))
}

fn ensure_success(output: &CommandOutput, operation: &str) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "niri msg --json {operation} failed: {}",
            output_failure_detail(output)
        )
    }
}

fn resolve_window_output(
    windows: Vec<Window>,
    workspaces: Vec<Workspace>,
    window_id: u64,
) -> Result<WindowOutput> {
    let workspace_id = windows
        .into_iter()
        .find(|window| window.id == window_id)
        .and_then(|window| window.workspace_id)
        .with_context(|| format!("niri window {window_id} is not assigned to a workspace"))?;
    let workspace = workspaces
        .into_iter()
        .find(|workspace| workspace.id == workspace_id)
        .with_context(|| format!("niri workspace {workspace_id} is no longer available"))?;
    let output_name = workspace
        .output
        .filter(|name| !name.trim().is_empty())
        .with_context(|| format!("niri workspace {workspace_id} is not assigned to an output"))?;

    Ok(WindowOutput {
        output_name,
        workspace_active: workspace.is_active,
    })
}

fn parse_outputs(outputs: HashMap<String, OutputWire>) -> Result<Vec<Output>> {
    let mut outputs = outputs
        .into_values()
        .filter_map(|output| {
            let logical = output.logical?;
            (logical.width > 0
                && logical.height > 0
                && logical.scale.is_finite()
                && logical.scale > 0.0)
                .then_some(Output {
                    name: output.name,
                    x: logical.x,
                    y: logical.y,
                    width: logical.width,
                    height: logical.height,
                    scale: logical.scale,
                    transform: logical.transform,
                })
        })
        .collect::<Vec<_>>();
    outputs.sort_by(|left, right| left.name.cmp(&right.name));
    if outputs.is_empty() {
        bail!("niri did not report any enabled logical outputs");
    }
    Ok(outputs)
}

fn niri_output(args: &[&str]) -> std::io::Result<CommandOutput> {
    let output = niri_command(args, None).output()?;
    if output.status.success() {
        return Ok(output);
    }

    let current = std::env::var_os(NIRI_SOCKET_ENV).filter(|value| !value.is_empty());
    let session = systemd_niri_socket();
    let Some(socket) = retry_socket_candidate(
        current.as_deref(),
        session.as_deref(),
        socket_is_connectable,
    ) else {
        return Ok(output);
    };

    let Ok(retry) = niri_command(args, Some(&socket)).output() else {
        return Ok(output);
    };
    if retry.status.success() {
        std::env::set_var(NIRI_SOCKET_ENV, &socket);
    }
    Ok(retry)
}

fn systemd_niri_socket() -> Option<OsString> {
    let output = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let prefix = format!("{NIRI_SOCKET_ENV}=").into_bytes();
    output
        .stdout
        .split(|byte| *byte == b'\n')
        .find_map(|entry| entry.strip_prefix(prefix.as_slice()))
        .filter(|value| !value.is_empty())
        .map(|value| OsString::from_vec(value.to_vec()))
}

fn niri_command(args: &[&str], socket: Option<&OsStr>) -> Command {
    let mut command = Command::new("niri");
    command.args(["msg", "--json"]).args(args);
    if let Some(socket) = socket {
        command.env(NIRI_SOCKET_ENV, socket);
    }
    command
}

fn retry_socket_candidate<F>(
    current: Option<&OsStr>,
    session: Option<&OsStr>,
    mut is_connectable: F,
) -> Option<OsString>
where
    F: FnMut(&OsStr) -> bool,
{
    if current.is_some_and(&mut is_connectable) {
        return None;
    }

    let session = session.filter(|value| !value.is_empty())?;
    if current == Some(session) || !is_connectable(session) {
        return None;
    }
    Some(session.to_os_string())
}

fn socket_is_connectable(path: &OsStr) -> bool {
    UnixStream::connect(Path::new(path)).is_ok()
}

fn output_failure_detail(output: &CommandOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    output.status.to_string()
}

#[derive(Debug, Deserialize)]
struct OutputWire {
    name: String,
    #[serde(default)]
    logical: Option<LogicalOutput>,
}

#[derive(Debug, Deserialize)]
struct LogicalOutput {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f64,
    transform: OutputTransform,
}

#[derive(Debug, Deserialize)]
struct Workspace {
    id: u64,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    is_active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_with_session_socket_when_inherited_socket_is_unreachable() {
        let inherited = OsStr::new("inherited-socket");
        let session = OsStr::new("session-socket");

        let selected = retry_socket_candidate(Some(inherited), Some(session), |candidate| {
            candidate == session
        });

        assert_eq!(selected.as_deref(), Some(session));
    }

    #[test]
    fn preserves_a_reachable_inherited_socket() {
        let inherited = OsStr::new("inherited-socket");
        let session = OsStr::new("session-socket");

        let selected = retry_socket_candidate(Some(inherited), Some(session), |candidate| {
            candidate == inherited || candidate == session
        });

        assert_eq!(selected, None);
    }

    #[test]
    fn does_not_retry_an_unreachable_duplicate_socket() {
        let inherited = OsStr::new("same-socket");

        let selected = retry_socket_candidate(Some(inherited), Some(inherited), |_| false);

        assert_eq!(selected, None);
    }

    #[test]
    fn parses_enabled_outputs_in_stable_name_order() {
        let outputs: HashMap<String, OutputWire> = serde_json::from_str(
            r#"{
                "DP-2": {
                    "name": "DP-2",
                    "logical": {
                        "x": 0,
                        "y": -1080,
                        "width": 1920,
                        "height": 1080,
                        "scale": 1.0,
                        "transform": "90"
                    }
                },
                "DP-1": {
                    "name": "DP-1",
                    "logical": {
                        "x": -2560,
                        "y": 0,
                        "width": 2560,
                        "height": 1440,
                        "scale": 1.25,
                        "transform": "Normal"
                    }
                },
                "HDMI-A-1": {
                    "name": "HDMI-A-1",
                    "logical": null
                }
            }"#,
        )
        .unwrap();
        let outputs = parse_outputs(outputs).unwrap();

        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].name, "DP-1");
        assert_eq!((outputs[0].x, outputs[0].y), (-2560, 0));
        assert_eq!(outputs[0].scale, 1.25);
        assert_eq!(outputs[1].name, "DP-2");
        assert_eq!(outputs[1].transform, OutputTransform::Rotate90);
    }

    #[test]
    fn rejects_output_sets_without_enabled_logical_outputs() {
        let outputs: HashMap<String, OutputWire> = serde_json::from_str(
            r#"{
                "HDMI-A-1": {
                    "name": "HDMI-A-1",
                    "logical": null
                }
            }"#,
        )
        .unwrap();
        let error = parse_outputs(outputs).unwrap_err();

        assert!(error.to_string().contains("any enabled logical outputs"));
    }

    #[test]
    fn maps_windows_across_active_and_inactive_workspaces_to_outputs() {
        let windows: Vec<Window> = serde_json::from_slice(
            br#"[
                {"id": 51, "workspace_id": 1},
                {"id": 62, "workspace_id": 2},
                {"id": 63, "workspace_id": 3}
            ]"#,
        )
        .unwrap();
        let workspaces: Vec<Workspace> = serde_json::from_slice(
            br#"[
                {"id": 1, "output": "DP-1", "is_active": true},
                {"id": 2, "output": "DP-1", "is_active": false},
                {"id": 3, "output": "HDMI-A-1", "is_active": true}
            ]"#,
        )
        .unwrap();

        assert_eq!(
            resolve_window_output(windows, workspaces, 62).unwrap(),
            WindowOutput {
                output_name: "DP-1".to_string(),
                workspace_active: false,
            }
        );
    }
}
