use midi_file::midly::MidiMessage;
use neothesia_core::render::{
    GlowRenderer, GuidelineRenderer, NoteLabels, QuadRenderer, SheetMusic, TextRenderer,
};
use std::time::Duration;
use winit::{
    event::WindowEvent,
    keyboard::{Key, NamedKey},
};

use self::top_bar::TopBar;

use super::{NuonRenderer, Scene};
use crate::{
    NeothesiaEvent, context::Context, icons, render::WaterfallRenderer,
    scene::MouseToMidiEventState, song::Song, utils::window::WinitEvent,
};

mod keyboard;
pub use keyboard::Keyboard;

pub(crate) mod midi_player;
use midi_player::{HitKind, MidiPlayer};

mod effects;
use effects::EffectsSystem;

mod sfx;
use sfx::Sfx;

mod rewind_controller;
use rewind_controller::RewindController;

mod toast_manager;
use toast_manager::ToastManager;

mod animation;
mod top_bar;

/// Song-title marquee under the performer selector.
const TITLE_FONT_SIZE: f32 = 14.0;
/// Logical pixels per second the title crawls leftwards.
const TITLE_SCROLL_SPEED: f32 = 45.0;

/// Gap kept between the sheet-music strip and the keyboard when the strip is
/// dropped down, so its own fade-out never touches the keys.
const SHEET_REPOSITION_GAP: f32 = 10.0;
/// How much bigger the strip reads once it's dropped down over the keyboard,
/// where there's both the room and the reason (it's close enough to the
/// hands now to be worth reading in detail) for the extra size.
const SHEET_REPOSITION_BOTTOM_ZOOM: f32 = 1.35;
/// Footprint of the hover-revealed down-arrow that moves the sheet strip.
const SHEET_REPOSITION_ARROW_SIZE: f32 = 18.0;
/// Inset of the arrow from the strip's own top-right corner.
const SHEET_REPOSITION_ARROW_PAD: f32 = 6.0;
/// Matches `neothesia_core::render::sheet`'s brighter neon tone, so the arrow
/// reads as part of the same glow rather than a foreign UI colour.
const SHEET_REPOSITION_ARROW_COLOR: [f32; 3] = [0.72, 1.0, 0.85];

pub struct PlayingScene {
    keyboard: Keyboard,
    waterfall: WaterfallRenderer,
    guidelines: GuidelineRenderer,
    text_renderer: TextRenderer,
    nuon_renderer: NuonRenderer,

    note_labels: Option<NoteLabels>,

    /// Scrolling staff notation across the top. `None` only when the song has
    /// nothing to engrave (a drum-only file, say).
    sheet: Option<SheetMusic>,
    show_sheet: bool,
    /// Dropped down over the keyboard instead of hugging the top bar — for a
    /// player who reads notation well enough to want it close to their
    /// hands, not skimmed at the top of the screen. Toggled by hovering the
    /// strip (which surfaces a down-arrow) and clicking it.
    sheet_at_bottom: bool,

    player: MidiPlayer,
    rewind_controller: RewindController,
    quad_renderer_bg: QuadRenderer,
    quad_renderer_fg: QuadRenderer,
    glow: Option<GlowRenderer>,
    effects: EffectsSystem,
    sfx: Sfx,
    toast_manager: ToastManager,

    nuon: nuon::Ui,
    mouse_to_midi_state: MouseToMidiEventState,

    deduced_chord_name: String,

    /// Song title under the performer selector, so what is playing is
    /// readable without leaving the song. It sits still when it fits in the
    /// selector's width and only crawls right to left when it does not.
    /// Width is measured once up front; `title_scroll` is how far the crawl
    /// has travelled from that same flush-left start, and switching performer
    /// mode drops it back to 0 as a visible receipt that the click landed.
    title: String,
    title_width: f32,
    title_scroll: f32,

    top_bar: TopBar,

    /// Song ended in play-along mode: show the results screen until the
    /// player restarts (Enter) or leaves (Backspace/Esc).
    finished: bool,
}

