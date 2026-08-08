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
//!
//! Under that list, in a row built to match, is microphone passthrough: sing
//! into the mic and hear yourself out of the same speakers as the piano. It is
//! nothing to do with the song, which is why it sits under a caption of its
//! own; the audio side lives in [`crate::microphone`], and it stays on into the
//! playing scene, which is the only reason you would switch it on from here.

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
/// Between the parts and the microphone row under them — enough that the mic
/// does not read as a seventh part.
const SECTION_GAP: f32 = 14.0;
/// Room kept at the right of the microphone row for its ON/OFF word.
const STATUS_W: f32 = 90.0;
/// The level meter beside it.
const METER_W: f32 = 90.0;
const METER_H: f32 = 6.0;
/// Quietest level the meter draws anything for. Below roughly this a microphone
/// is sending its own noise and nothing else, so an empty bar is the honest
/// reading — and the scale is in decibels because that is the range a gain dial
/// moves through, and a linear bar would sit flat across most of its travel.
const METER_FLOOR_DB: f32 = -60.0;

impl super::MenuScene {
    /// Tracks worth listing: the ones with notes in them.
    fn listed_tracks(&self) -> usize {
        self.state
            .song()
            .map(|song| song.file.tracks.iter().filter(|t| !t.notes.is_empty()).count())
            .unwrap_or(0)
    }

    /// How tall the rows come out, so what sits under them knows where to
    /// start. Mirrors the placement in `track_rows_ui`: rows on a pitch of
    /// `ROW_H + ROW_GAP`, and the "+N more" line where a seventh row would go.
    fn track_rows_height(&self) -> f32 {
        let listed = self.listed_tracks();
        if listed == 0 {
            return 0.0;
        }

        if listed > MAX_ROWS {
            MAX_ROWS as f32 * (ROW_H + ROW_GAP) + MORE_H
        } else {
            (listed - 1) as f32 * (ROW_H + ROW_GAP) + ROW_H
        }
    }

