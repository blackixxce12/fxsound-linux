//! The FxSound binary: parse the command line, be the single instance, run the window.
//!
//! Everything interesting lives in the library next door. This file is the shell that wires the
//! controller to eframe, the tray and the control socket, and it is deliberately the only place
//! that knows all four exist.
//!
//! ## Startup, in order
//!
//! 1. Parse the command line.
//! 2. Try to become the single instance. If another one is already running, forward the command
//!    line to it over the control socket, print whatever it says and exit — which is what makes
//!    `fxsound --next-preset` usable as a compositor keybind. `--quit` with nobody to forward to
//!    reports that FxSound is not running and stops right here, before step 3 could claim the
//!    session default sink.
//! 3. Start the audio engine. A missing or broken PipeWire is **not** fatal: the window still
//!    opens and says so. The Windows build quits hard when its driver is missing; on Linux the
//!    same conditions are routine and recoverable (`docs/spec/00-architecture.md` §10, open
//!    question 6).
//! 4. Apply the cold-start options, register the tray, then alternate between the two states
//!    below until something asks to quit.
//!
//! ## Two states: a window, or the tray alone
//!
//! The original hides to the tray on ✕, on the minimise button and on the compositor's close
//! request, and quits only from the tray's Exit (`docs/spec/01-window-layout.md` §7). On Wayland
//! a client cannot unmap and later remap its toplevel through winit —
//! `Window::set_visible` is a no-op there
//! (`winit-0.30.13/src/platform_impl/linux/wayland/window/mod.rs:253`) — so "hidden" cannot be a
//! window that exists but is not shown. Instead there is no window at all: eframe's default
//! `run_and_return` keeps the winit event loop in a thread-local and drives it with
//! `run_app_on_demand` (`eframe-0.36.0/src/native/run.rs:57-75`, `:374-383`), which means
//! [`eframe::run_native`] may be called again and again in one process. To hide, the shell
//! sends `ViewportCommand::Close`; the window is destroyed and `run_native` returns. To show, a
//! fresh window is run. In between, [`Runtime::run_headless`] pumps the engine, the control
//! socket and the tray at 10 Hz — audio never stops, `fxsound --show` and the tray still work.
//!
//! The window renders without vsync. eframe's glow backend otherwise lets Mesa's
//! `eglSwapBuffers` wait for a frame callback, which a Wayland compositor never sends for a
//! surface it is not showing — so a window parked on another Hyprland workspace stopped
//! answering `xdg_wm_base` pings and the compositor called the app unresponsive. Without vsync
//! the loop is paced by [`FRAME_INTERVAL`] instead.

#![forbid(unsafe_code)]

use clap::Parser as _;
use eframe::egui;
use fxsound_app::{
    App, WindowVisibility,
    app::{FORBIDDEN_PRESET_NAME_CHARS, MAX_PRESET_NAME_CHARS, preset_name_available},
    cli::Cli,
    commands::{self, WindowRequest},
    ipc::{self, Instance},
    tray::{self, TrayCommand, TrayDevice, TrayHandle, TrayPreset, TrayState},
};
use fxsound_core::{ThemeMode, ViewMode, i18n::tr};
use fxsound_ui::{
    FxColor, Palette, UiAction,
    dialogs::{
        self, ExportDialog, ExportState, ImportDialog, ImportState, PresetsAction,
        changelog::{ChangelogAction, ChangelogPane},
        settings::{NavIcons, SettingsDialog, SettingsState},
    },
    layout,
    state::PresetEntry,
    theme,
    views::ViewScratch,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often the engine, the control socket and the tray are polled when nothing else wakes the
/// loop. Ten hertz is imperceptible in CPU terms and puts a hard ceiling on how long a forwarded
/// command (`fxsound --next-preset` from a compositor keybind) can sit unread.
const PUMP_INTERVAL: Duration = Duration::from_millis(100);

/// The changelog Settings ▸ Help shows; bundled, since this fork never sends the user to the
/// upstream website.
const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// How far the fitted zoom may drift from the one in effect before it is re-applied — a set on
/// every frame would re-lay the fonts out on every frame.
const ZOOM_TOLERANCE: f32 = 0.01;

/// The repaint cadence while the visualizer is live. Without vsync (see the module docs) this
/// is what keeps the loop at roughly 60 Hz instead of spinning.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    // Step 2: single instance. A second invocation is a remote control, not a second app.
    let listener = match Instance::acquire() {
        Ok(Instance::Primary(listener)) => listener,
        Ok(Instance::Secondary(client)) => {
            // The primary re-parses our argv itself, so nothing is lost in translation.
            match client.forward() {
                Ok(response) => {
                    if !response.stdout.is_empty() {
                        println!("{}", response.stdout);
                    }
                    if !response.stderr.is_empty() {
                        eprintln!("{}", response.stderr);
                    }
                    std::process::exit(response.exit_code());
                }
                Err(err) => {
                    eprintln!("could not reach the running instance: {err}");
                    std::process::exit(1);
                }
            }
        }
        Err(err) => {
            eprintln!("could not claim the control socket: {err}");
            std::process::exit(1);
        }
    };
    let server = match listener.serve() {
        Ok(server) => server,
        Err(err) => {
            eprintln!("could not serve the control socket: {err}");
            std::process::exit(1);
        }
    };

    // `--quit` addresses a running instance, and holding the lock means there is none. Say so
    // and leave before step 3 claims the session default sink. `Command::Quit` is not on the
    // cold-start list (`Command::honoured_at_cold_start`), so nothing further down would ever
    // see it. Dropping `server` unlinks the socket again.
    if cli.quit {
        eprintln!("FxSound is not running");
        return Ok(());
    }
    // `--status` is a question for a running instance as well (`docs/spec/00-architecture.md`
    // §4.7): with none there is nothing to report, and starting one to answer would be the
    // opposite of what was asked. Fail, after unlinking the socket `server` owns.
    if cli.status {
        eprintln!("FxSound is not running");
        drop(server);
        std::process::exit(1);
    }

    // The UI language, decided before anything is drawn or named: the desktop's unless the
    // settings say otherwise (`fxsound_core::i18n`). The engine gets it too, for the node
    // descriptions the sound settings show.
    let language = fxsound_core::Settings::load().effective_language();
    fxsound_core::i18n::set_language(language);

    // Step 3: audio. Report the failure and carry on.
    let engine = match fxsound_audio::AudioEngine::start_with_language(language) {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::error!("audio engine did not start: {err}");
            None
        }
    };

    let mut app = App::new(engine);

    // Step 4: the options a cold start honours, before anything is drawn.
    let cold = commands::run(&mut app, &cli.cold_start_commands());
    if !cold.stdout.is_empty() {
        println!("{}", cold.stdout);
    }
    // A cold start reports and carries on: the device list has not arrived yet, so `--output` has
    // been held rather than refused, and there is nothing here worth refusing to start over.
    if !cold.stderr.is_empty() {
        eprintln!("{}", cold.stderr);
    }
    // `--hide` and the saved "start minimised" preference start in the tray-only state; there is
    // no such thing as a hidden window here (see the module docs).
    let mut visibility = if cold.window.hide || app.settings_run_minimized() {
        WindowVisibility::Hidden
    } else {
        WindowVisibility::Shown
    };

    let (tray_tx, tray_rx) = crossbeam_channel::unbounded();
    let tray_state = app.tray_state();
    let tray_fingerprint = TrayFingerprint::of(&tray_state);
    let tray = match tray::spawn(tray_state, tray_tx) {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::warn!("no system tray: {err}");
            None
        }
    };

    // A Wayland client never sees `kill`, `systemctl --user stop` or a logout hook as a close
    // request, and a process that just dies leaves `default.configured.audio.*` naming a node
    // that vanished with its socket. SIGTERM, SIGINT and SIGHUP therefore become the tray's Exit:
    // the flag is polled by the pump (`Runtime::tick`), and the quit it triggers runs the same
    // hand-back-then-stop sequence, which blocks until the server has confirmed the write. A
    // second Ctrl+C while that is in flight still gets the user out, with the conventional 130.
    let terminate = Arc::new(AtomicBool::new(false));
    if let Err(err) = signal_hook::flag::register_conditional_shutdown(
        signal_hook::consts::SIGINT,
        130,
        Arc::clone(&terminate),
    ) {
        log::warn!("could not install the second-Ctrl+C handler: {err}");
    }
    for signal in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ] {
        if let Err(err) = signal_hook::flag::register(signal, Arc::clone(&terminate)) {
            log::warn!("could not install the handler for signal {signal}: {err}");
        }
    }

    let mut runtime = Runtime {
        app,
        server,
        tray,
        tray_rx,
        tray_fingerprint,
        exit: WindowExit::Hidden,
        settings_requested: false,
        terminate,
        terminating: false,
    };

    loop {
        match visibility {
            WindowVisibility::Shown => match runtime.run_window() {
                Ok(WindowExit::Hidden) => visibility = WindowVisibility::Hidden,
                Ok(WindowExit::Quit) => break,
                Err(err) => {
                    // Better to leave than to loop on a window that cannot be created; the
                    // shutdown below still restores the system default device.
                    log::error!("the window could not be run: {err}");
                    runtime.shutdown();
                    return Err(err);
                }
            },
            WindowVisibility::Hidden => match runtime.run_headless() {
                HeadlessExit::Show => visibility = WindowVisibility::Shown,
                HeadlessExit::Quit => break,
            },
        }
    }

    runtime.shutdown();
    Ok(())
}

