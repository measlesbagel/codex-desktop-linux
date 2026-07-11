use super::{
    geometry::{normalized_output_name, OutputLayout},
    ipc::{self, Output as NiriOutput, OutputTransform as NiriOutputTransform},
    wayland::{create_named_memfd, monotonic_millis, WaylandClient},
};
use anyhow::{bail, Context, Result};
use std::{
    collections::HashMap,
    fmt::Write as _,
    fs::File,
    os::{fd::AsFd, unix::fs::FileExt},
    thread,
    time::Duration,
};
use wayland_client::protocol::{wl_keyboard, wl_pointer};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;
use xkeysym::Keysym;

pub(super) const MAX_TEXT_KEYCODES: usize = 247;
pub(super) const MAX_TEXT_CHARS_PER_KEYMAP: usize = 2_048;
const TEXT_KEY_DELAY: Duration = Duration::from_millis(5);

pub(super) fn click(
    output_name: Option<&str>,
    x: i32,
    y: i32,
    button: u32,
    count: u32,
) -> Result<()> {
    let mut session = PointerSession::connect(output_name)?;
    session.move_with_focus_refresh(x, y)?;
    for _ in 0..count.max(1) {
        session.button(button, wl_pointer::ButtonState::Pressed);
        thread::sleep(Duration::from_millis(35));
        session.button(button, wl_pointer::ButtonState::Released);
    }
    session.finish()
}

pub(super) fn scroll(
    output_name: Option<&str>,
    target: Option<(i32, i32)>,
    horizontal: bool,
    steps: i32,
) -> Result<()> {
    let mut session = PointerSession::connect(output_name)?;
    if let Some((x, y)) = target {
        session.move_with_focus_refresh(x, y)?;
    }
    let axis = if horizontal {
        wl_pointer::Axis::HorizontalScroll
    } else {
        wl_pointer::Axis::VerticalScroll
    };
    let steps = steps.clamp(-120, 120);
    session
        .pointer
        .axis_discrete(monotonic_millis(), axis, f64::from(steps) * 10.0, steps);
    session.pointer.axis_source(wl_pointer::AxisSource::Wheel);
    session.pointer.frame();
    session.finish()
}

pub(super) fn drag(
    output_name: Option<&str>,
    start: (i32, i32),
    end: (i32, i32),
    button: u32,
) -> Result<()> {
    let mut session = PointerSession::connect(output_name)?;
    session.space.validate_point(end.0, end.1)?;
    session.move_with_focus_refresh(start.0, start.1)?;
    session.button(button, wl_pointer::ButtonState::Pressed);
    thread::sleep(Duration::from_millis(35));
    session.move_absolute(end.0, end.1)?;
    thread::sleep(Duration::from_millis(35));
    session.button(button, wl_pointer::ButtonState::Released);
    session.finish()
}

pub(super) fn press_key(events: &[(u16, bool)]) -> Result<()> {
    if events.is_empty() {
        bail!("cannot send an empty Niri key sequence");
    }

    let mut session = KeyboardSession::connect()?;
    let keyboard = session.keyboard_with_active_keymap()?;
    for (key, pressed) in events {
        keyboard.key(
            monotonic_millis(),
            u32::from(*key),
            if *pressed {
                wl_keyboard::KeyState::Pressed
            } else {
                wl_keyboard::KeyState::Released
            }
            .into(),
        );
        thread::sleep(Duration::from_millis(8));
    }
    session.finish()
}

pub(super) fn type_text(text: &str) -> Result<()> {
    validate_text_keysyms(text)?;
    if text.is_empty() {
        return Ok(());
    }

    let mut session = KeyboardSession::connect()?;

    emit_text_keymap_chunks(text, |chunk| {
        let (keymap_file, keymap_size) = create_text_keymap_memfd(&chunk.keymap)?;
        let keyboard = session.keyboard_with_keymap(&keymap_file, keymap_size)?;
        keyboard.modifiers(0, 0, 0, 0);
        for keycode in chunk.keycodes {
            keyboard.key(
                monotonic_millis(),
                keycode,
                wl_keyboard::KeyState::Pressed.into(),
            );
            thread::sleep(TEXT_KEY_DELAY);
            keyboard.key(
                monotonic_millis(),
                keycode,
                wl_keyboard::KeyState::Released.into(),
            );
            thread::sleep(TEXT_KEY_DELAY);
        }
        keyboard.destroy();
        session.finish()
    })
}

