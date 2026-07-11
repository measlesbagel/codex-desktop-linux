use super::{
    capture::*,
    geometry::OutputLayout,
    input::*,
    ipc::{self as niri, Output as NiriOutput, OutputTransform as NiriOutputTransform},
    wayland::{BufferOffer, WaylandClient},
    PointerScope,
};
use image::{Rgba, RgbaImage};
use std::{
    io::Write as _,
    os::{fd::AsFd, unix::fs::FileExt},
    process::Stdio,
};
use wayland_client::protocol::{wl_keyboard, wl_shm};
use xkeysym::Keysym;

fn output(name: &str, x: i32, y: i32, width: u32, height: u32) -> NiriOutput {
    NiriOutput {
        name: name.to_string(),
        x,
        y,
        width,
        height,
        scale: 1.0,
        transform: NiriOutputTransform::Normal,
    }
}

#[test]
fn desktop_pointer_scope_stays_unscoped_without_an_explicit_output() {
    let scope = PointerScope::requested(None, false).unwrap();

    scope.validate_window(None).unwrap();
    assert!(!scope.is_output_scoped());
}

#[test]
fn text_keymap_preserves_literal_symbols_unicode_and_repetition() {
    let chunks = text_keymap_chunks(":✓🙂:").unwrap();

    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].keycodes, [1, 2, 3, 1]);
    for character in [':', '✓', '🙂'] {
        let keysym = Keysym::from_char(character).raw();
        assert!(chunks[0].keymap.contains(&format!("0x{keysym:08x}")));
    }
    assert!(!chunks[0].keymap.contains('🙂'));
}

#[test]
fn text_keymap_uses_xkb_control_keysyms() {
    let chunks = text_keymap_chunks("\t\n\r").unwrap();

    assert_eq!(chunks[0].keycodes, [1, 2, 3]);
    assert!(chunks[0].keymap.contains("0x0000ff09"));
    assert!(chunks[0].keymap.contains("0x0000ff0a"));
    assert!(chunks[0].keymap.contains("0x0000ff0d"));
}

#[test]
fn text_keymap_validates_all_unicode_before_input() {
    let error = text_keymap_chunks("valid\u{FDD0}not-sent")
        .unwrap_err()
        .to_string();

    assert!(error.contains("U+FDD0"));
    assert!(text_keymap_chunks("").unwrap().is_empty());
}

#[test]
fn text_keymap_chunks_keycode_and_request_sizes() {
    let unique = (0x10000..0x10000 + MAX_TEXT_KEYCODES as u32 + 1)
        .map(|codepoint| char::from_u32(codepoint).unwrap())
        .collect::<String>();
    let unique_chunks = text_keymap_chunks(&unique).unwrap();

    assert_eq!(unique_chunks.len(), 2);
    assert_eq!(unique_chunks[0].keycodes.len(), MAX_TEXT_KEYCODES);
    assert_eq!(unique_chunks[1].keycodes, [1]);
    assert_eq!(
        unique_chunks[0].keycodes.iter().copied().max(),
        Some(MAX_TEXT_KEYCODES as u32)
    );

    let repeated_chunks = text_keymap_chunks(&"x".repeat(MAX_TEXT_CHARS_PER_KEYMAP + 1)).unwrap();
    assert_eq!(repeated_chunks.len(), 2);
    assert_eq!(repeated_chunks[0].keycodes.len(), MAX_TEXT_CHARS_PER_KEYMAP);
    assert_eq!(repeated_chunks[1].keycodes, [1]);
}

#[test]
fn text_keymap_memfd_is_nul_terminated() {
    let keymap = &text_keymap_chunks("Codex").unwrap()[0].keymap;
    let (file, size) = create_text_keymap_memfd(keymap).unwrap();
    let mut contents = vec![0; size as usize];
    file.read_exact_at(&mut contents, 0).unwrap();

    assert_eq!(&contents[..keymap.len()], keymap.as_bytes());
    assert_eq!(contents.last(), Some(&0));
}