impl PlayingScene {
    pub fn new(ctx: &mut Context, song: Song) -> Self {
        let keyboard = Keyboard::new(ctx, song.config.clone());

        let keyboard_layout = keyboard.layout();

        let guidelines = GuidelineRenderer::new(
            keyboard_layout.clone(),
            *keyboard.pos(),
            ctx.config.vertical_guidelines(),
            ctx.config.horizontal_guidelines(),
            song.file.measures.clone(),
        );

        let hidden_tracks: Vec<usize> = song
            .config
            .tracks
            .iter()
            .filter(|t| !t.visible)
            .map(|t| t.track_id)
            .collect();

        let mut waterfall = WaterfallRenderer::new(
            &ctx.gpu,
            &song.file.tracks,
            &hidden_tracks,
            &ctx.config,
            &ctx.transform,
            keyboard_layout.clone(),
        );

        let text_renderer = ctx.text_renderer_factory.new_renderer();

        let note_labels = ctx.config.note_labels().then_some(NoteLabels::new(
            *keyboard.pos(),
            waterfall.notes(),
            ctx.text_renderer_factory.new_renderer(),
        ));

        // The staff shows the part the player is responsible for: the Human
        // track if the song has one, otherwise everything visible. Reading
        // along with the accompaniment as well would be unreadable.
        let sheet = {
            let human: Vec<usize> = song
                .config
                .tracks
                .iter()
                .filter(|t| t.player == crate::song::PlayerConfig::Human)
                .map(|t| t.track_id)
                .collect();

            let wanted: &[usize] = if human.is_empty() {
                &[]
            } else {
                &human
            };

            let notes: Vec<_> = song
                .file
                .tracks
                .iter()
                .filter(|t| {
                    if wanted.is_empty() {
                        !hidden_tracks.contains(&t.track_id)
                    } else {
                        wanted.contains(&t.track_id)
                    }
                })
                .flat_map(|t| t.notes.iter().cloned())
                .collect();

            let sheet = SheetMusic::new(
                &notes,
                &song.file.measures,
                song.file.time_signature.quarters_per_bar(),
                ctx.text_renderer_factory.new_renderer(),
                ctx.quad_renderer_factory.new_renderer(),
            );
            (!sheet.is_empty()).then_some(sheet)
        };
        let show_sheet = ctx.config.sheet_music();
        let sheet_at_bottom = ctx.config.sheet_music_bottom();

        // Measured with the same font the label will draw with, so the
        // marquee knows exactly when the title has cleared the left edge.
        let title = song_title(&song.file.name);
        let title_buffer = TextRenderer::gen_buffer(TITLE_FONT_SIZE, &title);
        let title_width = TextRenderer::measure(&title_buffer).0;

        let player = MidiPlayer::new(
            ctx.output_manager.connection().clone(),
            song,
            keyboard_layout.range.clone(),
            ctx.config.separate_channels(),
            ctx.perform_mode,
        );
        waterfall.update(player.time_without_lead_in());

        let quad_renderer_bg = ctx.quad_renderer_factory.new_renderer();
        let quad_renderer_fg = ctx.quad_renderer_factory.new_renderer();

        let glow = ctx.config.glow().then_some(GlowRenderer::new(
            &ctx.gpu,
            &ctx.transform,
            keyboard.layout(),
        ));

        Self {
            keyboard,
            guidelines,
            note_labels,
            sheet,
            show_sheet,
            sheet_at_bottom,
            text_renderer,
            nuon_renderer: NuonRenderer::new(ctx),

            waterfall,
            player,
            rewind_controller: RewindController::new(),
            quad_renderer_bg,
            quad_renderer_fg,
            glow,
            effects: EffectsSystem::new(),
            sfx: Sfx::new(),
            toast_manager: ToastManager::default(),

            nuon: nuon::Ui::new(),
            mouse_to_midi_state: MouseToMidiEventState::default(),
            deduced_chord_name: String::new(),

            title,
            title_width,
            title_scroll: 0.0,

            top_bar: TopBar::new(),

            finished: false,
        }
    }

    fn update_glow(&mut self, delta: Duration) {
        let Some(glow) = &mut self.glow else {
            return;
        };

        glow.clear();

        let keys = &self.keyboard.layout().keys;
        let states = self.keyboard.key_states();

        for (key, state) in keys.iter().zip(states) {
            let Some(color) = state.pressed_by_file() else {
                continue;
            };

            glow.push(
                key.id(),
                *color,
                key.x(),
                self.keyboard.pos().y,
                key.width(),
                delta,
            );
        }
    }

    /// Spawn Guitar-Hero sparks for hit events and advance the particle sim.
    /// `time` is the same waterfall time computed in [`Self::update`].
    fn update_effects(&mut self, delta: Duration, time: f32) {
        let dt = delta.as_secs_f32();
        let pos = *self.keyboard.pos();
        let neutral_w = self.keyboard.layout().sizing.neutral_width;
        let board_width = self.keyboard.layout().width;
        let range_start = self.keyboard.range().start();
        let hit_line_y = pos.y;

        // Wait mode grades the human against stalled targets; jam mode
        // grades whatever they play over the self-playing song. Only freeze
        // the tallies once the results screen is up.
        let scoring = !self.finished;

        for e in self.player.take_hit_events() {
            if !scoring {
                continue;
            }
            let id = e.note_id.wrapping_sub(range_start) as usize;
            let Some(key) = self.keyboard.layout().keys.get(id) else {
                continue;
            };
            let cx = pos.x + key.x() + key.width() / 2.0;

            match e.kind {
                HitKind::Good { delta, late } => {
                    self.effects.good_hit(
                        e.note_id,
                        cx,
                        hit_line_y,
                        neutral_w,
                        delta.as_secs_f32(),
                        late,
                        e.chord,
                    );

                    // Find the note bar being struck (the one crossing the hit
                    // line on this key right now) and light it up.
                    let struck = self
                        .waterfall
                        .notes()
                        .iter()
                        .filter(|n| n.note == e.note_id && n.channel != 9)
                        .filter(|n| {
                            n.start.as_secs_f32() <= time + 0.6
                                && n.end.as_secs_f32() >= time - 0.15
                        })
                        .min_by(|a, b| {
                            let da = (a.start.as_secs_f32() - time).abs();
                            let db = (b.start.as_secs_f32() - time).abs();
                            da.total_cmp(&db)
                        });

                    if let Some(n) = struck {
                        self.effects.note_struck(
                            e.note_id,
                            pos.x + key.x(),
                            key.width() - 1.0,
                            n.start.as_secs_f32(),
                            n.duration.as_secs_f32(),
                        );
                    }
                }
                HitKind::Wrong => {
                    self.effects.wrong_hit(cx, hit_line_y);
                    self.sfx.fail();
                }
                // A note the song asked for that nobody played: fails
                // silently — quiet combo reset, no buzzer, no text.
                HitKind::Miss => self.effects.miss(),
            }
        }

        // Thunder for a bolt that landed while those hits were judged.
        if self.effects.take_strike() {
            self.sfx.lightning();
        }

        self.effects.update(dt, hit_line_y, pos.x, board_width);
    }

