use crate::{
    render::{QuadInstance, QuadRenderer},
    utils::Point,
};

use piano_layout::range::KeyboardRange;

mod key_state;
pub use key_state::KeyState;

use super::TextRenderer;

use wgpu_jumpstart::Color;

/// Rainbow note-guide square color for a white (neutral) key, keyed by its
/// note within the octave. Returns `None` for notes that don't get a square.
///
/// C=Red, D=Orange, E=Yellow, F=Green, G=Blue, A=Purple, B=Pink
fn rainbow_color(note_id: u8) -> Option<Color> {
    let (r, g, b) = match note_id {
        0 => (230, 30, 30),    // C - Red
        2 => (255, 140, 0),    // D - Orange
        4 => (255, 215, 0),    // E - Yellow
        5 => (40, 190, 60),    // F - Green
        7 => (40, 90, 230),    // G - Blue
        9 => (150, 60, 210),   // A - Purple
        11 => (255, 105, 180), // B - Pink
        _ => return None,
    };
    Some(Color::from_rgba8(r, g, b, 1.0))
}

/// The note name drawn on a black key, or `None` for white keys.
fn sharp_name(note_id: u8) -> Option<&'static str> {
    Some(match note_id {
        1 => "C#",
        3 => "D#",
        6 => "F#",
        8 => "G#",
        10 => "A#",
        _ => return None,
    })
}

/// The note letter drawn inside the rainbow square, or `None` for keys that
/// don't get a square.
fn rainbow_letter(note_id: u8) -> Option<&'static str> {
    Some(match note_id {
        0 => "C",
        2 => "D",
        4 => "E",
        5 => "F",
        7 => "G",
        9 => "A",
        11 => "B",
        _ => return None,
    })
}

/// Geometry of the rainbow square for a white key: (color, x, y, side).
/// Returns `None` for keys that don't get a square.
fn rainbow_square(key: &piano_layout::Key, pos: Point<f32>) -> Option<(Color, f32, f32, f32)> {
    let color = rainbow_color(key.note_id())?;

    // Visual white-key width matches `to_quad` (which trims 1px).
    let key_w = key.width() - 1.0;
    let side = key_w * 0.7;

    let x = pos.x + key.x() + (key_w - side) / 2.0;
    // Sit the square near the bottom of the key, on the space the stock
    // octave markers used to occupy, with a small margin below it.
    let y = pos.y + key.height() - side - key.height() * 0.05;

    Some((color, x, y, side))
}

pub struct KeyboardRenderer {
    pos: Point<f32>,

    key_states: Vec<KeyState>,

    layout: piano_layout::KeyboardLayout,

    cache: Vec<QuadInstance>,
    text_cache: Vec<super::text::TextArea>,
}

impl KeyboardRenderer {
    pub fn new(layout: piano_layout::KeyboardLayout) -> Self {
        let key_states: Vec<KeyState> = layout
            .range
            .iter()
            .map(|id| KeyState::new(id.is_black()))
            .collect();

        let cache = Vec::with_capacity(key_states.len() + 1);

        Self {
            pos: Default::default(),

            key_states,

            layout,
            cache,
            text_cache: Vec::new(),
        }
    }

    pub fn reset_notes(&mut self) {
        for key in self.key_states.iter_mut() {
            key.pressed_by_file_off();
        }
        self.invalidate_cache();
    }

    pub fn range(&self) -> &KeyboardRange {
        &self.layout.range
    }

    pub fn key_states(&self) -> &[KeyState] {
        &self.key_states
    }

    pub fn key_states_mut(&mut self) -> &mut [KeyState] {
        &mut self.key_states
    }

    pub fn pos(&self) -> &Point<f32> {
        &self.pos
    }

    pub fn position_on_bottom_of_parent(&mut self, parent_height: f32) {
        let h = self.layout.height;
        let y = parent_height - h;

        self.set_pos((0.0, y).into());
    }

    pub fn set_pos(&mut self, pos: Point<f32>) {
        self.pos = pos;
        self.invalidate_cache();
    }

    pub fn layout(&self) -> &piano_layout::KeyboardLayout {
        &self.layout
    }

    pub fn set_layout(&mut self, layout: piano_layout::KeyboardLayout) {
        self.layout = layout;
        self.invalidate_cache();
    }

    pub fn invalidate_cache(&mut self) {
        self.cache.clear();
        self.text_cache.clear();
    }

