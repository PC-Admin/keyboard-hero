//! Inline favourites list on the main page.
//!
//! The .mid files in `~/Music/MIDI/Favourites` are listed right under the
//! main menu buttons with one row highlighted. Arrow up/down moves the
//! highlight and loads that song on the spot; Enter then starts it (the main
//! page's existing play binding). Clicking a row does the same as
//! highlighting it, and double-clicking one starts it straight away.
//! The folder is rescanned whenever the menu is (re)opened.
//!
//! The list fills the space between the menu and the bottom bar and scrolls
//! within it, so it stays tidy no matter how many files get added: the wheel
//! scrolls it freely while the pointer is over it, a draggable bar appears on
//! the right once there is more than fits, and moving the highlight with the
//! arrow keys scrolls it back into view.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{context::Context, song::Song};

pub const ROW_H: f32 = 30.0;
pub const ROW_GAP: f32 = 4.0;
/// Pitch of one row: what the scroll offset advances by per entry.
const ROW_PITCH: f32 = ROW_H + ROW_GAP;
const CAPTION_H: f32 = 18.0;
/// Width nuon's scroll widget draws its bar at, plus breathing room, kept
/// clear on the right of each row so text never runs under the bar.
const SCROLLBAR_W: f32 = 14.0;
/// How close together two clicks on the same row have to be to count as a
/// double-click and start the song.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Space kept free for the bottom bar (song title + play/freeplay buttons).
const BOTTOM_RESERVED: f32 = 92.0;

/// Collect and alphabetically sort the .mid/.midi files in the favourites dir.
pub fn scan_favourites() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dir = PathBuf::from(home).join("Music/MIDI/Favourites");

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("mid") || e.eq_ignore_ascii_case("midi"))
                .unwrap_or(false)
        })
        .collect();

    files.sort_by_cached_key(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default()
    });

    files
}

impl super::MenuScene {
    /// Draw the list at the current translate origin (directly under the Exit
    /// button). `list_x`/`list_top` are that origin in absolute window
    /// coordinates, which the wheel handler needs to hit-test the pointer
    /// against the list, and which decide whether the window still has room
    /// to show it at all.
    pub fn favourites_list_ui(
        &mut self,
        ctx: &mut Context,
        ui: &mut nuon::Ui,
        w: f32,
        list_x: f32,
        list_top: f32,
        win_w: f32,
        win_h: f32,
    ) {
        let count = self.favourites.len();

        let caption = if count == 0 {
            "FAVOURITES — none in ~/Music/MIDI/Favourites".to_string()
        } else {
            format!("FAVOURITES — {count} songs · arrows + Enter · double-click to play")
        };
        nuon::label()
            .text(caption)
            .font_size(11.0)
            .color(nuon::Color::new_u8(150, 150, 150, 1.0))
            .text_justify(nuon::TextJustify::Left)
            .x(4.0)
            .size(w - 8.0, 12.0)
            .build(ui);

        if count == 0 {
            self.fav_viewport = None;
            return;
        }

        nuon::translate().y(CAPTION_H).add_to_current(ui);

        if self.fav_selected >= count {
            self.fav_selected = count - 1;
        }

        // The list gets everything between the caption and the bottom bar.
        let view_top = list_top + CAPTION_H;
        let view_h = win_h - BOTTOM_RESERVED - view_top;

        // A scissor rect hanging off the window is a fatal wgpu validation
        // error, not a clipped draw, so when the window is too small to hold
        // the list, drop it rather than clamp it to something that overflows.
        // Nothing is lost: at that size the menu buttons above have already
        // run out of room, and the arrow keys still work.
        if view_h < ROW_H || view_top < 0.0 || list_x < 0.0 || list_x + w > win_w {
            self.fav_viewport = None;
            return;
        }

        // Two rects, and they are not interchangeable: nuon translates a
        // layer's scissor origin by the current translation, so the widget
        // wants it local (the origin here *is* the list's top-left), while
        // the wheel hit-test compares against an absolute pointer position.
        let clip = nuon::Rect::new(nuon::Point::zero(), nuon::Size::new(w, view_h));
        self.fav_viewport = Some(nuon::Rect::new(
            nuon::Point::new(list_x, view_top),
            nuon::Size::new(w, view_h),
        ));

        // Anything past the viewport is reachable by scrolling, so the bar
        // only shows up when there is actually something to scroll to.
        let content_h = count as f32 * ROW_PITCH;
        let scrollable = content_h > view_h;
        let row_w = if scrollable { w - SCROLLBAR_W } else { w };

        // A pending arrow-key move is applied here rather than at the key
        // event, because that is where the viewport height is known.
        self.fav_scroll.set_max((content_h - view_h).max(0.0));
        if std::mem::take(&mut self.fav_reveal) {
            let row_top = self.fav_selected as f32 * ROW_PITCH;
            let mut v = self.fav_scroll.value();
            v = v.min(row_top);
            v = v.max(row_top + ROW_H - view_h);
            self.fav_scroll.set_value(v);
        }

        let scroll = self.fav_scroll.value();
        let first = (scroll / ROW_PITCH).floor().max(0.0) as usize;
        let last = (((scroll + view_h) / ROW_PITCH).ceil() as usize).min(count);

        let mut clicked: Option<usize> = None;
        let favourites = &self.favourites;
        let selected = self.fav_selected;

        let scrolled = nuon::scroll()
            .scissor_rect(clip)
            .scroll(self.fav_scroll)
            .build(ui, |ui| {
                // Every row advances the origin so the widget measures the
                // full content height, but only the ones on screen are drawn.
                for idx in 0..count {
                    if (first..last).contains(&idx)
                        && Self::favourite_row_ui(ui, &favourites[idx], row_w, idx == selected)
                    {
                        clicked = Some(idx);
                    }
                    nuon::translate().y(ROW_PITCH).add_to_current(ui);
                }
            });
        self.fav_scroll = scrolled;

        if let Some(idx) = clicked {
            let now = Instant::now();
            // Taking it means a third click starts a fresh pair rather than
            // counting as another double.
            let double = self
                .fav_last_click
                .take()
                .is_some_and(|(prev, at)| prev == idx && now.duration_since(at) <= DOUBLE_CLICK);

            if idx != self.fav_selected || self.state.song.is_none() {
                self.fav_selected = idx;
                self.load_selected_favourite(ctx);
            }

            if double {
                super::state::play(&self.state, ctx);
            } else {
                self.fav_last_click = Some((idx, now));
            }
        }
    }

