use super::{
    geometry::{normalized_output_name, OutputLayout},
    ipc::{self, Output as NiriOutput, OutputTransform as NiriOutputTransform},
    wayland::{create_named_memfd, BufferOffer, CaptureEvents, WaylandClient},
};
use anyhow::{bail, Context, Result};
use image::{imageops, Rgba, RgbaImage};
use std::{
    fs::File,
    os::{fd::AsFd, unix::fs::FileExt},
};
use wayland_client::protocol::wl_shm;

const MAX_CAPTURE_BYTES: usize = 512 * 1024 * 1024;
pub(super) const MAX_DESKTOP_PIXELS: u64 = 128 * 1024 * 1024;
const MAX_OUTPUTS: usize = 32;

#[derive(Debug)]
pub(crate) struct CapturedImage {
    pub image: RgbaImage,
    pub output_name: Option<String>,
}

pub(super) fn capture(output_name: Option<&str>) -> Result<CapturedImage> {
    let outputs = ipc::outputs()?;
    if outputs.len() > MAX_OUTPUTS {
        bail!(
            "niri reported {} outputs, over the capture limit of {MAX_OUTPUTS}",
            outputs.len()
        );
    }

    let layout = OutputLayout::new(&outputs)?;
    if let Some(output_name) = normalized_output_name(output_name) {
        let output = layout.find_output(output_name)?;
        validate_output_capture(output)?;
        return Ok(CapturedImage {
            image: capture_logical_output(output)?,
            output_name: Some(output.name.clone()),
        });
    }

    validate_desktop_capture(&layout)?;
    let images = layout
        .outputs()
        .iter()
        .map(capture_logical_output)
        .collect::<Result<Vec<_>>>()?;
    let desktop = compose_desktop(&layout, images)?;

    Ok(CapturedImage {
        image: desktop,
        output_name: None,
    })
}

fn capture_logical_output(output: &NiriOutput) -> Result<RgbaImage> {
    let mut client = WaylandClient::connect()?;
    let manager = client
        .state
        .screencopy_manager
        .clone()
        .context("Niri did not advertise zwlr_screencopy_manager_v1")?;
    if client.state.screencopy_version.unwrap_or_default() < 3 {
        bail!("Niri screencopy manager version 3 is required");
    }
    let shm = client
        .state
        .shm
        .clone()
        .context("Niri did not advertise wl_shm")?;
    let wl_output = client.output(&output.name)?;

    client.state.capture = Some(CaptureEvents::default());
    let frame = manager.capture_output(0, &wl_output, &client.qh, ());
    client.dispatch_until(|state| {
        state
            .capture
            .as_ref()
            .is_some_and(|capture| capture.buffer_done || capture.failed)
    })?;
    let capture = client
        .state
        .capture
        .as_ref()
        .context("Niri screencopy state disappeared")?;
    if capture.failed {
        bail!("Niri rejected the screencopy request for {}", output.name);
    }
    let offer = capture
        .offer
        .context("Niri screencopy did not offer a supported wl_shm buffer")?;
    let byte_len = checked_capture_len(offer.stride, offer.height)?;
    let file = create_memfd(byte_len)?;
    let pool = shm.create_pool(
        file.as_fd(),
        i32::try_from(byte_len).context("Niri screencopy buffer is too large for wl_shm")?,
        &client.qh,
        (),
    );
    let buffer = pool.create_buffer(
        0,
        i32::try_from(offer.width).context("Niri screencopy width is too large")?,
        i32::try_from(offer.height).context("Niri screencopy height is too large")?,
        i32::try_from(offer.stride).context("Niri screencopy stride is too large")?,
        offer.format,
        &client.qh,
        (),
    );
    frame.copy(&buffer);
    client.dispatch_until(|state| {
        state
            .capture
            .as_ref()
            .is_some_and(|capture| capture.ready || capture.failed)
    })?;
    let capture = client
        .state
        .capture
        .as_ref()
        .context("Niri screencopy state disappeared")?;
    if capture.failed {
        bail!(
            "Niri failed to copy output {} into shared memory",
            output.name
        );
    }

    let mut raw = vec![0_u8; byte_len];
    file.read_exact_at(&mut raw, 0)
        .with_context(|| format!("failed to read Niri screencopy buffer for {}", output.name))?;
    let mut image = orient_capture(
        rgba_from_shm(&raw, offer)?,
        capture.y_invert,
        output.transform,
    );
    if image.width() != output.width || image.height() != output.height {
        image = imageops::resize(
            &image,
            output.width,
            output.height,
            imageops::FilterType::Lanczos3,
        );
    }
    Ok(image)
}

