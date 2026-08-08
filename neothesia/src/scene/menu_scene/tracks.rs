//! Performer controls on the main menu: who plays the song, and which of its
//! tracks are heard and drawn.
//!
//! These used to be a page of their own, reached by a Tracks button, where each
//! track carried its own Mute/Auto/Human choice. Who performs is one decision
//! about the song, not a per-track one — the in-game toggle always treated it
//! that way — so it is a single three-way selector now, the same one the
//! playing scene shows, sitting where the song is picked. What is genuinely
//! per-track is what remains beside each one: whether it is muted, and whether
//! it is drawn in the waterfall.

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
/// Rows listed, so a 16-track arrangement cannot push the menu off the screen.
/// The rest are counted in a line underneath rather than scrolled to: a scroll
/// container here swallows clicks on its own children, because its clip rect is
/// only positioned correctly when built from the window origin.
const MAX_ROWS: usize = 6;
/// The "+N more" line, when a song has more tracks than that.
const MORE_H: f32 = 18.0;

const MUTE_W: f32 = 74.0;
const DOT: f32 = 22.0;

impl super::MenuScene {
    /// Tracks worth listing: the ones with notes in them.
    fn listed_tracks(&self) -> usize {
        self.state
            .song()
            .map(|song| song.file.tracks.iter().filter(|t| !t.notes.is_empty()).count())
            .unwrap_or(0)
    }

    /// Vertical room [`Self::track_list_ui`] needs, so the menu can give the
    /// favourites list whatever is left rather than overflowing. Zero when
    /// there is no song loaded to list.
    pub fn track_list_height(&self) -> f32 {
        let listed = self.listed_tracks();
        let rows = listed.min(MAX_ROWS);
        if rows == 0 {
            return 0.0;
        }

        let mut h = rows as f32 * (ROW_H + ROW_GAP) - ROW_GAP;
        if listed > MAX_ROWS {
            h += ROW_GAP + MORE_H;
        }
        h
    }

    /// Draw the track rows at the current origin, `w` wide.
    pub fn track_list_ui(&mut self, ctx: &mut Context, ui: &mut nuon::Ui, w: f32) {
        if self.listed_tracks() == 0 {
            return;
        }

        self.track_rows_ui(ctx, ui, w);
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
        let mode = ctx.perform_mode;
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

        match ev {
            RowEvent::ToggleVisible => {
                let config = &mut song.config.tracks[track_id];
                config.visible = !config.visible;
            }
            RowEvent::ToggleMute => {
                if song.config.tracks[track_id].player == PlayerConfig::Mute {
                    // Back in: which of Auto or Human it lands on is the
                    // performer mode's call, not this button's.
                    song.config.tracks[track_id].player = PlayerConfig::Auto;
                    song.set_mode(mode);
                } else {
                    song.config.tracks[track_id].player = PlayerConfig::Mute;
                }
            }
        }
    }
}

enum RowEvent {
    ToggleVisible,
    ToggleMute,
}

/// One track: a coloured dot that shows and toggles whether the track is drawn,
/// its name and note count, and a Mute button.
fn track_row(
    ctx: &Context,
    ui: &mut nuon::Ui,
    w: f32,
    row: usize,
    track: &MidiTrack,
    config: &TrackConfig,
) -> Option<RowEvent> {
    let muted = config.player == PlayerConfig::Mute;

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

    let label_x = pad + DOT + 10.0;
    nuon::label()
        .pos(label_x, 0.0)
        .size(w - label_x - MUTE_W - pad * 2.0, ROW_H)
        .text(format!("{title}  ·  {} notes", track.notes.len()))
        .text_justify(TextJustify::Left)
        .font_size(14.0)
        .color(if muted {
            nuon::Color::new_u8(130, 130, 130, 1.0)
        } else {
            nuon::Color::new_u8(235, 235, 235, 1.0)
        })
        .build(ui);

    let mute_h = ROW_H - 10.0;
    if nuon::button()
        .id(nuon::Id::hash_with(|h| {
            "track_mute".hash(h);
            row.hash(h);
        }))
        .pos(w - MUTE_W - pad, (ROW_H - mute_h) / 2.0)
        .size(MUTE_W, mute_h)
        .color(if muted {
            nuon::Color::new_u8(150, 60, 60, 1.0)
        } else {
            nuon::Color::new_u8(74, 68, 88, 1.0)
        })
        .hover_color(if muted {
            nuon::Color::new_u8(175, 75, 75, 1.0)
        } else {
            nuon::Color::new_u8(87, 81, 101, 1.0)
        })
        .border_radius([6.0; 4])
        .label("Mute")
        .build(ui)
    {
        res = Some(RowEvent::ToggleMute);
    }

    res
}
