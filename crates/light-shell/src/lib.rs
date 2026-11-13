//! Light's host-facing desktop shell. Process-wide servers, logging, updates and GPU
//! backend selection remain the host binary's responsibility.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use lightcraft_engine::Session;
use lightcraft_ui_egui::panels::library_problem::LibraryProblem;
use lightcraft_ui_egui::{LightcraftApp, Services, UiState};
use std::path::PathBuf;

// Share the standalone platform glue, not a second set of dialog/editor implementations.
#[path = "../../../apps/lightcraft/src/appearance.rs"]
mod appearance;
#[cfg(target_os = "linux")]
#[path = "../../../apps/lightcraft/src/colord.rs"]
mod colord;
#[path = "../../../apps/lightcraft/src/dialog_filter.rs"]
mod dialog_filter;
#[path = "../../../apps/lightcraft/src/editors.rs"]
mod editors;
#[cfg(target_os = "macos")]
#[path = "../../../apps/lightcraft/src/native_menu.rs"]
mod native_menu;
#[path = "../../../apps/lightcraft/src/platform.rs"]
mod platform;
#[allow(dead_code)] // standalone window-placement helper is not needed by a hosted module
#[path = "../../../apps/lightcraft/src/prefs.rs"]
mod prefs;
#[cfg(any(target_os = "linux", windows))]
#[path = "../../../apps/lightcraft/src/profile_file.rs"]
mod profile_file;
#[cfg(target_os = "macos")]
#[path = "../../../apps/lightcraft/src/screen_profile.rs"]
mod screen_profile;
#[path = "../../../apps/lightcraft/src/session.rs"]
mod session;
#[cfg(windows)]
#[path = "../../../apps/lightcraft/src/win_screen_profile.rs"]
mod win_screen_profile;
use dialog_filter::FileDialogExt as _;

/// How long the process waits for the GPU work in flight before it ends anyway.
const EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Default)]
pub struct Launch {
    pub files: Vec<String>,
    /// Explicit library; otherwise use the app's existing launch preference and legacy paths.
    pub library: Option<PathBuf>,
    /// A throwaway session that never writes preferences or a catalog.
    pub memory: bool,
    pub sample: bool,
    pub log_file: Option<PathBuf>,
}

pub struct Shell {
    app: LightcraftApp,
    prefs: prefs::PrefsWriter,
    zoom: f32,
    #[cfg(target_os = "macos")]
    ctx: egui::Context,
    #[cfg(target_os = "macos")]
    menu: Option<native_menu::NativeMenu>,
}

impl Shell {
    pub fn new(cc: &eframe::CreationContext<'_>, launch: Launch) -> Self {
        let log_file = launch.log_file.clone();
        let mut shell = Self::new_for_host(&cc.egui_ctx, cc.storage, launch);
        shell.app.services = session::services(cc, log_file.as_deref());
        shell
    }

    /// eframe storage is unused: the existing library and ui.json remain authoritative.
    pub fn new_for_host(ctx: &egui::Context, _storage: Option<&dyn eframe::Storage>, launch: Launch) -> Self {
        use lightcraft_ui_egui::panels::library_problem::{Launch as LibraryLaunch, launch_plan};
        let memory = launch.memory || launch.sample;
        let (saved, warning, keep_file) = prefs::load_prefs(memory);
        let plan = launch_plan(memory, launch.library, None, saved.as_ref().map(|u| &u.settings), lightcraft_engine::library::default_dir());
        let (session, mut problem) = session::open_session(memory, &plan, launch.sample);
        let mut app = LightcraftApp::new(session.with_default_sam3_model(), session::services_for_host(ctx, launch.log_file.as_deref()));
        if let Some(saved) = saved {
            app.ui = saved;
        }
        if plan == LibraryLaunch::Ask {
            problem = Some(LibraryProblem::choose_later(&app.ui.settings));
        }
        app.library_problem = problem;
        app.notices.extend(warning);
        // No startup updater, listener, native menu event handler, logger or process exit.
        app.install_look(ctx);
        let prefs = prefs::PrefsWriter {
            path: if memory { None } else { prefs::prefs_path() },
            written: serde_json::to_vec_pretty(&app.ui).unwrap_or_default(),
            library: app.ui.settings.library_path.clone(),
            keep_file,
            ..Default::default()
        };
        // Snapshot the saved preferences first, then remember an explicitly opened library.
        // This mirrors the standalone launch path, so the next launch opens the same catalog.
        if !memory && let Some(library) = app.session.library.as_ref().filter(|l| l.on_disk) {
            app.ui.settings.library_path = library.dir.to_string_lossy().into_owned();
        }
        let mut shell = Self {
            app,
            prefs,
            zoom: 1.0,
            #[cfg(target_os = "macos")]
            ctx: ctx.clone(),
            #[cfg(target_os = "macos")]
            menu: None,
        };
        for path in launch.files {
            shell.open_path(&path);
        }
        shell
    }

