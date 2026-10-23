use crate::Result;
use crate::game::bw2::field::{Direction, FieldSession, MoveResult};
use crate::render::MapScene;
use crate::runtime::wgpu_renderer::WgpuRenderer;
use gilrs::{EventType, Gilrs};
use kira::{AudioManager, AudioManagerSettings, DefaultBackend};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

pub fn run(session: FieldSession, scene: MapScene) -> Result<()> {
    let event_loop =
        EventLoop::new().map_err(|error| format!("could not create event loop: {error}"))?;
    let mut app = DesktopApp::new(session, scene);
    event_loop
        .run_app(&mut app)
        .map_err(|error| format!("desktop event loop failed: {error}"))?;
    if let Some(error) = app.fatal_error {
        return Err(error);
    }
    Ok(())
}

struct DesktopApp {
    session: FieldSession,
    scene: MapScene,
    window: Option<Arc<Window>>,
    renderer: Option<WgpuRenderer>,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    gilrs: Option<Gilrs>,
    audio: Option<AudioManager<DefaultBackend>>,
    fatal_error: Option<String>,
}

impl DesktopApp {
    fn new(session: FieldSession, scene: MapScene) -> Self {
        let gilrs = match Gilrs::new() {
            Ok(gilrs) => {
                info!("controller input initialized");
                Some(gilrs)
            }
            Err(error) => {
                warn!(error = %error, "controller input is unavailable");
                None
            }
        };
        let audio = match AudioManager::<DefaultBackend>::new(AudioManagerSettings::default()) {
            Ok(audio) => {
                info!("audio output initialized");
                Some(audio)
            }
            Err(error) => {
                warn!(error = %error, "audio output is unavailable; field rendering will continue");
                None
            }
        };
        Self {
            session,
            scene,
            window: None,
            renderer: None,
            egui_ctx: egui::Context::default(),
            egui_state: None,
            gilrs,
            audio,
            fatal_error: None,
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: String) {
        warn!(%error, "desktop runtime is shutting down");
        self.fatal_error = Some(error);
        event_loop.exit();
    }

    fn step(&mut self, direction: Direction) {
        match self.session.move_player(direction) {
            MoveResult::Moved => info!(
                x = self.session.player.x,
                z = self.session.player.z,
                ?direction,
                "player moved"
            ),
            MoveResult::BlockedByEdge => debug!(?direction, "player reached map edge"),
            MoveResult::BlockedByTerrain(tile) => debug!(
                tile_type = format_args!("0x{:08X}", tile.tile_type),
                ?direction,
                "player movement blocked"
            ),
            MoveResult::BlockedByScript => debug!("field script locked movement"),
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn press_action(&mut self) {
        if let Err(error) = self.session.press_action() {
            warn!(%error, "could not execute NPC field script");
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn poll_gamepad(&mut self) {
        let mut directions = Vec::new();
        let mut action_pressed = false;
        if let Some(gilrs) = &mut self.gilrs {
            while let Some(event) = gilrs.next_event() {
                if let EventType::ButtonPressed(button, _) = event.event {
                    if let Some(direction) = controller_direction(button) {
                        directions.push(direction);
                    } else if button == gilrs::Button::South {
                        action_pressed = true;
                    }
                }
            }
        }
        for direction in directions {
            self.step(direction);
        }
        if action_pressed {
            self.press_action();
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let Some(egui_state) = self.egui_state.as_mut() else {
            return;
        };
        let input = egui_state.take_egui_input(window);
        let result = renderer.render(
            window,
            &self.session,
            &self.egui_ctx,
            egui_state,
            input,
            self.audio.is_some(),
        );
        if let Err(error) = result {
            self.fail(event_loop, error);
        }
    }
}

fn controller_direction(button: gilrs::Button) -> Option<Direction> {
    match button {
        gilrs::Button::DPadUp => Some(Direction::North),
        gilrs::Button::DPadRight => Some(Direction::East),
        gilrs::Button::DPadDown => Some(Direction::South),
        gilrs::Button::DPadLeft => Some(Direction::West),
        _ => None,
    }
}

impl ApplicationHandler for DesktopApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(format!(
                "Gen5 Native · {}",
                self.session.zone.display_name()
            ))
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.fail(event_loop, format!("could not create game window: {error}"));
                return;
            }
        };
        self.egui_state = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(8192),
        ));
        match pollster::block_on(WgpuRenderer::new(
            window.clone(),
            &self.session,
            &self.scene,
        )) {
            Ok(renderer) => {
                self.renderer = Some(renderer);
                self.window = Some(window.clone());
                info!(
                    zone = self.session.zone.zone_id,
                    "desktop field window ready"
                );
                window.request_redraw();
            }
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self
            .window
            .as_ref()
            .is_none_or(|window| window.id() != window_id)
        {
            return;
        }
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let consumed = self
            .egui_state
            .as_mut()
            .is_some_and(|state| state.on_window_event(window, &event).consumed);
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            WindowEvent::KeyboardInput { event, .. }
                if !consumed && event.state == ElementState::Pressed && !event.repeat =>
            {
                let direction = match event.physical_key {
                    PhysicalKey::Code(KeyCode::KeyW | KeyCode::ArrowUp) => Some(Direction::North),
                    PhysicalKey::Code(KeyCode::KeyD | KeyCode::ArrowRight) => Some(Direction::East),
                    PhysicalKey::Code(KeyCode::KeyS | KeyCode::ArrowDown) => Some(Direction::South),
                    PhysicalKey::Code(KeyCode::KeyA | KeyCode::ArrowLeft) => Some(Direction::West),
                    PhysicalKey::Code(KeyCode::Enter | KeyCode::Space) => {
                        self.press_action();
                        None
                    }
                    PhysicalKey::Code(KeyCode::Escape) => {
                        event_loop.exit();
                        None
                    }
                    _ => None,
                };
                if let Some(direction) = direction {
                    self.step(direction);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.poll_gamepad();
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(16),
        ));
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}