/// Why a window run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum WindowExit {
    /// Hide to the tray: ✕, the minimise button, `--hide`/`--toggle-window`, the tray's left
    /// click, or the compositor's close request. The engine keeps running.
    #[default]
    Hidden,
    /// Leave: the tray's Exit item or `fxsound --quit`.
    Quit,
}

/// Why the tray-only state ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadlessExit {
    Show,
    Quit,
}

/// Everything that outlives a window: the controller, the control socket and the tray.
///
/// A [`Shell`] borrows this for the life of one window run; between runs
/// [`Runtime::run_headless`] drives the same parts directly.
struct Runtime {
    app: App,
    server: ipc::Server,
    tray: Option<TrayHandle>,
    tray_rx: crossbeam_channel::Receiver<TrayCommand>,
    /// What the tray was last told, so it is only updated on a real change.
    tray_fingerprint: TrayFingerprint,
    /// Set by the shell before it closes the window; read once `run_native` has returned.
    exit: WindowExit,
    /// The tray's Settings item was chosen and no window has acted on it yet. Set from
    /// [`Runtime::tick`], cleared by the shell that opens the pane — possibly a shell that does
    /// not exist yet, when the item is chosen while hidden.
    settings_requested: bool,
    /// Raised by the signal handlers; `true` means "quit at the next tick".
    terminate: Arc<AtomicBool>,
    /// The signal has been seen and turned into a quit request, so it is only logged once.
    terminating: bool,
}

impl Runtime {
    /// One tick of everything that has to keep happening whether or not a window exists: pull
    /// what the audio thread published, answer forwarded command lines, act on tray clicks, push
    /// the model into the tray. Returns what the window layer was asked to do.
    fn tick(&mut self) -> WindowRequest {
        self.app.poll_audio();

        let mut request = WindowRequest::default();
        if self.terminate.load(Ordering::Relaxed) {
            if !self.terminating {
                log::info!("termination signal received; handing the session default back");
                self.terminating = true;
            }
            request.quit = true;
        }
        // Commands forwarded by a second invocation — the compositor keybind path.
        for forwarded in self.server.drain() {
            let outcome = commands::run(&mut self.app, forwarded.commands());
            merge(&mut request, outcome.window);
            forwarded.respond_with(outcome.stdout, outcome.stderr, outcome.failed);
        }
        while let Ok(command) = self.tray_rx.try_recv() {
            match command {
                TrayCommand::ToggleWindow => request.toggle = true,
                TrayCommand::Open => request.show = true,
                TrayCommand::Exit => request.quit = true,
                TrayCommand::OpenSettings => {
                    // Only a window can open the pane; make sure there is one.
                    self.settings_requested = true;
                    request.show = true;
                }
                other => self.app.handle_tray(other),
            }
        }
        // Flush settings a tray or IPC path may have dirtied without going through `handle`.
        self.app.handle(&[]);
        self.sync_tray();
        request
    }

    /// Run one window until it is closed. Returns why.
    fn run_window(&mut self) -> eframe::Result<WindowExit> {
        self.exit = WindowExit::Hidden;
        // The cached textures belong to the previous egui context and would draw nothing on
        // the new one.
        self.app.assets.clear();
        let options = native_options(self.app.state.view);

        let runtime = &mut *self;
        eframe::run_native(
            "FxSound",
            options,
            Box::new(move |cc| {
                let palette = runtime.app.palette();
                theme::apply(&cc.egui_ctx, palette);
                // The zoom factor is ours (see `fit_zoom`); Ctrl+/- must not fight it.
                cc.egui_ctx
                    .options_mut(|options| options.zoom_with_keyboard = false);
                Ok(Box::new(Shell::new(runtime, palette.mode())))
            }),
        )?;
        Ok(self.exit)
    }

    /// The tray-only state: pump until something asks for the window or for the exit.
    fn run_headless(&mut self) -> HeadlessExit {
        log::info!("window hidden; FxSound keeps running in the system tray");
        loop {
            let request = self.tick();
            if request.quit {
                return HeadlessExit::Quit;
            }
            // With no window, toggle means show.
            if request.show || request.toggle || self.settings_requested {
                return HeadlessExit::Show;
            }
            std::thread::sleep(PUMP_INTERVAL);
        }
    }

