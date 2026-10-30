use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use calloop::ping::{Ping, make_ping};
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, LoopSignal};
use calloop_wayland_source::WaylandSource;
use slint::platform::WindowEvent;
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel,
};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, LogicalSize};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::delegate_compositor;
use smithay_client_toolkit::delegate_layer;
use smithay_client_toolkit::delegate_output;
use smithay_client_toolkit::delegate_pointer;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::delegate_seat;
use smithay_client_toolkit::delegate_shm;
use smithay_client_toolkit::globals::GlobalData;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::ShmHandler;
use smithay_client_toolkit::shm::slot::SlotPool;
use tracing::{debug, info, warn};
use wayland_client::protocol::wl_output;
use wayland_client::protocol::wl_pointer;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_seat;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, globals::registry_queue_init};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;

use crate::core::{AppEvent, AppState, EventSender};
use crate::platform::kde::KWinBlur;
use crate::ui::Island;

pub const HOST_WIDTH: f32 = 420.0;
pub const HOST_HEIGHT: f32 = 180.0;
const TOP_MARGIN: i32 = 8;
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const EXPAND_DURATION: Duration = Duration::from_millis(280);

/// Shared state between Slint (UI thread) and the Wayland host.
pub struct Shared {
    pub ping: Ping,
    pub ui: Option<slint::Weak<Island>>,
    pub expanded: bool,
    /// 0.0 = compact, 1.0 = expanded.
    pub expansion: f32,
    pub pill_w: f32,
    pub pill_h: f32,
    pub quit: bool,
    suppress_ping: u32,
    animation: Option<Animation>,
    notification_auto_expanded: bool,
    notification_generation: u64,
    expansion_observers: Vec<Box<dyn Fn(bool)>>,
}

#[derive(Clone, Copy)]
struct Animation {
    start: Instant,
    duration: Duration,
    from: f32,
    to: f32,
}

impl Shared {
    fn animate_to(&mut self, target: f32) {
        self.expanded = target > 0.5;
        self.animation = Some(Animation {
            start: Instant::now(),
            duration: EXPAND_DURATION,
            from: self.expansion,
            to: target,
        });
        self.publish_expansion();
        self.ping.ping();
    }

    fn publish_expansion(&self) {
        for observer in &self.expansion_observers {
            observer(self.expanded);
        }
    }

    pub fn add_expansion_observer(&mut self, observer: Box<dyn Fn(bool)>) {
        observer(self.expanded);
        self.expansion_observers.push(observer);
    }

    pub fn toggle(&mut self) {
        self.notification_auto_expanded = false;
        let target = if self.expanded { 0.0 } else { 1.0 };
        self.animate_to(target);
    }

    pub fn show_notification(&mut self) -> u64 {
        self.notification_generation = self.notification_generation.wrapping_add(1);
        if !self.expanded {
            self.notification_auto_expanded = true;
            self.animate_to(1.0);
        }
        self.notification_generation
    }

    pub fn clear_notification(&mut self) {
        self.notification_generation = self.notification_generation.wrapping_add(1);
        if self.notification_auto_expanded {
            self.notification_auto_expanded = false;
            self.animate_to(0.0);
        } else {
            self.ping.ping();
        }
    }

    pub fn quit(&mut self) {
        self.quit = true;
        self.ping.ping();
    }
}

/// A small BGRA pixel used directly as a wl_shm ARGB8888 texel on little endian.
///
/// Slint's own `PremultipliedRgbaColor` is R,G,B,A in memory, while wl_shm
/// ARGB8888 expects B,G,R,A on little endian. Implementing `TargetPixel` for
/// this local type avoids a second full-frame swizzle pass.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
struct Bgra8 {
    b: u8,
    g: u8,
    r: u8,
    a: u8,
}

impl TargetPixel for Bgra8 {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let a = (u8::MAX - color.alpha) as u16;
        self.r = (self.r as u16 * a / 255) as u8 + color.red;
        self.g = (self.g as u16 * a / 255) as u8 + color.green;
        self.b = (self.b as u16 * a / 255) as u8 + color.blue;
        self.a =
            (self.a as u16 + color.alpha as u16 - (self.a as u16 * color.alpha as u16) / 255) as u8;
    }

    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self {
            b: blue,
            g: green,
            r: red,
            a: 255,
        }
    }

    fn background() -> Self {
        Self::default()
    }
}

/// Slint adapter. It delegates rendering to `MinimalSoftwareWindow` and turns
/// Slint redraw requests into a calloop wakeup.
struct IslandWindowAdapter {
    inner: Rc<MinimalSoftwareWindow>,
    shared: Rc<RefCell<Shared>>,
}

impl WindowAdapter for IslandWindowAdapter {
    fn window(&self) -> &slint::Window {
        self.inner.window()
    }

    fn renderer(&self) -> &dyn slint::platform::Renderer {
        self.inner.renderer()
    }

    fn size(&self) -> slint::PhysicalSize {
        self.inner.size()
    }

    fn set_size(&self, size: slint::WindowSize) {
        self.inner.set_size(size);
    }

    fn request_redraw(&self) {
        self.inner.request_redraw();
        if let Ok(shared) = self.shared.try_borrow()
            && shared.suppress_ping == 0
        {
            shared.ping.ping();
        }
    }
}

struct LayerHandles {
    _layer_surface: LayerSurface,
    surface: WlSurface,
}