    pub fn logic(&mut self, ctx: &egui::Context) {
        self.app.logic(ctx);
        self.prefs.tick(&mut self.app, ctx);
    }

    pub fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(ctx, raw);
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        #[cfg(target_os = "macos")]
        if let Some(menu) = self.menu.as_mut()
            && menu.update(&mut self.app, ui.ctx())
            && lightcraft_ui_egui::panels::notices::may_close(&mut self.app)
        {
            // A request, never process::exit: the host negotiates with every module.
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.app.ui(ui);
        self.zoom = ui.ctx().zoom_factor();
    }

    pub fn backdrop(&self) -> [f32; 4] {
        egui::Color32::from_rgb(12, 12, 12).to_normalized_gamma_f32()
    }

    pub fn open_path(&mut self, path: &str) {
        use lightcraft_engine::import::{LaunchPath, launch_path};
        let cwd = std::env::current_dir().ok();
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
        let path = match launch_path(path, cwd.as_deref(), home.as_deref(), |_| None) {
            Some(LaunchPath::Import(path)) => path,
            Some(LaunchPath::Library(path)) => {
                if let Err(e) = self.app.run("app.openLibrary", serde_json::json!({"path": path})) {
                    self.app.notices.push(e.to_string());
                }
                return;
            }
            Some(LaunchPath::Refused { path, why }) => {
                self.app.notices.push(format!("{path}: {why}"));
                return;
            }
            None => return,
        };
        if let Some(problem) = self.app.library_problem.as_mut().filter(|p| !p.dismissed) {
            problem.pending_import.push(path);
        } else if let Err(e) = lightcraft_ui_egui::import::start_paths(&mut self.app, vec![path]) {
            self.app.notices.push(format!("Import: {e}"));
        }
    }

    pub fn save_state(&mut self) {
        if let Err(e) = self.prefs.save(&self.app) {
            log::error!("{e}");
            self.app.notices.push(e);
        }
        if let Err(e) = self.app.session.close_library() {
            log::error!("saving the library failed: {e}");
            self.app.notices.push(e.to_string());
        }
    }

    /// Let the GPU work finish before the process ends.
    ///
    /// The device is created on a background thread, inside the driver: a process that
    /// ends while it is there crashes in the driver (Light's issue #620). The standalone
    /// app does this in its own `on_exit`; hosted, the host says when.
    pub fn shutdown(&mut self) {
        self.app.shutdown(EXIT_WAIT);
    }

    /// Conservative read-only counterpart of notices::may_close. On a viewport close the
    /// app's logic performs that full flush/retry/prompt negotiation before the host asks.
    pub fn can_close(&self) -> bool {
        self.app.quit_confirmed
            || (self.app.session.unsaved().is_none()
                && self.app.session.activity.running_cancellable().is_empty()
                && self.app.import.is_none()
                && self.app.waiting_import.files.is_empty())
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    pub fn fonts(&self) -> egui::FontDefinitions {
        self.app.font_definitions()
    }

    /// Only the host calls this on activation: muda's application menu and event handler
    /// are process-global. Never install it merely by constructing a background module.
    pub fn install_native_menu(&mut self) {
        #[cfg(target_os = "macos")]
        {
            self.menu = Some(native_menu::NativeMenu::install(&mut self.app, &self.ctx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_look_precedes_logic_and_the_app_draws_without_a_creation_context() {
        let ctx = egui::Context::default();
        let mut shell = Shell::new_for_host(&ctx, None, Launch { memory: true, ..Default::default() });
        shell.app.ui.settings.gpu = false;
        shell.app.ui.seen_version = Some(env!("CARGO_PKG_VERSION").into());
        assert!(shell.fonts().font_data.contains_key("Inter"));
        assert!(!egui::FontDefinitions::default().font_data.contains_key("Inter"));
        assert_eq!(lightcraft_ui_egui::theme::current(&ctx), Some(shell.app.ui.settings.theme(shell.app.system_theme(&ctx))));
        assert!(shell.can_close());
        let mut painted = false;
        for _ in 0..3 {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                shell.logic(ui.ctx());
                shell.ui(ui);
            });
            painted |= !output.shapes.is_empty();
            output.textures_delta.clear();
        }
        assert!(painted, "the font warm-up frame may be empty, but the app must draw afterwards");
        shell.app.waiting_import.files.push("pending.jpg".into());
        assert!(!shell.can_close());
        shell.app.quit_confirmed = true;
        assert!(shell.can_close());
        shell.save_state();
    }
}