    /// Push the model into the tray.
    ///
    /// Only when something it draws actually changed: every call is a D-Bus round trip and a set
    /// of property signals, and this runs on every tick.
    fn sync_tray(&mut self) {
        let Some(tray) = &self.tray else { return };
        let state = self.app.tray_state();
        let fingerprint = TrayFingerprint::of(&state);
        if fingerprint == self.tray_fingerprint {
            return;
        }
        tray.update(|mirror| *mirror = state);
        self.tray_fingerprint = fingerprint;
    }

    /// The only way out: stash unsaved edits, restore the system default device and stop the
    /// engine, remove the tray item. Dropping the server unlinks the control socket.
    fn shutdown(mut self) {
        self.app.shutdown();
        if let Some(tray) = &self.tray {
            tray.shutdown();
        }
    }
}

/// `WindowRequest::merge` is private to the commands module; this is the same OR.
fn merge(into: &mut WindowRequest, other: WindowRequest) {
    into.show |= other.show;
    into.hide |= other.hide;
    into.toggle |= other.toggle;
    into.quit |= other.quit;
}

/// The window eframe is asked for, sized for the current view.
fn native_options(view: ViewMode) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FxSound")
            // Must match packaging/fxsound.desktop's filename stem, or the compositor cannot
            // associate the window with its icon and rules.
            .with_app_id("com.fxsound.FxSound")
            .with_inner_size(fxsound_ui::window_size(view))
            .with_min_inner_size(layout::lite::WINDOW_SIZE)
            .with_resizable(false)
            // FxSound draws its own title bar, so no server-side decorations.
            .with_decorations(false)
            // Needed for the 21 px rounded corners not to be filled in black.
            .with_transparent(true),
        // See the module docs: vsync blocks the main thread on a workspace the compositor is
        // not showing. The loop is paced by `request_repaint_after` instead.
        glow_options: eframe::egui_glow::GlowConfiguration {
            vsync: false,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Everything in a [`TrayState`] the tray draws — all of it but the pixmaps, which
/// [`App::tray_state`] never fills and `ksni::Icon` could not be compared anyway.
///
/// A count and an index are not enough: the Always On Top tick, a preset's trailing ` *`, a
/// renamed preset and a device's description all change without either moving, and the tray
/// relies on the application answering with [`TrayHandle::update`] (its Always On Top item
/// deliberately does not flip itself).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrayFingerprint {
    power: bool,
    processing: bool,
    power_enabled: bool,
    theme: ThemeMode,
    always_on_top: bool,
    presets: Vec<TrayPreset>,
    selected_preset: Option<usize>,
    devices: Vec<TrayDevice>,
    selected_device: Option<usize>,
    language: String,
}

impl TrayFingerprint {
    fn of(state: &TrayState) -> Self {
        // Destructured in full, so that a new field on `TrayState` has to decide here whether
        // the tray draws it.
        let TrayState {
            power,
            processing,
            power_enabled,
            theme,
            always_on_top,
            presets,
            selected_preset,
            devices,
            selected_device,
            language,
            pixmaps: _,
        } = state;
        Self {
            power: *power,
            processing: *processing,
            power_enabled: *power_enabled,
            theme: *theme,
            always_on_top: *always_on_top,
            presets: presets.clone(),
            selected_preset: *selected_preset,
            devices: devices.clone(),
            selected_device: *selected_device,
            language: language.clone(),
        }
    }
}

// =============================================================================================
// The window
// =============================================================================================

/// The eframe application: one window, one frame at a time, over a borrowed [`Runtime`].
///
/// Everything here is per-window and is rebuilt from scratch each time a window is shown: the
/// views' scratch state, the applied size and theme, the menu, and the panes. Nothing in it may
/// stop the engine or the tray — `eframe::App::on_exit` is deliberately left at its no-op
/// default, because the window going away is how *hiding* works (see the module docs), and only
/// [`Runtime::shutdown`] ends the audio.
struct Shell<'a> {
    rt: &'a mut Runtime,
    scratch: ViewScratch,
    /// The size the viewport was last told to be, so a resize is sent only on a real change.
    applied_size: Option<egui::Vec2>,
    /// The theme the fonts and visuals were installed for.
    applied_theme: ThemeMode,
    /// The hamburger menu.
    menu: Menu,
    /// `Some` while the Settings pane is open.
    ///
    /// The C++ runs a modal loop here (`FxSettingsDialog.cpp`); see [`Shell::show_settings`] for
    /// why it is a pane inside the main window and not a window of its own.
    settings: Option<SettingsState>,
    settings_icons: NavIcons,
    /// `Some` while the Import Presets pane is open.
    import: Option<ImportState>,
    /// `Some` while the Export Presets pane is open.
    export: Option<ExportState>,
    /// A native folder picker is running on its own thread; its answer arrives here.
    folder_picker: Option<crossbeam_channel::Receiver<Option<PathBuf>>>,
    /// The changelog pane is open on top of Settings.
    changelog: bool,
    /// Where the design-size content starts this frame: the viewport's origin, or the centred
    /// offset when the surface is larger than the design (see [`fit_zoom`]).
    content_origin: egui::Pos2,
}

impl<'a> Shell<'a> {
    fn new(rt: &'a mut Runtime, applied_theme: ThemeMode) -> Self {
        // The tray's Settings item may have been chosen while there was no window.
        let settings = std::mem::take(&mut rt.settings_requested).then(|| rt.app.settings_state());
        Self {
            rt,
            scratch: ViewScratch::default(),
            applied_size: None,
            applied_theme,
            menu: Menu::default(),
            settings,
            settings_icons: NavIcons::new(),
            import: None,
            export: None,
            folder_picker: None,
            changelog: false,
            content_origin: egui::Pos2::ZERO,
        }
    }