pub struct WaylandState {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    qh: QueueHandle<WaylandState>,

    shared: Rc<RefCell<Shared>>,
    window: Rc<MinimalSoftwareWindow>,
    loop_handle: LoopHandle<'static, WaylandState>,
    loop_signal: LoopSignal,

    blur: Option<KWinBlur>,
    layer: Option<LayerHandles>,
    viewport: Option<WpViewport>,
    fractional_scale_obj: Option<WpFractionalScaleV1>,
    fractional_scale_value: Option<f32>,
    pool: Option<SlotPool>,
    scale: f32,
    host_w: i32,
    host_h: i32,
    configured: bool,
    pointer: Option<wl_pointer::WlPointer>,
    anim_timer_active: bool,
    progress_timer_active: bool,
    notification_visible: bool,
    media_playing: bool,
    media_length_us: u64,
    media_progress_base: Option<(u64, u64, Instant)>,
    last_art_path: Option<String>,
    app_state: AppState,
}

impl WaylandState {
    fn ensure_pool(&mut self) {
        if self.pool.is_some() {
            return;
        }
        let (width, height) = buffer_dimensions(self.host_w, self.host_h, self.scale);
        let len = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        match SlotPool::new(len.max(1), &self.shm) {
            Ok(pool) => self.pool = Some(pool),
            Err(error) => warn!(%error, "failed to create wl_shm pool"),
        }
    }

    fn geometry(&self) -> (i32, i32, i32, i32, f32) {
        let shared = self.shared.borrow();
        pill_geometry(
            self.host_w,
            self.host_h,
            shared.pill_w,
            shared.pill_h,
            shared.expansion,
        )
    }

    fn update_viewport(&self) {
        if let Some(viewport) = &self.viewport {
            let (width, height) = buffer_dimensions(self.host_w, self.host_h, self.scale);
            viewport.set_source(0.0, 0.0, width as f64, height as f64);
            viewport.set_destination(self.host_w, self.host_h);
        }
    }

    fn set_effective_scale(&mut self, scale: f32) {
        let scale = scale.clamp(0.5, 4.0);
        if (self.scale - scale).abs() < 0.001 {
            return;
        }
        self.scale = scale;
        self.pool = None;
        self.window
            .window()
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
        self.window
            .set_size(LogicalSize::new(self.host_w as f32, self.host_h as f32));
        self.update_viewport();
        self.window.request_redraw();
        debug!(scale, "effective surface scale changed");
    }

    fn redraw(&mut self) {
        if !self.configured || self.layer.is_none() {
            return;
        }
        let Some(surface) = self.layer.as_ref().map(|layer| layer.surface.clone()) else {
            return;
        };
        let compositor = self.compositor.clone();
        let qh = self.qh.clone();
        let (width, height) = buffer_dimensions(self.host_w, self.host_h, self.scale);
        if width <= 0 || height <= 0 {
            return;
        }
        let buffer_scale = if self.viewport.is_some() {
            1
        } else {
            self.scale.round().max(1.0) as i32
        };
        let (x, y, w, h, radius) = self.geometry();
        self.ensure_pool();
        let Some(pool) = self.pool.as_mut() else {
            return;
        };
        let blur = self.blur.as_ref();
        let window = self.window.clone();

        let drew = window.draw_if_needed(|renderer| {
            let stride = width * 4;
            let (buffer, canvas) =
                match pool.create_buffer(width, height, stride, wl_shm::Format::Argb8888) {
                    Ok(value) => value,
                    Err(error) => {
                        warn!(%error, "failed to allocate wl_shm buffer");
                        return;
                    }
                };

            canvas.fill(0);
            let pixels: &mut [Bgra8] = bytemuck::cast_slice_mut(canvas);
            let _ = renderer.render(pixels, width as usize);

            let alpha_at = |px: i32, py: i32| -> u8 {
                let index = py.max(0) as usize * width as usize + px.max(0) as usize;
                pixels.get(index).map_or(0, |pixel| pixel.a)
            };
            let outside_alpha = [
                alpha_at(2, 2),
                alpha_at(width - 3, 2),
                alpha_at(2, height - 3),
                alpha_at(width - 3, height - 3),
                alpha_at(width / 2, height - 3),
            ]
            .into_iter()
            .max()
            .unwrap_or(0);
            debug!(outside_alpha, "frame alpha outside pill");

            let input_region = compositor.wl_compositor().create_region(&qh, ());
            add_rounded_rect(&input_region, x, y, w, h, radius);
            surface.set_input_region(Some(&input_region));

            // The blur region must be committed before the surface commit so the
            // compositor never applies default (full-surface) blur to this frame.
            let blur_region = if let Some(blur) = blur {
                let region = compositor.wl_compositor().create_region(&qh, ());
                add_rounded_rect(&region, x, y, w, h, radius);
                blur.set_region(Some(&region));
                Some(region)
            } else {
                None
            };

            // Explicitly declare that this surface has no opaque region. The
            // 420x180 host is a transparent compositing host; only pill pixels
            // are visible.
            surface.set_opaque_region(None);
            surface.set_buffer_scale(buffer_scale);
            surface.damage_buffer(0, 0, width, height);

            if let Err(error) = buffer.attach_to(&surface) {
                warn!(%error, "failed to attach wl_shm buffer");
            }
            surface.commit();
            input_region.destroy();
            if let Some(region) = blur_region {
                region.destroy();
            }
        });

        if drew {
            debug!(width, height, "presented island frame");
        }
    }

