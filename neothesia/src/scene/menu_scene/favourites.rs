//! Favourites page: lists the .mid files in `~/Music/MIDI/Favourites` so a
//! practice song is two clicks away. The folder is rescanned every time the
//! page is opened and the list scrolls, so dropping more files in later just
//! works.

use std::path::PathBuf;

use crate::{context::Context, song::Song};

use super::{icons, neo_btn_icon, state::Page};

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

/// One row in the favourites list. Returns true when clicked.
fn song_row(ui: &mut nuon::Ui, w: f32, h: f32, id: &PathBuf, name: &str) -> bool {
    let event = nuon::click_area(nuon::Id::hash(id)).size(w, h).build(ui);

    let (bg, accent) = if event.is_hovered() || event.is_pressed() {
        (
            nuon::Color::new_u8(9, 9, 9, 0.6),
            nuon::Color::new_u8(56, 145, 255, 1.0),
        )
    } else {
        (
            nuon::Color::new_u8(17, 17, 17, 0.6),
            nuon::Color::new_u8(160, 81, 255, 1.0),
        )
    };

    nuon::quad()
        .size(w, h)
        .color(bg)
        .border_radius([7.0; 4])
        .build(ui);
    // Slim accent stripe on the left, echoing the main menu buttons.
    nuon::quad()
        .size(5.0, h)
        .color(accent)
        .border_radius([7.0, 0.0, 0.0, 7.0])
        .build(ui);

    nuon::label()
        .text(name)
        .font_size(20.0)
        .text_justify(nuon::TextJustify::Left)
        .x(18.0)
        .size(w - 36.0, h)
        .build(ui);

    event.is_clicked()
}

impl super::MenuScene {
    pub fn favourites_page_ui(&mut self, ctx: &mut Context, ui: &mut nuon::Ui) {
        let win_w = ctx.window_state.logical_size.width;
        let win_h = ctx.window_state.logical_size.height;
        let bottom_bar_h = 60.0;

        // Bottom bar: back button, like the tracks page.
        nuon::translate().x(0.0).y(win_h).build(ui, |ui| {
            nuon::translate().y(-10.0).add_to_current(ui);
            nuon::translate().y(-bottom_bar_h).add_to_current(ui);

            nuon::translate().x(10.0).add_to_current(ui);
            if neo_btn_icon(ui, 80.0, bottom_bar_h, icons::left_arrow_icon()) {
                self.state.go_back();
            }
        });

        let mut clicked: Option<PathBuf> = None;

        self.favourites_scroll = nuon::scroll()
            .scissor_size(win_w, (win_h - bottom_bar_h - 20.0).max(0.0))
            .scroll(self.favourites_scroll)
            .build(ui, |ui| {
                nuon::translate().y(30.0).add_to_current(ui);

                nuon::label()
                    .text("Favourites")
                    .font_size(30.0)
                    .size(win_w, 34.0)
                    .build(ui);
                nuon::label()
                    .text("~/Music/MIDI/Favourites")
                    .font_size(13.0)
                    .color(nuon::Color::new_u8(150, 150, 150, 1.0))
                    .y(38.0)
                    .size(win_w, 14.0)
                    .build(ui);

                nuon::translate().y(80.0).add_to_current(ui);

                if self.favourites.is_empty() {
                    nuon::label()
                        .text("No .mid files found — drop some into the folder!")
                        .font_size(18.0)
                        .color(nuon::Color::new_u8(170, 170, 170, 1.0))
                        .size(win_w, 20.0)
                        .build(ui);
                    return;
                }

                let item_w = 560.0f32.min(win_w - 40.0);
                let item_h = 52.0;
                let gap = 8.0;

                nuon::translate()
                    .x(nuon::center_x(win_w, item_w))
                    .build(ui, |ui| {
                        for path in &self.favourites {
                            let name = path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("(unreadable name)");

                            if song_row(ui, item_w, item_h, path, name) {
                                clicked = Some(path.clone());
                            }

                            nuon::translate().y(item_h + gap).add_to_current(ui);
                        }
                    });
            });

        if let Some(path) = clicked {
            match midi_file::MidiFile::new(&path) {
                Ok(midi) => {
                    ctx.config.set_last_opened_song(Some(path));
                    self.state.song = Some(Song::new(midi));
                    // Back to the main page with the song loaded, ready to play.
                    self.state.go_back();
                }
                Err(e) => {
                    log::error!("failed to load favourite: {e}");
                }
            }
        }
    }

    pub fn open_favourites(&mut self) {
        // Rescan on every open so newly added files show up.
        self.favourites = scan_favourites();
        self.favourites_scroll = nuon::ScrollState::new();
        self.state.go_to(Page::Favourites);
    }
}