    /// Carry out what a command line, the tray or a title-bar button asked of the window.
    ///
    /// Hiding and quitting both close the window — the difference is what [`Runtime::exit`]
    /// says when `run_native` returns. `show` wins over `hide`, and `toggle` on a window that is
    /// showing means hide.
    fn apply_window_request(&mut self, ctx: &egui::Context, request: WindowRequest) {
        if request.quit {
            self.rt.exit = WindowExit::Quit;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if request.show {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        } else if request.hide || request.toggle {
            self.hide(ctx);
        }
    }

    /// Hide to the tray: destroy the window and let `run_native` return with
    /// [`WindowExit::Hidden`], which is what [`Runtime::exit`] already says.
    fn hide(&mut self, ctx: &egui::Context) {
        self.rt.app.notify_hidden_to_tray();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Resize the viewport when the user flips between Pro and Lite, or opens a pane.
    fn sync_view_size(&mut self, ctx: &egui::Context) {
        let wanted = self.window_size();
        if self.applied_size == Some(wanted) {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(wanted));
        self.applied_size = Some(wanted);
    }

    /// Re-install fonts and visuals after a theme change.
    fn sync_theme(&mut self, ctx: &egui::Context) {
        let palette = self.rt.app.palette();
        if palette.mode() == self.applied_theme {
            return;
        }
        theme::apply(ctx, palette);
        self.applied_theme = palette.mode();
    }

    /// The window size the viewport should have right now: the view's own, grown to fit
    /// whichever pane is open.
    fn window_size(&self) -> egui::Vec2 {
        let mut size = fxsound_ui::window_size(self.rt.app.state.view);
        if self.settings.is_some() {
            size = grown(size, dialogs::settings::WINDOW_SIZE);
        }
        if let Some(import) = &self.import {
            size = grown(size, ImportDialog::new(import).window_size());
        }
        if self.export.is_some() {
            size = grown(size, dialogs::presets::export::WINDOW_SIZE);
        }
        size
    }

    /// The whole window, for the panes' backdrop.
    fn window_rect(&self) -> egui::Rect {
        egui::Rect::from_min_size(self.content_origin, self.window_size())
    }

    /// `true` while a pane owns the window; the menu stays shut meanwhile, as the original's
    /// modal dialogs keep it shut.
    fn pane_open(&self) -> bool {
        self.settings.is_some() || self.import.is_some() || self.export.is_some() || self.changelog
    }

    fn open_settings(&mut self) {
        self.menu.close();
        if self.settings.is_none() {
            self.settings = Some(self.rt.app.settings_state());
        }
    }

    fn open_import(&mut self) {
        self.menu.close();
        if self.import.is_none() {
            self.import = Some(ImportState::default());
        }
    }

    fn open_export(&mut self) {
        self.menu.close();
        if self.export.is_none() {
            // Every preset, factory and user alike (`FxPresetExportDialog.cpp:138-141`).
            let presets = self
                .rt
                .app
                .state
                .presets
                .iter()
                .map(|p| p.name.clone())
                .collect();
            self.export = Some(ExportState {
                presets,
                ..ExportState::default()
            });
        }
    }

    // ---- the hamburger menu ------------------------------------------------------------------

    /// The hamburger menu (`FxMainWindow.cpp:507-553`, `docs/spec/01-window-layout.md` §4.4,
    /// `docs/spec/03-controls.md` §8.5).
    ///
    /// Absent on purpose: Donate (removed from this fork), Download Bonus Presets and Check for
    /// updates (no network; the bonus presets ship in the package), Always On Top (winit's
    /// Wayland backend ignores window levels, and a control that does nothing is worse than
    /// none). Save New Preset and Rename Preset open the original's inline name editor
    /// (`FxPresetMenuItem`, `FxMainWindow.cpp:27-176`) under their row instead of in a submenu.
    fn show_menu(&mut self, ctx: &egui::Context, palette: Palette) {
        if !self.menu.open {
            return;
        }

        let app = &self.rt.app;
        let chrome = match app.state.view {
            ViewMode::Pro => layout::Chrome::PRO,
            ViewMode::Lite => layout::Chrome::LITE,
        };
        let anchor = chrome.menu.rect().translate(self.content_origin.to_vec2());

        // The enablement predicates of `FxMainWindow.cpp:536-543`.
        let preset = app.state.preset();
        let power = app.state.power;
        let modified = preset.is_some_and(|p| p.modified);
        let user_preset = preset.is_some_and(|p| !p.factory);
        let can_save_new = modified && app.user_preset_count() < app.max_user_presets() && power;
        let can_overwrite = modified && user_preset && power;
        let can_undo = modified && power;
        let can_rename = !modified && user_preset && power;
        let can_delete = user_preset && power;
        let can_transfer = !modified && power;
        let overwrite_label = if can_overwrite {
            format!(
                "{} - {}",
                tr("Overwrite Existing Preset"),
                preset.map(|p| p.name.as_str()).unwrap_or_default()
            )
        } else {
            tr("Overwrite Existing Preset")
        };
        let dark = palette.is_dark();
        let presets = &app.state.presets;

        // An editor whose item has since gone grey has nothing left to do.
        if self.menu.editor.as_ref().is_some_and(|e| match e.purpose {
            EditorPurpose::SaveNew => !can_save_new,
            EditorPurpose::Rename => !can_rename,
        }) {
            self.menu.editor = None;
        }
        let just_opened = self.menu.just_opened;
        let editor = &mut self.menu.editor;

        let mut chosen: Option<MenuChoice> = None;
        let mut committed: Option<(EditorPurpose, String)> = None;
        let mut close = false;

        egui::Area::new(egui::Id::new("fxsound.menu"))
            .fixed_pos(egui::pos2(anchor.left(), anchor.bottom() + 4.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(palette.color(FxColor::DefaultFill))
                    .stroke(egui::Stroke::new(1.0, palette.color(FxColor::Outline)))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(8, 8))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;

                        if menu_row(ui, &tr("Settings"), true, Mark::None, palette) {
                            chosen = Some(MenuChoice::Settings);
                        }
                        menu_separator(ui, palette);

                        let saving = editor
                            .as_ref()
                            .is_some_and(|e| e.purpose == EditorPurpose::SaveNew);
                        let mark = Mark::Submenu { expanded: saving };
                        if menu_row(ui, &tr("Save New Preset"), can_save_new, mark, palette) {
                            chosen = Some(MenuChoice::Editor(EditorPurpose::SaveNew));
                        }
                        if saving
                            && let Some(editor) = editor.as_mut()
                            && let Some(name) = name_editor(ui, editor, presets, palette)
                        {
                            committed = Some((EditorPurpose::SaveNew, name));
                        }

                        if menu_row(ui, &overwrite_label, can_overwrite, Mark::None, palette) {
                            chosen = Some(MenuChoice::Overwrite);
                        }
                        if menu_row(
                            ui,
                            &tr("Undo Preset Changes"),
                            can_undo,
                            Mark::None,
                            palette,
                        ) {
                            chosen = Some(MenuChoice::Undo);
                        }

                        let renaming = editor
                            .as_ref()
                            .is_some_and(|e| e.purpose == EditorPurpose::Rename);
                        let mark = Mark::Submenu { expanded: renaming };
                        if menu_row(ui, &tr("Rename Preset"), can_rename, mark, palette) {
                            chosen = Some(MenuChoice::Editor(EditorPurpose::Rename));
                        }
                        if renaming
                            && let Some(editor) = editor.as_mut()
                            && let Some(name) = name_editor(ui, editor, presets, palette)
                        {
                            committed = Some((EditorPurpose::Rename, name));
                        }

                        if menu_row(ui, &tr("Delete Preset"), can_delete, Mark::None, palette) {
                            chosen = Some(MenuChoice::Delete);
                        }
                        menu_separator(ui, palette);

                        if menu_row(ui, &tr("Export Presets"), can_transfer, Mark::None, palette) {
                            chosen = Some(MenuChoice::Export);
                        }
                        if menu_row(ui, &tr("Import Presets"), can_transfer, Mark::None, palette) {
                            chosen = Some(MenuChoice::Import);
                        }
                        menu_separator(ui, palette);

                        // `Theme ▸ Dark / Light`, ticked by the current mode, flattened into the
                        // menu under a heading.
                        menu_row(ui, &tr("Theme"), false, Mark::None, palette);
                        let tick = |on: bool| if on { Mark::Tick } else { Mark::None };
                        if menu_row(ui, &tr("Dark"), true, tick(dark), palette) {
                            chosen = Some(MenuChoice::Theme(ThemeMode::Dark));
                        }
                        if menu_row(ui, &tr("Light"), true, tick(!dark), palette) {
                            chosen = Some(MenuChoice::Theme(ThemeMode::Light));
                        }
                    });

                // A click anywhere else closes the menu — except the click that opened it, which
                // lands on the hamburger and is by definition outside the menu.
                if !just_opened
                    && ui.ctx().input(|i| i.pointer.any_click())
                    && !ui.rect_contains_pointer(ui.min_rect())
                {
                    close = true;
                }
            });