    fn advance_animation(&mut self) {
        let (animation, ui) = {
            let shared = self.shared.borrow();
            (shared.animation, shared.ui.clone())
        };

        let Some(animation) = animation else {
            return;
        };
        let Some(ui) = ui.and_then(|weak| weak.upgrade()) else {
            self.shared.borrow_mut().animation = None;
            return;
        };

        let now = Instant::now();
        let elapsed = now.saturating_duration_since(animation.start);
        let t = (elapsed.as_secs_f32() / animation.duration.as_secs_f32()).clamp(0.0, 1.0);
        let expansion = animation.from + (animation.to - animation.from) * ease_in_out_cubic(t);

        {
            let mut shared = self.shared.borrow_mut();
            shared.suppress_ping = shared.suppress_ping.saturating_add(1);
        }
        ui.set_expansion(expansion);
        let pill_w = ui.get_pill_width();
        let pill_h = ui.get_pill_height();
        {
            let mut shared = self.shared.borrow_mut();
            shared.suppress_ping = shared.suppress_ping.saturating_sub(1);
            shared.expansion = expansion;
            shared.pill_w = pill_w;
            shared.pill_h = pill_h;
            if t >= 1.0 {
                shared.animation = None;
            }
        }
    }

    fn on_ping(&mut self) {
        if self.shared.borrow().quit {
            self.loop_signal.stop();
            return;
        }

        self.advance_animation();
        self.redraw();

        if self.shared.borrow().animation.is_some() {
            self.schedule_animation_timer();
        }
        self.ensure_progress_timer();
    }

    fn ensure_progress_timer(&mut self) {
        if self.progress_timer_active
            || self.notification_visible
            || !self.shared.borrow().expanded
            || !self.media_playing
            || self.media_length_us == 0
        {
            return;
        }

        self.progress_timer_active = true;
        debug!("media progress timer started");
        let timer = Timer::from_duration(Duration::from_secs(1));
        let result = self
            .loop_handle
            .insert_source(timer, |_, _, state: &mut WaylandState| {
                state.update_media_progress()
            });

        if result.is_err() {
            self.progress_timer_active = false;
            warn!("failed to schedule media progress timer");
        }
    }

    fn update_media_progress(&mut self) -> TimeoutAction {
        if self.notification_visible
            || !self.shared.borrow().expanded
            || !self.media_playing
            || self.media_length_us == 0
        {
            self.progress_timer_active = false;
            return TimeoutAction::Drop;
        }

        if let Some((base, length, start)) = self.media_progress_base {
            let elapsed = start.elapsed().as_micros() as u64;
            let position = base.saturating_add(elapsed).min(length);
            let progress = (position as f32 / length as f32).clamp(0.0, 1.0);
            debug!(position, length, progress, "media progress tick");
            if let Some(ui) = self
                .shared
                .borrow()
                .ui
                .clone()
                .and_then(|weak| weak.upgrade())
            {
                ui.set_media_progress(progress);
                ui.set_media_time_text(format_media_time(position, length).into());
            }
        }

        TimeoutAction::ToDuration(Duration::from_secs(1))
    }

    fn schedule_animation_timer(&mut self) {
        if self.anim_timer_active {
            return;
        }
        self.anim_timer_active = true;
        let timer = Timer::from_duration(FRAME_INTERVAL);
        let result = self
            .loop_handle
            .insert_source(timer, |_, _, state: &mut WaylandState| {
                state.advance_animation();
                state.redraw();
                if state.shared.borrow().animation.is_some() {
                    TimeoutAction::ToDuration(FRAME_INTERVAL)
                } else {
                    state.anim_timer_active = false;
                    TimeoutAction::Drop
                }
            });

        if result.is_err() {
            self.anim_timer_active = false;
            warn!("failed to schedule island animation timer");
        }
    }