pub(super) fn compose_desktop(
    layout: &OutputLayout<'_>,
    images: Vec<RgbaImage>,
) -> Result<RgbaImage> {
    let outputs = layout.outputs();
    if outputs.len() != images.len() {
        bail!(
            "received {} Niri output images for {} outputs",
            images.len(),
            outputs.len()
        );
    }
    let bounds = layout.bounds();
    let pixels = u64::from(bounds.width)
        .checked_mul(u64::from(bounds.height))
        .context("niri desktop dimensions overflowed")?;
    if pixels > MAX_DESKTOP_PIXELS {
        bail!(
            "niri logical desktop is {}x{} ({} pixels), over the capture limit of {} pixels",
            bounds.width,
            bounds.height,
            pixels,
            MAX_DESKTOP_PIXELS
        );
    }

    let mut desktop = RgbaImage::from_pixel(bounds.width, bounds.height, Rgba([0, 0, 0, 255]));
    for (output, image) in outputs.iter().zip(images) {
        if image.width() != output.width || image.height() != output.height {
            bail!(
                "Niri output {} image is {}x{}, expected {}x{}",
                output.name,
                image.width(),
                image.height(),
                output.width,
                output.height
            );
        }
        let x = i64::from(output.x) - i64::from(bounds.min_x);
        let y = i64::from(output.y) - i64::from(bounds.min_y);
        imageops::replace(&mut desktop, &image, x, y);
    }
    Ok(desktop)
}

pub(super) fn validate_output_capture(output: &NiriOutput) -> Result<()> {
    let pixels = u64::from(output.width)
        .checked_mul(u64::from(output.height))
        .context("Niri output dimensions overflowed")?;
    if pixels == 0 || pixels > MAX_DESKTOP_PIXELS {
        bail!(
            "Niri output {} is {}x{} ({} pixels), outside the supported capture range of 1..={} pixels",
            output.name,
            output.width,
            output.height,
            pixels,
            MAX_DESKTOP_PIXELS
        );
    }
    Ok(())
}

pub(super) fn validate_desktop_capture(layout: &OutputLayout<'_>) -> Result<()> {
    let outputs = layout.outputs();
    let mut output_pixels = 0_u64;
    for output in outputs {
        validate_output_capture(output)?;
        output_pixels = output_pixels
            .checked_add(u64::from(output.width) * u64::from(output.height))
            .context("Niri output pixel count overflowed")?;
    }
    if output_pixels > MAX_DESKTOP_PIXELS {
        bail!(
            "Niri outputs contain {output_pixels} logical pixels, over the capture limit of {MAX_DESKTOP_PIXELS} pixels"
        );
    }

    let bounds = layout.bounds();
    let desktop_pixels = u64::from(bounds.width)
        .checked_mul(u64::from(bounds.height))
        .context("Niri desktop dimensions overflowed")?;
    if desktop_pixels > MAX_DESKTOP_PIXELS {
        bail!(
            "Niri logical desktop is {}x{} ({} pixels), over the capture limit of {} pixels",
            bounds.width,
            bounds.height,
            desktop_pixels,
            MAX_DESKTOP_PIXELS
        );
    }
    Ok(())
}