        // Escape closes the menu, and cancels the editor with it (`FxMainWindow.cpp:63-66`).
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        self.menu.just_opened = false;

        if let Some((purpose, name)) = committed {
            self.menu.close();
            match purpose {
                EditorPurpose::SaveNew => self.rt.app.handle(&[UiAction::SavePresetAs(name)]),
                EditorPurpose::Rename => self.rt.app.rename_preset(&name),
            }
            return;
        }
        match chosen {
            Some(MenuChoice::Editor(purpose)) => {
                // Clicking the row again folds the editor back up.
                self.menu.editor = match self.menu.editor.take() {
                    Some(editor) if editor.purpose == purpose => None,
                    _ => Some(NameEditor::new(purpose)),
                };
            }
            Some(choice) => {
                self.menu.close();
                self.act_on_menu(choice);
            }
            None if close => self.menu.close(),
            None => {}
        }
    }

    fn act_on_menu(&mut self, choice: MenuChoice) {
        match choice {
            MenuChoice::Settings => self.open_settings(),
            MenuChoice::Overwrite => self.rt.app.handle(&[UiAction::SavePreset]),
            MenuChoice::Undo => self.rt.app.handle(&[UiAction::UndoPresetChanges]),
            MenuChoice::Delete => self.rt.app.handle(&[UiAction::DeletePreset]),
            MenuChoice::Export => self.open_export(),
            MenuChoice::Import => self.open_import(),
            MenuChoice::Theme(mode) => {
                if mode != self.rt.app.state.theme {
                    self.rt.app.handle(&[UiAction::ToggleTheme]);
                }
            }
            // Dealt with in `show_menu`: it keeps the menu open.
            MenuChoice::Editor(_) => {}
        }
    }

    // ---- panes -------------------------------------------------------------------------------

    /// Draw the Settings pane, if it is open.
    ///
    /// The Windows build gives Settings its own window, and so did this port at first — but
    /// eframe 0.36's glow multi-viewport path creates the child toplevel and its GL surface on
    /// Wayland and then never composites anything into it (verified here on Hyprland 0.56 with
    /// NVIDIA: the window appears in `hyprctl clients` at the right size, and even a solid fill
    /// over its whole `max_rect` stays invisible). Rather than ship a Settings button that opens
    /// an empty window, the pane is drawn inside the main window over a dimmed backdrop, which is
    /// the alternative `docs/spec/00-architecture.md` §7.3 already lists.
    ///
    /// The main window grows to fit it while it is open, so nothing is clipped; closing it
    /// restores the view's own size through [`Shell::sync_view_size`].
    fn show_settings(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, palette: Palette) {
        let window = self.window_rect();
        let Some(mut state) = self.settings.take() else {
            return;
        };
        dim_backdrop(ui, window);
        let outer = egui::Rect::from_center_size(window.center(), dialogs::settings::WINDOW_SIZE);

        let response = SettingsDialog::new(&state).show(
            ui,
            outer,
            palette,
            &mut self.rt.app.assets,
            &mut self.settings_icons,
        );

        // Escape closes it, as it does in the original (`FxSettingsDialog.cpp:78-88`) — unless
        // the changelog is open on top, in which case Escape is its.
        let mut closed = !self.changelog && ctx.input(|i| i.key_pressed(egui::Key::Escape));
        for action in &response.actions {
            match action {
                dialogs::settings::SettingsAction::Close => closed = true,
                dialogs::settings::SettingsAction::ShowChangelog => self.changelog = true,
                _ => {}
            }
            self.rt.app.handle_settings(action, &mut state);
        }
        if !closed {
            self.settings = Some(state);
        }
    }

    /// Draw the changelog pane over Settings, if it is open.
    fn show_changelog(&mut self, ui: &mut egui::Ui, palette: Palette) {
        if !self.changelog {
            return;
        }
        let window = self.window_rect();
        dim_backdrop(ui, window);
        let outer = egui::Rect::from_center_size(window.center(), dialogs::changelog::WINDOW_SIZE);
        let response =
            ChangelogPane::new(CHANGELOG).show(ui, outer, palette, &mut self.rt.app.assets);
        if response.contains(&ChangelogAction::Close) {
            self.changelog = false;
        }
    }

    /// Draw the Import Presets pane, if it is open — the chooser, then the summary
    /// (`docs/spec/06-dialogs.md` §2).
    fn show_import(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, palette: Palette) {
        self.poll_folder_picker(ctx);
        let window = self.window_rect();
        let Some(mut state) = self.import.take() else {
            return;
        };
        dim_backdrop(ui, window);
        let dialog = ImportDialog::new(&state);
        let outer = egui::Rect::from_center_size(window.center(), dialog.window_size());
        let response = dialog.show(ui, outer, palette, &mut self.rt.app.assets, "main");

        let mut closed = false;
        for action in &response.actions {
            match action {
                // A native picker, not something this crate draws (§9.3).
                PresetsAction::ChooseImportFolder => self.start_folder_picker(),
                other => closed |= self.rt.app.handle_import(other, &mut state),
            }
        }
        if closed {
            // An answer that arrives now has nowhere to go.
            self.folder_picker = None;
        } else {
            self.import = Some(state);
        }
    }

    /// Draw the Export Presets pane, if it is open (`docs/spec/06-dialogs.md` §3).
    fn show_export(&mut self, ui: &mut egui::Ui, palette: Palette) {
        let window = self.window_rect();
        let Some(mut state) = self.export.take() else {
            return;
        };
        dim_backdrop(ui, window);
        let outer =
            egui::Rect::from_center_size(window.center(), dialogs::presets::export::WINDOW_SIZE);
        let response =
            ExportDialog::new(&state).show(ui, outer, palette, &mut self.rt.app.assets, "main");

        let mut closed = false;
        for action in &response.actions {
            closed |= self.rt.app.handle_export(action, &mut state);
        }
        if !closed {
            self.export = Some(state);
        }
    }

    /// Run the folder picker on its own thread. The blocking `rfd::FileDialog` would freeze the
    /// window if called from here, and on Linux the portal backend does not need the main
    /// thread (`docs/api/linux-desktop-crates.md` §4.4).
    fn start_folder_picker(&mut self) {
        if self.folder_picker.is_some() {
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        // JUCE's browser starts in the documents folder (`FxPresetImportDialog.cpp`).
        let start = dirs::document_dir().or_else(dirs::home_dir);
        let spawned = std::thread::Builder::new()
            .name("fxsound-folder-picker".into())
            .spawn(move || {
                let mut dialog =
                    rfd::FileDialog::new().set_title(dialogs::presets::SELECT_FOLDER_LABEL);
                if let Some(start) = start {
                    dialog = dialog.set_directory(start);
                }
                // The receiver may be gone if the pane was closed meanwhile; nothing to do then.
                let _ = tx.send(dialog.pick_folder());
            });
        match spawned {
            Ok(_) => self.folder_picker = Some(rx),
            Err(err) => {
                log::warn!("could not start the folder picker: {err}");
                self.rt.app.state.notification = Some(tr("Could not open the folder picker"));
            }
        }
    }

    /// Take the picker's answer, if it has one.
    fn poll_folder_picker(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.folder_picker else {
            return;
        };
        match rx.try_recv() {
            Ok(Some(folder)) => {
                if let Some(state) = &mut self.import {
                    state.folder = Some(folder);
                }
                self.folder_picker = None;
            }
            // Cancelled, or the thread is gone — a failed dialog looks the same as a cancel.
            Ok(None) | Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.folder_picker = None;
            }
            // Still up; egui will not repaint on its own while the user is in another window.
            Err(crossbeam_channel::TryRecvError::Empty) => ctx.request_repaint_after(PUMP_INTERVAL),
        }
    }
}