    fn apply_event(&mut self, event: AppEvent) {
        let ui = self
            .shared
            .borrow()
            .ui
            .clone()
            .and_then(|weak| weak.upgrade());
        let Some(ui) = ui else {
            return;
        };

        match &event {
            AppEvent::Battery(Some(status)) => {
                self.app_state.battery = Some(*status);
                let percent = (status.percentage * 100.0).round() as i32;
                ui.set_battery_level(status.percentage);
                ui.set_charging(status.state.is_charging());
                ui.set_battery_percent_text(format!("{percent}%").into());
                ui.set_battery_text(format!("{} · {percent}%", status.state.label()).into());
            }
            AppEvent::Battery(None) => {
                self.app_state.battery = None;
                ui.set_battery_level(-1.0);
                ui.set_charging(false);
                ui.set_battery_percent_text(String::new().into());
                ui.set_battery_text("Battery unavailable".into());
            }
            AppEvent::Media(Some(media)) => {
                self.app_state.media = Some(media.clone());
                ui.set_media_title(media.title.clone().into());
                ui.set_media_detail(
                    if media.artist.is_empty() {
                        if media.playing {
                            "Playing".to_string()
                        } else {
                            "Not playing".to_string()
                        }
                    } else {
                        media.artist.clone()
                    }
                    .into(),
                );
                let initial = media
                    .title
                    .chars()
                    .next()
                    .and_then(|character| character.to_uppercase().next())
                    .map(|character| character.to_string())
                    .unwrap_or_else(|| "♪".to_string());
                ui.set_media_initial(initial.into());

                if self.last_art_path.as_deref() != media.art_path.as_deref() {
                    self.last_art_path = media.art_path.clone();
                    if let Some(artwork) = media.art_path.as_deref().and_then(load_artwork) {
                        debug!(path = ?media.art_path, "media artwork loaded");
                        ui.set_media_art(artwork);
                        ui.set_media_art_available(true);
                    } else {
                        ui.set_media_art_available(false);
                    }
                }

                self.media_playing = media.playing;
                self.media_length_us = media.length_us.unwrap_or(0);
                let position = media.position_us.unwrap_or(0).min(self.media_length_us);
                self.media_progress_base = (self.media_length_us > 0).then_some((
                    position,
                    self.media_length_us,
                    Instant::now(),
                ));
                let progress = if self.media_length_us > 0 {
                    position as f32 / self.media_length_us as f32
                } else {
                    0.0
                };
                ui.set_media_progress(progress.clamp(0.0, 1.0));
                ui.set_media_progress_available(self.media_length_us > 0);
                ui.set_media_time_text(format_media_time(position, self.media_length_us).into());
                self.ensure_progress_timer();
            }
            AppEvent::Media(None) => {
                self.app_state.media = None;
                self.last_art_path = None;
                self.media_playing = false;
                self.media_length_us = 0;
                self.media_progress_base = None;
                ui.set_media_title("Nothing playing".into());
                ui.set_media_detail("No active player".into());
                ui.set_media_initial("♪".into());
                ui.set_media_art_available(false);
                ui.set_media_progress(0.0);
                ui.set_media_progress_available(false);
                ui.set_media_time_text(String::new().into());
            }
            AppEvent::Volume(Some(volume)) => {
                self.app_state.volume = Some(*volume);
                ui.set_volume_available(true);
                ui.set_volume_muted(volume.muted);
                let text = if volume.muted {
                    "MUTED".to_string()
                } else {
                    format!("VOL {:.0}%", volume.level * 100.0)
                };
                ui.set_volume_text(text.into());
            }
            AppEvent::Volume(None) => {
                self.app_state.volume = None;
                ui.set_volume_available(false);
                ui.set_volume_muted(false);
                ui.set_volume_text(String::new().into());
            }
            AppEvent::Notification(Some(notification)) => {
                self.app_state.notification = Some(notification.clone());
                self.notification_visible = true;
                ui.set_notification_active(true);
                let initial = notification
                    .app
                    .chars()
                    .next()
                    .and_then(|character| character.to_uppercase().next())
                    .map(|character| character.to_string())
                    .unwrap_or_else(|| "!".to_string());
                ui.set_notification_initial(initial.into());
                ui.set_notification_app(notification.app.clone().into());
                ui.set_notification_summary(notification.summary.clone().into());
                ui.set_notification_body(notification.body.clone().into());
                let generation = {
                    let mut shared = self.shared.borrow_mut();
                    shared.show_notification()
                };
                let display_ms = notification.display_ms;
                let timer = Timer::from_duration(Duration::from_millis(display_ms));
                let result =
                    self.loop_handle
                        .insert_source(timer, move |_, _, state: &mut WaylandState| {
                            if state.shared.borrow().notification_generation == generation {
                                state.apply_event(AppEvent::Notification(None));
                            }
                            TimeoutAction::Drop
                        });
                if result.is_err() {
                    warn!(display_ms, "failed to schedule notification timeout");
                }
            }
            AppEvent::Notification(None) => {
                self.app_state.notification = None;
                self.notification_visible = false;
                ui.set_notification_active(false);
                ui.set_notification_app(String::new().into());
                ui.set_notification_summary(String::new().into());
                ui.set_notification_body(String::new().into());
                self.shared.borrow_mut().clear_notification();
                self.ensure_progress_timer();
            }
            AppEvent::System(Some(system)) => {
                self.app_state.system = Some(*system);
                ui.set_system_available(true);
                ui.set_cpu_text(format!("CPU {:.0}%", system.cpu * 100.0).into());
                ui.set_ram_text(format!("RAM {:.0}%", system.ram * 100.0).into());
            }
            AppEvent::System(None) => {
                self.app_state.system = None;
                ui.set_system_available(false);
                ui.set_cpu_text(String::new().into());
                ui.set_ram_text(String::new().into());
            }
        }

        self.window.request_redraw();
    }
}

impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        new_factor: i32,
    ) {
        let new_factor = new_factor.max(1);
        let effective = self.fractional_scale_value.unwrap_or(new_factor as f32);
        self.set_effective_scale(effective);
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl SeatHandler for WaylandState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => {
                    self.pointer = Some(pointer);
                    debug!("pointer capability acquired");
                }
                Err(error) => warn!(%error, "failed to create pointer"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            // A removed seat cannot deliver Leave, so clear Slint's hover and
            // pressed state explicitly to avoid a stuck TouchArea.
            self.window
                .window()
                .dispatch_event(WindowEvent::PointerExited);
            pointer.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl PointerHandler for WaylandState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        let Some(surface) = self.layer.as_ref().map(|layer| layer.surface.clone()) else {
            return;
        };
        let window = self.window.window();

        for event in events {
            if event.surface != surface {
                continue;
            }
            // wl_pointer coordinates are surface-local logical coordinates.
            let position =
                slint::LogicalPosition::new(event.position.0 as f32, event.position.1 as f32);

            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    window.dispatch_event(WindowEvent::PointerMoved { position });
                }
                PointerEventKind::Leave { .. } => {
                    window.dispatch_event(WindowEvent::PointerExited);
                }
                PointerEventKind::Press { button, .. } => {
                    if let Some(button) = map_button(button) {
                        debug!(x = position.x, y = position.y, ?button, "pointer press");
                        window.dispatch_event(WindowEvent::PointerPressed { position, button });
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if let Some(button) = map_button(button) {
                        debug!(x = position.x, y = position.y, ?button, "pointer release");
                        window.dispatch_event(WindowEvent::PointerReleased { position, button });
                    }
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    window.dispatch_event(WindowEvent::PointerScrolled {
                        position,
                        delta_x: horizontal.absolute as f32,
                        delta_y: vertical.absolute as f32,
                    });
                }
            }
        }
    }
}

impl LayerShellHandler for WaylandState {
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (width, height) = configure.new_size;
        if width > 0 && height > 0 && (width as i32 != self.host_w || height as i32 != self.host_h)
        {
            debug!(
                width,
                height,
                host_w = self.host_w,
                host_h = self.host_h,
                "compositor suggested a different layer size; keeping configured host size"
            );
        }

        if !self.configured {
            self.configured = true;
            self.window.request_redraw();
            self.ensure_pool();
            self.redraw();
            if let Some(ui) = self
                .shared
                .borrow()
                .ui
                .clone()
                .and_then(|weak| weak.upgrade())
            {
                let _ = ui;
            }
        }
    }

    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        info!("layer surface closed by compositor");
        {
            let mut shared = self.shared.borrow_mut();
            shared.quit = true;
            shared.ping.ping();
        }
        self.loop_signal.stop();
    }
}

impl Dispatch<WlRegion, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &WlRegion,
        _: <WlRegion as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        unreachable!("wl_region has no events");
    }
}

impl Dispatch<OrgKdeKwinBlurManager, GlobalData> for WaylandState {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinBlurManager,
        _: <OrgKdeKwinBlurManager as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        unreachable!("org_kde_kwin_blur_manager has no events");
    }
}

impl Dispatch<OrgKdeKwinBlur, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinBlur,
        _: <OrgKdeKwinBlur as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        unreachable!("org_kde_kwin_blur has no events");
    }
}

impl Dispatch<WpFractionalScaleManagerV1, GlobalData> for WaylandState {
    fn event(
        _: &mut Self,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        unreachable!("wp_fractional_scale_manager_v1 has no events");
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: <WpFractionalScaleV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let scale = scale as f32 / 120.0;
            state.fractional_scale_value = Some(scale);
            state.set_effective_scale(scale);
        }
    }
}

impl Dispatch<WpViewporter, GlobalData> for WaylandState {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        unreachable!("wp_viewporter has no events");
    }
}

impl Dispatch<WpViewport, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        unreachable!("wp_viewport has no events");
    }
}

delegate_compositor!(WaylandState);
delegate_output!(WaylandState);
delegate_shm!(WaylandState);
delegate_seat!(WaylandState);
delegate_pointer!(WaylandState);
delegate_layer!(WaylandState);
delegate_registry!(WaylandState);

pub struct WaylandRuntime {
    conn: Connection,
    event_queue: Option<wayland_client::EventQueue<WaylandState>>,
    state: WaylandState,
    event_loop: EventLoop<'static, WaylandState>,
    shared: Rc<RefCell<Shared>>,
    event_tx: EventSender,
}

impl WaylandRuntime {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        let conn = Connection::connect_to_env()?;
        let (globals, mut event_queue) = registry_queue_init::<WaylandState>(&conn)?;
        let qh = event_queue.handle();