    /// Electric wash while a lightning strike's surge holds: the keyboard, the
    /// hit line and every falling bar on screen light up. Drawn under the
    /// particles and the bolt itself, which stay the brightest things around.
    fn render_surge(&mut self, ctx: &Context, time: f32) {
        let pos = *self.keyboard.pos();
        let board_width = self.keyboard.layout().width;

        self.effects.render_surge_glow(
            &mut self.quad_renderer_fg,
            pos.y,
            pos.x,
            board_width,
            ctx.window_state.logical_size.width,
            ctx.window_state.logical_size.height,
        );

        if !self.effects.surging() {
            return;
        }

        // Only bars anywhere near the lane are worth a glow quad; the rest of
        // the song is minutes away in either direction.
        let range_start = self.keyboard.range().start();
        let keys = &self.keyboard.layout().keys;
        let bars = self
            .waterfall
            .notes()
            .iter()
            .filter(|n| n.channel != 9)
            .filter(|n| n.end.as_secs_f32() > time - 0.5 && n.start.as_secs_f32() < time + 20.0)
            .filter_map(|n| {
                let key = keys.get(n.note.wrapping_sub(range_start) as usize)?;
                Some((
                    pos.x + key.x(),
                    key.width() - 1.0,
                    n.start.as_secs_f32(),
                    n.duration.as_secs_f32(),
                ))
            });

        self.effects.render_surge_notes(
            &mut self.quad_renderer_fg,
            bars,
            time,
            ctx.config.animation_speed() / ctx.window_state.scale_factor as f32,
            pos.y,
        );
    }

    /// Guitar-Hero HUD: top-left streak counter + audience sentiment, rising
    /// PERFECT/GOOD grades out of the keys, and the pulsing combo counter.
    /// Arcade results screen shown over the dimmed scene when a play-along
    /// song ends.
    fn results_overlay_ui(&mut self, ctx: &Context) {
        let win_w = ctx.window_state.logical_size.width;
        let win_h = ctx.window_state.logical_size.height;

        let results = self.effects.results();
        let (grade, (gr, gg, gb)) = results.grade();
        let accuracy = (results.accuracy() * 100.0).round() as u32;
        let performance = (results.performance() * 100.0).round() as u32;

        let top = win_h * 0.16;

        nuon::label()
            .text("SONG COMPLETE")
            .font_size(26.0)
            .color(nuon::Color::new_u8(220, 220, 220, 1.0))
            .y(top)
            .height(28.0)
            .width(win_w)
            .build(&mut self.nuon);

        nuon::label()
            .text(grade)
            .font_size(150.0)
            .color(nuon::Color::new_u8(gr, gg, gb, 1.0))
            .bold(true)
            .y(top + 40.0)
            .height(150.0)
            .width(win_w)
            .build(&mut self.nuon);

        nuon::label()
            .text(format!("{performance}% performance  ·  {accuracy}% accuracy"))
            .font_size(24.0)
            .color(nuon::Color::new_u8(255, 255, 255, 1.0))
            .bold(true)
            .y(top + 210.0)
            .height(26.0)
            .width(win_w)
            .build(&mut self.nuon);

        nuon::label()
            .text(format!(
                "PERFECT {}    GOOD {}    OK {}    MISS {}    WRONG {}",
                results.perfect, results.good, results.ok, results.missed, results.wrong
            ))
            .font_size(19.0)
            .color(nuon::Color::new_u8(230, 230, 230, 1.0))
            .y(top + 250.0)
            .height(20.0)
            .width(win_w)
            .build(&mut self.nuon);

        nuon::label()
            .text(format!(
                "SCORE {}    ·    Best streak: {}",
                effects::thousands(results.score),
                results.best_combo
            ))
            .font_size(19.0)
            .color(nuon::Color::new_u8(255, 200, 90, 1.0))
            .y(top + 280.0)
            .height(20.0)
            .width(win_w)
            .build(&mut self.nuon);

        // Clickable, and the keyboard shortcuts they name still work.
        let (btn_w, btn_h, btn_gap) = (220.0, 46.0, 18.0);
        let btn_x = (win_w - (btn_w * 2.0 + btn_gap)) / 2.0;
        let btn_y = top + 322.0;

        if nuon::button()
            .id("results-play-again")
            .pos(btn_x, btn_y)
            .size(btn_w, btn_h)
            .color(nuon::Color::new_u8(160, 81, 238, 1.0))
            .hover_color(nuon::Color::new_u8(184, 110, 255, 1.0))
            .border_radius([8.0; 4])
            .label("PLAY AGAIN")
            .build(&mut self.nuon)
        {
            ctx.proxy
                .send_event(NeothesiaEvent::Play(self.player.song().clone()))
                .ok();
        }

        if nuon::button()
            .id("results-menu")
            .pos(btn_x + btn_w + btn_gap, btn_y)
            .size(btn_w, btn_h)
            .color(nuon::Color::new_u8(58, 58, 70, 1.0))
            .hover_color(nuon::Color::new_u8(80, 80, 94, 1.0))
            .border_radius([8.0; 4])
            .label("MENU")
            .build(&mut self.nuon)
        {
            ctx.proxy
                .send_event(NeothesiaEvent::MainMenu(Some(self.player.song().clone())))
                .ok();
        }

        // Keyboard equivalents, quiet and out of the way beneath each button.
        for (x, key) in [(btn_x, "Enter"), (btn_x + btn_w + btn_gap, "Backspace")] {
            nuon::label()
                .text(key)
                .font_size(12.0)
                .color(nuon::Color::new_u8(130, 130, 130, 1.0))
                .pos(x, btn_y + btn_h + 7.0)
                .size(btn_w, 14.0)
                .build(&mut self.nuon);
        }
    }