impl eframe::App for Shell<'_> {
    /// Transparent, so the window's own rounded corners are what the compositor composites.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let request = self.rt.tick();
        if std::mem::take(&mut self.rt.settings_requested) {
            self.open_settings();
        }
        self.apply_window_request(ctx, request);

        // Keep the loop ticking even with nothing on screen to redraw: the control socket and the
        // tray are drained from here, and eframe otherwise sleeps until something asks for a
        // frame — which left a compositor keybind doing nothing until the user happened to move
        // the mouse over the window.
        ctx.request_repaint_after(PUMP_INTERVAL);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.sync_theme(&ctx);
        self.sync_view_size(&ctx);

        let palette = self.rt.app.palette();

        // Fullscreen or maximised: the compositor hands over a surface larger than the design
        // size (1040×588 Pro, 550×189 Lite). Scale the design up to fit and centre it, rather
        // than drawing into a corner of the surface with the desktop showing through the rest.
        let design = self.window_size();
        let native_ppp = ctx
            .input(|i| i.viewport().native_pixels_per_point)
            .unwrap_or(1.0);
        let surface_px = ui.max_rect().size() * ctx.pixels_per_point();
        let zoom = fit_zoom(surface_px, design, native_ppp);
        if (zoom - ctx.zoom_factor()).abs() > ZOOM_TOLERANCE {
            // Applies at the start of the next pass; ask for it now rather than at the tick.
            ctx.set_zoom_factor(zoom);
            ctx.request_repaint();
        }
        let screen = ui.max_rect();
        let larger_than_design =
            screen.width() > design.x + 1.0 || screen.height() > design.y + 1.0;
        let content = if larger_than_design {
            // Opaque behind the centred content: the transparent rounded corners only make sense
            // when the surface *is* the window.
            ui.painter().rect_filled(
                screen,
                egui::CornerRadius::ZERO,
                palette.color(FxColor::WindowBackground),
            );
            egui::Rect::from_center_size(screen.center(), design)
        } else {
            screen
        };
        self.content_origin = content.min;
        let mut content_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(content)
                .id_salt("fx_window_content"),
        );
        let ui = &mut content_ui;

        let response = match self.rt.app.state.view {
            ViewMode::Pro => fxsound_ui::views::pro::show(
                ui,
                &self.rt.app.state,
                &mut self.scratch,
                palette,
                &mut self.rt.app.assets,
            ),
            ViewMode::Lite => fxsound_ui::views::lite::show(
                ui,
                &self.rt.app.state,
                &mut self.scratch,
                palette,
                &mut self.rt.app.assets,
            ),
        };

        for action in &response.actions {
            match action {
                // The original's close and minimise buttons both hide to the tray rather than
                // quitting (`FxMainWindow.cpp:579-585`, `:613-616`); Exit in the tray menu is the
                // only way out.
                UiAction::Close | UiAction::Minimise => self.hide(&ctx),
                UiAction::DragWindow => ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag),
                UiAction::OpenSettings => self.open_settings(),
                // A pane owns the window while it is open, as the original's modal dialogs do.
                UiAction::OpenMenu if !self.pane_open() => self.menu.toggle(),
                _ => {}
            }
        }
        self.rt.app.handle(&response.actions);

        self.show_settings(&ctx, ui, palette);
        self.show_changelog(ui, palette);
        self.show_import(&ctx, ui, palette);
        self.show_export(ui, palette);
        self.show_menu(&ctx, palette);

        // The visualizer and the meters are live, so keep painting while audio flows — at a
        // fixed cadence, since there is no vsync to pace the loop.
        if self.rt.app.state.audio_active {
            ctx.request_repaint_after(FRAME_INTERVAL);
        }
    }
}

/// `size` grown to fit `pane`.
fn grown(size: egui::Vec2, pane: egui::Vec2) -> egui::Vec2 {
    egui::vec2(size.x.max(pane.x), size.y.max(pane.y))
}

/// Dim what is behind a pane, so it reads as modal the way the original's dialogs did — inside
/// the window's own rounded outline, so the transparent corners stay transparent.
fn dim_backdrop(ui: &egui::Ui, window: egui::Rect) {
    ui.painter().rect_filled(
        window,
        egui::CornerRadius::same(layout::WINDOW_CORNER_RADIUS as u8),
        egui::Color32::from_black_alpha(160),
    );
}