    /// The song's parts, listed in the top-left corner — out of the way of the
    /// centred menu, and mirroring the performer selector opposite it — with
    /// the microphone row beneath them.
    pub fn track_list_ui(&mut self, ctx: &mut Context, ui: &mut nuon::Ui) {
        nuon::translate()
            .x(LIST_MARGIN)
            .y(LIST_TOP)
            .build(ui, |ui| {
                // Nothing loaded means no parts to list, and the microphone
                // rides up to the top corner in their place — it is worth
                // reaching whether or not a song is open.
                let parts_h = if self.listed_tracks() > 0 {
                    nuon::label()
                        .size(LIST_W, CAPTION_H)
                        .text("PARTS (Left & Right)")
                        .text_justify(TextJustify::Left)
                        .font_size(12.0)
                        .color(nuon::Color::new_u8(150, 150, 150, 1.0))
                        .build(ui);

                    nuon::translate()
                        .y(CAPTION_H)
                        .build(ui, |ui| self.track_rows_ui(ctx, ui, LIST_W));

                    CAPTION_H + self.track_rows_height() + SECTION_GAP
                } else {
                    0.0
                };

                nuon::translate().y(parts_h).build(ui, |ui| {
                    nuon::label()
                        .size(LIST_W, CAPTION_H)
                        .text("AUDIO")
                        .text_justify(TextJustify::Left)
                        .font_size(12.0)
                        .color(nuon::Color::new_u8(150, 150, 150, 1.0))
                        .build(ui);

                    nuon::translate().y(CAPTION_H).build(ui, |ui| {
                        if mic_row(ctx, ui, LIST_W) {
                            ctx.mic_passthrough.toggle();
                        }
                    });
                });
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

/// Microphone passthrough as one wide toggle, shaped like a track row so the
/// two read as one panel: an indicator where a track keeps its colour, the name
/// where a track keeps its instrument, and the state where a track keeps its
/// buttons. The whole row is the click target — there is only one thing to say
/// to it. Returns true when it was clicked.
fn mic_row(ctx: &Context, ui: &mut nuon::Ui, w: f32) -> bool {
    let on = ctx.mic_passthrough.is_on();
    let error = ctx.mic_passthrough.error();

    let clicked = nuon::button()
        .id("mic_passthrough")
        .size(w, ROW_H)
        .icon("")
        .color(nuon::Color::new_u8(37, 35, 42, 1.0))
        .hover_color(nuon::Color::new_u8(50, 47, 58, 1.0))
        .preseed_color(nuon::Color::new_u8(58, 54, 68, 1.0))
        .border_radius([8.0; 4])
        .build(ui);

    let pad = 10.0;

    // Lit while it is live, in the place a track keeps its colour dot. Drawn
    // rather than built as a button: there is nothing separate to click here.
    nuon::quad()
        .pos(pad, (ROW_H - DOT) / 2.0)
        .size(DOT, DOT)
        .color(if on {
            nuon::Color::new_u8(80, 200, 120, 1.0)
        } else {
            nuon::Color::new_u8(78, 73, 92, 1.0)
        })
        .border_radius([255.0; 4])
        .build(ui);

    // A failure is worth saying out loud in the row itself — switched on and
    // silent is otherwise indistinguishable from a mic nobody is singing into.
    let title = match error {
        Some(err) => format!("Microphone passthrough · {}", truncate(err, 34)),
        None => "Microphone passthrough".to_string(),
    };

    let label_x = pad + DOT + 8.0;
    let meter_x = w - pad - STATUS_W - METER_W;
    nuon::label()
        .pos(label_x, 0.0)
        .size((meter_x - label_x - 8.0).max(0.0), ROW_H)
        .text(title)
        .text_justify(TextJustify::Left)
        .font_size(13.0)
        .color(if on {
            nuon::Color::new_u8(235, 235, 235, 1.0)
        } else {
            nuon::Color::new_u8(170, 170, 170, 1.0)
        })
        .build(ui);

    // Only while it is live: an empty bar sitting there with the mic off would
    // read as "nothing is getting through", which is not what off means.
    if on {
        let meter_y = (ROW_H - METER_H) / 2.0;

        nuon::quad()
            .pos(meter_x, meter_y)
            .size(METER_W, METER_H)
            .color(nuon::Color::new_u8(24, 23, 28, 1.0))
            .border_radius([3.0; 4])
            .build(ui);

        let fill = meter_fill(ctx.mic_passthrough.level());
        if fill > 0.0 {
            nuon::quad()
                .pos(meter_x, meter_y)
                .size(METER_W * fill, METER_H)
                .color(if fill > 0.98 {
                    // Against the ceiling, where a voice starts to square off.
                    nuon::Color::new_u8(220, 110, 90, 1.0)
                } else {
                    nuon::Color::new_u8(80, 200, 120, 1.0)
                })
                .border_radius([3.0; 4])
                .build(ui);
        }
    }

    let (status, status_color) = match (on, error.is_some()) {
        (true, _) => ("ON", nuon::Color::new_u8(120, 220, 150, 1.0)),
        (false, true) => ("UNAVAILABLE", nuon::Color::new_u8(200, 110, 110, 1.0)),
        (false, false) => ("OFF", nuon::Color::new_u8(130, 130, 130, 1.0)),
    };

    nuon::label()
        .pos(w - STATUS_W - pad, 0.0)
        .size(STATUS_W, ROW_H)
        .text(status)
        .text_justify(TextJustify::Right)
        .font_size(12.0)
        .color(status_color)
        .build(ui);

    clicked
}

/// How much of the meter a peak fills, on a decibel scale running from
/// [`METER_FLOOR_DB`] up to full scale.
fn meter_fill(peak: f32) -> f32 {
    if peak <= 0.0 {
        return 0.0;
    }

    let db = 20.0 * peak.log10();
    ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

/// Cut to `max` characters, counted as characters rather than bytes so a device
/// name with anything non-ASCII in it cannot land mid-codepoint.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_meter_reads_in_decibels() {
        assert_eq!(meter_fill(0.0), 0.0, "silence");
        assert_eq!(meter_fill(1.0), 1.0, "full scale");

        // Half the bar is half the way up in decibels, not in amplitude: -30
        // dBFS, which is an amplitude of about 0.032.
        assert!((meter_fill(0.0316) - 0.5).abs() < 0.01);

        // A microphone sending nothing but its own noise leaves it near empty,
        // and anything past full scale cannot push it further.
        assert!(meter_fill(0.0001) < 0.05);
        assert_eq!(meter_fill(4.0), 1.0);
    }
}
