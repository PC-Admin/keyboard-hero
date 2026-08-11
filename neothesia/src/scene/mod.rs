pub mod freeplay;
pub mod menu_scene;
pub mod playing_scene;

use crate::{
    NeothesiaEvent, context::Context, scene::playing_scene::Keyboard, utils::window::WinitEvent,
};
use midi_file::midly::MidiMessage;
use neothesia_core::render::{Image, ImageIdentifier, ImageRenderer, QuadRenderer, TextRenderer};
use std::{collections::HashMap, time::Duration};
use winit::{
    dpi::{LogicalPosition, LogicalSize},
    event::{ElementState, KeyEvent, WindowEvent},
    keyboard::Key,
};

/// Geometry of the performer selector, so anything that lines up with it (the
/// playing scene's song title sits directly underneath) measures from here
/// rather than keeping its own copy of the numbers.
const SEGMENT_W: f32 = 64.0;
const SEGMENT_GAP: f32 = 2.0;
pub const PERFORMER_SELECTOR_W: f32 = SEGMENT_W * 3.0 + SEGMENT_GAP * 2.0;
pub const PERFORMER_SELECTOR_H: f32 = 32.0;
/// Inset from the right edge of the window.
pub const PERFORMER_SELECTOR_MARGIN: f32 = 16.0;

/// Left edge of the performer selector in a window `win_w` wide.
pub fn performer_selector_x(win_w: f32) -> f32 {
    win_w - PERFORMER_SELECTOR_W - PERFORMER_SELECTOR_MARGIN
}

/// The HERO / AUTO / HUMAN performer selector, top-right.
///
/// One function, called by both the menu and the playing scene, because the
/// control is meant to be the same thing in the same place wherever you are —
/// two copies would be free to drift apart. `top` is the y it hangs from (the
/// playing scene rides it down as the top bar expands). Returns the mode that
/// was clicked, if it is not the current one.
pub fn performer_selector(
    ui: &mut nuon::Ui,
    win_w: f32,
    top: f32,
    current: crate::song::PerformMode,
) -> Option<crate::song::PerformMode> {
    use crate::song::PerformMode;

    let (w, h, gap) = (SEGMENT_W, PERFORMER_SELECTOR_H, SEGMENT_GAP);
    let x0 = performer_selector_x(win_w);

    let segments = [
        (PerformMode::Hero, "HERO", [8.0, 0.0, 0.0, 8.0]),
        (PerformMode::Auto, "AUTO", [0.0; 4]),
        (PerformMode::Human, "HUMAN", [0.0, 8.0, 8.0, 0.0]),
    ];

    let mut picked = None;

    for (i, (mode, label, radius)) in segments.into_iter().enumerate() {
        let active = current == mode;
        let color = if active {
            nuon::Color::new_u8(160, 81, 238, 1.0)
        } else {
            nuon::Color::new_u8(50, 50, 60, 0.9)
        };

        if nuon::button()
            .id(label)
            .pos(x0 + i as f32 * (w + gap), top)
            .size(w, h)
            .color(color)
            .border_radius(radius)
            .label(label)
            .build(ui)
            && !active
        {
            picked = Some(mode);
        }
    }

    picked
}

pub trait Scene {
    fn update(&mut self, ctx: &mut Context, delta: Duration);
    fn render<'pass>(&'pass mut self, rpass: &mut wgpu_jumpstart::RenderPass<'pass>);
    fn window_event(&mut self, _ctx: &mut Context, _event: &WindowEvent) {}
    fn midi_event(&mut self, _ctx: &mut Context, _channel: u8, _message: &MidiMessage) {}
}

/// The one key that switches the microphone, wherever you are.
///
/// Shared rather than written out per scene so it cannot end up meaning
/// different things in different rooms — the menu is where you set it up before
/// a song, freeplay is where you notice mid-take that you wanted it on.
///
/// `m` is not one of the letters `handle_pc_keyboard_to_midi_event` maps to a
/// note, so this can sit alongside it without stealing a key from the keyboard.
pub fn handle_mic_toggle_event(ctx: &mut Context, event: &WindowEvent) {
    if event.key_pressed(Key::Character("m")) {
        ctx.mic_passthrough.toggle();
    }
}