    fn update_hud(&mut self, ctx: &mut Context, delta: Duration) {
        if self.finished {
            self.results_overlay_ui(ctx);
            return;
        }

        // Slide the whole top-left block down as the top bar expands so the
        // dropdown never covers it. The sheet strip keeps to the middle of
        // the screen, so it needs no room made for it here.
        let hud_top = self
            .top_bar
            .topbar_expand_animation
            .animate_bool(0.0, 75.0, ctx.frame_timestamp);

        // --- top-right performer selector -------------------------------------
        // Three-way segmented toggle, all options visible: HERO (song rolls,
        // target notes silent, the player performs them), AUTO (song plays
        // itself, jam over the top), HUMAN (song waits). Tallies keep
        // running across switches.
        {
            let win_w = ctx.window_state.logical_size.width;

            if let Some(mode) = super::performer_selector(
                &mut self.nuon,
                win_w,
                hud_top + 10.0,
                self.player.mode(),
            ) {
                self.player.set_mode(mode);
                // Remembered for the session, so leaving the song — to replay
                // it or to pick another — comes back to the mode the player
                // asked for rather than the song's default.
                ctx.perform_mode = mode;
                // Snap the marquee back to its starting point: a switch you
                // can see even when the music does not change much.
                self.title_scroll = 0.0;
            }

            // --- song title, tucked under the selector --------------------
            // Measured off the selector, so the two stay aligned.
            let x0 = super::performer_selector_x(win_w);
            let band_y = hud_top + 46.0;
            let band_h = TITLE_FONT_SIZE + 6.0;
            let band_w = super::PERFORMER_SELECTOR_W;

            let has_title = !self.title.is_empty() && self.title_width > 0.0;

            // A scissor rect that leaves the window is a fatal wgpu
            // validation error rather than a clipped draw, so on a window
            // too small to hold the band, skip the title entirely.
            let band_on_screen = x0 >= 0.0
                && band_y >= 0.0
                && x0 + band_w <= win_w
                && band_y + band_h <= ctx.window_state.logical_size.height;

            if has_title && self.title_width <= band_w {
                // Short enough to read at a glance: sit still, left-aligned
                // under the selector. No clipping needed — it fits.
                self.title_scroll = 0.0;

                nuon::label()
                    .text(self.title.clone())
                    .font_size(TITLE_FONT_SIZE)
                    .color(nuon::Color::new_u8(190, 190, 190, 1.0))
                    .text_justify(nuon::TextJustify::Left)
                    .pos(x0, band_y)
                    .size(band_w, band_h)
                    .build(&mut self.nuon);
            } else if has_title && band_on_screen {
                // Too wide to show at once, so crawl it past instead. Travel
                // is measured from the same flush-left spot a short title
                // would sit in, so the first thing you read is the start of
                // the name; only once it has cleared the left edge do later
                // laps come back in from the right, the way a ticker does.
                // Clamped, because the first frame's delta covers all of GPU
                // and asset startup: unclamped it would skip the title
                // straight past the flush-left position nobody had seen yet.
                // Long stalls (a drag, an alt-tab) are held back the same way.
                let dt = delta.as_secs_f32().min(1.0 / 30.0);
                self.title_scroll += TITLE_SCROLL_SPEED * dt;

                let first_pass = self.title_width;
                let lap = band_w + self.title_width;
                if self.title_scroll >= first_pass + lap {
                    // Back onto the start of a lap, keeping the accumulator
                    // bounded however long the song runs.
                    self.title_scroll -= lap;
                }

                let clip = nuon::Rect::new(
                    nuon::Point::new(x0, band_y),
                    nuon::Size::new(band_w, band_h),
                );

                let text = self.title.clone();
                let text_x = if self.title_scroll < first_pass {
                    x0 - self.title_scroll
                } else {
                    x0 + band_w - (self.title_scroll - first_pass)
                };
                let text_w = self.title_width;

                nuon::layer().scissor_rect(clip).build(&mut self.nuon, |ui| {
                    nuon::label()
                        .text(text)
                        .font_size(TITLE_FONT_SIZE)
                        .color(nuon::Color::new_u8(190, 190, 190, 1.0))
                        .text_justify(nuon::TextJustify::Left)
                        .pos(text_x, band_y)
                        .size(text_w, band_h)
                        .build(ui);
                });
            }
        }

        // --- top-left streak counter + audience face + dial -----------------
        if self.effects.has_activity() {
            let on_fire = self.effects.on_fire();
            let streak_color = if on_fire {
                nuon::Color::new_u8(255, 150, 40, 1.0)
            } else {
                nuon::Color::new_u8(255, 255, 255, 1.0)
            };

            nuon::label()
                .text(format!("{}", self.effects.combo()))
                .font_size(36.0)
                .color(streak_color)
                .bold(true)
                .text_justify(nuon::TextJustify::Left)
                .pos(16.0, hud_top + 10.0)
                .size(120.0, 36.0)
                .build(&mut self.nuon);

            nuon::label()
                .text("STREAK")
                .font_size(12.0)
                .color(nuon::Color::new_u8(190, 190, 190, 1.0))
                .text_justify(nuon::TextJustify::Left)
                .pos(16.0, hud_top + 48.0)
                .size(120.0, 12.0)
                .build(&mut self.nuon);

            nuon::label()
                .text(format!("BEST {}", self.effects.best_combo()))
                .font_size(12.0)
                .color(nuon::Color::new_u8(150, 150, 150, 1.0))
                .text_justify(nuon::TextJustify::Left)
                .pos(16.0, hud_top + 66.0)
                .size(160.0, 12.0)
                .build(&mut self.nuon);

            // Score readout. Nothing but its colour marks the surge: it turns
            // electric blue for as long as notes are worth half again as much,
            // and the keyboard behind it is already saying the rest.
            let surging = self.effects.surging();
            const SCORE_SIZE: f32 = 14.0;
            let score_y = hud_top + 86.0;

            nuon::label()
                .text(format!("SCORE {}", effects::thousands(self.effects.score())))
                .font_size(SCORE_SIZE)
                .color(if surging {
                    nuon::Color::new_u8(105, 195, 255, 1.0)
                } else {
                    nuon::Color::new_u8(255, 222, 84, 1.0)
                })
                .bold(true)
                .text_justify(nuon::TextJustify::Left)
                .pos(16.0, score_y)
                .size(220.0, SCORE_SIZE)
                .build(&mut self.nuon);

            // Charge pips for the next bolt, in the slot below the score. Only
            // while it is being built — during a surge there is nothing to
            // charge, and the blue score says so.
            if !surging && self.effects.perfect_chords() > 0 {
                let row_y = score_y + SCORE_SIZE + 6.0;
                let pips_w =
                    self.effects
                        .render_bolt_charge(&mut self.quad_renderer_fg, 16.0, row_y + 6.0);

                // A full chain still waits on a maxed-out crowd, so say so —
                // full pips and no bolt would otherwise look broken.
                let chain_full = self.effects.perfect_chords() >= effects::BOLT_CHORDS;
                let text = if chain_full && !self.effects.crowd_maxed() {
                    "CHAIN READY · WIN THE CROWD".to_string()
                } else {
                    format!(
                        "PERFECT CHAIN {}/{}",
                        self.effects.perfect_chords(),
                        effects::BOLT_CHORDS
                    )
                };

                nuon::label()
                    .text(text)
                    .font_size(12.0)
                    .color(nuon::Color::new_u8(150, 200, 230, 1.0))
                    .text_justify(nuon::TextJustify::Left)
                    .pos(16.0 + pips_w + 8.0, row_y)
                    .size(240.0, 12.0)
                    .build(&mut self.nuon);
            }

            // The audience weighs in: a drawn face + speedometer-style dial.
            if let Some(level) = self.effects.sentiment_level() {
                self.effects.render_sentiment_face(
                    &mut self.quad_renderer_fg,
                    130.0,
                    hud_top + 32.0,
                    level,
                );
                self.effects.render_sentiment_dial(
                    &mut self.quad_renderer_fg,
                    212.0,
                    hud_top + 52.0,
                    28.0,
                );
            }
        }

        // --- PERFECT / GOOD / TOO SLOW rising out of the keys ---------------
        for r in self.effects.rising_texts() {
            use effects::RisingGrade;
            let color = match r.grade {
                RisingGrade::Perfect => nuon::Color::new_u8(255, 222, 84, r.alpha()),
                RisingGrade::Good => nuon::Color::new_u8(196, 255, 196, r.alpha()),
                RisingGrade::Slow => nuon::Color::new_u8(190, 150, 150, r.alpha()),
            };

            let size = r.font_size();
            nuon::label()
                .text(r.text())
                .font_size(size)
                .color(color)
                .bold(r.grade == RisingGrade::Perfect)
                .x(r.x() - 90.0)
                .y(r.y())
                .size(180.0, size)
                .build(&mut self.nuon);
        }

        // --- centre combo pulse ---------------------------------------------
        let combo = self.effects.combo();
        if combo < 2 {
            return;
        }

        let pop = self.effects.combo_pop();
        let on_fire = self.effects.on_fire();
        let mult = self.effects.multiplier();
        let win_w = ctx.window_state.logical_size.width;

        // Pulses on each hit, grows gently with the streak.
        let grow = (combo as f32 / 50.0).min(1.0);
        let font_size = 40.0 + pop * 22.0 + grow * 14.0;
        let y = self.keyboard.pos().y - 150.0;

        // Colour ramps white -> gold -> blazing orange.
        let color = if on_fire {
            nuon::Color::new_u8(255, 120, 35, 1.0)
        } else if combo >= 10 {
            nuon::Color::new_u8(255, 216, 74, 1.0)
        } else {
            nuon::Color::new_u8(255, 255, 255, 1.0)
        };

        nuon::label()
            .text(format!("x{mult}   {combo} COMBO"))
            .font_size(font_size)
            .color(color)
            .bold(true)
            .y(y)
            .height(font_size)
            .width(win_w)
            .build(&mut self.nuon);

        if on_fire {
            let fire_size = 22.0 + pop * 6.0;
            nuon::label()
                .text("ON FIRE!")
                .font_size(fire_size)
                .color(nuon::Color::new_u8(255, 170, 40, 1.0))
                .bold(true)
                .y(y - fire_size - 8.0)
                .height(fire_size)
                .width(win_w)
                .build(&mut self.nuon);
        }
    }