#[derive(Debug)]
pub(super) struct TextKeymapChunk {
    pub(super) keymap: String,
    pub(super) keycodes: Vec<u32>,
}

fn validate_text_keysyms(text: &str) -> Result<()> {
    for character in text.chars() {
        if Keysym::from_char(character) == Keysym::NoSymbol {
            bail!(
                "character U+{:04X} cannot be represented as an X11 keysym",
                character as u32
            );
        }
    }
    Ok(())
}

fn emit_text_keymap_chunks(
    text: &str,
    mut emit: impl FnMut(TextKeymapChunk) -> Result<()>,
) -> Result<()> {
    let mut symbols = Vec::new();
    let mut assigned_keycodes = HashMap::new();
    let mut keycodes = Vec::new();

    for character in text.chars() {
        let keysym = Keysym::from_char(character);
        debug_assert_ne!(keysym, Keysym::NoSymbol);
        let keysym = keysym.raw();
        let needs_new_keycode = !assigned_keycodes.contains_key(&keysym);
        if keycodes.len() >= MAX_TEXT_CHARS_PER_KEYMAP
            || (needs_new_keycode && symbols.len() >= MAX_TEXT_KEYCODES)
        {
            emit(TextKeymapChunk {
                keymap: render_text_keymap(&symbols),
                keycodes: std::mem::take(&mut keycodes),
            })?;
            symbols.clear();
            assigned_keycodes.clear();
        }

        let keycode = *assigned_keycodes.entry(keysym).or_insert_with(|| {
            symbols.push(keysym);
            u32::try_from(symbols.len()).expect("text keymap is limited to 247 keycodes")
        });
        keycodes.push(keycode);
    }

    if !keycodes.is_empty() {
        emit(TextKeymapChunk {
            keymap: render_text_keymap(&symbols),
            keycodes,
        })?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn text_keymap_chunks(text: &str) -> Result<Vec<TextKeymapChunk>> {
    validate_text_keysyms(text)?;
    let mut chunks = Vec::new();
    emit_text_keymap_chunks(text, |chunk| {
        chunks.push(chunk);
        Ok(())
    })?;
    Ok(chunks)
}

fn render_text_keymap(symbols: &[u32]) -> String {
    let mut keymap = String::from(
        "xkb_keymap {\n\
         xkb_keycodes \"codex\" {\n\
          minimum = 8;\n\
          maximum = 255;\n",
    );
    for (index, _) in symbols.iter().enumerate() {
        let protocol_keycode = index + 1;
        let xkb_keycode = protocol_keycode + 8;
        writeln!(keymap, "  <K{protocol_keycode:03}> = {xkb_keycode};")
            .expect("writing to a String cannot fail");
    }
    keymap.push_str(
        " };\n\
         xkb_types \"codex\" {\n\
          type \"ONE_LEVEL\" {\n\
           modifiers = None;\n\
           level_name[Level1] = \"Any\";\n\
          };\n\
         };\n\
         xkb_compatibility \"codex\" {};\n\
         xkb_symbols \"codex\" {\n",
    );
    for (index, keysym) in symbols.iter().enumerate() {
        let protocol_keycode = index + 1;
        writeln!(
            keymap,
            "  key <K{protocol_keycode:03}> {{ type[Group1] = \"ONE_LEVEL\", symbols[Group1] = [ 0x{keysym:08x} ] }};"
        )
        .expect("writing to a String cannot fail");
    }
    keymap.push_str(" };\n};\n");
    keymap
}

struct PointerSession {
    client: WaylandClient,
    pointer: ZwlrVirtualPointerV1,
    space: PointerSpace,
}

impl PointerSession {
    fn connect(output_name: Option<&str>) -> Result<Self> {
        let outputs = ipc::outputs()?;
        let space = PointerSpace::new(&outputs, output_name)?;
        let client = WaylandClient::connect()?;
        let pointer = client.create_pointer(space.output_name.as_deref())?;
        Ok(Self {
            client,
            pointer,
            space,
        })
    }

    fn move_with_focus_refresh(&mut self, x: i32, y: i32) -> Result<()> {
        pointer_motion_with_focus_refresh(&mut self.client, &self.pointer, &self.space, x, y)
    }

    fn move_absolute(&self, x: i32, y: i32) -> Result<()> {
        pointer_motion_absolute(&self.pointer, &self.space, x, y)?;
        self.pointer.frame();
        Ok(())
    }

    fn button(&self, button: u32, state: wl_pointer::ButtonState) {
        self.pointer.button(monotonic_millis(), button, state);
        self.pointer.frame();
    }

    fn finish(&mut self) -> Result<()> {
        self.client.finish_requests()
    }
}

struct KeyboardSession {
    client: WaylandClient,
}

impl KeyboardSession {
    fn connect() -> Result<Self> {
        Ok(Self {
            client: WaylandClient::connect()?,
        })
    }

    fn keyboard_with_active_keymap(&mut self) -> Result<ZwpVirtualKeyboardV1> {
        self.client.request_keyboard_keymap()?;
        let keymap = self
            .client
            .state
            .keymap
            .as_ref()
            .context("Niri did not provide the active wl_keyboard XKB keymap")?;
        let keyboard = self.keyboard()?;
        keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1.into(),
            keymap.fd.as_fd(),
            keymap.size,
        );
        Ok(keyboard)
    }

    fn keyboard_with_keymap(&self, keymap: &File, size: u32) -> Result<ZwpVirtualKeyboardV1> {
        let keyboard = self.keyboard()?;
        keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1.into(),
            keymap.as_fd(),
            size,
        );
        Ok(keyboard)
    }

    fn keyboard(&self) -> Result<ZwpVirtualKeyboardV1> {
        let seat = self
            .client
            .state
            .seat
            .as_ref()
            .context("Niri did not advertise wl_seat")?;
        let manager = self
            .client
            .state
            .virtual_keyboard_manager
            .as_ref()
            .context("Niri did not advertise zwp_virtual_keyboard_manager_v1")?;
        Ok(manager.create_virtual_keyboard(seat, &self.client.qh, ()))
    }

    fn finish(&mut self) -> Result<()> {
        self.client.finish_requests()
    }
}