pub fn handle_pc_keyboard_to_midi_event(ctx: &mut Context, event: &WindowEvent) {
    let WindowEvent::KeyboardInput {
        event:
            KeyEvent {
                state,
                logical_key: Key::Character(ch),
                repeat: false,
                ..
            },
        ..
    } = event
    else {
        return;
    };

    let mut note: u8 = match ch.as_str() {
        "a" => 0,
        "w" => 1,
        "s" => 2,
        "e" => 3,
        "d" => 4,
        "f" => 5,
        "t" => 6,
        "g" => 7,
        "y" => 8,
        "h" => 9,
        "u" => 10,
        "j" => 11,
        "k" => 12,
        "o" => 13,
        "l" => 14,
        "p" => 15,
        ";" => 16,
        "'" => 17,
        _ => return,
    };

    note += 21; // Start of 88 keyboard
    note += 3; // Offset to C
    note += 12 * 3; // Move 3oct up

    let message = match state {
        ElementState::Pressed => MidiMessage::NoteOn {
            key: note.into(),
            vel: 100.into(),
        },
        ElementState::Released => MidiMessage::NoteOff {
            key: note.into(),
            vel: 0.into(),
        },
    };
    ctx.proxy
        .send_event(NeothesiaEvent::MidiInput {
            channel: 0,
            message,
        })
        .ok();
}

#[derive(Default, Debug)]
struct MouseToMidiEventState {
    mouse_key_press: Option<u8>,
}

fn handle_mouse_to_midi_event(
    keyboard: &mut Keyboard,
    state: &mut MouseToMidiEventState,
    ctx: &Context,
    event: &WindowEvent,
) {
    if !(event.left_mouse_pressed() || event.left_mouse_released() || event.cursor_moved()) {
        return;
    }

    fn cancel_mouse_key_press(state: &mut MouseToMidiEventState, ctx: &Context) {
        let Some(key) = state.mouse_key_press else {
            return;
        };

        state.mouse_key_press = None;

        let message = MidiMessage::NoteOff {
            key: key.into(),
            vel: 0.into(),
        };
        ctx.proxy
            .send_event(NeothesiaEvent::MidiInput {
                channel: 0,
                message,
            })
            .ok();
    }

    let bbox = nuon::Rect::new(
        (keyboard.pos().x, keyboard.pos().y).into(),
        (keyboard.layout().width, keyboard.layout().height).into(),
    );
    let mouse_pos = nuon::Point::new(
        ctx.window_state.cursor_logical_position.x,
        ctx.window_state.cursor_logical_position.y,
    );

    if !bbox.contains(mouse_pos) || !ctx.window_state.left_mouse_btn {
        cancel_mouse_key_press(state, ctx);
        return;
    }

    let sharp = keyboard
        .layout()
        .keys
        .iter()
        .filter(|key| key.kind().is_sharp());
    let neutral = keyboard
        .layout()
        .keys
        .iter()
        .filter(|key| key.kind().is_neutral());

    for key in sharp.chain(neutral) {
        let pos = nuon::Point::new(key.x(), keyboard.pos().y);
        let size = nuon::Size::from(key.size());
        let rect = nuon::Rect::new(pos, size);
        if !rect.contains(mouse_pos) {
            continue;
        }

        let key = keyboard.layout().range.start() + key.id() as u8;

        if Some(key) == state.mouse_key_press {
            return;
        }

        cancel_mouse_key_press(state, ctx);
        state.mouse_key_press = Some(key);

        let message = MidiMessage::NoteOn {
            key: key.into(),
            vel: 100.into(),
        };
        ctx.proxy
            .send_event(NeothesiaEvent::MidiInput {
                channel: 0,
                message,
            })
            .ok();
        return;
    }
}

struct NuonLayer {
    quad_renderer: QuadRenderer,
    text_renderer: TextRenderer,
    images: Vec<Image>,
}

pub struct NuonRenderer {
    layers: Vec<NuonLayer>,
    image_map: HashMap<ImageIdentifier, Image>,
    image_renderer: ImageRenderer,
}

impl NuonRenderer {
    pub fn new(ctx: &Context) -> Self {
        Self {
            layers: Vec::new(),
            image_map: HashMap::new(),
            image_renderer: ImageRenderer::new(
                &ctx.gpu.device,
                ctx.gpu.texture_format,
                &ctx.transform,
            ),
        }
    }

    fn ensure_layers(&mut self, ctx: &mut Context, len: usize) {
        self.layers.resize_with(len, || NuonLayer {
            quad_renderer: ctx.quad_renderer_factory.new_renderer(),
            text_renderer: ctx.text_renderer_factory.new_renderer(),
            images: Vec::new(),
        });
    }

    pub fn add_image(&mut self, image: Image) -> ImageIdentifier {
        let ident = image.identifier();
        self.image_map.insert(ident, image);
        ident
    }