    fn update_chord_identifier(&mut self, enabled: bool) {
        if !enabled {
            return;
        }

        let start = self.keyboard.layout().range.start();
        let notes = self
            .keyboard
            .key_states()
            .iter()
            .enumerate()
            .filter(|(_, state)| state.pressed_by_user().is_some())
            .map(|(id, _)| id as u8 + start)
            .collect::<Vec<_>>();

        self.deduced_chord_name = super::freeplay::chords::deduce_name(&notes).unwrap_or_default();
    }

    #[profiling::function]
    fn update_midi_player(&mut self, ctx: &Context, delta: Duration) -> f32 {
        if self.top_bar.is_looper_active() && self.player.time() > self.top_bar.loop_end_timestamp()
        {
            self.player.set_time(self.top_bar.loop_start_timestamp());
            self.keyboard.reset_notes();
        }

        // Only wait mode stalls the song on unplayed targets. Jam mode (no
        // Human track) must keep rolling — its targets are graded against a
        // moving song and expire as misses, so freezing on them would
        // deadlock playback on the first note nobody played.
        let waiting = self.player.waits_for_player()
            && !self.player.play_along().are_required_keys_pressed();

        if !waiting {
            let delta = (delta / 10) * (ctx.config.speed_multiplier() * 10.0) as u32;
            let midi_events = self.player.update(delta);
            self.keyboard.file_midi_events(&ctx.config, &midi_events);
        } else {
            // Wait-mode stall: playback is frozen, but wrong presses must
            // still expire and be judged while the song waits.
            self.player.tick_play_along();
        }

        self.player.time_without_lead_in() + ctx.config.animation_offset()
    }

