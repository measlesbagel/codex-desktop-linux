use crate::niri;
use crate::terminal::enrich_terminal_windows;
use crate::windowing::registry::BackendProbe;
use crate::windowing::types::{WindowBounds, WindowInfo};
use anyhow::Result;

pub const NIRI_BACKEND: &str = "niri";

pub fn probe() -> BackendProbe {
    match niri::probe_windows() {
        Ok(()) => BackendProbe {
            id: NIRI_BACKEND,
            ok: true,
            can_list_windows: true,
            can_focus_apps: true,
            can_focus_windows: true,
            detail: "niri msg --json windows returned a JSON array".to_string(),
        },
        Err(error) => BackendProbe {
            id: NIRI_BACKEND,
            ok: false,
            can_list_windows: false,
            can_focus_apps: false,
            can_focus_windows: false,
            detail: format!("{error:#}"),
        },
    }
}

pub fn list_windows() -> Result<Vec<WindowInfo>> {
    Ok(map_windows(niri::windows()?))
}

pub fn activate_window(window_id: u64) -> Result<()> {
    niri::activate_window(window_id)
}

fn map_windows(windows: Vec<niri::Window>) -> Vec<WindowInfo> {
    let mut windows = windows
        .into_iter()
        .map(WindowInfo::from)
        .collect::<Vec<_>>();
    windows.sort_by_key(|window| window.window_id);
    enrich_terminal_windows(&mut windows);
    windows
}

impl From<niri::Window> for WindowInfo {
    fn from(window: niri::Window) -> Self {
        let title = clean_string(window.title);
        let app_id = clean_string(window.app_id);
        let bounds = window
            .layout
            .and_then(|layout| layout.window_size)
            .map(|[width, height]| WindowBounds {
                // Niri's stable IPC exposes window size but not a global
                // origin, so relative coordinate actions must remain disabled.
                x: None,
                y: None,
                width,
                height,
            });

        Self {
            window_id: window.id,
            title,
            app_id: app_id.clone(),
            wm_class: app_id,
            pid: window.pid.and_then(|pid| u32::try_from(pid).ok()),
            bounds,
            workspace: window
                .workspace_id
                .and_then(|workspace| i32::try_from(workspace).ok()),
            focused: window.is_focused,
            hidden: false,
            client_type: Some("wayland".to_string()),
            backend: NIRI_BACKEND.to_string(),
            terminal: None,
        }
    }
}

fn clean_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_niri_windows_to_window_info() {
        let windows: Vec<niri::Window> = serde_json::from_str(
            r#"[
              {
                "id": 42,
                "title": "Codex",
                "app_id": "codex-desktop",
                "pid": 68986,
                "workspace_id": 2,
                "is_focused": true,
                "is_floating": false,
                "layout": {
                  "window_size": [1200, 800],
                  "tile_pos_in_workspace_view": null
                },
                "future_field": "ignored"
              },
              {
                "id": 7,
                "title": "  ",
                "app_id": "terminal",
                "pid": 4294967296,
                "workspace_id": 4294967296,
                "is_focused": false
              }
            ]"#,
        )
        .unwrap();
        let windows = map_windows(windows);

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].window_id, 7);
        assert_eq!(windows[0].title, None);
        assert_eq!(windows[0].pid, None);
        assert_eq!(windows[0].workspace, None);
        assert_eq!(windows[1].window_id, 42);
        assert_eq!(windows[1].title.as_deref(), Some("Codex"));
        assert_eq!(windows[1].app_id.as_deref(), Some("codex-desktop"));
        assert_eq!(windows[1].wm_class.as_deref(), Some("codex-desktop"));
        assert_eq!(windows[1].pid, Some(68986));
        assert_eq!(windows[1].workspace, Some(2));
        assert!(windows[1].focused);
        assert!(!windows[1].hidden);
        assert_eq!(windows[1].client_type.as_deref(), Some("wayland"));
        assert_eq!(windows[1].backend, NIRI_BACKEND);
        let bounds = windows[1].bounds.as_ref().unwrap();
        assert_eq!((bounds.x, bounds.y), (None, None));
        assert_eq!((bounds.width, bounds.height), (1200, 800));
    }
}
