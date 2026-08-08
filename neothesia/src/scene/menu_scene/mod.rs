mod state;
use bytes::Bytes;
use state::{Page, UiState};

mod midi_picker;
use midi_picker::open_midi_file_picker;

mod neo_btn;
use neo_btn::{neo_btn, neo_btn_icon};

mod favourites;
mod settings;
mod tracks;

use std::{future::Future, time::Duration};

use crate::utils::{BoxFuture, noop_waker_ref, window::WinitEvent};
use neothesia_core::render::{BgPipeline, ImageIdentifier, QuadRenderer, TextRenderer};

use winit::{
    event::WindowEvent,
    keyboard::{Key, NamedKey},
};

use crate::{NeothesiaEvent, context::Context, icons, scene::Scene, song::Song};

use super::NuonRenderer;

type MsgFn = Box<dyn FnOnce(&mut UiState, &mut Context)>;

fn on_async<T, Fut, FN>(future: Fut, f: FN) -> BoxFuture<MsgFn>
where
    T: 'static,
    Fut: Future<Output = T> + Send + 'static,
    FN: FnOnce(T, &mut UiState, &mut Context) + Send + 'static,
{
    Box::pin(async {
        let res = future.await;
        let f: MsgFn = Box::new(move |data, ctx| f(res, data, ctx));
        f
    })
}

#[derive(Default, Debug, Clone, Copy, Eq, PartialEq)]
enum Popup {
    #[default]
    None,
    OutputSelector,
    InputSelector,
}

impl Popup {
    fn toggle(&mut self, new: Self) {
        *self = if *self == new { Self::None } else { new }
    }

    fn close(&mut self) {
        *self = Self::None;
    }
}

pub struct MenuScene {
    bg_pipeline: BgPipeline,
    text_renderer: TextRenderer,
    nuon_renderer: NuonRenderer,

    logo: ImageIdentifier,

    state: UiState,

    context: std::task::Context<'static>,
    futures: Vec<BoxFuture<MsgFn>>,

    quad_pipeline: QuadRenderer,
    nuon: nuon::Ui,

    settings_scroll: nuon::ScrollState,
    favourites: Vec<std::path::PathBuf>,
    fav_selected: usize,
    fav_scroll: nuon::ScrollState,
    /// Where the list was last drawn, in window coordinates: the wheel only
    /// scrolls it while the pointer is inside this. `None` until it has been
    /// drawn, or while there is nothing to list.
    fav_viewport: Option<nuon::Rect>,
    /// Set when the highlight moves, asking the next layout to scroll it back
    /// into view (which needs the viewport height, only known while drawing).
    fav_reveal: bool,
    /// Row index and time of the last favourites click, for double-click detection.
    fav_last_click: Option<(usize, std::time::Instant)>,
    popup: Popup,
}

impl MenuScene {
    pub fn new(ctx: &mut Context, song: Option<Song>) -> Self {
        let iced_state = UiState::new(ctx, song);

        let quad_pipeline = ctx.quad_renderer_factory.new_renderer();
        let text_renderer = ctx.text_renderer_factory.new_renderer();

        let mut nuon_renderer = NuonRenderer::new(ctx);

        let logo = Bytes::from_static(include_bytes!("../../../../assets/banner.png"));
        let logo = nuon_renderer.add_image(neothesia_core::render::Image::new(
            &ctx.gpu.device,
            &ctx.gpu.queue,
            logo,
        ));

        // Rainbow-keys fork: scan the favourites folder up front and highlight
        // the song opened last time, so boot -> Enter replays it and the
        // arrows browse from there.
        let favourites = favourites::scan_favourites();
        let fav_selected = ctx
            .config
            .last_opened_song()
            .and_then(|last| favourites.iter().position(|p| p == last))
            .unwrap_or(0);

        let mut scene = Self {
            bg_pipeline: BgPipeline::new(&ctx.gpu),
            text_renderer,
            state: iced_state,
            nuon_renderer,

            logo,

            context: std::task::Context::from_waker(noop_waker_ref()),
            futures: Vec::new(),

            quad_pipeline,
            nuon: nuon::Ui::new(),
            settings_scroll: nuon::ScrollState::new(),
            favourites,
            fav_selected,
            fav_scroll: nuon::ScrollState::new(),
            fav_viewport: None,
            // The remembered song can be anywhere in the folder, so let the
            // first layout scroll it into view.
            fav_reveal: true,
            fav_last_click: None,
            popup: Popup::None,
        };

        // Nothing loaded yet (fresh install / cleared config)? Load the
        // highlighted favourite so Enter works immediately.
        if scene.state.song.is_none() && !scene.favourites.is_empty() {
            scene.load_selected_favourite(ctx);
        }

        scene
    }