    #[profiling::function]
    fn resize(&mut self, ctx: &mut Context) {
        self.keyboard.resize(ctx);

        self.guidelines.set_layout(self.keyboard.layout().clone());
        self.guidelines.set_pos(*self.keyboard.pos());
        if let Some(note_labels) = self.note_labels.as_mut() {
            note_labels.set_pos(*self.keyboard.pos());
        }

        self.waterfall
            .resize(&ctx.config, self.keyboard.layout().clone());
    }
}

impl Scene for PlayingScene {
    #[profiling::function]
    fn update(&mut self, ctx: &mut Context, delta: Duration) {
        self.quad_renderer_bg.clear();
        self.quad_renderer_fg.clear();

        self.rewind_controller.update(&mut self.player, ctx, delta);
        self.toast_manager.update(&mut self.text_renderer);

        let time = self.update_midi_player(ctx, delta);
        self.waterfall.update(time);
        self.guidelines.update(
            &mut self.quad_renderer_bg,
            ctx.config.animation_speed(),
            ctx.window_state.scale_factor as f32,
            time,
            ctx.window_state.logical_size,
        );
        self.keyboard
            .update(&mut self.quad_renderer_fg, &mut self.text_renderer);
        self.update_chord_identifier(ctx.config.chord_identifier());
        if let Some(note_labels) = self.note_labels.as_mut() {
            note_labels.update(
                ctx.window_state.physical_size,
                ctx.window_state.scale_factor as f32,
                self.keyboard.renderer(),
                ctx.config.animation_speed(),
                time,
            );
        }

        if self.show_sheet
            && let Some(sheet) = self.sheet.as_mut()
        {
            // At the top, ride down with the expanding top bar, as the HUD
            // does, so the dropdown never sits on top of the staff. Dropped
            // down, there is no top bar to dodge — it sits just above the
            // keyboard instead, close enough to read without covering keys.
            // Dropped down, there's room — and a reason, being close to the
            // hands now — to read the strip bigger, so it also zooms in
            // place from its own bottom edge.
            let (y_offset, zoom) = if self.sheet_at_bottom {
                (
                    (self.keyboard.pos().y - SHEET_REPOSITION_GAP - sheet.height()).max(0.0),
                    SHEET_REPOSITION_BOTTOM_ZOOM,
                )
            } else {
                (
                    self.top_bar.topbar_expand_animation.animate_bool(
                        0.0,
                        75.0,
                        ctx.frame_timestamp,
                    ),
                    1.0,
                )
            };
            sheet.update(
                ctx.window_state.physical_size,
                ctx.window_state.scale_factor as f32,
                time,
                ctx.window_state.logical_size,
                y_offset,
                zoom,
            );

            // Silent by default: nothing marks the strip as clickable until
            // the cursor finds it, then an arrow says so — pointing the way
            // the strip will move — and clicking anywhere on it sends it
            // there. Reading sheet music well enough to want it isn't the
            // common case, so the control stays out of the way of everyone
            // else.
            let rect = sheet.rect();
            let event = nuon::click_area(nuon::Id::hash("sheet_reposition"))
                .rect(rect)
                .build(&mut self.nuon);

            if event.is_clicked() {
                self.sheet_at_bottom = !self.sheet_at_bottom;
                ctx.config.set_sheet_music_bottom(self.sheet_at_bottom);
                self.toast_manager.toast(if self.sheet_at_bottom {
                    "Sheet Music: Moved to bottom"
                } else {
                    "Sheet Music: Moved to top"
                });
            }

            if event.is_hovered() || event.is_pressed() {
                let arrow_x = rect.origin.x + rect.size.width
                    - SHEET_REPOSITION_ARROW_SIZE
                    - SHEET_REPOSITION_ARROW_PAD;
                let arrow_y = rect.origin.y + SHEET_REPOSITION_ARROW_PAD;
                let [r, g, b] = SHEET_REPOSITION_ARROW_COLOR;
                // Points the way a click sends the strip: down while it's at
                // the top, up once it's already at the bottom.
                let icon = if self.sheet_at_bottom {
                    icons::caret_up()
                } else {
                    icons::caret_down()
                };

                nuon::label()
                    .pos(arrow_x, arrow_y)
                    .size(SHEET_REPOSITION_ARROW_SIZE, SHEET_REPOSITION_ARROW_SIZE)
                    .icon(icon)
                    .font_size(SHEET_REPOSITION_ARROW_SIZE)
                    .text_justify(nuon::TextJustify::Center)
                    .color(nuon::Color::new(r, g, b, 1.0))
                    .build(&mut self.nuon);
            }
        }

        self.update_glow(delta);

        self.update_effects(delta, time);

        // Results screen: dim everything below, then let the celebration
        // fireworks (drawn next) sparkle on top of the dimmer.
        if self.finished {
            self.quad_renderer_fg.push(neothesia_core::render::QuadInstance {
                position: [0.0, 0.0],
                size: [
                    ctx.window_state.logical_size.width,
                    ctx.window_state.logical_size.height,
                ],
                color: [0.0, 0.0, 0.0, 0.55],
                border_radius: [0.0; 4],
            });

            if self.effects.results().celebratory() {
                self.effects.celebrate(
                    delta.as_secs_f32(),
                    ctx.window_state.logical_size.width,
                    ctx.window_state.logical_size.height,
                );
            }
        }

        self.render_surge(ctx, time);
        self.effects.render(&mut self.quad_renderer_fg);
        self.effects.render_note_flashes(
            &mut self.quad_renderer_fg,
            time,
            ctx.config.animation_speed() / ctx.window_state.scale_factor as f32,
            self.keyboard.pos().y,
        );
        self.effects.render_bolts(&mut self.quad_renderer_fg);
        self.update_hud(ctx, delta);

        TopBar::update(self, ctx);

        if ctx.config.chord_identifier() {
            nuon::label()
                .text(&self.deduced_chord_name)
                .font_size(25.0)
                .y(self.keyboard.pos().y - 35.0)
                .height(25.0)
                .width(ctx.window_state.logical_size.width)
                .build(&mut self.nuon);
        }

        super::render_nuon(&mut self.nuon, &mut self.nuon_renderer, ctx);

        self.quad_renderer_bg.prepare();
        self.quad_renderer_fg.prepare();

        if let Some(glow) = &mut self.glow {
            glow.prepare();
        }

        #[cfg(debug_assertions)]
        self.text_renderer.queue_fps(
            ctx.fps_ticker.avg(),
            self.top_bar
                .topbar_expand_animation
                .animate_bool(5.0, 80.0, ctx.frame_timestamp),
        );
        self.text_renderer.update(
            ctx.window_state.physical_size,
            ctx.window_state.scale_factor as f32,
        );

        if self.player.is_finished() && !self.player.is_paused() {
            if self.effects.has_activity() && !self.finished {
                // Anything scored — human or auto — freezes here and gets the
                // arcade results screen, with the crowd voicing its verdict.
                self.finished = true;
                self.player.pause();
                self.sfx.crowd(self.effects.results().grade().0);
            } else if !self.finished {
                ctx.proxy
                    .send_event(NeothesiaEvent::MainMenu(Some(self.player.song().clone())))
                    .ok();
            }
        }
    }

