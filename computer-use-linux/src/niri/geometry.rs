use super::ipc::Output;
use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, Copy)]
pub(super) struct DesktopBounds {
    pub(super) min_x: i32,
    pub(super) min_y: i32,
    pub(super) width: u32,
    pub(super) height: u32,
}

pub(super) struct OutputLayout<'a> {
    outputs: &'a [Output],
    bounds: DesktopBounds,
}

impl<'a> OutputLayout<'a> {
    pub(super) fn new(outputs: &'a [Output]) -> Result<Self> {
        let first = outputs
            .first()
            .context("Niri reported no logical outputs")?;
        let mut min_x = i64::from(first.x);
        let mut min_y = i64::from(first.y);
        let mut max_x = min_x + i64::from(first.width);
        let mut max_y = min_y + i64::from(first.height);
        for output in &outputs[1..] {
            let x = i64::from(output.x);
            let y = i64::from(output.y);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x + i64::from(output.width));
            max_y = max_y.max(y + i64::from(output.height));
        }
        let width = u32::try_from(max_x - min_x).context("Niri desktop width is invalid")?;
        let height = u32::try_from(max_y - min_y).context("Niri desktop height is invalid")?;
        if width == 0 || height == 0 {
            bail!("Niri desktop has invalid dimensions {width}x{height}");
        }

        Ok(Self {
            outputs,
            bounds: DesktopBounds {
                min_x: i32::try_from(min_x).context("Niri desktop X origin is out of range")?,
                min_y: i32::try_from(min_y).context("Niri desktop Y origin is out of range")?,
                width,
                height,
            },
        })
    }

    pub(super) fn outputs(&self) -> &'a [Output] {
        self.outputs
    }

    pub(super) fn bounds(&self) -> DesktopBounds {
        self.bounds
    }

    pub(super) fn find_output(&self, name: &str) -> Result<&'a Output> {
        self.outputs
            .iter()
            .find(|output| output.name == name)
            .with_context(|| format!("Niri output {name:?} is not available"))
    }
}

pub(super) fn normalized_output_name(output_name: Option<&str>) -> Option<&str> {
    output_name.map(str::trim).filter(|name| !name.is_empty())
}