#[test]
fn generated_text_keymap_compiles_when_xkbcli_is_available() {
    let keymap = &text_keymap_chunks("Codex :✓🙂\t\n").unwrap()[0].keymap;
    let Ok(mut child) = std::process::Command::new("xkbcli")
        .args(["compile-keymap", "--from-xkb", "-", "--test"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return;
    };
    child
        .stdin
        .take()
        .unwrap()
        .write_all(keymap.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "xkbcli rejected the generated keymap: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn desktop_bounds_normalize_negative_origins_and_gaps() {
    let outputs = [
        output("left", -1920, 100, 1920, 1080),
        output("right", 200, -200, 2560, 1440),
    ];
    let bounds = OutputLayout::new(&outputs).unwrap().bounds();

    assert_eq!((bounds.min_x, bounds.min_y), (-1920, -200));
    assert_eq!((bounds.width, bounds.height), (4680, 1440));
}

#[test]
fn pointer_space_rejects_monitor_gaps() {
    let space = PointerSpace::new(
        &[
            output("left", 0, 0, 100, 100),
            output("right", 200, 0, 100, 100),
        ],
        None,
    )
    .unwrap();

    assert!(space.validate_point(50, 50).is_ok());
    assert!(space.validate_point(250, 50).is_ok());
    assert!(space
        .validate_point(150, 50)
        .unwrap_err()
        .to_string()
        .contains("gap"));
}

#[test]
fn pointer_focus_refresh_stays_inside_the_target_output() {
    let space = PointerSpace::new(
        &[
            output("left", 0, 0, 100, 100),
            output("right", 200, 0, 100, 100),
        ],
        None,
    )
    .unwrap();

    assert_eq!(space.focus_refresh_point(0, 0), Some((1, 0)));
    assert_eq!(space.focus_refresh_point(50, 50), Some((49, 50)));
    assert_eq!(space.focus_refresh_point(200, 0), Some((201, 0)));
    assert_eq!(space.focus_refresh_point(299, 99), Some((298, 99)));
    assert_eq!(space.focus_refresh_point(150, 50), None);

    let single_pixel = PointerSpace::new(&[output("tiny", 0, 0, 1, 1)], None).unwrap();
    assert_eq!(single_pixel.focus_refresh_point(0, 0), None);
}

#[test]
fn output_scoped_pointer_coordinates_are_local() {
    let space = PointerSpace::new(&[output("DP-1", -2560, 400, 2560, 1440)], Some("DP-1")).unwrap();

    assert_eq!(space.output_name.as_deref(), Some("DP-1"));
    assert_eq!((space.width, space.height), (2560, 1440));
    assert!(space.validate_point(2559, 1439).is_ok());
    assert!(space.validate_point(2560, 1439).is_err());
}

#[test]
fn output_scoped_pointer_coordinates_invert_output_transforms() {
    let cases = [
        (NiriOutputTransform::Normal, (10, 20, 100, 50)),
        (NiriOutputTransform::Rotate90, (20, 90, 50, 100)),
        (NiriOutputTransform::Rotate180, (90, 30, 100, 50)),
        (NiriOutputTransform::Rotate270, (30, 10, 50, 100)),
        (NiriOutputTransform::Flipped, (90, 20, 100, 50)),
        (NiriOutputTransform::Flipped90, (20, 10, 50, 100)),
        (NiriOutputTransform::Flipped180, (10, 30, 100, 50)),
        (NiriOutputTransform::Flipped270, (30, 90, 50, 100)),
    ];

    for (transform, expected) in cases {
        let mut output = output("DP-1", 0, 0, 100, 50);
        output.transform = transform;
        let space = PointerSpace::new(&[output], Some("DP-1")).unwrap();
        assert_eq!(space.protocol_coordinates(10, 20).unwrap(), expected);
    }
}

#[test]
fn converts_xrgb_with_padded_stride() {
    let offer = BufferOffer {
        format: wl_shm::Format::Xrgb8888,
        width: 2,
        height: 1,
        stride: 12,
    };
    let values = [0x0011_2233_u32, 0x00aa_bbcc_u32];
    let mut raw = Vec::new();
    raw.extend_from_slice(&values[0].to_ne_bytes());
    raw.extend_from_slice(&values[1].to_ne_bytes());
    raw.extend_from_slice(&[9, 9, 9, 9]);

    let image = rgba_from_shm(&raw, offer).unwrap();

    assert_eq!(image.get_pixel(0, 0).0, [0x11, 0x22, 0x33, 0xff]);
    assert_eq!(image.get_pixel(1, 0).0, [0xaa, 0xbb, 0xcc, 0xff]);
}

#[test]
fn preserves_argb_alpha() {
    let offer = BufferOffer {
        format: wl_shm::Format::Argb8888,
        width: 1,
        height: 1,
        stride: 4,
    };
    let raw = 0x8011_2233_u32.to_ne_bytes();

    let image = rgba_from_shm(&raw, offer).unwrap();

    assert_eq!(image.get_pixel(0, 0).0, [0x11, 0x22, 0x33, 0x80]);
}

#[test]
fn output_transform_rotates_and_flips_pixels() {
    let mut image = RgbaImage::new(2, 1);
    image.put_pixel(0, 0, Rgba([1, 0, 0, 255]));
    image.put_pixel(1, 0, Rgba([2, 0, 0, 255]));

    let rotated = transform_image(image.clone(), NiriOutputTransform::Rotate90);
    assert_eq!((rotated.width(), rotated.height()), (1, 2));
    assert_eq!(rotated.get_pixel(0, 0).0[0], 1);
    assert_eq!(rotated.get_pixel(0, 1).0[0], 2);

    let flipped = transform_image(image, NiriOutputTransform::Flipped);
    assert_eq!(flipped.get_pixel(0, 0).0[0], 2);
    assert_eq!(flipped.get_pixel(1, 0).0[0], 1);
}

#[test]
fn output_transform_matches_niri_geometry_for_every_flipped_rotation() {
    let mut image = RgbaImage::new(2, 3);
    for (index, pixel) in image.pixels_mut().enumerate() {
        *pixel = Rgba([(index + 1) as u8, 0, 0, 255]);
    }
    let values = |image: &RgbaImage| {
        image
            .rows()
            .map(|row| row.map(|pixel| pixel.0[0]).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        values(&transform_image(
            image.clone(),
            NiriOutputTransform::Flipped90
        )),
        vec![vec![1, 3, 5], vec![2, 4, 6]]
    );
    assert_eq!(
        values(&transform_image(
            image.clone(),
            NiriOutputTransform::Flipped180
        )),
        vec![vec![5, 6], vec![3, 4], vec![1, 2]]
    );
    assert_eq!(
        values(&transform_image(image, NiriOutputTransform::Flipped270)),
        vec![vec![6, 4, 2], vec![5, 3, 1]]
    );
}

#[test]
fn capture_orientation_applies_y_inversion_before_output_transform() {
    let mut image = RgbaImage::new(1, 2);
    image.put_pixel(0, 0, Rgba([1, 0, 0, 255]));
    image.put_pixel(0, 1, Rgba([2, 0, 0, 255]));

    let oriented = orient_capture(image, true, NiriOutputTransform::Rotate90);

    assert_eq!((oriented.width(), oriented.height()), (2, 1));
    assert_eq!(oriented.get_pixel(0, 0).0[0], 1);
    assert_eq!(oriented.get_pixel(1, 0).0[0], 2);
}

#[test]
fn desktop_composition_places_outputs_and_leaves_gaps_black() {
    let outputs = [output("left", -2, 0, 2, 2), output("right", 1, 1, 2, 2)];
    let images = vec![
        RgbaImage::from_pixel(2, 2, Rgba([10, 0, 0, 255])),
        RgbaImage::from_pixel(2, 2, Rgba([20, 0, 0, 255])),
    ];

    let layout = OutputLayout::new(&outputs).unwrap();
    let desktop = compose_desktop(&layout, images).unwrap();

    assert_eq!((desktop.width(), desktop.height()), (5, 3));
    assert_eq!(desktop.get_pixel(0, 0).0[0], 10);
    assert_eq!(desktop.get_pixel(3, 1).0[0], 20);
    assert_eq!(desktop.get_pixel(2, 0).0, [0, 0, 0, 255]);
}

#[test]
fn checked_capture_size_rejects_overflow_and_large_buffers() {
    assert_eq!(checked_capture_len(16, 4).unwrap(), 64);
    assert!(checked_capture_len(u32::MAX, u32::MAX).is_err());
}

#[test]
fn capture_limits_are_checked_before_allocating_output_images() {
    let oversized = output(
        "DP-1",
        0,
        0,
        u32::try_from(MAX_DESKTOP_PIXELS + 1).unwrap(),
        1,
    );
    assert!(validate_output_capture(&oversized).is_err());

    let overlapping = vec![
        output("DP-1", 0, 0, 8_192, 8_192),
        output("DP-2", 0, 0, 8_192, 8_192),
        output("DP-3", 0, 0, 1, 1),
    ];
    let layout = OutputLayout::new(&overlapping).unwrap();
    assert!(validate_desktop_capture(&layout).is_err());
}

#[test]
fn live_output_capture_matches_niri_logical_dimensions_when_requested() {
    let Ok(output_name) = std::env::var("CODEX_TEST_NIRI_OUTPUT") else {
        return;
    };
    let output = niri::outputs()
        .unwrap()
        .into_iter()
        .find(|output| output.name == output_name)
        .unwrap();

    let capture = capture(Some(&output_name)).unwrap();

    assert_eq!(capture.output_name.as_deref(), Some(output_name.as_str()));
    assert_eq!(
        (capture.image.width(), capture.image.height()),
        (output.width, output.height)
    );
}

#[test]
fn live_read_only_capture_distinguishes_active_workspaces_when_requested() {
    if std::env::var("CODEX_TEST_NIRI_WORKSPACES").as_deref() != Ok("1") {
        return;
    }
    let windows = crate::windowing::backends::niri::list_windows().unwrap();
    let mut saw_active = false;
    let mut saw_inactive = false;
    for window in windows {
        let Ok(output) = niri::output_for_window(window.window_id) else {
            continue;
        };
        let result = super::read_only_capture_output(Some(&window));
        if output.workspace_active {
            saw_active = true;
            assert_eq!(
                result.unwrap().as_deref(),
                Some(output.output_name.as_str())
            );
        } else {
            saw_inactive = true;
            assert!(result.unwrap_err().contains("inactive workspace"));
        }
    }
    assert!(saw_active, "expected a Niri window on an active workspace");
    assert!(
        saw_inactive,
        "expected a Niri window on an inactive workspace"
    );
}

#[test]
fn live_virtual_input_paths_accept_no_op_events_when_requested() {
    if std::env::var("CODEX_TEST_NIRI_PROTOCOL_OBJECTS").as_deref() != Ok("1") {
        return;
    }
    let output_name = niri::outputs().unwrap().remove(0).name;
    let mut pointer_client = WaylandClient::connect().unwrap();
    let _pointer = pointer_client.create_pointer(Some(&output_name)).unwrap();
    pointer_client.finish_requests().unwrap();

    let mut keyboard_client = WaylandClient::connect().unwrap();
    keyboard_client.request_keyboard_keymap().unwrap();
    let seat = keyboard_client.state.seat.clone().unwrap();
    let manager = keyboard_client
        .state
        .virtual_keyboard_manager
        .clone()
        .unwrap();
    let keymap = keyboard_client.state.keymap.as_ref().unwrap();
    let keyboard = manager.create_virtual_keyboard(&seat, &keyboard_client.qh, ());
    keyboard.keymap(
        wl_keyboard::KeymapFormat::XkbV1.into(),
        keymap.fd.as_fd(),
        keymap.size,
    );
    keyboard_client.finish_requests().unwrap();

    scroll(None, None, false, 0).unwrap();
    press_key(&[(0, true), (0, false)]).unwrap();
}