// =============================================================================================
// Menu widgets
// =============================================================================================

/// Row width inside the menu frame. Wide enough for the 200 px name editor at its indent and for
/// "Overwrite Existing Preset - <name>" with a typical name.
const MENU_WIDTH: f32 = 244.0;
const MENU_ROW_HEIGHT: f32 = 26.0;
/// Text starts past a gutter that holds the theme ticks.
const MENU_TEXT_INSET: f32 = 26.0;
const MENU_SEPARATOR_HEIGHT: f32 = 9.0;
/// `FxPresetMenuItem::WIDTH` × `HEIGHT` (`FxMainWindow.cpp:96-97`).
const NAME_EDITOR_SIZE: egui::Vec2 = egui::vec2(200.0, 30.0);

/// The hamburger menu's per-window state.
#[derive(Default)]
struct Menu {
    open: bool,
    /// Set on the frame the menu was opened. The click that opens it lands on the hamburger,
    /// outside the menu, and must not be read as the click-outside that closes it.
    just_opened: bool,
    /// The inline name editor, when Save New Preset or Rename Preset is unfolded.
    editor: Option<NameEditor>,
}

impl Menu {
    fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open = true;
            self.just_opened = true;
        }
    }

    fn close(&mut self) {
        self.open = false;
        self.just_opened = false;
        self.editor = None;
    }
}

/// What the two inline editors do with the name once Enter is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorPurpose {
    /// `FxController::savePreset(name)`.
    SaveNew,
    /// `FxController::renamePreset(name)`.
    Rename,
}

/// `FxPresetMenuItem` (`FxMainWindow.cpp:27-176`): a live-validating name field.
struct NameEditor {
    purpose: EditorPurpose,
    text: String,
    /// Focus is requested once, on the first frame — not on every paint as the original does
    /// (`docs/spec/03-controls.md` §11.4).
    focused: bool,
}

impl NameEditor {
    fn new(purpose: EditorPurpose) -> Self {
        Self {
            purpose,
            text: String::new(),
            focused: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuChoice {
    Settings,
    /// Unfold (or fold) one of the inline editors.
    Editor(EditorPurpose),
    Overwrite,
    Undo,
    Delete,
    Export,
    Import,
    Theme(ThemeMode),
}

/// Decoration on a menu row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    None,
    /// A tick in the left gutter: the current theme.
    Tick,
    /// A triangle at the right edge, pointing down while the editor under the row is unfolded.
    Submenu {
        expanded: bool,
    },
}

/// One menu row. Returns `true` when it was clicked; a disabled row is drawn grey and inert.
fn menu_row(ui: &mut egui::Ui, label: &str, enabled: bool, mark: Mark, palette: Palette) -> bool {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(MENU_WIDTH, MENU_ROW_HEIGHT), sense);
    if !ui.is_rect_visible(rect) {
        return false;
    }
    let painter = ui.painter();

    let hovered = enabled && response.hovered();
    if hovered {
        painter.rect_filled(
            rect,
            egui::CornerRadius::same(4),
            palette.color(FxColor::MenuHighlightBackground),
        );
    }
    let color = palette.color(match (enabled, hovered) {
        (false, _) => FxColor::HintText,
        (true, true) => FxColor::MenuText,
        (true, false) => FxColor::DefaultText,
    });
    painter.text(
        egui::pos2(rect.left() + MENU_TEXT_INSET, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        theme::regular(14.0),
        color,
    );

    // Painted rather than typed: the app's font has no guarantee of ✓ or ▸ glyphs.
    let stroke = egui::Stroke::new(1.5, color);
    match mark {
        Mark::None => {}
        Mark::Tick => {
            let c = egui::pos2(rect.left() + MENU_TEXT_INSET / 2.0, rect.center().y);
            painter.line_segment(
                [c + egui::vec2(-4.0, 0.0), c + egui::vec2(-1.0, 3.0)],
                stroke,
            );
            painter.line_segment(
                [c + egui::vec2(-1.0, 3.0), c + egui::vec2(5.0, -4.0)],
                stroke,
            );
        }
        Mark::Submenu { expanded } => {
            let c = egui::pos2(rect.right() - 12.0, rect.center().y);
            let points = if expanded {
                vec![
                    c + egui::vec2(-4.0, -2.0),
                    c + egui::vec2(4.0, -2.0),
                    c + egui::vec2(0.0, 3.0),
                ]
            } else {
                vec![
                    c + egui::vec2(-2.0, -4.0),
                    c + egui::vec2(3.0, 0.0),
                    c + egui::vec2(-2.0, 4.0),
                ]
            };
            painter.add(egui::Shape::convex_polygon(
                points,
                color,
                egui::Stroke::NONE,
            ));
        }
    }

    enabled && response.clicked()
}

fn menu_separator(ui: &mut egui::Ui, palette: Palette) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(MENU_WIDTH, MENU_SEPARATOR_HEIGHT),
        egui::Sense::hover(),
    );
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, palette.color(FxColor::Outline)),
    );
}