    fn main_ui(&mut self, ctx: &mut Context) {
        if self.state.is_loading() {
            let width = ctx.window_state.logical_size.width;
            let height = ctx.window_state.logical_size.height;

            nuon::label()
                .size(width, height)
                .font_size(30.0)
                .text("Loading...")
                .text_justify(nuon::TextJustify::Center)
                .build(&mut self.nuon);
            return;
        }

        let mut nuon = std::mem::replace(&mut self.nuon, nuon::Ui::new());

        match self.state.current() {
            Page::Exit => self.exit_page_ui(ctx, &mut nuon),
            Page::Main => self.main_page_ui(ctx, &mut nuon),
            Page::Settings => self.settings_page_ui(ctx, &mut nuon),
        }

        self.nuon = nuon;
    }

    fn exit_page_ui(&mut self, ctx: &mut Context, ui: &mut nuon::Ui) {
        let win_w = ctx.window_state.logical_size.width;
        let win_h = ctx.window_state.logical_size.height;

        let btn_w = 320.0;
        let btn_h = 50.0;
        let btn_gap = 5.0;

        let text_h = 80.0;

        let full_w = btn_w * 2.0 + btn_gap;
        let full_h = btn_h + text_h;

        nuon::translate()
            .x(nuon::center_x(win_w, full_w))
            .y(nuon::center_y(win_h, full_h))
            .build(ui, |ui| {
                nuon::label()
                    .text("Do you want to exit?")
                    .font_size(30.0)
                    .size(full_w, text_h)
                    .build(ui);

                nuon::translate().y(text_h).add_to_current(ui);

                if neo_btn().size(btn_w, btn_h).label("No").build(ui) {
                    self.state.go_back();
                }

                nuon::translate().x(btn_w).add_to_current(ui);
                nuon::translate().x(btn_gap).add_to_current(ui);

                if neo_btn().size(btn_w, btn_h).label("Yes").build(ui) {
                    ctx.proxy.send_event(NeothesiaEvent::Exit).ok();
                }
            });
    }

    fn main_page_ui(&mut self, ctx: &mut Context, ui: &mut nuon::Ui) {
        let win_w = ctx.window_state.logical_size.width;
        let win_h = ctx.window_state.logical_size.height;

        let w = 450.0;
        // A little slimmer than stock (80) to leave room for the favourites
        // list below the menu.
        let h = 56.0;
        let gap = 10.0;

        // KEYBOARD HERO banner is a wider lockup than the old one (1208x166),
        // so keep the width and let the height follow the aspect.
        let logo_w = 650.0;
        let logo_h = 650.0 * 166.0 / 1208.0;
        let post_logo_gap = 24.0;

        let menu_top = win_h / 6.0;

        nuon::translate()
            .x(win_w / 2.0)
            .y(menu_top)
            .build(ui, |ui| {
                nuon::image(self.logo)
                    .x(-logo_w / 2.0)
                    .size(logo_w, logo_h)
                    .build(ui);

                nuon::translate()
                    .x(-w / 2.0)
                    .y(logo_h + post_logo_gap)
                    .build(ui, |ui| {
                        // Favourites sit between the logo and the buttons.
                        // They take only the room they need, capped at what
                        // is left once the buttons and bottom bar have
                        // theirs — so a long folder scrolls rather than
                        // pushing the menu off screen.
                        let list_x = win_w / 2.0 - w / 2.0;
                        let list_top = menu_top + logo_h + post_logo_gap;
                        let list_gap = 16.0;

                        let buttons_h = 3.0 * h + 2.0 * gap;
                        // The track rows sit between the list and the buttons
                        // and have first claim on the room — the favourites
                        // list is the elastic one. (The performer selector is
                        // in the corner and takes nothing from this column.)
                        let tracks_h = self.track_list_height();
                        let tracks_gap = if tracks_h > 0.0 { 14.0 } else { 0.0 };
                        let list_room = win_h
                            - favourites::BOTTOM_RESERVED
                            - buttons_h
                            - list_gap
                            - list_top
                            - tracks_h
                            - tracks_gap;
                        let list_h = self.favourites_preferred_height().min(list_room.max(0.0));

                        // Scoped: the list advances the origin as it draws,
                        // and the buttons below need a known starting point.
                        nuon::translate().build(ui, |ui| {
                            let area = nuon::Rect::new(
                                nuon::Point::new(list_x, list_top),
                                nuon::Size::new(w, list_h),
                            );
                            self.favourites_list_ui(ctx, ui, area, win_w, win_h);
                        });

                        nuon::translate().y(list_h + list_gap).add_to_current(ui);

                        self.track_list_ui(ctx, ui, w);

                        nuon::translate().y(tracks_h + tracks_gap).add_to_current(ui);

                        if neo_btn().size(w, h).label("Select File").build(ui) {
                            self.futures.push(open_midi_file_picker(&mut self.state));
                        }

                        nuon::translate().y(h + gap).add_to_current(ui);

                        if neo_btn().size(w, h).label("Settings").build(ui) {
                            self.state.go_to(Page::Settings);
                        }

                        nuon::translate().y(h + gap).add_to_current(ui);

                        if neo_btn().size(w, h).label("Exit").build(ui) {
                            self.state.go_back();
                        }
                    });
            });

        nuon::translate().x(0.0).y(win_h).build(ui, |ui| {
            let gap = 10.0;
            let btn_w = 80.0;
            let btn_h = 60.0;

            nuon::translate().y(-gap).add_to_current(ui);
            nuon::translate().y(-btn_h).add_to_current(ui);

            if let Some(song) = self.state.song() {
                nuon::label()
                    .text(&song.file.name)
                    .size(win_w, 60.0)
                    .font_size(16.0)
                    .build(ui);
            }

            nuon::translate().build(ui, |ui| {
                nuon::translate().x(gap).add_to_current(ui);

                if neo_btn()
                    .size(btn_w, btn_h)
                    .icon(icons::balloon_icon())
                    .color([100; 3])
                    .tooltip("FreePlay")
                    .build(ui)
                {
                    state::freeplay(&self.state, ctx);
                }
            });

            if self.state.song().is_none() {
                return;
            }

            nuon::translate().x(win_w).build(ui, |ui| {
                nuon::translate().x(-btn_w - gap).add_to_current(ui);

                if neo_btn()
                    .size(btn_w, btn_h)
                    .icon(icons::play_icon())
                    .tooltip("Play")
                    .build(ui)
                {
                    state::play(&self.state, ctx);
                }
            });
        });

        // Corner-anchored, and drawn last: hit testing goes to whatever was
        // drawn on top, and the favourites list reaches across the window.
        self.performer_selector_ui(ctx, ui);
    }
}