    /// Reupload instances to GPU
    #[profiling::function]
    fn rebuild_quad_cache(&mut self) {
        let instances = &mut self.cache;

        instances.push(QuadInstance {
            position: [self.pos.x, self.pos.y - 4.0],
            size: [self.layout.width, 4.0],
            color: [0.25, 0.02, 0.02, 1.0],
            ..Default::default()
        });

        // black_background
        instances.push(QuadInstance {
            position: self.pos.into(),
            size: [self.layout.width, self.layout.height],
            color: [0.0, 0.0, 0.0, 1.0],
            ..Default::default()
        });

        for key in self
            .layout
            .keys
            .iter()
            .filter(|key| key.kind().is_neutral())
        {
            let id = key.id();
            let color = self.key_states[id].color();

            instances.push(key_state::to_quad(key, color, self.pos));
        }

        // Rainbow note-guide squares on the white keys.
        // A small square (~70% of the key width) sits in the lower playing area
        // of each white key, below where the black keys reach.
        for key in self
            .layout
            .keys
            .iter()
            .filter(|key| key.kind().is_neutral())
        {
            let Some((color, x, y, side)) = rainbow_square(key, self.pos) else {
                continue;
            };

            let r = side * 0.15;

            instances.push(QuadInstance {
                position: [x, y],
                size: [side, side],
                color: color.into_linear_rgba(),
                border_radius: [r, r, r, r],
            });
        }

        for key in self.layout.keys.iter().filter(|key| key.kind().is_sharp()) {
            let id = key.id();
            let color = self.key_states[id].color();

            instances.push(key_state::to_quad(key, color, self.pos));
        }
    }

    #[profiling::function]
    fn rebuild_text_cache(&mut self) {
        let font_system = crate::font_system::font_system();
        let font_system = &mut font_system.borrow_mut();

        // (The stock grey C4/C5/... octave markers used to be drawn here —
        // the rainbow squares have superseded them.)

        // Small white note name near the bottom of each black key ("A#").
        for key in self.layout.keys.iter().filter(|key| key.kind().is_sharp()) {
            let Some(name) = sharp_name(key.note_id()) else {
                continue;
            };

            let x = self.pos.x + key.x();
            let y = self.pos.y;
            let w = key.width();
            let h = key.height();

            let font_size = w * 0.42;

            let mut buffer =
                glyphon::Buffer::new(font_system, glyphon::Metrics::new(font_size, font_size));
            buffer.set_size(Some(w), Some(h));
            buffer.set_wrap(glyphon::Wrap::None);
            buffer.set_text(
                name,
                &glyphon::Attrs::new().family(glyphon::Family::SansSerif),
                glyphon::Shaping::Basic,
                Some(glyphon::cosmic_text::Align::Center),
            );
            buffer.shape_until_scroll(font_system, false);

            self.text_cache.push(super::text::TextArea {
                buffer,
                left: x,
                top: y + h - font_size * 1.6,
                scale: 1.0,
                bounds: glyphon::TextBounds {
                    left: x.round() as i32,
                    top: y.round() as i32,
                    right: x.round() as i32 + w.round() as i32,
                    bottom: y.round() as i32 + h.round() as i32,
                },
                default_color: glyphon::Color::rgba(255, 255, 255, 225),
            });
        }

        // Note letter centered inside each rainbow square. White reads best on
        // most of the squares; the light yellow (E) and pink (B) get dark text.
        for key in self.layout.keys.iter().filter(|key| key.kind().is_neutral()) {
            let Some((_, sx, sy, side)) = rainbow_square(key, self.pos) else {
                continue;
            };
            let Some(letter) = rainbow_letter(key.note_id()) else {
                continue;
            };

            let font_size = side * 0.72;

            let mut buffer =
                glyphon::Buffer::new(font_system, glyphon::Metrics::new(font_size, font_size));
            buffer.set_size(Some(side), Some(side));
            buffer.set_wrap(glyphon::Wrap::None);
            buffer.set_text(
                letter,
                &glyphon::Attrs::new().family(glyphon::Family::SansSerif),
                glyphon::Shaping::Basic,
                Some(glyphon::cosmic_text::Align::Center),
            );
            buffer.shape_until_scroll(font_system, false);

            let text_color = match key.note_id() {
                4 | 11 => glyphon::Color::rgba(20, 20, 20, 255), // E (yellow), B (pink)
                _ => glyphon::Color::rgba(255, 255, 255, 255),
            };

            self.text_cache.push(super::text::TextArea {
                buffer,
                left: sx,
                top: sy + (side - font_size) / 2.0 - font_size * 0.08,
                scale: 1.0,
                bounds: glyphon::TextBounds {
                    left: sx.round() as i32,
                    top: sy.round() as i32,
                    right: sx.round() as i32 + side.round() as i32,
                    bottom: sy.round() as i32 + side.round() as i32,
                },
                default_color: text_color,
            });
        }
    }

    #[profiling::function]
    pub fn update(&mut self, quads: &mut QuadRenderer, text: &mut TextRenderer) {
        if self.cache.is_empty() {
            self.rebuild_quad_cache();
        }

        if self.text_cache.is_empty() {
            self.rebuild_text_cache();
        }

        {
            profiling::scope!("push quads from cache");
            quads.layer().extend(&self.cache);
        }

        {
            profiling::scope!("push text from cache");
            text.queue_mut().extend_from_slice(&self.text_cache);
        }
    }
}