    #[profiling::function]
    fn render<'pass>(&'pass mut self, rpass: &mut wgpu_jumpstart::RenderPass<'pass>) {
        self.quad_renderer_bg.render(rpass);
        self.waterfall.render(rpass);
        if let Some(note_labels) = self.note_labels.as_mut() {
            note_labels.render(rpass);
        }
        self.quad_renderer_fg.render(rpass);
        if self.show_sheet
            && let Some(sheet) = self.sheet.as_mut()
        {
            sheet.render(rpass);
        }
        if let Some(glow) = &self.glow {
            glow.render(rpass);
        }
        self.text_renderer.render(rpass);

        self.nuon_renderer.render(rpass);
    }

    fn window_event(&mut self, ctx: &mut Context, event: &WindowEvent) {
        self.rewind_controller
            .handle_window_event(ctx, event, &mut self.player);

        if self.rewind_controller.is_rewinding() {
            self.keyboard.reset_notes();
        }

        if event.back_mouse_pressed() || event.key_released(Key::Named(NamedKey::Escape)) {
            ctx.proxy
                .send_event(NeothesiaEvent::MainMenu(Some(self.player.song().clone())))
                .ok();
        }

        if self.finished && event.key_released(Key::Named(NamedKey::Enter)) {
            ctx.proxy
                .send_event(NeothesiaEvent::Play(self.player.song().clone()))
                .ok();
        }

        if self.finished && event.key_released(Key::Named(NamedKey::Backspace)) {
            ctx.proxy
                .send_event(NeothesiaEvent::MainMenu(Some(self.player.song().clone())))
                .ok();
        }

        if !self.finished && event.key_released(Key::Named(NamedKey::Space)) {
            self.player.pause_resume();
        }

        if let Some("m" | "M") = event.character_released() {
            self.show_sheet = !self.show_sheet;
            ctx.config.set_sheet_music(self.show_sheet);
            self.toast_manager.toast(if self.show_sheet {
                "Sheet Music: On"
            } else {
                "Sheet Music: Off"
            });
        }

        handle_settings_input(ctx, &mut self.toast_manager, &mut self.waterfall, event);
        super::handle_pc_keyboard_to_midi_event(ctx, event);
        super::handle_mouse_to_midi_event(
            &mut self.keyboard,
            &mut self.mouse_to_midi_state,
            ctx,
            event,
        );

        if event.window_resized() || event.scale_factor_changed() {
            self.resize(ctx)
        }

        super::handle_nuon_window_event(&mut self.nuon, event, ctx);
    }

    fn midi_event(&mut self, _ctx: &mut Context, channel: u8, message: &MidiMessage) {
        self.player.user_midi_event(channel, message);
        self.keyboard.user_midi_event(message);
    }
}

