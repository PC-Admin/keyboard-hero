//! Performer controls on the main menu: how the song is performed, and which of
//! its parts are yours.
//!
//! These used to be a page of their own, reached by a Tracks button, where each
//! track carried a Mute/Auto/Human choice of its own. *How* the song is
//! performed is one decision about the song, not a per-track one — the in-game
//! toggle always treated it that way — so that is a single three-way selector
//! now, the same one the playing scene shows, in the same corner.
//!
//! What is genuinely per-track is listed top-left, a row each: hand a part to
//! **Auto** and the app performs it while you keep the rest (which is how you
//! practise one hand at a time), **Mute** it to silence it, or click its colour
//! to stop it being drawn in the waterfall. A row with neither button lit is
//! yours to play.

use midi_file::MidiTrack;
use nuon::TextJustify;
use std::hash::Hash;

use crate::{
    context::Context,
    song::{PlayerConfig, TrackConfig},
};

/// Where the performer selector hangs from. The playing scene uses the same
/// inset below its (animating) top bar.
const SEG_TOP: f32 = 10.0;
/// One track's row.
const ROW_H: f32 = 38.0;
const ROW_GAP: f32 = 6.0;
/// Rows listed, so a 16-track arrangement cannot run off the screen. The rest
/// are counted in a line underneath rather than scrolled to — worth knowing if
/// you add a scroll here: `Scroll::scissor_size` leaves the clip rect's origin
/// at zero and `Layer::build` then offsets it by the current translation, so the
/// rect only lands where you meant it to if you build from that origin.
const MAX_ROWS: usize = 6;
/// The "+N more" line, when a song has more tracks than that.
const MORE_H: f32 = 18.0;

/// The two per-track buttons.
const BTN_W: f32 = 60.0;
const BTN_GAP: f32 = 4.0;
const DOT: f32 = 22.0;
/// Row width, and the inset of the whole list from the top-left corner. Lines
/// up with the performer selector's inset in the opposite corner.
/// Wide enough for the longest instrument name and its note count beside two
/// buttons — "Acoustic Grand Piano · 1931 notes" is about as long as it gets.
const LIST_W: f32 = 430.0;
const LIST_MARGIN: f32 = 16.0;
const LIST_TOP: f32 = 10.0;
/// Caption above the rows.
const CAPTION_H: f32 = 20.0;

impl super::MenuScene {
    /// Tracks worth listing: the ones with notes in them.
    fn listed_tracks(&self) -> usize {
        self.state
            .song()
            .map(|song| song.file.tracks.iter().filter(|t| !t.notes.is_empty()).count())
            .unwrap_or(0)
    }

    /// The song's parts, listed in the top-left corner — out of the way of the
    /// centred menu, and mirroring the performer selector opposite it.
    pub fn track_list_ui(&mut self, ctx: &mut Context, ui: &mut nuon::Ui) {
        if self.listed_tracks() == 0 {
            return;
        }

        nuon::translate()
            .x(LIST_MARGIN)
            .y(LIST_TOP)
            .build(ui, |ui| {
                nuon::label()
                    .size(LIST_W, CAPTION_H)
                    .text("PARTS")
                    .text_justify(TextJustify::Left)
                    .font_size(12.0)
                    .color(nuon::Color::new_u8(150, 150, 150, 1.0))
                    .build(ui);

                nuon::translate()
                    .y(CAPTION_H)
                    .build(ui, |ui| self.track_rows_ui(ctx, ui, LIST_W));
            });
    }

    /// HERO / AUTO / HUMAN, top-right — the same control the playing scene
    /// draws, from the same function, so the two cannot look or sit
    /// differently. There is no top bar to ride down here, so it hangs from the
    /// window's top edge.
    pub fn performer_selector_ui(&mut self, ctx: &mut Context, ui: &mut nuon::Ui) {
        let win_w = ctx.window_state.logical_size.width;

        if let Some(mode) =
            crate::scene::performer_selector(ui, win_w, SEG_TOP, ctx.perform_mode)
        {
            ctx.perform_mode = mode;
            // The song already loaded was assigned for the old mode, so bring
            // it along rather than waiting for a reload.
            if let Some(song) = self.state.song.as_mut() {
                song.set_mode(mode);
            }
        }
    }

    fn track_rows_ui(&mut self, ctx: &mut Context, ui: &mut nuon::Ui, w: f32) {
        let listed = self.listed_tracks();

        // Applied after the walk: the rows borrow the song to draw themselves.
        let mut event = None;

        if let Some(song) = self.state.song.as_ref() {
            for (row, track) in song
                .file
                .tracks
                .iter()
                .filter(|t| !t.notes.is_empty())
                .take(MAX_ROWS)
                .enumerate()
            {
                let config = &song.config.tracks[track.track_id];

                nuon::translate()
                    .y(row as f32 * (ROW_H + ROW_GAP))
                    .build(ui, |ui| {
                        if let Some(ev) = track_row(ctx, ui, w, row, track, config) {
                            event = Some((track.track_id, ev));
                        }
                    });
            }

            // Say what is not being shown, rather than quietly cutting it off.
            if listed > MAX_ROWS {
                nuon::label()
                    .pos(0.0, MAX_ROWS as f32 * (ROW_H + ROW_GAP))
                    .size(w, MORE_H)
                    .text(format!("+{} more tracks", listed - MAX_ROWS))
                    .font_size(12.0)
                    .color(nuon::Color::new_u8(140, 140, 140, 1.0))
                    .build(ui);
            }
        }

        let Some((track_id, ev)) = event else {
            return;
        };
        let Some(song) = self.state.song.as_mut() else {
            return;
        };

        let player = &mut song.config.tracks[track_id].player;

        match ev {
            RowEvent::ToggleVisible => {
                let config = &mut song.config.tracks[track_id];
                config.visible = !config.visible;
            }
            // Both buttons toggle back to "mine", so one click hands a part
            // over and another takes it back.
            RowEvent::ToggleAuto => {
                *player = if *player == PlayerConfig::Auto {
                    PlayerConfig::Human
                } else {
                    PlayerConfig::Auto
                };
            }
            RowEvent::ToggleMute => {
                *player = if *player == PlayerConfig::Mute {
                    PlayerConfig::Human
                } else {
                    PlayerConfig::Mute
                };
            }
        }
    }
}