impl Scene for MenuScene {
    #[profiling::function]
    fn update(&mut self, ctx: &mut Context, delta: Duration) {
        self.quad_pipeline.clear();
        self.bg_pipeline.update_time(delta);
        self.state.tick(ctx);

        self.futures
            .retain_mut(|f| match f.as_mut().poll(&mut self.context) {
                std::task::Poll::Ready(msg) => {
                    msg(&mut self.state, ctx);
                    false
                }
                std::task::Poll::Pending => true,
            });

        self.state.tick(ctx);

        self.main_ui(ctx);

        super::render_nuon(&mut self.nuon, &mut self.nuon_renderer, ctx);

        self.text_renderer.update(
            ctx.window_state.physical_size,
            ctx.window_state.scale_factor as f32,
        );
        self.quad_pipeline.prepare();
    }

    #[profiling::function]
    fn render<'pass>(&'pass mut self, rpass: &mut wgpu_jumpstart::RenderPass<'pass>) {
        self.bg_pipeline.render(rpass);
        self.quad_pipeline.render(rpass);
        self.text_renderer.render(rpass);
        self.nuon_renderer.render(rpass);
    }

    fn window_event(&mut self, ctx: &mut Context, event: &WindowEvent) {
        if let WindowEvent::MouseWheel { delta, .. } = event {
            let amount = match delta {
                winit::event::MouseScrollDelta::LineDelta(_, y) => y * 60.0,
                winit::event::MouseScrollDelta::PixelDelta(position) => position.y as f32,
            };

            let cursor = ctx.window_state.cursor_logical_position;
            let over_favourites = *self.state.current() == Page::Main
                && self.favourites_scroll(nuon::Point::new(cursor.x, cursor.y), amount);

            // The favourites list sits on the main page, where the settings
            // list is not visible, but keep the wheel to one list at a time.
            if !over_favourites {
                self.settings_scroll.update(amount);
            }
        }

        if event.cursor_moved() {
            self.nuon.mouse_move(
                ctx.window_state.cursor_logical_position.x,
                ctx.window_state.cursor_logical_position.y,
            );
        } else if event.left_mouse_pressed() {
            self.nuon.mouse_down();
        } else if event.left_mouse_released() {
            self.nuon.mouse_up();
        } else if event.back_mouse_pressed() {
            self.state.go_back();
        }

        match self.state.current() {
            Page::Exit => {
                if event.key_pressed(Key::Named(NamedKey::Enter)) {
                    ctx.proxy.send_event(NeothesiaEvent::Exit).unwrap();
                }

                if event.key_pressed(Key::Named(NamedKey::Escape)) {
                    self.state.go_back();
                }
            }
            Page::Main => {
                if event.key_pressed(Key::Named(NamedKey::Tab)) {
                    self.futures.push(open_midi_file_picker(&mut self.state));
                }

                if event.key_pressed(Key::Named(NamedKey::ArrowUp)) {
                    self.favourites_move(ctx, -1);
                }

                if event.key_pressed(Key::Named(NamedKey::ArrowDown)) {
                    self.favourites_move(ctx, 1);
                }

                if event.key_pressed(Key::Named(NamedKey::Enter)) {
                    state::play(&self.state, ctx)
                }

                if event.key_pressed(Key::Named(NamedKey::Escape)) {
                    self.state.go_back();
                }

                if event.key_pressed(Key::Character("s")) {
                    self.state.go_to(Page::Settings);
                }

                if event.key_pressed(Key::Character("f")) {
                    state::freeplay(&self.state, ctx);
                }
            }
            Page::Settings => {
                if event.key_pressed(Key::Named(NamedKey::Escape)) {
                    self.state.go_back();
                }
            }
        }
    }
}