pub(super) struct PointerSpace {
    pub(super) output_name: Option<String>,
    pub(super) width: u32,
    pub(super) height: u32,
    output_transform: Option<NiriOutputTransform>,
    output_rects: Vec<(i32, i32, u32, u32)>,
}

impl PointerSpace {
    pub(super) fn new(outputs: &[NiriOutput], output_name: Option<&str>) -> Result<Self> {
        if let Some(output_name) = normalized_output_name(output_name) {
            let output = OutputLayout::new(outputs)?.find_output(output_name)?;
            return Ok(Self {
                output_name: Some(output.name.clone()),
                width: output.width,
                height: output.height,
                output_transform: Some(output.transform),
                output_rects: vec![(0, 0, output.width, output.height)],
            });
        }

        let layout = OutputLayout::new(outputs)?;
        let bounds = layout.bounds();
        let output_rects = layout
            .outputs()
            .iter()
            .map(|output| {
                Ok((
                    i32::try_from(i64::from(output.x) - i64::from(bounds.min_x))
                        .context("Niri output X offset is out of range")?,
                    i32::try_from(i64::from(output.y) - i64::from(bounds.min_y))
                        .context("Niri output Y offset is out of range")?,
                    output.width,
                    output.height,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            output_name: None,
            width: bounds.width,
            height: bounds.height,
            output_transform: None,
            output_rects,
        })
    }

    pub(super) fn validate_point(&self, x: i32, y: i32) -> Result<()> {
        let point_in_bounds = x >= 0
            && y >= 0
            && u32::try_from(x).is_ok_and(|x| x < self.width)
            && u32::try_from(y).is_ok_and(|y| y < self.height);
        if !point_in_bounds {
            bail!(
                "point ({x}, {y}) is outside the Niri coordinate space {}x{}",
                self.width,
                self.height
            );
        }
        let on_output = self.output_rects.iter().any(|(left, top, width, height)| {
            i64::from(x) >= i64::from(*left)
                && i64::from(y) >= i64::from(*top)
                && i64::from(x) < i64::from(*left) + i64::from(*width)
                && i64::from(y) < i64::from(*top) + i64::from(*height)
        });
        if !on_output {
            bail!("point ({x}, {y}) falls in a gap between Niri outputs");
        }
        Ok(())
    }

    pub(super) fn protocol_coordinates(&self, x: i32, y: i32) -> Result<(u32, u32, u32, u32)> {
        self.validate_point(x, y)?;
        let x = u32::try_from(x).context("pointer X coordinate is negative")?;
        let y = u32::try_from(y).context("pointer Y coordinate is negative")?;
        let width = self.width;
        let height = self.height;
        let coordinates = match self.output_transform {
            None | Some(NiriOutputTransform::Normal) => (x, y, width, height),
            Some(NiriOutputTransform::Rotate90) => (y, width - x, height, width),
            Some(NiriOutputTransform::Rotate180) => (width - x, height - y, width, height),
            Some(NiriOutputTransform::Rotate270) => (height - y, x, height, width),
            Some(NiriOutputTransform::Flipped) => (width - x, y, width, height),
            Some(NiriOutputTransform::Flipped90) => (y, x, height, width),
            Some(NiriOutputTransform::Flipped180) => (x, height - y, width, height),
            Some(NiriOutputTransform::Flipped270) => (height - y, width - x, height, width),
        };
        Ok(coordinates)
    }

    pub(super) fn focus_refresh_point(&self, x: i32, y: i32) -> Option<(i32, i32)> {
        let (left, top, width, height) =
            self.output_rects
                .iter()
                .copied()
                .find(|(left, top, width, height)| {
                    i64::from(x) >= i64::from(*left)
                        && i64::from(y) >= i64::from(*top)
                        && i64::from(x) < i64::from(*left) + i64::from(*width)
                        && i64::from(y) < i64::from(*top) + i64::from(*height)
                })?;
        if x > left {
            Some((x - 1, y))
        } else if i64::from(x) + 1 < i64::from(left) + i64::from(width) {
            Some((x + 1, y))
        } else if y > top {
            Some((x, y - 1))
        } else if i64::from(y) + 1 < i64::from(top) + i64::from(height) {
            Some((x, y + 1))
        } else {
            None
        }
    }
}

fn pointer_motion_absolute(
    pointer: &ZwlrVirtualPointerV1,
    space: &PointerSpace,
    x: i32,
    y: i32,
) -> Result<()> {
    let (x, y, width, height) = space.protocol_coordinates(x, y)?;
    pointer.motion_absolute(monotonic_millis(), x, y, width, height);
    Ok(())
}

fn pointer_motion_with_focus_refresh(
    client: &mut WaylandClient,
    pointer: &ZwlrVirtualPointerV1,
    space: &PointerSpace,
    x: i32,
    y: i32,
) -> Result<()> {
    if let Some((refresh_x, refresh_y)) = space.focus_refresh_point(x, y) {
        pointer_motion_absolute(pointer, space, refresh_x, refresh_y)?;
        pointer.frame();
        client.finish_requests()?;
    }
    pointer_motion_absolute(pointer, space, x, y)?;
    pointer.frame();
    client.finish_requests()
}

pub(super) fn create_text_keymap_memfd(keymap: &str) -> Result<(File, u32)> {
    let size = keymap
        .len()
        .checked_add(1)
        .context("Niri text keymap size overflowed")?;
    let file = create_named_memfd("codex-niri-text-keymap", size, "text keymap")?;
    file.write_all_at(keymap.as_bytes(), 0)
        .context("failed to write the Niri text keymap")?;
    file.write_all_at(
        &[0],
        u64::try_from(keymap.len()).context("Niri text keymap offset is too large")?,
    )
    .context("failed to terminate the Niri text keymap")?;
    let size = u32::try_from(size).context("Niri text keymap is too large")?;
    Ok((file, size))
}
