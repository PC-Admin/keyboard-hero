use midi_file::midly::MidiMessage;
use neothesia_core::render::{
    GlowRenderer, GuidelineRenderer, NoteLabels, QuadRenderer, TextRenderer,
};
use std::time::Duration;
use winit::{
    event::WindowEvent,
    keyboard::{Key, NamedKey},
};

use self::top_bar::TopBar;

use super::{NuonRenderer, Scene};
use crate::{
    NeothesiaEvent, context::Context, render::WaterfallRenderer, scene::MouseToMidiEventState,
    song::Song, utils::window::WinitEvent,
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

pub struct PlayingScene {
    keyboard: Keyboard,
    waterfall: WaterfallRenderer,
    guidelines: GuidelineRenderer,
    text_renderer: TextRenderer,
    nuon_renderer: NuonRenderer,

    note_labels: Option<NoteLabels>,

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

        let player = MidiPlayer::new(
            ctx.output_manager.connection().clone(),
            song,
            keyboard_layout.range.clone(),
            ctx.config.separate_channels(),
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

        // Keyboard-hero mode: Auto tracks report their own notes as perfect
        // hits, so the effects run whether a human is playing or the song is.
        // Only freeze the tallies once the results screen is up.
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
            }
        }

        self.effects.update(dt, hit_line_y, pos.x, board_width);
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
                "PERFECT {}    GOOD {}    OK {}    WRONG {}",
                results.perfect, results.good, results.ok, results.wrong
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

        nuon::label()
            .text("Enter: play again    Backspace: menu")
            .font_size(15.0)
            .color(nuon::Color::new_u8(160, 160, 160, 1.0))
            .y(top + 330.0)
            .height(16.0)
            .width(win_w)
            .build(&mut self.nuon);
    }

    fn update_hud(&mut self, ctx: &Context) {
        if self.finished {
            self.results_overlay_ui(ctx);
            return;
        }

        // Slide the whole top-left block down as the top bar expands so the
        // dropdown never covers it.
        let hud_top = self
            .top_bar
            .topbar_expand_animation
            .animate_bool(0.0, 75.0, ctx.frame_timestamp);

        // --- top-right performer toggle --------------------------------------
        // One click hands the song to the machine (AUTO light show) or takes
        // it back (HUMAN play-along). Tallies keep running across the switch.
        {
            let win_w = ctx.window_state.logical_size.width;
            let human = self.player.has_human_track();
            let (label, color) = if human {
                ("HUMAN", nuon::Color::new_u8(160, 81, 238, 1.0))
            } else {
                ("AUTO", nuon::Color::new_u8(58, 58, 70, 1.0))
            };

            let (w, h) = (96.0, 32.0);
            if nuon::button()
                .id("performer-toggle")
                .pos(win_w - w - 16.0, hud_top + 10.0)
                .size(w, h)
                .color(color)
                .border_radius([8.0; 4])
                .label(label)
                .build(&mut self.nuon)
            {
                self.player.toggle_human();
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

            nuon::label()
                .text(format!("SCORE {}", effects::thousands(self.effects.score())))
                .font_size(14.0)
                .color(nuon::Color::new_u8(255, 222, 84, 1.0))
                .bold(true)
                .text_justify(nuon::TextJustify::Left)
                .pos(16.0, hud_top + 86.0)
                .size(220.0, 14.0)
                .build(&mut self.nuon);

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

        if self.player.play_along().are_required_keys_pressed() {
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

        self.effects.render(&mut self.quad_renderer_fg);
        self.effects.render_note_flashes(
            &mut self.quad_renderer_fg,
            time,
            ctx.config.animation_speed() / ctx.window_state.scale_factor as f32,
            self.keyboard.pos().y,
        );
        self.update_hud(ctx);

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