pub(super) fn transform_image(image: RgbaImage, transform: NiriOutputTransform) -> RgbaImage {
    match transform {
        NiriOutputTransform::Normal => image,
        NiriOutputTransform::Rotate90 => imageops::rotate90(&image),
        NiriOutputTransform::Rotate180 => imageops::rotate180(&image),
        NiriOutputTransform::Rotate270 => imageops::rotate270(&image),
        NiriOutputTransform::Flipped => imageops::flip_horizontal(&image),
        NiriOutputTransform::Flipped90 => imageops::flip_horizontal(&imageops::rotate90(&image)),
        NiriOutputTransform::Flipped180 => imageops::flip_horizontal(&imageops::rotate180(&image)),
        NiriOutputTransform::Flipped270 => imageops::flip_horizontal(&imageops::rotate270(&image)),
    }
}

pub(super) fn orient_capture(
    image: RgbaImage,
    y_invert: bool,
    transform: NiriOutputTransform,
) -> RgbaImage {
    let image = if y_invert {
        imageops::flip_vertical(&image)
    } else {
        image
    };
    transform_image(image, transform)
}

pub(super) fn rgba_from_shm(raw: &[u8], offer: BufferOffer) -> Result<RgbaImage> {
    let required = checked_capture_len(offer.stride, offer.height)?;
    if raw.len() < required {
        bail!(
            "Niri screencopy returned {} bytes, expected at least {required}",
            raw.len()
        );
    }
    let row_bytes = usize::try_from(offer.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .context("Niri screencopy row size overflowed")?;
    let stride = usize::try_from(offer.stride).context("Niri screencopy stride is invalid")?;
    if stride < row_bytes {
        bail!(
            "Niri screencopy stride {} is smaller than the {}-byte pixel row",
            offer.stride,
            row_bytes
        );
    }

    let mut image = RgbaImage::new(offer.width, offer.height);
    for y in 0..offer.height {
        let row_start = usize::try_from(y)
            .ok()
            .and_then(|row| row.checked_mul(stride))
            .context("Niri screencopy row offset overflowed")?;
        for x in 0..offer.width {
            let pixel_start = row_start
                + usize::try_from(x)
                    .ok()
                    .and_then(|column| column.checked_mul(4))
                    .context("Niri screencopy pixel offset overflowed")?;
            let value = u32::from_ne_bytes(
                raw[pixel_start..pixel_start + 4]
                    .try_into()
                    .expect("pixel slice is exactly four bytes"),
            );
            let (red, green, blue, alpha) = match offer.format {
                wl_shm::Format::Xrgb8888 => (
                    ((value >> 16) & 0xff) as u8,
                    ((value >> 8) & 0xff) as u8,
                    (value & 0xff) as u8,
                    255,
                ),
                wl_shm::Format::Argb8888 => (
                    ((value >> 16) & 0xff) as u8,
                    ((value >> 8) & 0xff) as u8,
                    (value & 0xff) as u8,
                    ((value >> 24) & 0xff) as u8,
                ),
                wl_shm::Format::Xbgr8888 => (
                    (value & 0xff) as u8,
                    ((value >> 8) & 0xff) as u8,
                    ((value >> 16) & 0xff) as u8,
                    255,
                ),
                wl_shm::Format::Abgr8888 => (
                    (value & 0xff) as u8,
                    ((value >> 8) & 0xff) as u8,
                    ((value >> 16) & 0xff) as u8,
                    ((value >> 24) & 0xff) as u8,
                ),
                format => bail!("unsupported Niri screencopy wl_shm format {format:?}"),
            };
            image.put_pixel(x, y, Rgba([red, green, blue, alpha]));
        }
    }
    Ok(image)
}

pub(super) fn checked_capture_len(stride: u32, height: u32) -> Result<usize> {
    let bytes = usize::try_from(stride)
        .ok()
        .and_then(|stride| {
            usize::try_from(height)
                .ok()
                .and_then(|height| stride.checked_mul(height))
        })
        .context("Niri screencopy buffer size overflowed")?;
    if bytes == 0 || bytes > MAX_CAPTURE_BYTES {
        bail!(
            "Niri screencopy buffer size {bytes} is outside the supported range 1..={MAX_CAPTURE_BYTES}"
        );
    }
    Ok(bytes)
}

fn create_memfd(size: usize) -> Result<File> {
    create_named_memfd("codex-niri-screencopy", size, "screencopy")
}