        let event_loop = EventLoop::<WaylandState>::try_new()?;
        let loop_handle = event_loop.handle();
        let loop_signal = event_loop.get_signal();
        let (ping, ping_source) = make_ping()?;
        event_loop
            .handle()
            .insert_source(ping_source, |_, _, state| {
                state.on_ping();
            })?;
        let (event_tx, event_channel) = crate::core::channel();
        event_loop
            .handle()
            .insert_source(event_channel, |event, _, state| {
                if let calloop::channel::Event::Msg(event) = event {
                    state.apply_event(event);
                }
            })?;
        let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])?;
        event_loop
            .handle()
            .insert_source(signals, |event, _, state| {
                info!(signal = ?event.signal(), "received shutdown signal");
                state.shared.borrow_mut().quit();
            })?;

        let shared = Rc::new(RefCell::new(Shared {
            ping,
            ui: None,
            expanded: false,
            expansion: 0.0,
            pill_w: 256.0,
            pill_h: 58.0,
            quit: false,
            suppress_ping: 0,
            animation: None,
            notification_auto_expanded: false,
            notification_generation: 0,
            expansion_observers: Vec::new(),
        }));

        let registry_state = RegistryState::new(&globals);
        let output_state = OutputState::new(&globals, &qh);
        let seat_state = SeatState::new(&globals, &qh);
        let compositor = CompositorState::bind(&globals, &qh)?;
        let shm = Shm::bind(&globals, &qh)?;
        let layer_shell = match LayerShell::bind(&globals, &qh) {
            Ok(layer_shell) => layer_shell,
            Err(error) => {
                return Err(format!(
                    "zwlr_layer_shell_v1 is not available ({error}); \
                     Glass Island requires a compositor with wlr-layer-shell support \
                     (KWin on Wayland supports it)"
                )
                .into());
            }
        };
        let fractional_scale_manager = globals
            .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, GlobalData)
            .ok();
        let viewporter = globals
            .bind::<WpViewporter, _, _>(&qh, 1..=1, GlobalData)
            .ok();
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);

        let mut state = WaylandState {
            registry_state,
            output_state,
            seat_state,
            shm,
            compositor,
            layer_shell,
            qh: qh.clone(),
            shared: shared.clone(),
            window: window.clone(),
            loop_handle: loop_handle.clone(),
            loop_signal,
            blur: None,
            layer: None,
            viewport: None,
            fractional_scale_obj: None,
            fractional_scale_value: None,
            pool: None,
            scale: 1.0,
            host_w: HOST_WIDTH as i32,
            host_h: HOST_HEIGHT as i32,
            configured: false,
            pointer: None,
            anim_timer_active: false,
            progress_timer_active: false,
            notification_visible: false,
            media_playing: false,
            media_length_us: 0,
            media_progress_base: None,
            last_art_path: None,
            app_state: AppState::default(),
        };

        event_queue.roundtrip(&mut state)?;

        let (output, output_scale) = select_output(&state.output_state);
        let fractional_supported = fractional_scale_manager.is_some() && viewporter.is_some();
        let initial_scale = if fractional_supported {
            1.0
        } else {
            output_scale as f32
        };
        state.scale = initial_scale;
        window
            .window()
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: initial_scale,
            });
        window.set_size(LogicalSize::new(state.host_w as f32, state.host_h as f32));

        let surface = state.compositor.create_surface(&qh);
        let layer_surface = state.layer_shell.create_layer_surface(
            &qh,
            surface.clone(),
            Layer::Overlay,
            Some("glass-island"),
            output.as_ref(),
        );
        layer_surface.set_size(state.host_w as u32, state.host_h as u32);
        layer_surface.set_anchor(Anchor::TOP);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer_surface.set_margin(TOP_MARGIN, 0, 0, 0);
        // Install an empty input region before the first commit. Without this,
        // the compositor would use the full 420x180 surface as the default
        // input region until the first configured frame arrives.
        let empty_input = state.compositor.wl_compositor().create_region(&qh, ());
        surface.set_input_region(Some(&empty_input));
        surface.set_opaque_region(None);
        surface.commit();
        empty_input.destroy();

        if let (Some(manager), Some(viewporter)) = (fractional_scale_manager, viewporter) {
            let fractional_scale = manager.get_fractional_scale(&surface, &qh, ());
            let viewport = viewporter.get_viewport(&surface, &qh, ());
            state.fractional_scale_obj = Some(fractional_scale);
            state.viewport = Some(viewport);
            state.update_viewport();
            info!("fractional scaling protocols available");
        } else {
            info!("fractional scaling unavailable; using integer output scale");
        }

        let blur_disabled = std::env::var_os("GLASS_ISLAND_DISABLE_BLUR").is_some();
        if blur_disabled {
            info!("KWin compositor blur disabled by environment");
        } else {
            state.blur = KWinBlur::bind(&globals, &qh, &surface);
            if state.blur.is_some() {
                info!("KWin compositor blur available");
            }
        }
        state.layer = Some(LayerHandles {
            _layer_surface: layer_surface,
            surface,
        });

        info!(
            output_scale,
            logical_scale = state.scale,
            "Wayland layer-shell host initialized"
        );

        Ok(Self {
            conn,
            event_queue: Some(event_queue),
            state,
            event_loop,
            shared,
            event_tx,
        })
    }

    pub fn shared(&self) -> Rc<RefCell<Shared>> {
        self.shared.clone()
    }

    pub fn event_sender(&self) -> EventSender {
        self.event_tx.clone()
    }

    pub fn window_adapter(&self) -> Rc<dyn WindowAdapter> {
        Rc::new(IslandWindowAdapter {
            inner: self.state.window.clone(),
            shared: self.shared.clone(),
        })
    }

    #[cfg(debug_assertions)]
    pub fn schedule_demo(&mut self) {
        let _ = self.state.loop_handle.insert_source(
            Timer::from_duration(Duration::from_millis(900)),
            |_, _, state: &mut WaylandState| {
                state.shared.borrow_mut().toggle();
                TimeoutAction::Drop
            },
        );
        if std::env::var_os("GLASS_ISLAND_DEMO_HOLD").is_none() {
            let _ = self.state.loop_handle.insert_source(
                Timer::from_duration(Duration::from_millis(2400)),
                |_, _, state: &mut WaylandState| {
                    state.shared.borrow_mut().toggle();
                    TimeoutAction::Drop
                },
            );
        }
    }

    pub fn start(&mut self, ui: &Island) -> Result<(), Box<dyn Error>> {
        {
            let mut shared = self.shared.borrow_mut();
            shared.ui = Some(ui.as_weak());
            shared.pill_w = ui.get_pill_width();
            shared.pill_h = ui.get_pill_height();
        }

        if let Some(mut event_queue) = self.event_queue.take() {
            event_queue.roundtrip(&mut self.state)?;
            let source = WaylandSource::new(self.conn.clone(), event_queue);
            source.insert(self.state.loop_handle.clone())?;
        }

        self.state.window.request_redraw();
        self.state.redraw();
        Ok(())
    }

    pub fn run(&mut self) -> Result<(), Box<dyn Error>> {
        self.event_loop.run(None, &mut self.state, |_| {})?;
        Ok(())
    }
}