/// The inline preset-name editor (`FxPresetMenuItem`, `docs/spec/03-controls.md` §11).
///
/// A 200 × 30 field with a 2 px outline: `ValidTextBorder` while the typed name is unique and
/// non-empty, `InvalidTextBorder` otherwise — so it starts red and turns blue once a usable name
/// is in it (§11.3, §11.4). Input is filtered through [`FORBIDDEN_PRESET_NAME_CHARS`] and capped
/// at [`MAX_PRESET_NAME_CHARS`]. Returns the name on Enter when it is valid (§11.5); Escape is
/// the menu's business.
fn name_editor(
    ui: &mut egui::Ui,
    editor: &mut NameEditor,
    presets: &[PresetEntry],
    palette: Palette,
) -> Option<String> {
    let (row, _) = ui.allocate_exact_size(
        egui::vec2(MENU_WIDTH, NAME_EDITOR_SIZE.y + 4.0),
        egui::Sense::hover(),
    );
    let field =
        egui::Rect::from_min_size(row.min + egui::vec2(MENU_TEXT_INSET, 2.0), NAME_EDITOR_SIZE);
    let hint = tr(match editor.purpose {
        EditorPurpose::SaveNew => "Enter your preset name",
        EditorPurpose::Rename => "Enter new preset name",
    });

    // Square corners, `DefaultFill` (§11.4). Painted first so the text lands on top of it.
    ui.painter().rect_filled(
        field,
        egui::CornerRadius::ZERO,
        palette.color(FxColor::DefaultFill),
    );

    let inner = field.shrink(2.0);
    let response = ui.place(
        inner,
        egui::TextEdit::singleline(&mut editor.text)
            .id(egui::Id::new("fxsound.menu.name_editor"))
            .font(theme::semibold(17.0))
            .text_color(palette.color(FxColor::DefaultText))
            .hint_text(
                egui::RichText::new(hint)
                    .font(theme::semibold(17.0))
                    .color(palette.color(FxColor::HintText)),
            )
            .char_limit(MAX_PRESET_NAME_CHARS)
            .frame(egui::Frame::NONE)
            .margin(egui::Margin::symmetric(4, 2))
            .vertical_align(egui::Align::Center)
            .min_size(inner.size())
            .desired_width(inner.width() - 8.0),
    );
    if !editor.focused {
        response.request_focus();
        editor.focused = true;
    }

    // `PresetNameInputFilter` (`FxPresetNameEditor.cpp:20-33`).
    if editor
        .text
        .contains(|c| FORBIDDEN_PRESET_NAME_CHARS.contains(c))
    {
        editor
            .text
            .retain(|c| !FORBIDDEN_PRESET_NAME_CHARS.contains(c));
    }
    let valid = preset_name_available(presets, &editor.text);

    let border = palette.color(if valid {
        FxColor::ValidTextBorder
    } else {
        FxColor::InvalidTextBorder
    });
    ui.painter().rect_stroke(
        field,
        egui::CornerRadius::ZERO,
        egui::Stroke::new(2.0, border),
        egui::StrokeKind::Inside,
    );

    let entered = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if entered {
        if valid {
            return Some(editor.text.trim().to_owned());
        }
        // Enter on a name that cannot be used does nothing but must not leave the field dead.
        response.request_focus();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use fxsound_core::DeviceDirection;

    fn state() -> TrayState {
        TrayState {
            presets: vec![
                TrayPreset {
                    name: "General".to_owned(),
                    factory: true,
                    modified: false,
                },
                TrayPreset {
                    name: "Mine".to_owned(),
                    factory: false,
                    modified: false,
                },
            ],
            selected_preset: Some(1),
            devices: vec![TrayDevice {
                name: "Built-in Audio Analogue Stereo".to_owned(),
                channels: 2,
                direction: DeviceDirection::Output,
            }],
            selected_device: Some(0),
            ..TrayState::default()
        }
    }

    /// Every change the tray renders has to move the fingerprint, or `Runtime::sync_tray` skips
    /// the `TrayHandle::update` and the menu keeps showing what it showed before.
    #[test]
    fn fingerprint_moves_with_everything_the_tray_draws() {
        let base = TrayFingerprint::of(&state());

        let mut on_top = state();
        on_top.always_on_top = true;
        assert_ne!(TrayFingerprint::of(&on_top), base, "the Always On Top tick");

        let mut edited = state();
        edited.presets[1].modified = true;
        assert_ne!(
            TrayFingerprint::of(&edited),
            base,
            "a preset's trailing ` *`"
        );

        let mut renamed = state();
        renamed.presets[1].name = "Renamed".to_owned();
        assert_ne!(TrayFingerprint::of(&renamed), base, "a renamed preset");

        let mut relabelled = state();
        relabelled.devices[0].name = "fifine Microphone Analogue Stereo".to_owned();
        assert_ne!(
            TrayFingerprint::of(&relabelled),
            base,
            "a device's description"
        );

        let mut turned = state();
        turned.devices[0].direction = DeviceDirection::Input;
        assert_ne!(TrayFingerprint::of(&turned), base, "a device's direction");

        let mut off = state();
        off.power = false;
        assert_ne!(
            TrayFingerprint::of(&off),
            base,
            "the Turn On/Turn Off label"
        );
    }

    /// The pixmaps are the one thing left out: `App::tray_state` never fills them, and they must
    /// not make every tick look like a change.
    #[test]
    fn fingerprint_ignores_the_pixmaps() {
        assert_eq!(TrayFingerprint::of(&state()), TrayFingerprint::of(&state()));

        let mut with_pixmaps = state();
        with_pixmaps.pixmaps.on.push(ksni::Icon {
            width: 1,
            height: 1,
            data: vec![0; 4],
        });
        assert_eq!(
            TrayFingerprint::of(&with_pixmaps),
            TrayFingerprint::of(&state())
        );
    }
}

/// The zoom factor that fits `design` (points) into a surface of `surface_px` physical pixels
/// at the compositor's `native_ppp`, never below 1.0: a surface *smaller* than the design is
/// clipped rather than shrunk, and one exactly the design size draws at the native scale.
fn fit_zoom(surface_px: egui::Vec2, design: egui::Vec2, native_ppp: f32) -> f32 {
    if design.x <= 0.0 || design.y <= 0.0 || native_ppp <= 0.0 {
        return 1.0;
    }
    let zoom = (surface_px.x / (design.x * native_ppp)).min(surface_px.y / (design.y * native_ppp));
    if zoom.is_finite() && zoom > 1.0 + ZOOM_TOLERANCE {
        zoom
    } else {
        1.0
    }
}

#[cfg(test)]
mod zoom_tests {
    use super::*;

    #[test]
    fn fullscreen_on_the_reference_monitor_scales_the_pro_window_to_the_height() {
        // 2560×1440 physical at scale 1.6, Pro design 1040×588 points.
        let zoom = fit_zoom(egui::vec2(2560.0, 1440.0), egui::vec2(1040.0, 588.0), 1.6);
        // 1440 / (588 · 1.6) = 1.5306 is the limiting axis; the width would allow 1.5385.
        assert!((zoom - 1.5306).abs() < 1e-3, "{zoom}");
        // The scaled design fits: 588 · 1.6 · zoom ≈ 1440 and 1040 · 1.6 · zoom < 2560.
        assert!(588.0 * 1.6 * zoom <= 1440.0 + 0.5);
        assert!(1040.0 * 1.6 * zoom <= 2560.0);
    }

    #[test]
    fn the_lite_window_scales_further_and_a_normal_window_does_not_scale_at_all() {
        let lite = fit_zoom(egui::vec2(2560.0, 1440.0), egui::vec2(550.0, 189.0), 1.6);
        assert!(lite > 2.9 && lite < 2.91, "{lite}");
        // The window at its own design size, in physical pixels.
        let exact = fit_zoom(
            egui::vec2(1040.0 * 1.6, 588.0 * 1.6),
            egui::vec2(1040.0, 588.0),
            1.6,
        );
        assert_eq!(exact, 1.0);
        // A surface smaller than the design is never shrunk.
        let small = fit_zoom(egui::vec2(800.0, 400.0), egui::vec2(1040.0, 588.0), 1.6);
        assert_eq!(small, 1.0);
        // Garbage in, native scale out.
        assert_eq!(
            fit_zoom(egui::vec2(0.0, 0.0), egui::vec2(1040.0, 588.0), 0.0),
            1.0
        );
    }
}