enum RowEvent {
    ToggleVisible,
    ToggleAuto,
    ToggleMute,
}

/// One track: a coloured dot that shows and toggles whether it is drawn, its
/// instrument and note count (the note count being how you tell two tracks of
/// the same instrument apart — left hand from right), and the Auto and Mute
/// buttons. Neither lit means the part is the player's.
fn track_row(
    ctx: &Context,
    ui: &mut nuon::Ui,
    w: f32,
    row: usize,
    track: &MidiTrack,
    config: &TrackConfig,
) -> Option<RowEvent> {
    let muted = config.player == PlayerConfig::Mute;
    let auto = config.player == PlayerConfig::Auto;

    let track_color = if !config.visible {
        nuon::Color::new_u8(102, 102, 102, 1.0)
    } else {
        let color_id = track.track_color_id % ctx.config.color_schema().len();
        let color = &ctx.config.color_schema()[color_id].base;
        nuon::Color::new_u8(color.0, color.1, color.2, 1.0)
    };

    let title = if track.has_drums && !track.has_other_than_drums {
        "Percussion"
    } else {
        let instrument_id = track
            .programs
            .last()
            .map(|p| p.program as usize)
            .unwrap_or(0);
        midi_file::INSTRUMENT_NAMES[instrument_id]
    };

    nuon::quad()
        .size(w, ROW_H)
        .color([37, 35, 42])
        .border_radius([8.0; 4])
        .build(ui);

    let mut res = None;
    let pad = 10.0;

    if nuon::button()
        .id(nuon::Id::hash_with(|h| {
            "track_visible".hash(h);
            row.hash(h);
        }))
        .pos(pad, (ROW_H - DOT) / 2.0)
        .size(DOT, DOT)
        .color(track_color)
        .hover_color(nuon::Color::new(
            (track_color.r + 0.05).min(1.0),
            (track_color.g + 0.05).min(1.0),
            (track_color.b + 0.05).min(1.0),
            1.0,
        ))
        .preseed_color(track_color)
        .border_radius([255.0; 4])
        .build(ui)
    {
        res = Some(RowEvent::ToggleVisible);
    }

    let buttons_w = BTN_W * 2.0 + BTN_GAP;
    let label_x = pad + DOT + 8.0;
    nuon::label()
        .pos(label_x, 0.0)
        .size((w - label_x - buttons_w - pad * 2.0).max(0.0), ROW_H)
        .text(format!("{title} · {} notes", track.notes.len()))
        .text_justify(TextJustify::Left)
        .font_size(13.0)
        .color(if muted {
            nuon::Color::new_u8(120, 120, 120, 1.0)
        } else {
            nuon::Color::new_u8(235, 235, 235, 1.0)
        })
        .build(ui);

    let btn_h = ROW_H - 10.0;
    let btn_y = (ROW_H - btn_h) / 2.0;
    let buttons_x = w - buttons_w - pad;

    // Off = a plain slot, on = the state is doing something, so it lights up.
    let idle = nuon::Color::new_u8(58, 54, 68, 1.0);
    let idle_hover = nuon::Color::new_u8(78, 73, 92, 1.0);

    if nuon::button()
        .id(nuon::Id::hash_with(|h| {
            "track_auto".hash(h);
            row.hash(h);
        }))
        .pos(buttons_x, btn_y)
        .size(BTN_W, btn_h)
        .color(if auto {
            nuon::Color::new_u8(70, 110, 190, 1.0)
        } else {
            idle
        })
        .hover_color(if auto {
            nuon::Color::new_u8(90, 133, 220, 1.0)
        } else {
            idle_hover
        })
        .border_radius([6.0, 0.0, 0.0, 6.0])
        .label("Auto")
        .build(ui)
    {
        res = Some(RowEvent::ToggleAuto);
    }

    if nuon::button()
        .id(nuon::Id::hash_with(|h| {
            "track_mute".hash(h);
            row.hash(h);
        }))
        .pos(buttons_x + BTN_W + BTN_GAP, btn_y)
        .size(BTN_W, btn_h)
        .color(if muted {
            nuon::Color::new_u8(150, 60, 60, 1.0)
        } else {
            idle
        })
        .hover_color(if muted {
            nuon::Color::new_u8(175, 75, 75, 1.0)
        } else {
            idle_hover
        })
        .border_radius([0.0, 6.0, 6.0, 0.0])
        .label("Mute")
        .build(ui)
    {
        res = Some(RowEvent::ToggleMute);
    }

    res
}