pub struct IslandPlatform {
    runtime: Rc<RefCell<WaylandRuntime>>,
    window: Rc<dyn WindowAdapter>,
}

impl IslandPlatform {
    pub fn new(runtime: Rc<RefCell<WaylandRuntime>>, window: Rc<dyn WindowAdapter>) -> Self {
        Self { runtime, window }
    }
}

impl Platform for IslandPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        let runtime = self.runtime.clone();
        let mut runtime = runtime.borrow_mut();
        runtime
            .run()
            .map_err(|error| PlatformError::Other(error.to_string()))
    }
}

fn pill_geometry(
    host_w: i32,
    host_h: i32,
    pill_w: f32,
    pill_h: f32,
    expansion: f32,
) -> (i32, i32, i32, i32, f32) {
    let pill_w = pill_w.clamp(0.0, host_w as f32);
    let pill_h = pill_h.clamp(0.0, host_h as f32);
    let expansion = expansion.clamp(0.0, 1.0);
    let x = ((host_w as f32 - pill_w) / 2.0).round().max(0.0) as i32;
    let y = 0;
    let radius = (pill_h / 2.0).min(26.0 + 10.0 * expansion);
    (x, y, pill_w.round() as i32, pill_h.round() as i32, radius)
}

fn buffer_dimensions(host_w: i32, host_h: i32, scale: f32) -> (i32, i32) {
    let width = (host_w as f32 * scale).round().max(1.0) as i32;
    let height = (host_h as f32 * scale).round().max(1.0) as i32;
    (width, height)
}

fn select_output(output_state: &OutputState) -> (Option<wl_output::WlOutput>, i32) {
    let outputs: Vec<_> = output_state.outputs().collect();
    let selected = outputs
        .iter()
        .find(|output| {
            output_state
                .info(output)
                .and_then(|info| info.logical_position)
                .map(|position| position == (0, 0))
                .unwrap_or(false)
        })
        .or_else(|| outputs.first());

    match selected {
        Some(output) => {
            let scale = output_state
                .info(output)
                .map(|info| info.scale_factor.max(1))
                .unwrap_or(1);
            (Some(output.clone()), scale)
        }
        None => (None, 1),
    }
}

/// Build a rounded rectangle approximation from horizontal bands.
///
/// `wl_region` only supports rectangles, so this keeps hit-testing and KWin's
/// blur mask inside the visible pill rather than its bounding box.
fn rounded_rect_bands(x: i32, y: i32, w: i32, h: i32, radius: f32) -> Vec<(i32, i32, i32, i32)> {
    let mut bands = Vec::with_capacity(40);
    if w <= 0 || h <= 0 {
        return bands;
    }

    let radius = radius.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);
    if radius < 0.5 {
        bands.push((x, y, w, h));
        return bands;
    }

    const BANDS: i32 = 16;
    let band_height = (radius / BANDS as f32).ceil() as i32;
    if band_height <= 0 {
        bands.push((x, y, w, h));
        return bands;
    }

    let mut add_band = |band_y: i32, band_h: i32, distance_from_edge: f32| {
        let circle = radius * radius - (radius - distance_from_edge).max(0.0).powi(2);
        let inset = (radius - circle.max(0.0).sqrt()).round() as i32;
        let band_w = w - 2 * inset;
        if band_w > 0 && band_h > 0 {
            bands.push((x + inset, band_y, band_w, band_h));
        }
    };

    let mut top_band = 0;
    while top_band < radius as i32 {
        let next = (top_band + band_height).min(radius as i32);
        if next <= top_band {
            break;
        }
        let distance = (top_band as f32 + next as f32) / 2.0;
        add_band(y + top_band, next - top_band, distance);
        add_band(y + h - next, next - top_band, distance);
        top_band = next;
    }

    let middle_top = y + radius as i32;
    let middle_height = h - 2 * radius as i32;
    if middle_height > 0 {
        bands.push((x, middle_top, w, middle_height));
    }

    bands
}

fn add_rounded_rect(region: &WlRegion, x: i32, y: i32, w: i32, h: i32, radius: f32) {
    for (bx, by, bw, bh) in rounded_rect_bands(x, y, w, h, radius) {
        region.add(bx, by, bw, bh);
    }
}

fn map_button(button: u32) -> Option<slint::platform::PointerEventButton> {
    match button {
        0x110 => Some(slint::platform::PointerEventButton::Left),
        0x111 => Some(slint::platform::PointerEventButton::Right),
        0x112 => Some(slint::platform::PointerEventButton::Middle),
        _ => None,
    }
}

