//! Inline favourites list on the main page.
//!
//! The .mid files in `~/Music/MIDI/Favourites` are listed right under the
//! main menu buttons with one row highlighted. Arrow up/down moves the
//! highlight and loads that song on the spot; Enter then starts it (the main
//! page's existing play binding). Clicking a row does the same as
//! highlighting it. The folder is rescanned whenever the menu is (re)opened,
//! and the list shows however many rows fit — with the highlight kept in
//! view — so it stays tidy no matter how many files get added.

use std::path::PathBuf;

use crate::{context::Context, song::Song};

pub const ROW_H: f32 = 30.0;
pub const ROW_GAP: f32 = 4.0;
const CAPTION_H: f32 = 18.0;
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
    /// button). `list_top` is that origin's absolute y, used to work out how
    /// many rows fit above the bottom bar.
    pub fn favourites_list_ui(
        &mut self,
        ctx: &mut Context,
        ui: &mut nuon::Ui,
        w: f32,
        list_top: f32,
        win_h: f32,
    ) {
        let count = self.favourites.len();

        let caption = if count == 0 {
            "FAVOURITES — none in ~/Music/MIDI/Favourites".to_string()
        } else {
            format!("FAVOURITES — {count} songs · arrows + Enter")
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
            return;
        }

        nuon::translate().y(CAPTION_H).add_to_current(ui);

        // However many rows fit between the caption and the bottom bar.
        let avail = win_h - BOTTOM_RESERVED - list_top - CAPTION_H;
        let visible = (((avail + ROW_GAP) / (ROW_H + ROW_GAP)).floor().max(1.0) as usize).min(count);

        // Keep the highlighted row inside the visible window.
        if self.fav_selected >= count {
            self.fav_selected = count - 1;
        }
        if self.fav_selected < self.fav_scroll_top {
            self.fav_scroll_top = self.fav_selected;
        }
        if self.fav_selected >= self.fav_scroll_top + visible {
            self.fav_scroll_top = self.fav_selected + 1 - visible;
        }
        self.fav_scroll_top = self.fav_scroll_top.min(count - visible);

        let mut clicked: Option<usize> = None;
        let end = (self.fav_scroll_top + visible).min(count);

        for idx in self.fav_scroll_top..end {
            let path = &self.favourites[idx];
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("(unreadable name)");
            let selected = idx == self.fav_selected;

            let event = nuon::click_area(nuon::Id::hash(path)).size(w, ROW_H).build(ui);
            if event.is_clicked() {
                clicked = Some(idx);
            }
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

            nuon::translate().y(ROW_H + ROW_GAP).add_to_current(ui);
        }

        if let Some(idx) = clicked {
            self.fav_selected = idx;
            self.load_selected_favourite(ctx);
        }
    }

    /// Arrow-key navigation: move the highlight and load that song.
    pub fn favourites_move(&mut self, ctx: &mut Context, delta: i32) {
        if self.favourites.is_empty() {
            return;
        }
        let len = self.favourites.len() as i32;
        let cur = self.fav_selected as i32;
        let next = (cur + delta).clamp(0, len - 1);

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