    pub fn render<'rpass>(&'rpass self, rpass: &mut wgpu_jumpstart::RenderPass<'rpass>) {
        for layer in self.layers.iter() {
            layer.quad_renderer.render(rpass);
            layer.text_renderer.render(rpass);
            for image in layer.images.iter() {
                self.image_renderer.render(rpass, image);
            }
        }
    }
}

fn handle_nuon_window_event(nuon: &mut nuon::Ui, event: &WindowEvent, ctx: &Context) {
    if event.cursor_moved() {
        nuon.mouse_move(
            ctx.window_state.cursor_logical_position.x,
            ctx.window_state.cursor_logical_position.y,
        );
    } else if event.left_mouse_pressed() {
        nuon.mouse_down();
    } else if event.left_mouse_released() {
        nuon.mouse_up();
    }
}

fn render_nuon(ui: &mut nuon::Ui, nuon_renderer: &mut NuonRenderer, ctx: &mut Context) {
    nuon_renderer.ensure_layers(ctx, ui.layers.len());

    for (layer, out) in ui.layers.iter().zip(nuon_renderer.layers.iter_mut()) {
        out.quad_renderer.clear();
        out.images.clear();

        let scissor_rect = layer.scissor_rect;
        let pos = LogicalPosition::new(scissor_rect.origin.x, scissor_rect.origin.y)
            .to_physical::<u32>(ctx.window_state.scale_factor);
        let size = LogicalSize::new(scissor_rect.width(), scissor_rect.height())
            .to_physical::<u32>(ctx.window_state.scale_factor);

        // wgpu treats a scissor rect that leaves the render target as a fatal
        // validation error, so clamp it here: a layer laid out for a window
        // bigger than the current surface (a stale frame mid-resize, say)
        // should clip, not take the whole app down.
        let target = ctx.window_state.physical_size;
        let x = pos.x.min(target.width);
        let y = pos.y.min(target.height);
        let scissor_rect = neothesia_core::Rect::new(
            (x, y).into(),
            (
                size.width.min(target.width - x),
                size.height.min(target.height - y),
            )
                .into(),
        );

        out.quad_renderer.set_scissor_rect(scissor_rect);
        out.text_renderer.set_scissor_rect(scissor_rect);

        for quad in layer.quads.iter() {
            out.quad_renderer
                .push(neothesia_core::render::QuadInstance {
                    position: quad.rect.origin.into(),
                    size: quad.rect.size.into(),
                    color: wgpu_jumpstart::Color::new(
                        quad.color.r,
                        quad.color.g,
                        quad.color.b,
                        quad.color.a,
                    )
                    .into_linear_rgba(),
                    border_radius: quad.border_radius,
                });
        }

        for img in layer.images.iter() {
            if let Some(image) = nuon_renderer.image_map.get_mut(&img.image) {
                image.set_rect(img.rect, img.border_radius);
                out.images.push(image.clone());
            }
        }

        for icon in layer.icons.iter() {
            out.text_renderer.queue_icon(
                icon.origin.x,
                icon.origin.y,
                icon.size,
                &icon.icon,
                cosmic_text::Color(icon.color.packet_u32()),
            );
        }

        for text in layer.text.iter() {
            let buffer = if text.bold {
                TextRenderer::gen_buffer_with_attr(
                    text.size,
                    &text.text,
                    cosmic_text::Attrs::new()
                        .family(cosmic_text::Family::Name(&text.font_family))
                        .weight(cosmic_text::Weight::BOLD)
                        .color(cosmic_text::Color(text.color.packet_u32())),
                )
            } else {
                TextRenderer::gen_buffer_with_attr(
                    text.size,
                    &text.text,
                    cosmic_text::Attrs::new()
                        .family(cosmic_text::Family::Name(&text.font_family))
                        .color(cosmic_text::Color(text.color.packet_u32())),
                )
            };

            match text.text_justify {
                nuon::TextJustify::Left => {
                    out.text_renderer.queue_buffer_left(text.rect, buffer);
                }
                nuon::TextJustify::Right => {
                    out.text_renderer.queue_buffer_right(text.rect, buffer);
                }
                nuon::TextJustify::Center => {
                    out.text_renderer.queue_buffer_centered(text.rect, buffer);
                }
            }
        }

        out.quad_renderer.prepare();
        out.text_renderer.update(
            ctx.window_state.physical_size,
            ctx.window_state.scale_factor as f32,
        );
    }

    ui.done();
}