fn load_artwork(path: &str) -> Option<slint::Image> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > 20 * 1024 * 1024 {
        return None;
    }

    let source = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let thumbnail = image::imageops::thumbnail(&source, 96, 96);
    let (width, height) = thumbnail.dimensions();
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    for (target, source) in buffer.make_mut_slice().iter_mut().zip(thumbnail.pixels()) {
        *target = slint::Rgba8Pixel {
            r: source[0],
            g: source[1],
            b: source[2],
            a: source[3],
        };
    }
    Some(slint::Image::from_rgba8(buffer))
}

fn format_media_time(position_us: u64, length_us: u64) -> String {
    if length_us == 0 {
        return String::new();
    }
    format!(
        "{} / {}",
        format_duration(position_us.min(length_us)),
        format_duration(length_us)
    )
}

fn format_duration(microseconds: u64) -> String {
    let total_seconds = microseconds / 1_000_000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3600;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn ease_in_out_cubic(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let f = -2.0 * t + 2.0;
        1.0 - (f * f * f) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        let (ping, _source) = make_ping().expect("ping source");
        Shared {
            ping,
            ui: None,
            expanded: false,
            expansion: 0.0,
            pill_w: 256.0,
            pill_h: 58.0,
            quit: false,
            suppress_ping: 0,
            animation: None,
            notification_auto_expanded: false,
            notification_generation: 0,
            expansion_observers: Vec::new(),
        }
    }

    #[test]
    fn toggle_starts_animation_and_tracks_target() {
        let mut shared = shared();
        shared.toggle();
        assert!(shared.expanded);
        let animation = shared.animation.expect("expand animation");
        assert_eq!(animation.from, 0.0);
        assert_eq!(animation.to, 1.0);

        shared.toggle();
        assert!(!shared.expanded);
        let animation = shared.animation.expect("collapse animation");
        assert_eq!(animation.to, 0.0);
    }

    #[test]
    fn quit_sets_shutdown_flag() {
        let mut shared = shared();
        shared.quit();
        assert!(shared.quit);
    }

    #[test]
    fn rounded_rect_zero_radius_is_a_full_rectangle() {
        let bands = rounded_rect_bands(10, 20, 100, 58, 0.0);
        assert_eq!(bands, vec![(10, 20, 100, 58)]);
    }

    #[test]
    fn rounded_rect_bands_stay_inside_bounds_and_cover_center() {
        let bands = rounded_rect_bands(10, 20, 100, 58, 26.0);
        assert!(!bands.is_empty());

        for &(x, y, w, h) in &bands {
            assert!(x >= 10);
            assert!(y >= 20);
            assert!(w > 0);
            assert!(h > 0);
            assert!(x + w <= 110);
            assert!(y + h <= 78);
        }

        let covers = |px: i32, py: i32| {
            bands
                .iter()
                .any(|&(x, y, w, h)| px >= x && px < x + w && py >= y && py < y + h)
        };

        assert!(covers(60, 49), "center point must be inside the region");
        assert!(covers(60, 20), "top center must be inside the region");
        assert!(!covers(10, 20), "top-left corner must be rounded away");
        assert!(!covers(109, 77), "bottom-right corner must be rounded away");
    }

    #[test]
    fn pill_geometry_is_logical_and_centered() {
        assert_eq!(
            pill_geometry(420, 180, 256.0, 58.0, 0.0),
            (82, 0, 256, 58, 26.0)
        );
        assert_eq!(
            pill_geometry(420, 180, 392.0, 148.0, 1.0),
            (14, 0, 392, 148, 36.0)
        );
    }

    #[test]
    fn pill_geometry_tracks_mid_animation() {
        assert_eq!(
            pill_geometry(420, 180, 256.0 + 136.0 * 0.5, 58.0 + 106.0 * 0.5, 0.5),
            (48, 0, 324, 111, 31.0)
        );
    }

    #[test]
    fn buffer_dimensions_follow_fractional_scale() {
        assert_eq!(buffer_dimensions(420, 180, 1.0), (420, 180));
        assert_eq!(buffer_dimensions(420, 180, 1.25), (525, 225));
        assert_eq!(buffer_dimensions(420, 180, 1.5), (630, 270));
        assert_eq!(buffer_dimensions(420, 180, 2.0), (840, 360));
        assert_eq!(buffer_dimensions(420, 180, 1.2), (504, 216));
    }

    #[test]
    fn button_mapping_matches_linux_input_codes() {
        use slint::platform::PointerEventButton;
        assert_eq!(map_button(0x110), Some(PointerEventButton::Left));
        assert_eq!(map_button(0x111), Some(PointerEventButton::Right));
        assert_eq!(map_button(0x112), Some(PointerEventButton::Middle));
        assert_eq!(map_button(0x113), None);
    }

    #[test]
    fn media_time_is_formatted_and_clamped() {
        assert_eq!(format_media_time(0, 0), "");
        assert_eq!(format_media_time(0, 225_000_000), "0:00 / 3:45");
        assert_eq!(format_media_time(83_000_000, 225_000_000), "1:23 / 3:45");
        assert_eq!(format_media_time(999_000_000, 225_000_000), "3:45 / 3:45");
        assert_eq!(format_media_time(0, 3_725_000_000), "0:00 / 1:02:05");
    }

    #[test]
    fn cubic_easing_has_expected_endpoints() {
        assert_eq!(ease_in_out_cubic(0.0), 0.0);
        assert!((ease_in_out_cubic(0.5) - 0.5).abs() < 1e-6);
        assert_eq!(ease_in_out_cubic(1.0), 1.0);
    }
}