fn handle_settings_input(
    ctx: &mut Context,
    toast_manager: &mut ToastManager,
    waterfall: &mut WaterfallRenderer,
    event: &WindowEvent,
) {
    if event.key_released(Key::Named(NamedKey::ArrowUp))
        || event.key_released(Key::Named(NamedKey::ArrowDown))
    {
        let amount = if ctx.window_state.modifiers_state.shift_key() {
            0.5
        } else {
            0.1
        };

        if event.key_released(Key::Named(NamedKey::ArrowUp)) {
            ctx.config
                .set_speed_multiplier(ctx.config.speed_multiplier() + amount);
        } else {
            ctx.config
                .set_speed_multiplier(ctx.config.speed_multiplier() - amount);
        }

        toast_manager.speed_toast(ctx.config.speed_multiplier());
        return;
    }

    if event.key_released(Key::Named(NamedKey::PageUp))
        || event.key_released(Key::Named(NamedKey::PageDown))
    {
        let amount = if ctx.window_state.modifiers_state.shift_key() {
            500.0
        } else {
            100.0
        };

        if event.key_released(Key::Named(NamedKey::PageUp)) {
            ctx.config
                .set_animation_speed(ctx.config.animation_speed() + amount);
        } else {
            ctx.config
                .set_animation_speed(ctx.config.animation_speed() - amount);
        }

        waterfall
            .pipeline()
            .set_speed(&ctx.gpu.queue, ctx.config.animation_speed());
        toast_manager.animation_speed_toast(ctx.config.animation_speed());
        return;
    }

    if let Some(ch @ ("_" | "-" | "+" | "=")) = event.character_released() {
        let amount = if ctx.window_state.modifiers_state.shift_key() {
            0.1
        } else {
            0.01
        };

        if matches!(ch, "-" | "_") {
            ctx.config
                .set_animation_offset(ctx.config.animation_offset() - amount);
        } else {
            ctx.config
                .set_animation_offset(ctx.config.animation_offset() + amount);
        }

        toast_manager.offset_toast(ctx.config.animation_offset());
    }
}

/// Turn a MIDI file name into something worth reading on the HUD: drop the
/// extension, and let underscores stand in for the spaces they usually are.
fn song_title(file_name: &str) -> String {
    let stem = file_name
        .rsplit_once('.')
        .filter(|(stem, ext)| {
            !stem.is_empty()
                && (ext.eq_ignore_ascii_case("mid") || ext.eq_ignore_ascii_case("midi"))
        })
        .map(|(stem, _)| stem)
        .unwrap_or(file_name);

    stem.replace('_', " ").trim().to_string()
}
