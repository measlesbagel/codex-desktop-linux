mod capture;
mod geometry;
mod input;
mod ipc;
mod wayland;

#[cfg(test)]
mod tests;

use crate::windowing::{WindowInfo, NIRI_BACKEND};
use anyhow::{Context, Result};

pub(crate) use ipc::{
    activate_window, is_session, output_for_window, probe_windows, windows, Window,
};
pub(crate) use wayland::{probe_capabilities, ProtocolCapability};

pub(crate) type ActionResult = std::result::Result<String, String>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PointerScope {
    output_name: Option<String>,
}

impl PointerScope {
    pub(crate) fn requested(
        output_name: Option<&str>,
        has_explicit_coordinates: bool,
    ) -> std::result::Result<Self, String> {
        let output_name = output_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if output_name.is_some() && !is_session() {
            return Err(
                "output_name is only valid for output-scoped coordinates in a Niri session."
                    .to_string(),
            );
        }
        if output_name.is_some() && !has_explicit_coordinates {
            return Err(
                "output_name requires explicit x and y from an output-scoped Niri screenshot. Omit output_name when targeting an accessibility element."
                    .to_string(),
            );
        }
        Ok(Self { output_name })
    }

    pub(crate) fn validate_window(
        &self,
        window: Option<&WindowInfo>,
    ) -> std::result::Result<(), String> {
        let Some(requested_output) = self.output_name.as_deref() else {
            return Ok(());
        };
        let Some(window) = window.filter(|window| window.backend == NIRI_BACKEND) else {
            return Ok(());
        };
        let resolved = output_for_window(window.window_id)
            .map_err(|error| {
                format!(
                    "Did not send input because the Niri output for window {} could not be resolved: {error:#}",
                    window.window_id
                )
            })?
            .output_name;
        if requested_output != resolved {
            return Err(format!(
                "Did not send input because output_name {requested_output:?} does not match the focused Niri window output {resolved:?}."
            ));
        }
        Ok(())
    }

    pub(crate) fn is_output_scoped(&self) -> bool {
        self.output_name.is_some()
    }
}

pub(crate) fn read_only_capture_output(
    window: Option<&WindowInfo>,
) -> std::result::Result<Option<String>, String> {
    let Some((window_id, output)) = window_capture_output(window)? else {
        return Ok(None);
    };
    if !output.workspace_active {
        return Err(format!(
            "Niri window {} is on an inactive workspace. get_app_state is read-only and will not switch workspaces; use screenshot with this window target to focus it before capture.",
            window_id
        ));
    }
    Ok(Some(output.output_name))
}

pub(crate) fn focused_capture_output(
    window: &WindowInfo,
) -> std::result::Result<Option<String>, String> {
    let Some((window_id, output)) = window_capture_output(Some(window))? else {
        return Ok(None);
    };
    if !output.workspace_active {
        return Err(format!(
            "Niri window {} is still on an inactive workspace after the requested focus step",
            window_id
        ));
    }
    Ok(Some(output.output_name))
}

fn window_capture_output(
    window: Option<&WindowInfo>,
) -> std::result::Result<Option<(u64, ipc::WindowOutput)>, String> {
    let Some(window) = window.filter(|window| window.backend == NIRI_BACKEND) else {
        return Ok(None);
    };
    let output = output_for_window(window.window_id).map_err(|error| {
        format!(
            "could not resolve the Niri output for window {}: {error:#}",
            window.window_id
        )
    })?;
    Ok(Some((window.window_id, output)))
}

pub(crate) async fn capture(output_name: Option<String>) -> Result<capture::CapturedImage> {
    run_blocking("screencopy", move || {
        capture::capture(output_name.as_deref())
    })
    .await
}

pub(crate) async fn try_click(
    scope: &PointerScope,
    x: i32,
    y: i32,
    button: u32,
    count: u32,
) -> Option<ActionResult> {
    let output_name = scope.output_name.clone();
    try_input("virtual pointer", move || {
        input::click(output_name.as_deref(), x, y, button, count)
    })
    .await
}

pub(crate) async fn try_scroll(
    scope: &PointerScope,
    target: Option<(i32, i32)>,
    horizontal: bool,
    steps: i32,
) -> Option<ActionResult> {
    let output_name = scope.output_name.clone();
    try_input("virtual pointer", move || {
        input::scroll(output_name.as_deref(), target, horizontal, steps)
    })
    .await
}

pub(crate) async fn try_drag(
    scope: &PointerScope,
    start: (i32, i32),
    end: (i32, i32),
    button: u32,
) -> Option<ActionResult> {
    let output_name = scope.output_name.clone();
    try_input("virtual pointer", move || {
        input::drag(output_name.as_deref(), start, end, button)
    })
    .await
}

pub(crate) async fn try_press_key(events: Vec<(u16, bool)>) -> Option<ActionResult> {
    try_input("virtual keyboard", move || input::press_key(&events)).await
}

pub(crate) async fn try_type_text(text: String) -> Option<ActionResult> {
    try_input("virtual keyboard", move || input::type_text(&text)).await
}

async fn try_input<F>(device: &'static str, action: F) -> Option<ActionResult>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    if !is_session() {
        return None;
    }
    Some(input_action_result(
        device,
        run_blocking(device, action).await,
    ))
}

fn input_action_result(device: &str, result: Result<()>) -> ActionResult {
    match result {
        Ok(()) => Ok(format!("Action sent through the Niri {device}.")),
        Err(error) => Err(format!("Niri {device} failed: {error:#}")),
    }
}

async fn run_blocking<T, F>(operation: &str, action: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(action)
        .await
        .with_context(|| format!("Niri {operation} worker stopped unexpectedly"))?
}