    /// One row of the list, drawn at the current origin. Returns whether it
    /// was clicked this frame.
    fn favourite_row_ui(ui: &mut nuon::Ui, path: &PathBuf, w: f32, selected: bool) -> bool {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("(unreadable name)");

        let event = nuon::click_area(nuon::Id::hash(path)).size(w, ROW_H).build(ui);
        let hovered = event.is_hovered() || event.is_pressed();

        let bg = if selected {
            nuon::Color::new_u8(56, 145, 255, 0.85)
        } else if hovered {
            nuon::Color::new_u8(60, 60, 60, 0.6)
        } else {
            nuon::Color::new_u8(17, 17, 17, 0.55)
        };
        nuon::quad()
            .size(w, ROW_H)
            .color(bg)
            .border_radius([5.0; 4])
            .build(ui);

        if selected {
            nuon::quad()
                .size(5.0, ROW_H)
                .color(nuon::Color::new_u8(160, 81, 255, 1.0))
                .border_radius([5.0, 0.0, 0.0, 5.0])
                .build(ui);
        }

        let text_color = if selected {
            nuon::Color::new_u8(255, 255, 255, 1.0)
        } else {
            nuon::Color::new_u8(205, 205, 205, 1.0)
        };
        nuon::label()
            .text(name)
            .font_size(16.0)
            .color(text_color)
            .text_justify(nuon::TextJustify::Left)
            .x(14.0)
            .size(w - 28.0, ROW_H)
            .build(ui);

        event.is_clicked()
    }

    /// Wheel over the list scrolls it, leaving the highlight where it is.
    /// Returns whether the pointer was over the list at all.
    pub fn favourites_scroll(&mut self, cursor: nuon::Point, amount: f32) -> bool {
        let over = self
            .fav_viewport
            .is_some_and(|viewport| viewport.contains(cursor));
        if over {
            self.fav_scroll.update(amount);
        }
        over
    }

    /// Arrow-key navigation: move the highlight and load that song.
    pub fn favourites_move(&mut self, ctx: &mut Context, delta: i32) {
        if self.favourites.is_empty() {
            return;
        }
        let len = self.favourites.len() as i32;
        let cur = self.fav_selected as i32;
        let next = (cur + delta).clamp(0, len - 1);

        // Scroll the highlight back into view even when it did not move —
        // otherwise an arrow press after wheeling away appears to do nothing.
        self.fav_reveal = true;

        if next != cur || self.state.song.is_none() {
            self.fav_selected = next as usize;
            self.load_selected_favourite(ctx);
        }
    }

    pub fn load_selected_favourite(&mut self, ctx: &mut Context) {
        let Some(path) = self.favourites.get(self.fav_selected) else {
            return;
        };

        match midi_file::MidiFile::new(path) {
            Ok(midi) => {
                ctx.config.set_last_opened_song(Some(path.clone()));
                self.state.song = Some(Song::new(midi));
            }
            Err(e) => {
                log::error!("failed to load favourite: {e}");
            }
        }
    }
}
