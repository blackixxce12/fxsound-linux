//! The FxSound binary: parse the command line, be the single instance, run the window.
//!
//! Everything interesting lives in the library next door. This file is the shell that wires the
//! controller to eframe, the tray, the control socket and the D-Bus service, and it is
//! deliberately the only place that knows all five exist.
//!
//! ## Startup, in order
//!
//! 1. Parse the command line. `--self-test` is answered right here, before anything below can
//!    take a lock, open a socket, show a window or write a setting (0.4.0 design §13).
//! 2. Try to become the single instance. If another one is already running, forward the command
//!    line to it over the control socket, print whatever it says and exit — which is what makes
//!    `fxsound --next-preset` usable as a compositor keybind. `--watch` subscribes instead and
//!    prints the running instance's events until it quits. `--quit`, `--status` and `--watch`
//!    with nobody to forward to report that FxSound is not running and stop right here, before
//!    step 3 could claim the session default sink, and so do `--next-output` and `--next-input`;
//!    `--list-apps` is answered from the store on disk and stops here too.
//! 3. Start the audio engine. A missing or broken PipeWire is **not** fatal: the window still
//!    opens and says so. The Windows build quits hard when its driver is missing; on Linux the
//!    same conditions are routine and recoverable (`docs/spec/00-architecture.md` §10, open
//!    question 6).
//! 4. Apply the cold-start options — every option that sets something, where the Windows build
//!    drops the band lists, the effects and the preset management at a start — register the
//!    tray, then alternate between the two states below until something asks to quit. A line
//!    that only sets something — `fxsound --preset=Gaming` from a keybinding — starts in the
//!    tray, as a running FxSound leaves its window alone for it (0.4.0 audit R11); with no tray
//!    icon after a few seconds the window comes up minimised instead ([`TRAY_WAIT`]), and so it
//!    does for a start the remembered tray state put in the tray ([`waits_for_a_tray`]).
//!    `--next-output` and `--next-input` have no device to step from at a start: step 2 refuses
//!    a line with one, as it refuses `--status`.
//! 5. Just before that loop, start the D-Bus service (`fxsound_app::dbus`) on its own thread. It
//!    carries its calls to the same control channel the socket uses, so they are answered by the
//!    pump like a forwarded command line; no session bus, or its name taken, is a log line and
//!    not a failure. Then the suspend watcher (`fxsound_app::sleep`), on a thread of its own too,
//!    listening for logind on the system bus; no system bus is a log line as well.
//!
//! ## What wakes the pump
//!
//! Nothing is polled on a timer any more (0.4.0 design §12). Whatever hands the GUI thread work
//! wakes it through the one [`Waker`]: the control socket's connection threads and the D-Bus
//! service (through the channel they share), the tray's callbacks, the suspend watcher, the
//! termination signals, and the audio thread's notifications, which a small thread carries to the
//! controller ([`fxsound_app::wake`]). With a window, a wake-up is a repaint of it; without one,
//! the pump blocks on those channels ([`Runtime::wait`]). Either way it also looks once a second
//! ([`KEEPALIVE`]), sooner when a notice is due to go or a `--watch --meters` stream is open, and
//! the window paints at sixty a second only while the Pro view's visualizer has sound moving
//! through it ([`frame_interval`]).
//!
//! ## Two states: a window, or the tray alone
//!
//! The original hides to the tray on ✕ and on the compositor's close request, and quits only from
//! the tray's Exit (`docs/spec/01-window-layout.md` §7); its minimise button minimises
//! (`ShowWindow(SW_MINIMIZE)`, `FxMainWindow.cpp:579-585`). Here the minimise button hides to the
//! tray too while a tray icon is there to come back from — a Wayland compositor such as Hyprland
//! or sway has no minimised state to put a window in — and minimises the window as the original
//! does when there is none (0.4.0 audit #43), so that it cannot vanish on a GNOME session without
//! the AppIndicator extension. ✕ hides either way: FxSound processes the whole session's sound,
//! and a close that quit would stop it; with no tray the one-time notice says how to get the
//! window back (`fxsound --show`, or launching FxSound again). Either brings a minimised window
//! back too: out of the minimised state on X11, and on Wayland, where a client cannot do that,
//! as a fresh window in its place ([`show_action`]). On Wayland
//! a client cannot unmap and later remap its toplevel through winit —
//! `Window::set_visible` is a no-op there
//! (`winit-0.30.13/src/platform_impl/linux/wayland/window/mod.rs:253`) — so "hidden" cannot be a
//! window that exists but is not shown. Instead there is no window at all: eframe's default
//! `run_and_return` keeps the winit event loop in a thread-local and drives it with
//! `run_app_on_demand` (`eframe-0.36.0/src/native/run.rs:57-75`, `:374-383`), which means
//! [`eframe::run_native`] may be called again and again in one process. To hide, the shell
//! sends `ViewportCommand::Close`; the window is destroyed and `run_native` returns. To show, a
//! fresh window is run. In between, [`Runtime::run_headless`] pumps the engine, the control
//! socket and the tray as they wake it — audio never stops, `fxsound --show` and the tray still
//! work. What the window showed that is not the controller's — an open pane, the face the effect
//! column was turned to — is kept for the next window ([`Panes`]), so hiding and showing it again
//! looks the way hiding a window does.
//!
//! The window renders without vsync. eframe's glow backend otherwise lets Mesa's
//! `eglSwapBuffers` wait for a frame callback, which a Wayland compositor never sends for a
//! surface it is not showing — so a window parked on another Hyprland workspace stopped
//! answering `xdg_wm_base` pings and the compositor called the app unresponsive. Without vsync
//! the loop is paced by [`FRAME_INTERVAL`] instead, and only while something moves.

#![forbid(unsafe_code)]

use clap::Parser as _;
use eframe::egui;
use fxsound_app::{
    App, WindowVisibility,
    app::{
        FORBIDDEN_PRESET_NAME_CHARS, MAX_PRESET_NAME_CHARS, PresetMenu, preset_name_available,
        rename_name_available,
    },
    cli::{Cli, Command},
    commands::{self, WindowRequest},
    dbus::{self, DbusHandle},
    events::{self, AppEvent, EventSink, TraySink},
    ipc::{self, Instance},
    selftest,
    sleep::SleepWatch,
    tray::{self, TrayCommand, TrayHandle},
    wake::{Waker, WakingSender},
};
use fxsound_core::{ThemeMode, ViewMode, i18n::tr};
use fxsound_ui::{
    FxColor, Palette, UiAction,
    dialogs::{
        self, CalibrationDialog, CalibrationView, ConfirmChoice, ExportDialog, ExportState,
        ImportDialog, ImportState, MessageBox, PresetsAction,
        changelog::{ChangelogAction, ChangelogPane},
        settings::{NavIcons, SettingsAction, SettingsDialog, SettingsState},
    },
    layout,
    state::PresetEntry,
    theme,
    views::ViewScratch,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the pump goes without looking when nothing wakes it (0.4.0 design §12).
///
/// Everything that hands the GUI thread work wakes it (see the module docs), so this is only for
/// what nothing announces: sound starting or stopping on a device that was running already, which
/// the tray's icon and the window's logo show. Once a second costs nothing measurable; the 10 Hz
/// poll it replaces kept an idle FxSound on the CPU for as long as it ran.
const KEEPALIVE: Duration = Duration::from_secs(1);

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
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(fxsound_app::DEFAULT_LOG_FILTER),
    )
    .init();

    let cli = Cli::parse();

    // Step 1½: the self-test belongs to this process, not to an instance. It runs beside a
    // running FxSound and in a container with no session, and it leaves no trace in either.
    if cli.self_test {
        let report = selftest::run(&selftest::Environment::detect());
        println!("{}", report.render(cli.json));
        std::process::exit(report.exit_code());
    }

    // Step 2: single instance. A second invocation is a remote control, not a second app.
    let listener = match Instance::acquire() {
        Ok(Instance::Primary(listener)) => listener,
        Ok(Instance::Secondary(client)) => {
            // Started by the bus or the user unit for an instance that turned out to be running
            // already: it is the one the bus was waiting for, and nothing is forwarded — least of
            // all the `--hide` a plain `fxsound --hide` would send to a window just opened.
            if cli.activated {
                log::info!("FxSound is running already; nothing to activate");
                std::process::exit(0);
            }
            // A stream, not one answer: print events until the running instance quits. Decided by
            // the same `commands()` the primary runs, so `--status --watch` stays a `--status`.
            if let [Command::Watch { meters, .. }] = cli.commands().as_slice() {
                std::process::exit(client.watch(*meters));
            }
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
    // The one waker every producer below holds a clone of (see the module docs).
    let waker = Waker::new();
    let server = match listener.serve_waking(waker.clone()) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("could not serve the control socket: {err}");
            std::process::exit(1);
        }
    };

    // Holding the lock means no FxSound is running. `--status`, `--watch` and `--quit` address
    // one (`docs/spec/00-architecture.md` §4.7): say so and leave before step 3 claims the
    // session default sink — starting FxSound to answer would be the opposite of what was asked.
    // `--list-apps` is answered from the store on disk instead, which is the way to look up a
    // name for `--app-preset` before FxSound runs, and a `--forget-device` on its own is done to
    // the settings file, so the script that ran it gets its exit status rather than becoming
    // FxSound. `--next-output` and `--next-input` have no device to step from until FxSound has
    // started and attached its lanes, so a line with one is refused and starts nothing. Which of
    // them a line is, the same `commands()` decides that a running instance runs, so `--watch
    // --list-apps` lists here as it does there (`commands::answer_without_an_instance`). Dropping
    // `server` unlinks the socket again.
    if let Some(answer) = commands::answer_without_an_instance(
        &cli.commands(),
        &fxsound_core::AppRules::config_path(),
        &fxsound_core::Settings::config_path(),
    ) {
        drop(server);
        if !answer.stdout.is_empty() {
            println!("{}", answer.stdout);
        }
        if !answer.stderr.is_empty() {
            eprintln!("{}", answer.stderr);
        }
        std::process::exit(i32::from(answer.failed));
    }

    // A `--forget-device` beside options that start FxSound: done to the settings file before
    // they are read, as it is on a line of its own above. At a cold start there is no device list
    // to wait for yet, and nothing is attached to the device it names (0.4.0 audit #34), so
    // `Command::honoured_at_cold_start` leaves it out of step 4. Reported, and the start goes on.
    let forgotten = commands::forget_in_the_settings_file(
        &cli.commands(),
        &fxsound_core::Settings::config_path(),
    );
    if !forgotten.stderr.is_empty() {
        eprintln!("{}", forgotten.stderr);
    }

    // The UI language, decided before anything is drawn or named: the desktop's unless the
    // settings say otherwise (`fxsound_core::i18n`). The engine gets it too, for the node
    // descriptions the sound settings show.
    let settings = fxsound_core::Settings::load();
    let language = settings.effective_language();
    fxsound_core::i18n::set_language(language);

    // Step 3: audio, started with what its first pair of nodes depends on — the language, the
    // per-device volumes and both lanes' device rankings. Report the failure and carry on.
    let options = fxsound_app::app::engine_start_options(&settings, language);
    let engine = match fxsound_audio::AudioEngine::start_with_options(options) {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::error!("audio engine did not start: {err}");
            None
        }
    };

    let mut app = App::new(engine, &waker);

    // Step 4: the options a cold start honours — every one that sets something, the band lists,
    // the effects and the preset management included (`Command::honoured_at_cold_start`) — before
    // anything is drawn, on the presets `App::new` has read. No application stream
    // has been reported yet, so a preset named for an application the store does not know is held
    // until the engine has said which applications play and record: the game that was started
    // before FxSound is then reached by its own streams, as a running FxSound would reach it.
    app.hold_app_presets();
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
    // no such thing as a hidden window here (see the module docs). An explicit `--show` overrides
    // the preference, as `--hide` overrides it the other way (`Cli::cold_start_commands`), and so
    // does `--toggle-window`, since the window it toggles is not up yet. A line that only sets
    // something starts in the tray too (`cold_start_visibility`).
    let quiet = cli.only_sets_things();
    let mut visibility = cold_start_visibility(
        cold.window.hide,
        cold.window.show || cold.window.toggle,
        app.settings_run_minimized(),
        quiet,
    );

    // What start-up and the cold-start options changed is where every consumer starts from, not
    // news: the tray is built from the state as it is now, the D-Bus properties below read it,
    // and a `--watch` subscriber's first line is the status document.
    let _ = app.drain_events();
    let _ = app.take_tray_refresh();

    let (tray_tx, tray_rx) = crossbeam_channel::unbounded();
    let tray = match tray::spawn(app.tray_state(), WakingSender::new(tray_tx, waker.clone())) {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::warn!("no system tray: {err}");
            None
        }
    };

    // A Wayland client never sees `kill`, `systemctl --user stop` or a logout hook as a close
    // request, and a process that just dies leaves `default.configured.audio.*` naming a node
    // that vanished with its socket. SIGTERM, SIGINT and SIGHUP therefore become the tray's Exit:
    // the flag is read by the pump (`Runtime::tick`), which a thread of its own wakes for it, and
    // the quit it triggers runs the same hand-back-then-stop sequence, which blocks until the
    // server has confirmed the write. A second Ctrl+C while that is in flight still gets the user
    // out, with the conventional 130.
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
    let signals = wake_on_signals(&waker);

    // Step 5: the D-Bus service (0.4.0 design §9), on a thread of its own so that a slow bus
    // never holds the window up. With no session bus, or with the name owned already, it says so
    // in the log and the control socket carries on alone.
    let dbus = DbusHandle::start(server.control(), dbus::Properties::of(&app));

    // Step 6: suspend and resume (U13). logind's `PrepareForSleep` reaches the pump over a
    // channel, and the controller tells the engine, which silences both lanes on the way down and
    // starts them clean on the way up. Nothing holds the suspend up (upstream PR #533).
    let (sleep_tx, sleep_rx) = crossbeam_channel::unbounded();
    let sleep = SleepWatch::start(WakingSender::new(sleep_tx, waker.clone()));

    let mut runtime = Runtime {
        app,
        dbus: Some(dbus),
        sleep: Some(sleep),
        sleep_rx,
        server,
        tray,
        tray_rx,
        exit: WindowExit::Hidden,
        settings_requested: false,
        terminate,
        terminating: false,
        waker,
        signals,
        panes: Panes::default(),
        waiting_for_devices: Vec::new(),
        // A start in the tray that nobody asked for with `--hide` is in the tray only if a tray
        // icon comes up to be in; with none after `TRAY_WAIT`, the window comes up minimised.
        tray_wait: waits_for_a_tray(visibility, cold.window.hide)
            .then(|| Instant::now() + TRAY_WAIT),
        start_minimised: false,
        reopening: false,
    };

    loop {
        match visibility {
            WindowVisibility::Shown => match runtime.run_window() {
                Ok(WindowExit::Hidden) => visibility = WindowVisibility::Hidden,
                // A minimised window `--show` could not bring back on Wayland: a fresh one.
                Ok(WindowExit::Reopen) => {}
                Ok(WindowExit::Quit) => break,
                Err(err) => {
                    let tray_visible = runtime.tray_visible();
                    if runtime.window_failed(&err, tray_visible) {
                        visibility = WindowVisibility::Hidden;
                    } else {
                        // With no tray to fall back to, better to leave than to run with nothing
                        // on screen; the shutdown still restores the system default device.
                        runtime.shutdown();
                        return Err(err);
                    }
                }
            },
            WindowVisibility::Hidden => match runtime.run_headless() {
                HeadlessExit::Show => visibility = WindowVisibility::Shown,
                HeadlessExit::ShowMinimised => {
                    runtime.start_minimised = true;
                    visibility = WindowVisibility::Shown;
                }
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
    /// Close and map a fresh window at once: `--show` for a window the minimise button minimised,
    /// which a Wayland client cannot bring back itself ([`show_action`]). Not a hide: no tray tip,
    /// no `run_minimized`, no `window visible=false`.
    Reopen,
}

/// Why the tray-only state ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadlessExit {
    Show,
    /// A start in the tray found no tray icon to be in ([`TRAY_WAIT`]): the window, minimised.
    ShowMinimised,
    Quit,
}

/// How long a start in the tray waits for a tray icon to come up before it brings the window up
/// minimised instead ([`waits_for_a_tray`]) — on GNOME without the AppIndicator extension there
/// is none, and FxSound would run with nothing on screen to reach it by. A panel
/// that is there registers the icon well within it; one that starts later still finds FxSound in
/// the tray.
const TRAY_WAIT: Duration = Duration::from_secs(3);

/// Whether a start in the tray waits [`TRAY_WAIT`] for a tray icon and, with none by then, brings
/// the window up minimised: every start in the tray but one `--hide` or `--activated` asked for.
///
/// That is a line that only sets something (0.4.0 audit R11, review FA), and a start the
/// remembered tray state put there: FxSound quit in the tray on a desktop that had one and
/// launched again on one that has none (GNOME without an AppIndicator extension), where it would
/// otherwise run with nothing on screen to reach it by. The autostart entry's `--hide` and the
/// bus's and the user unit's `--activated` asked for no window, and get none.
const fn waits_for_a_tray(visibility: WindowVisibility, hide: bool) -> bool {
    matches!(visibility, WindowVisibility::Hidden) && !hide
}

/// Where a cold start begins (0.4.0 audit R11, review FA): in the tray for `--hide`, for the
/// remembered `run_minimized` unless `--show` overrides it, and for a line that only sets
/// something — `fxsound --preset=Gaming` from a keybinding in a fullscreen game, which a running
/// FxSound answers without raising its window; with the window otherwise.
const fn cold_start_visibility(
    hide: bool,
    show: bool,
    run_minimized: bool,
    only_sets_things: bool,
) -> WindowVisibility {
    if hide || ((run_minimized || only_sets_things) && !show) {
        WindowVisibility::Hidden
    } else {
        WindowVisibility::Shown
    }
}

/// Everything that outlives a window: the controller, the control socket, the D-Bus service and
/// the tray.
///
/// A [`Shell`] borrows this for the life of one window run; between runs
/// [`Runtime::run_headless`] drives the same parts directly.
struct Runtime {
    app: App,
    /// The D-Bus service. Declared before `server`, and taken first in [`Runtime::shutdown`]: the
    /// bus connection goes before the control channel its calls travel on.
    dbus: Option<DbusHandle>,
    server: ipc::Server,
    /// The suspend watcher; dropped in [`Runtime::shutdown`], which stops it.
    sleep: Option<SleepWatch>,
    /// What it heard: `true` as the system goes to sleep, `false` once it is back.
    sleep_rx: crossbeam_channel::Receiver<bool>,
    tray: Option<TrayHandle>,
    tray_rx: crossbeam_channel::Receiver<TrayCommand>,
    /// Why the window went, read once `run_native` has returned: [`WindowExit::Hidden`] from the
    /// start of each run ([`Runtime::run_window`]), [`WindowExit::Quit`] once the shell has been
    /// asked to quit. The compositor's close request never reaches the shell, and leaves it at
    /// `Hidden`.
    exit: WindowExit,
    /// The tray's Settings item was chosen and no window has acted on it yet. Set from
    /// [`Runtime::tick`], cleared by the shell that opens the pane — possibly a shell that does
    /// not exist yet, when the item is chosen while hidden.
    settings_requested: bool,
    /// Raised by the signal handlers; `true` means "quit at the next tick".
    terminate: Arc<AtomicBool>,
    /// The signal has been seen and turned into a quit request, so it is only logged once.
    terminating: bool,
    /// What every producer wakes the GUI thread with; the window's context is attached to it
    /// while there is a window.
    waker: Waker,
    /// The thread that wakes the pump for a termination signal ([`wake_on_signals`]); closed in
    /// [`Runtime::shutdown`].
    signals: Option<signal_hook::iterator::Handle>,
    /// What the last window showed that the next one shows again.
    panes: Panes,
    /// Forwarded lines held for PipeWire's first device list, each with the moment it stops
    /// waiting ([`commands::waits_for_the_device_list`]), in the order they arrived — and every
    /// line that arrived after one of them, which waits its turn behind it.
    waiting_for_devices: Vec<(ipc::Forwarded, Instant)>,
    /// A start in the tray that has not seen a tray icon yet, and when it stops waiting for one
    /// ([`TRAY_WAIT`]).
    tray_wait: Option<Instant>,
    /// The next window comes up minimised: a start in the tray that found no tray.
    start_minimised: bool,
    /// The window closing now is being opened again ([`WindowExit::Reopen`]): not news.
    reopening: bool,
}

impl Runtime {
    /// One tick of everything that has to keep happening whether or not a window exists: pull
    /// what the audio thread published, answer forwarded command lines, act on tray clicks, push
    /// the model into the tray. Returns what the window layer was asked to do.
    fn tick(&mut self) -> WindowRequest {
        // Before the engine is polled: a resume is heard before whatever the devices coming back
        // have to say.
        while let Ok(sleeping) = self.sleep_rx.try_recv() {
            self.app.system_sleeping(sleeping);
        }
        self.app.poll_audio();

        let mut request = WindowRequest::default();
        if self.terminate.load(Ordering::Relaxed) {
            if !self.terminating {
                log::info!("termination signal received; handing the session default back");
                self.terminating = true;
            }
            request.quit = true;
        }
        // Commands forwarded by a second invocation — the compositor keybind path — after those
        // still waiting for the device list, which the poll above may just have brought.
        let now = Instant::now();
        let arrived = self
            .server
            .drain()
            .into_iter()
            .map(|forwarded| (forwarded, now + commands::DEVICE_LIST_WAIT));
        let lines: Vec<_> = std::mem::take(&mut self.waiting_for_devices)
            .into_iter()
            .chain(arrived)
            .collect();
        for (forwarded, until) in lines {
            // Its caller was told FxSound did not answer in time and has gone: carried out now,
            // a burst of them held up by a busy GUI thread would all land at once.
            if forwarded.is_abandoned() {
                log::info!("not running a command whose caller stopped waiting for it");
                continue;
            }
            // A `--forget-device` for an instance PipeWire has not listed the devices to yet: the
            // whole line waits for the list, or until `until`, and is refused only then. A line
            // that came after a waiting one waits behind it (0.4.0 review FA): run first, a
            // keybind's `--preset=Day` would be undone by the held `--preset=Night` before it
            // when the list came. Its own wait is never the longer one — the line ahead of it
            // arrived first — so its caller still hears back in time.
            let behind = !self.waiting_for_devices.is_empty();
            if behind
                || (now < until
                    && commands::waits_for_the_device_list(&self.app, forwarded.commands()))
            {
                self.waiting_for_devices.push((forwarded, until));
                continue;
            }
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
        self.publish_events();
        request
    }

    /// Hand what the controller did since the last tick to the `--watch` subscribers, the D-Bus
    /// service and the tray: its own queue, drained once, the same events to each
    /// ([`events::fan_out`]). A tick in which nothing changed publishes nothing and leaves the
    /// tray alone.
    fn publish_events(&mut self) {
        // On the stack: this runs on every tick, and an idle tick should cost nothing.
        let server: &dyn EventSink = &self.server;
        let both: [&dyn EventSink; 2];
        let sinks = match &self.dbus {
            Some(dbus) => {
                both = [server, dbus];
                &both[..]
            }
            None => std::slice::from_ref(&server),
        };
        let tray = self.tray.as_ref().map(|tray| tray as &dyn TraySink);
        events::fan_out(&mut self.app, sinks, tray);
    }

    /// Say something the window layer did, rather than the controller, to the same consumers:
    /// the window coming and going, and the quit.
    fn announce(&self, event: &AppEvent) {
        self.server.publish(event);
        if let Some(dbus) = &self.dbus {
            dbus.publish(event);
        }
    }

    /// Run one window until it is closed. Returns why.
    fn run_window(&mut self) -> eframe::Result<WindowExit> {
        self.exit = WindowExit::Hidden;
        // The cached textures belong to the previous egui context and would draw nothing on
        // the new one.
        self.app.assets.clear();
        let mut options = native_options(self.app.state.view);
        if self.start_minimised {
            // Not taken from the game that started FxSound, where the compositor lets a client
            // say so.
            options.viewport = options.viewport.with_active(false);
        }

        // A window opened again in place of a minimised one was never gone.
        if !std::mem::take(&mut self.reopening) {
            self.announce(&AppEvent::Window { visible: true });
        }
        self.app.set_window_shown(true);
        // `showMainWindow` writes `run_minimized = false` (`FxController.cpp:933`).
        self.app.remember_window_hidden(false);
        let runtime = &mut *self;
        let run = eframe::run_native(
            "FxSound",
            options,
            Box::new(move |cc| {
                let palette = runtime.app.palette();
                theme::apply(&cc.egui_ctx, palette);
                // The zoom factor is ours (see `fit_zoom`); Ctrl+/- must not fight it.
                cc.egui_ctx
                    .options_mut(|options| options.zoom_with_keyboard = false);
                // From here on a producer's wake-up is a frame of this window.
                runtime.waker.attach(&cc.egui_ctx);
                Ok(Box::new(Shell::new(runtime, palette.mode())))
            }),
        );
        // Whatever arrives now is the headless pump's to wait for.
        self.waker.detach();
        // The meters went with the window, and let go of the microphone they held.
        self.app.set_window_shown(false);
        run?;
        let tray_visible = self.tray_visible();
        Ok(self.window_closed(tray_visible))
    }

    /// What happens once a window has gone, whatever closed it. Returns why it went.
    ///
    /// Every close but a quit is a hide to the tray — ✕, the minimise button, `--hide`,
    /// `--toggle-window`, the tray's left click, and the compositor's close request (Hyprland's
    /// `killactive`, Alt+F4 elsewhere), which closes the window without asking the shell — so
    /// what a hide leaves behind is done here, once, for all of them: the one-time tray tip
    /// ([`App::notify_hidden_to_tray`]), `run_minimized` for the next start as `hideMainWindow`
    /// writes it (`FxController.cpp:917`, 0.4.0 audit #35) — `true` only for a hide into a tray
    /// icon that is there, since a start in the tray is what the setting promises
    /// ([`App::remember_window_hidden`]) — and the `window visible=false` event. A quit is
    /// announced by [`Runtime::shutdown`], as `quit`, and leaves the setting as the window found
    /// it: a window still showing when FxSound was told to quit.
    fn window_closed(&mut self, tray_visible: bool) -> WindowExit {
        // The wizard is a pane of the window that just went: a run it was in the middle of stops,
        // and the microphone is let go rather than held for a window that is not there.
        self.app.cancel_calibration();
        if self.exit == WindowExit::Hidden {
            self.app.notify_hidden_to_tray(tray_visible);
            self.app.remember_window_hidden(tray_visible);
            self.announce(&AppEvent::Window { visible: false });
        }
        self.reopening = self.exit == WindowExit::Reopen;
        self.exit
    }

    /// What happens when a window could not be created or run. Returns whether FxSound stays in
    /// the tray; `false` leaves it to the caller to quit.
    ///
    /// With a tray icon on screen, it stays: the audio, the tray, the control socket and the bus
    /// all work without a window, and quitting would hand the defaults back, drop the sound, and —
    /// under the systemd user unit's `Restart=on-failure` — start it all again hidden a few seconds
    /// later, the same way every time the window is asked for. That is an instance the session bus
    /// started for a status bar's call, where the systemd user manager never had the compositor's
    /// `WAYLAND_DISPLAY` or `DISPLAY` to hand it (sway, or Hyprland without uwsm). So the reason is
    /// logged, a notification says the window could not be opened, and the window's coming is
    /// announced undone: `window visible=false`. `run_minimized` stays the `false`
    /// [`Runtime::run_window`] wrote, since the window was asked for: the notification tells the
    /// user to quit from the tray and start FxSound again, and that start, a bare `fxsound` from a
    /// launcher, opens the window. The bus's `--activated` start stays in the tray whatever the
    /// setting says (`Cli::window_command`), so this brings back no failing start. With no tray
    /// icon there is nothing to fall back to, and it quits as before.
    fn window_failed(&mut self, err: &dyn std::fmt::Display, tray_visible: bool) -> bool {
        if !tray_visible {
            log::error!("the window could not be run: {err}");
            return false;
        }
        log::error!(
            "the window could not be run: {err}; FxSound keeps running in the system tray (is \
             WAYLAND_DISPLAY or DISPLAY set in its environment?)"
        );
        self.app.cancel_calibration();
        self.announce(&AppEvent::Window { visible: false });
        self.app.window_unavailable();
        // Nothing asks for the window again on its own: a Settings item chosen in the tray would
        // otherwise have the tray-only state ask for it at once, and fail, for ever.
        self.settings_requested = false;
        self.start_minimised = false;
        self.reopening = false;
        true
    }

    /// Whether a tray icon is on screen to bring the window back from ([`TrayHandle::is_visible`]).
    fn tray_visible(&self) -> bool {
        self.tray.as_ref().is_some_and(TrayHandle::is_visible)
    }

    /// The tray-only state: pump until something asks for the window or for the exit, blocking
    /// in between until something arrives ([`Runtime::wait`]).
    fn run_headless(&mut self) -> HeadlessExit {
        log::info!("window hidden; FxSound keeps running in the system tray");
        loop {
            // Before the tick, so that a wake-up that comes during it is still pending after it.
            self.waker.clear();
            let request = self.tick();
            if request.quit {
                return HeadlessExit::Quit;
            }
            // With no window, toggle means show.
            if request.show || request.toggle || self.settings_requested {
                self.tray_wait = None;
                return HeadlessExit::Show;
            }
            if let Some(until) = self.tray_wait {
                if self.tray_visible() {
                    self.tray_wait = None;
                } else if Instant::now() >= until {
                    self.tray_wait = None;
                    log::info!("no tray icon to start in; the window comes up minimised");
                    return HeadlessExit::ShowMinimised;
                }
            }
            self.wait(self.pump_interval(Instant::now()));
        }
    }

    /// How long the pump may go before it looks again when nothing wakes it: [`KEEPALIVE`], or
    /// less when the controller has something due sooner ([`App::next_deadline`]) or a `--watch
    /// --meters` stream is open, whose meters go out four times a second
    /// ([`ipc::METER_INTERVAL`]).
    fn pump_interval(&self, now: Instant) -> Duration {
        let mut interval = KEEPALIVE;
        if self.server.wants_meters() {
            interval = interval.min(ipc::METER_INTERVAL);
        }
        if let Some(due) = self.app.next_deadline() {
            interval = interval.min(due.saturating_duration_since(now));
        }
        if let Some(until) = self.tray_wait {
            interval = interval.min(until.saturating_duration_since(now));
        }
        // A line held for the device list is refused when its wait is over, not at the keepalive
        // after; the list itself wakes the pump through the audio channel.
        if let Some(until) = self
            .waiting_for_devices
            .iter()
            .map(|(_, until)| *until)
            .min()
        {
            interval = interval.min(until.saturating_duration_since(now));
        }
        interval
    }

    /// Block until something has arrived for the pump, or `timeout` has passed — the headless
    /// state's sleep (0.4.0 design §12): a forwarded command line or a D-Bus call, a tray click,
    /// the suspend watcher, the audio thread, or a wake-up that has no channel of its own.
    fn wait(&self, timeout: Duration) {
        let watched: Vec<&dyn Watched> = [
            Some(self.server.arrivals() as &dyn Watched),
            Some(&self.tray_rx),
            Some(&self.sleep_rx),
            self.app
                .audio_notifications()
                .map(|audio| audio as &dyn Watched),
            Some(self.waker.pending()),
        ]
        .into_iter()
        .flatten()
        .collect();
        wait_for_any(&watched, Instant::now() + timeout);
    }

    /// The only way out: stash unsaved edits, restore the system default device and stop the
    /// engine, remove the tray item. Dropping the server unlinks the control socket and ends
    /// every `--watch` stream, right after the `quit` event said why.
    ///
    /// The bus goes first. Whatever was forwarded after the quit was decided is refused rather
    /// than left to wait out its timeout, and then the D-Bus service gives its names back — a
    /// call answered already, `Quit`'s own, still gets its reply — and closes its connection,
    /// before the control channel its calls travel on goes away.
    fn shutdown(mut self) {
        // Whatever the last tick left unsaid goes out before the stream ends.
        self.publish_events();
        self.announce(&AppEvent::Quit);
        self.server.refuse_pending();
        // Answered "shutting down" as they go, like the lines `refuse_pending` just refused.
        self.waiting_for_devices.clear();
        if let Some(dbus) = self.dbus.take() {
            dbus.shutdown();
        }
        drop(self.sleep.take());
        self.app.shutdown();
        if let Some(tray) = &self.tray {
            tray.shutdown();
        }
        if let Some(signals) = self.signals.take() {
            signals.close();
        }
    }
}

/// Wake the pump for SIGTERM, SIGINT and SIGHUP, on a thread of its own: the handlers only raise
/// the terminate flag, which is all a signal handler may do, and the pump would otherwise read it
/// at its next keepalive. Registered after the flag's handlers, so the flag is up before the
/// wake-up arrives. `None`, with a log line, when the thread cannot be had; the flag is still read
/// then, only later.
fn wake_on_signals(waker: &Waker) -> Option<signal_hook::iterator::Handle> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = match signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        Ok(signals) => signals,
        Err(err) => {
            log::warn!("a termination signal will wait for the next keepalive: {err}");
            return None;
        }
    };
    let handle = signals.handle();
    let waker = waker.clone();
    let spawned = std::thread::Builder::new()
        .name("fxsound-signals".to_owned())
        .spawn(move || {
            for _ in signals.forever() {
                waker.wake();
            }
        });
    match spawned {
        Ok(_) => Some(handle),
        Err(err) => {
            log::warn!("a termination signal will wait for the next keepalive: {err}");
            None
        }
    }
}

/// A channel the headless pump waits on, whatever it carries.
trait Watched {
    /// Wait on it in `select`; the operation's index.
    fn watch<'a>(&'a self, select: &mut crossbeam_channel::Select<'a>) -> usize;
    /// Whether a message is waiting in it.
    fn holds_message(&self) -> bool;
}

impl<T> Watched for crossbeam_channel::Receiver<T> {
    fn watch<'a>(&'a self, select: &mut crossbeam_channel::Select<'a>) -> usize {
        select.recv(self)
    }

    fn holds_message(&self) -> bool {
        !self.is_empty()
    }
}

/// Block until one of `channels` holds a message, or until `deadline`. Returns whether one does.
///
/// Waits without taking anything: the pump's tick drains each channel in its own way. A channel
/// that reports ready with nothing in it is one whose senders are all gone — a tray that could not
/// register, a suspend watcher with no system bus — and is dropped from this wait rather than
/// spun on; a spurious readiness drops one for this wait only, which a producer's
/// wake-up still covers ([`Waker::pending`] is among the channels and never closes).
fn wait_for_any(channels: &[&dyn Watched], deadline: Instant) -> bool {
    let mut select = crossbeam_channel::Select::new();
    for channel in channels {
        channel.watch(&mut select);
    }
    loop {
        let Ok(index) = select.ready_deadline(deadline) else {
            return false;
        };
        // Registered in order, so an operation's index is its channel's.
        if channels
            .get(index)
            .is_some_and(|channel| channel.holds_message())
        {
            return true;
        }
        select.remove(index);
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

// =============================================================================================
// The window
// =============================================================================================

/// What a window shows that is neither the controller's nor the window's own, and outlives the
/// window: the panes that were open, the folder picker still running for one of them, and the
/// face the effect column was turned to.
///
/// Hiding to the tray destroys the window (see the module docs); the next window takes these
/// back, so hiding and showing again looks the way it does in the original, whose window is only
/// hidden. The hamburger menu is not kept — a popup closes with its window — and neither is the
/// calibration wizard, whose run [`Runtime::run_window`] cancels rather than holding the
/// microphone for a window that is not there.
#[derive(Default)]
struct Panes {
    scratch: ViewScratch,
    settings: Option<SettingsState>,
    import: Option<ImportState>,
    export: Option<ExportState>,
    folder_picker: Option<crossbeam_channel::Receiver<Option<PathBuf>>>,
    changelog: bool,
}

/// The eframe application: one window, one frame at a time, over a borrowed [`Runtime`].
///
/// Built each time a window is shown, from the [`Panes`] the last window left, and giving them
/// back when it is dropped; the applied size and theme, the menu and the textures are the
/// window's own and start afresh. Nothing in it may stop the engine or the tray —
/// `eframe::App::on_exit` is deliberately left at its no-op default, because the window going
/// away is how *hiding* works (see the module docs), and only [`Runtime::shutdown`] ends the
/// audio.
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
    /// What the calibration wizard draws this frame, while it is open on top of Settings ▸
    /// Microphone.
    ///
    /// A copy: the wizard's state machine is the controller's ([`App::calibration_view`]), which
    /// times the phases from the input lane's meters (0.4.0 design §8). Taken again at the top
    /// of every frame, after the tick has driven it, so the window is sized for the phase it is
    /// about to draw.
    calibration: Option<CalibrationView>,
    /// Where the design-size content starts this frame: the viewport's origin, or the centred
    /// offset when the surface is larger than the design (see [`fit_zoom`]).
    content_origin: egui::Pos2,
    /// A question the window is asking before it does something that cannot be taken back from
    /// inside FxSound: deleting a preset, discarding every unsaved change.
    confirm: Option<Confirm>,
    /// The window asked the compositor to minimise it — the minimise button with no tray icon
    /// there (0.4.0 audit #43), or a start in the tray that found none — and has not had the
    /// focus or the pointer since. `--show` has to bring it back ([`show_action`]).
    minimised: bool,
    /// Whether the window had the focus at the last frame, to see it come back.
    was_focused: Option<bool>,
}

/// What `--show`, a bare relaunch or D-Bus `Show()` does to the window there is ([`show_action`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShowAction {
    /// Raise it: `ViewportCommand::Focus`.
    Focus,
    /// Take it out of the minimised state, then raise it (X11).
    Restore,
    /// Close it and map a fresh one ([`WindowExit::Reopen`]): a Wayland client cannot take its
    /// toplevel out of the minimised state, and winit's `focus_window` is a no-op there.
    Reopen,
}

/// How to show a window that is `minimised` (review FA; `--show` is "show and raise the window"
/// and the original's `showMainWindow` restores it). winit 0.30's `focus_window` returns early for
/// an iconic X11 window (`x11/window.rs:1773`) and does nothing at all on Wayland
/// (`wayland/window/mod.rs:629`), so a Focus alone left a minimised window where it was and
/// `fxsound --show` exited 0. On X11 the window is asked out of the minimised state first; on
/// Wayland, where nothing can do that from the client side, it is replaced by a fresh toplevel —
/// the path hiding to the tray and showing again takes already.
const fn show_action(minimised: bool, wayland: bool) -> ShowAction {
    match (minimised, wayland) {
        (false, _) => ShowAction::Focus,
        (true, false) => ShowAction::Restore,
        (true, true) => ShowAction::Reopen,
    }
}

/// Whether winit runs on Wayland here: its own rule (`platform_impl/linux/mod.rs:736-766`), a
/// non-empty `WAYLAND_DISPLAY` or `WAYLAND_SOCKET`, since FxSound forces no backend.
fn wayland_session(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
        .into_iter()
        .any(|name| var(name).is_some_and(|value| !value.is_empty()))
}

/// What [`Shell::confirm`] asks about. The answer is acted on only while it still means what was
/// asked: a Delete for the preset and lane on screen when it was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Confirm {
    /// Delete Preset (0.4.0 audit #16): the original deletes at once and for good.
    Delete {
        lane: fxsound_core::DeviceDirection,
        name: String,
    },
    /// Settings ▸ Reset Presets (0.4.0 audit #21): every unsaved change, on both lanes.
    ResetPresets,
}

impl Confirm {
    /// The question, in the user's language, as the message box shows it: wrapped over its two
    /// lines, and a preset name too wide for them cut in the middle rather than the question at
    /// its end ([`dialogs::message::message_with_name`]).
    fn question(&self, ctx: &egui::Context) -> String {
        let template = match self {
            Self::Delete { .. } => tr(DELETE_QUESTION),
            Self::ResetPresets => tr(RESET_QUESTION),
        };
        self.question_from(ctx, &template)
    }

    /// [`Confirm::question`] from its question in any language: [`DELETE_QUESTION`]'s `%s` is the
    /// preset's name.
    fn question_from(&self, ctx: &egui::Context, template: &str) -> String {
        match self {
            Self::Delete { name, .. } => dialogs::message::message_with_name(ctx, template, name),
            Self::ResetPresets => template.to_owned(),
        }
    }
}

/// The question Delete Preset asks. The preset goes to the desktop's trash
/// (`fxsound_preset::trash`), where a file manager can restore it.
const DELETE_QUESTION: &str = "Move the preset %s to the trash?";
/// The question Reset Presets asks: what it does, which is less than its label says — saved
/// presets stay.
const RESET_QUESTION: &str = "Discard the unsaved changes of every preset? Saved presets are kept.";

impl<'a> Shell<'a> {
    fn new(rt: &'a mut Runtime, applied_theme: ThemeMode) -> Self {
        let Panes {
            scratch,
            mut settings,
            import,
            export,
            folder_picker,
            changelog,
        } = std::mem::take(&mut rt.panes);
        // The tray's Settings item may have been chosen while there was no window.
        if std::mem::take(&mut rt.settings_requested) && settings.is_none() {
            settings = Some(rt.app.settings_state());
        }
        // A gesture in progress and the visualizer's history went with the last window, as the
        // original's visualizer is reset when it pauses; the column's face is the user's, and the
        // logo shows what is playing now rather than fading towards it.
        let lit = rt.app.state.audio_active && rt.app.state.power;
        let scratch = ViewScratch {
            column_face: scratch.column_face,
            logo_fade: if lit { 1.0 } else { 0.0 },
            ..ViewScratch::default()
        };
        Self {
            rt,
            scratch,
            applied_size: None,
            applied_theme,
            menu: Menu::default(),
            settings,
            settings_icons: NavIcons::new(),
            import,
            export,
            folder_picker,
            changelog,
            calibration: None,
            content_origin: egui::Pos2::ZERO,
            confirm: None,
            minimised: false,
            was_focused: None,
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
            let wayland = wayland_session(|name| std::env::var_os(name));
            match show_action(self.minimised, wayland) {
                ShowAction::Focus => ctx.send_viewport_cmd(egui::ViewportCommand::Focus),
                ShowAction::Restore => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.minimised = false;
                }
                ShowAction::Reopen => {
                    self.rt.exit = WindowExit::Reopen;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        } else if request.hide || request.toggle {
            Self::hide(ctx);
        }
    }

    /// Hide to the tray: destroy the window and let `run_native` return with
    /// [`WindowExit::Hidden`], which is what [`Runtime::exit`] already says. The tray tip and the
    /// `run_minimized` a hide leaves behind are [`Runtime::window_closed`]'s, which the
    /// compositor's close request reaches too.
    fn hide(ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// The title bar's minimise button: [`minimise_action`] for the tray there is now.
    fn minimise(&mut self, ctx: &egui::Context) {
        match minimise_action(self.rt.tray_visible()) {
            MinimiseAction::HideToTray => Self::hide(ctx),
            MinimiseAction::Minimise => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                self.minimised = true;
            }
        }
    }

    /// Notice the window coming back from minimised by the user's own hand — the dock, the
    /// taskbar, Alt+Tab: it has the focus again, or the pointer pressed in it.
    fn follow_minimised(&mut self, ctx: &egui::Context) {
        let (focused, pressed) = ctx.input(|i| (i.viewport().focused, i.pointer.any_pressed()));
        if self.minimised && (pressed || (focused == Some(true) && self.was_focused == Some(false)))
        {
            self.minimised = false;
        }
        self.was_focused = focused;
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
        if let Some(calibration) = &self.calibration {
            size = grown(size, calibration.window_size());
        }
        if self.confirm.is_some() {
            size = grown(size, dialogs::message::WINDOW_SIZE);
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
        self.settings.is_some()
            || self.import.is_some()
            || self.export.is_some()
            || self.changelog
            || self.calibration.is_some()
            || self.confirm.is_some()
    }

    fn open_settings(&mut self) {
        self.menu.close();
        if self.settings.is_none() {
            self.settings = Some(self.rt.app.settings_state());
        }
    }

    /// Settings ▸ Microphone ▸ "Calibrate microphone…". The pane only offers it with a microphone
    /// selected, and the microphone is asked for again here because the device list can change
    /// between the frame that drew the button and this one.
    fn open_calibration(&mut self) {
        if self.rt.app.open_calibration() {
            self.calibration = self.rt.app.calibration_view();
        }
    }

    fn open_import(&mut self) {
        self.menu.close();
        if self.import.is_none() {
            // Into the lane the window is on now, whatever the tray, a keybind or D-Bus does to
            // the edit direction before Import is pressed.
            self.import = Some(ImportState {
                lane: self.rt.app.state.direction,
                ..ImportState::default()
            });
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
            // The names are this lane's, so the files written are too, whatever happens to the
            // edit direction before Export is pressed.
            self.export = Some(ExportState {
                lane: self.rt.app.state.direction,
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

        // The enablement predicates of `FxMainWindow.cpp:536-543`: the preset items are the
        // controller's one rule, which the command line and D-Bus are refused by too, with the
        // power on or off (0.4.0 audit R7); Export and Import are always offered (audit #18,
        // [`App::preset_menu`]).
        let preset = app.state.preset();
        let PresetMenu {
            save_new: can_save_new,
            overwrite: can_overwrite,
            undo: can_undo,
            rename: can_rename,
            delete: can_delete,
            export: can_export,
            import: can_import,
        } = app.preset_menu();
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

        if self.menu.editor.as_ref().is_some_and(|editor| {
            !editor.still_applies(
                app.state.direction,
                preset.map(|p| p.name.as_str()),
                can_save_new,
                can_rename,
            )
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
                    .stroke(egui::Stroke::new(1.0, palette.divider()))
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
                            && let Some(name) = name_editor(ui, editor, presets, None, palette)
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
                            && let Some(name) = name_editor(
                                ui,
                                editor,
                                presets,
                                preset.map(|p| p.name.as_str()),
                                palette,
                            )
                        {
                            committed = Some((EditorPurpose::Rename, name));
                        }

                        if menu_row(ui, &tr("Delete Preset"), can_delete, Mark::None, palette) {
                            chosen = Some(MenuChoice::Delete);
                        }
                        menu_separator(ui, palette);

                        if menu_row(ui, &tr("Export Presets"), can_export, Mark::None, palette) {
                            chosen = Some(MenuChoice::Export);
                        }
                        if menu_row(ui, &tr("Import Presets"), can_import, Mark::None, palette) {
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
            let opened_for = self
                .menu
                .editor
                .as_ref()
                .map(|editor| (editor.lane, editor.preset.clone()));
            self.menu.close();
            match purpose {
                EditorPurpose::SaveNew => self.rt.app.handle(&[UiAction::SavePresetAs(name)]),
                // For the preset the editor was opened on, if it is still the one selected and
                // still one the menu would offer to rename: the answer comes a frame or many
                // after the offer, as the Delete question's does.
                EditorPurpose::Rename => {
                    if let Some((lane, Some(old))) = opened_for {
                        self.rt.app.rename_preset_opened_for(lane, &old, &name);
                    }
                }
            }
            return;
        }
        match chosen {
            Some(MenuChoice::Editor(purpose)) => {
                // Clicking the row again folds the editor back up.
                let app = &self.rt.app;
                self.menu.editor = match self.menu.editor.take() {
                    Some(editor) if editor.purpose == purpose => None,
                    _ => Some(NameEditor::new(
                        purpose,
                        app.state.direction,
                        app.state.preset().map(|p| p.name.as_str()),
                    )),
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
            // Asked first (0.4.0 audit #16); `show_confirmation` deletes on Yes.
            MenuChoice::Delete => {
                if let Some(preset) = self.rt.app.state.preset() {
                    self.confirm = Some(Confirm::Delete {
                        lane: self.rt.app.state.direction,
                        name: preset.name.clone(),
                    });
                }
            }
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
        dim_backdrop(ui, window, "settings");
        let outer = egui::Rect::from_center_size(window.center(), dialogs::settings::WINDOW_SIZE);

        // What the audio thread has said since the pane opened: the echo canceller's state and
        // whether there is still a microphone to calibrate.
        self.rt.app.refresh_settings_state(&mut state);
        let response = SettingsDialog::new(&state).show(
            ui,
            outer,
            palette,
            &mut self.rt.app.assets,
            &mut self.settings_icons,
        );

        // A wizard open on top owns the input: Settings is drawn under it and nothing it reports
        // counts — its Escape and its clicks belong to the wizard.
        if self.calibration.is_some() {
            self.settings = Some(state);
            return;
        }

        // Escape closes it, as it does in the original (`FxSettingsDialog.cpp:78-88`) — unless
        // the changelog is open on top, in which case Escape is its.
        let escaped = !self.changelog && ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let closed = self.act_on_settings(&response.actions, &mut state);
        if !(escaped || closed) {
            self.settings = Some(state);
        }
    }

    /// Act on what the Settings pane reported this frame, on the pane's `state`; true if it asked
    /// to be closed. The pane's own buttons open the changelog and the wizard here, the reset is
    /// asked about first, and the rest goes to the controller.
    fn act_on_settings(&mut self, actions: &[SettingsAction], state: &mut SettingsState) -> bool {
        let mut closed = false;
        for action in actions {
            match action {
                SettingsAction::Close => closed = true,
                SettingsAction::ShowChangelog => self.changelog = true,
                SettingsAction::OpenCalibration => self.open_calibration(),
                // Asked first (0.4.0 audit #21); `show_confirmation` resets on Yes.
                SettingsAction::ResetPresets => {
                    self.confirm = Some(Confirm::ResetPresets);
                    continue;
                }
                _ => {}
            }
            self.rt.app.handle_settings(action, state);
        }
        closed
    }

    /// Ask the question in [`Shell::confirm`], if there is one, and act on the answer.
    fn show_confirmation(&mut self, ctx: &egui::Context, palette: Palette) {
        let Some(confirm) = self.confirm.clone() else {
            return;
        };
        let question = confirm.question(ctx);
        if let Some(choice) = MessageBox::new(&question).show_modal(
            ctx,
            palette,
            &mut self.rt.app.assets,
            "fxsound.confirm",
        ) {
            self.answer(choice);
        }
    }

    /// Put the question away and act on a Yes.
    ///
    /// Delete goes ahead only if the preset asked about is still the one selected on the lane
    /// asked about: a keybind, the tray or D-Bus can move the selection while the question is up,
    /// and a Yes to one name must not delete another.
    fn answer(&mut self, choice: ConfirmChoice) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        if choice != ConfirmChoice::Yes {
            return;
        }
        match confirm {
            Confirm::Delete { lane, name } => {
                let app = &mut self.rt.app;
                if app.state.direction == lane && app.state.preset().is_some_and(|p| p.name == name)
                {
                    app.handle(&[UiAction::DeletePreset]);
                } else {
                    log::info!("{name} is no longer the selected preset; not deleting it");
                }
            }
            Confirm::ResetPresets => {
                let open = self.settings.is_some();
                let mut state = self
                    .settings
                    .take()
                    .unwrap_or_else(|| self.rt.app.settings_state());
                self.rt
                    .app
                    .handle_settings(&SettingsAction::ResetPresets, &mut state);
                self.rt.app.refresh_settings_state(&mut state);
                if open {
                    self.settings = Some(state);
                }
            }
        }
    }

    /// Draw the changelog pane over Settings, if it is open.
    fn show_changelog(&mut self, ui: &mut egui::Ui, palette: Palette) {
        if !self.changelog {
            return;
        }
        let window = self.window_rect();
        dim_backdrop(ui, window, "changelog");
        let outer = egui::Rect::from_center_size(window.center(), dialogs::changelog::WINDOW_SIZE);
        let response =
            ChangelogPane::new(CHANGELOG).show(ui, outer, palette, &mut self.rt.app.assets);
        if response.contains(&ChangelogAction::Close) {
            self.changelog = false;
        }
    }

    /// Draw the calibration wizard over Settings, if it is open (0.4.0 design §8). What the user
    /// presses goes to the controller, which runs the wizard; the next frame draws what it made
    /// of it.
    fn show_calibration(&mut self, ui: &mut egui::Ui, palette: Palette) {
        let window = self.window_rect();
        let Some(view) = self.calibration.take() else {
            return;
        };
        dim_backdrop(ui, window, "calibration");
        let outer = egui::Rect::from_center_size(window.center(), view.window_size());
        let response =
            CalibrationDialog::new(&view).show(ui, outer, palette, &mut self.rt.app.assets, "main");
        for action in &response.actions {
            self.rt.app.handle_calibration(*action);
        }
        self.calibration = self.rt.app.calibration_view();
    }

    /// Draw the Import Presets pane, if it is open — the chooser, then the summary
    /// (`docs/spec/06-dialogs.md` §2).
    fn show_import(&mut self, ui: &mut egui::Ui, palette: Palette) {
        self.poll_folder_picker();
        let window = self.window_rect();
        let Some(mut state) = self.import.take() else {
            return;
        };
        dim_backdrop(ui, window, "import");
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
        dim_backdrop(ui, window, "export");
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
        // The answer wakes the window, or the headless pump if the window was hidden meanwhile.
        let waker = self.rt.waker.clone();
        let spawned = std::thread::Builder::new()
            .name("fxsound-folder-picker".into())
            .spawn(move || {
                let mut dialog =
                    rfd::FileDialog::new().set_title(dialogs::presets::SELECT_FOLDER_LABEL);
                if let Some(start) = start {
                    dialog = dialog.set_directory(start);
                }
                // The receiver may be gone if the pane was closed meanwhile; nothing to do then.
                if tx.send(dialog.pick_folder()).is_ok() {
                    waker.wake();
                }
            });
        match spawned {
            Ok(_) => self.folder_picker = Some(rx),
            Err(err) => {
                log::warn!("could not start the folder picker: {err}");
                self.rt
                    .app
                    .raise_notice(tr("Could not open the folder picker"));
            }
        }
    }

    /// Take the picker's answer, if it has one.
    fn poll_folder_picker(&mut self) {
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
            // Still up. Its answer wakes the window when it comes (`start_folder_picker`).
            Err(crossbeam_channel::TryRecvError::Empty) => {}
        }
    }
}

impl eframe::App for Shell<'_> {
    /// Transparent, so the window's own rounded corners are what the compositor composites.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // A start in the tray that found no tray icon: minimised where the compositor has such a
        // thing, or into the tray after all should an icon have come up since.
        if std::mem::take(&mut self.rt.start_minimised) {
            self.minimise(ctx);
        }
        self.follow_minimised(ctx);
        let request = self.rt.tick();
        if std::mem::take(&mut self.rt.settings_requested) {
            self.open_settings();
        }
        self.apply_window_request(ctx, request);

        // The keepalive: what the producers bring wakes the window on its own (see the module
        // docs), and this is for what nothing announces — a notice due to go, sound starting on a
        // device that was running already. Once a second, sooner when something is due.
        ctx.request_repaint_after(self.rt.pump_interval(Instant::now()));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // The wizard as the tick left it, before anything sizes the window for it.
        self.calibration = self.rt.app.calibration_view();
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
                // The original's close button hides to the tray rather than quitting
                // (`FxMainWindow.cpp:608-616`); Exit in the tray menu is the only way out. Its
                // minimise button minimises (`:579-585`); here it hides to the tray while there is
                // one and minimises while there is not (0.4.0 audit #43, see `minimise_action`).
                UiAction::Close => Self::hide(&ctx),
                UiAction::Minimise => self.minimise(&ctx),
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
        self.show_calibration(ui, palette);
        self.show_import(ui, palette);
        self.show_export(ui, palette);
        self.show_menu(&ctx, palette);
        self.show_confirmation(&ctx, palette);

        // Sixty frames a second only while they show something moving (see `frame_interval`).
        let (minimised, focused) = ctx.input(|i| (i.viewport().minimized, i.viewport().focused));
        let pacing = Pacing {
            pro: self.rt.app.state.view == ViewMode::Pro,
            visualizer_settled: self.scratch.visualizer.is_settled(),
            meters_moved: self.rt.app.meters_moved(),
            calibrating: self.rt.app.calibration_is_live(),
            out_of_sight: minimised == Some(true) && focused != Some(true),
        };
        if let Some(interval) = frame_interval(&pacing) {
            ctx.request_repaint_after(interval);
        }
    }
}

/// The window gives what it showed back to the runtime for the next window ([`Panes`]).
impl Drop for Shell<'_> {
    fn drop(&mut self) {
        // A gesture ends with its window, and a solo is one: the window hidden with the button
        // still down on a band leaves nobody to let go of it.
        self.rt.app.end_solo();
        self.rt.panes = Panes {
            scratch: std::mem::take(&mut self.scratch),
            settings: self.settings.take(),
            import: self.import.take(),
            export: self.export.take(),
            folder_picker: self.folder_picker.take(),
            changelog: self.changelog,
        };
    }
}

/// What the title bar's minimise button does ([`minimise_action`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MinimiseAction {
    /// Destroy the window and go on in the tray, as ✕ does.
    HideToTray,
    /// Ask the compositor to minimise the window (`xdg_toplevel.set_minimized`), as the original's
    /// `ShowWindow(SW_MINIMIZE)` does (`FxMainWindow.cpp:579-585`).
    Minimise,
}

/// The minimise button's action (0.4.0 audit #43): into the tray while a tray icon is on screen,
/// since Hyprland, sway and the other tiling compositors ignore a minimise request and would leave
/// the window where it is; a real minimise while none is, where hiding would leave nothing on
/// screen to come back through — the port hid the window on GNOME without AppIndicator, and the
/// user saw FxSound vanish with only `fxsound --show` to bring it back.
const fn minimise_action(tray_visible: bool) -> MinimiseAction {
    if tray_visible {
        MinimiseAction::HideToTray
    } else {
        MinimiseAction::Minimise
    }
}

/// What decides the window's frame rate beyond the keepalive, as one frame left it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pacing {
    /// The Pro view is showing: the only one with a visualizer.
    pro: bool,
    /// The visualizer has come to rest ([`fxsound_ui::VisualizerAnimation::is_settled`]).
    visualizer_settled: bool,
    /// The shown lane's meters changed since the poll before ([`App::meters_moved`]).
    meters_moved: bool,
    /// The calibration wizard is counting down, measuring or analysing.
    calibrating: bool,
    /// Minimised and not focused, where the compositor can say so (X11; Wayland never tells).
    out_of_sight: bool,
}

/// When the window wants its next frame for what it shows moving (0.4.0 design §12), beyond the
/// keepalive ([`KEEPALIVE`]) and the frames the widgets ask for their own animations — the
/// visualizer's 30 Hz ripple while it settles, the logo's fade, a pane's progress bar.
///
/// [`FRAME_INTERVAL`] while the Pro view's visualizer has sound moving through it — not yet at
/// rest, and fed meters that changed since the last frame — and while the calibration wizard runs.
/// Never in the Lite view, which has no visualizer, and never for a window minimised out of
/// sight: 0.3.0 painted sixty frames a second whenever sound played, wherever the window was.
fn frame_interval(pacing: &Pacing) -> Option<Duration> {
    if pacing.calibrating {
        return Some(FRAME_INTERVAL);
    }
    let moving = pacing.pro && !pacing.visualizer_settled && pacing.meters_moved;
    (moving && !pacing.out_of_sight).then_some(FRAME_INTERVAL)
}

/// `size` grown to fit `pane`.
fn grown(size: egui::Vec2, pane: egui::Vec2) -> egui::Vec2 {
    egui::vec2(size.x.max(pane.x), size.y.max(pane.y))
}

/// Dim what is behind a pane, so it reads as modal the way the original's dialogs did — inside
/// the window's own rounded outline, so the transparent corners stay transparent.
///
/// And make it modal: the backdrop takes the pointer over the whole window, so the view under
/// the pane neither answers a click nor puts up a tooltip through it — an equalizer knob's hint
/// used to appear over Settings wherever the pointer rested above one. What the pane draws
/// afterwards sits on top of the backdrop and gets the pointer as before.
fn dim_backdrop(ui: &egui::Ui, window: egui::Rect, pane: &str) {
    // One per pane: the changelog and the wizard open over Settings, each over its own backdrop.
    let id = egui::Id::new(("fxsound.pane.backdrop", pane));
    ui.interact(window, id, egui::Sense::click_and_drag());
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
/// `"Overwrite Existing Preset - <name>"` with a typical name.
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
    /// The lane it was opened for: the editor closes when the window moves to the other one.
    lane: fxsound_core::DeviceDirection,
    /// The preset selected when it was opened. A rename is that preset's: the editor closes when
    /// another is selected, and Enter renames nothing else (0.4.0 review FA).
    preset: Option<String>,
    text: String,
    /// Focus is requested once, on the first frame — not on every paint as the original does
    /// (`docs/spec/03-controls.md` §11.4).
    focused: bool,
}

impl NameEditor {
    fn new(
        purpose: EditorPurpose,
        lane: fxsound_core::DeviceDirection,
        preset: Option<&str>,
    ) -> Self {
        Self {
            purpose,
            lane,
            preset: preset.map(str::to_owned),
            text: String::new(),
            focused: false,
        }
    }

    /// Whether the editor still has something to do with the window editing `lane` with the
    /// preset `selected`, and its item's enablement. One whose item has since gone grey has
    /// nothing left to do; nor has one opened for the other lane, which the tray, a keybind or
    /// D-Bus has moved the window off since: the name typed was for that lane's controls and
    /// list, and committed now it would save or rename the lane the menu shows instead. Nor has
    /// a Rename opened for a preset that is no longer the one selected — a headphone plugged in
    /// brings back its own, a keybind steps to the next — which the new name was never for.
    fn still_applies(
        &self,
        lane: fxsound_core::DeviceDirection,
        selected: Option<&str>,
        can_save_new: bool,
        can_rename: bool,
    ) -> bool {
        self.lane == lane
            && match self.purpose {
                EditorPurpose::SaveNew => can_save_new,
                EditorPurpose::Rename => can_rename && self.preset.as_deref() == selected,
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
        egui::Stroke::new(1.0, palette.divider()),
    );
}

/// The inline preset-name editor (`FxPresetMenuItem`, `docs/spec/03-controls.md` §11).
///
/// A 200 × 30 field with a 2 px outline: `ValidTextBorder` while the typed name is unique and
/// non-empty, `InvalidTextBorder` otherwise — so it starts red and turns blue once a usable name
/// is in it (§11.3, §11.4). Input is filtered through [`FORBIDDEN_PRESET_NAME_CHARS`] and capped
/// at [`MAX_PRESET_NAME_CHARS`] and at the bytes a Windows FxSound reads a name in
/// ([`fxsound_preset::MAX_NAME_BYTES`], 0.4.0 audit #15). Renaming, the preset's own name is not
/// taken, so its letter case can change (audit #19). Returns the name on Enter when it is valid
/// (§11.5); Escape is the menu's business.
fn name_editor(
    ui: &mut egui::Ui,
    editor: &mut NameEditor,
    presets: &[PresetEntry],
    renaming: Option<&str>,
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
    cap_name_bytes(&mut editor.text);
    let valid = match (editor.purpose, renaming) {
        (EditorPurpose::Rename, Some(old)) => rename_name_available(presets, old, &editor.text),
        _ => preset_name_available(presets, &editor.text),
    };

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

/// Cut a name being typed to [`fxsound_preset::MAX_NAME_BYTES`] on a character boundary, as the
/// sixty-four characters are cut by the field itself: sixty-four Cyrillic letters are two bytes
/// more than a Windows FxSound reads (0.4.0 audit #15).
fn cap_name_bytes(text: &mut String) {
    if text.len() > fxsound_preset::MAX_NAME_BYTES {
        let mut end = fxsound_preset::MAX_NAME_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}

/// The runtime's glue, over a real control socket in a scratch directory and a headless
/// controller: never the session's socket, bus or PipeWire.
#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crossbeam_channel::{Receiver, Sender};
    use std::io::Write;
    use std::path::Path;
    use std::time::Instant;

    /// A runtime with no engine, no tray and no bus, serving the socket in `dir`.
    fn runtime(dir: &Path) -> Runtime {
        let Instance::Primary(listener) = Instance::acquire_in(dir).expect("acquire") else {
            panic!("expected to be primary");
        };
        let (_tray_tx, tray_rx) = crossbeam_channel::unbounded();
        Runtime {
            app: App::headless_for_tests(),
            dbus: None,
            server: listener.serve().expect("serve"),
            sleep: None,
            sleep_rx: crossbeam_channel::never(),
            tray: None,
            tray_rx,
            exit: WindowExit::Hidden,
            settings_requested: false,
            terminate: Arc::new(AtomicBool::new(false)),
            terminating: false,
            waker: Waker::new(),
            signals: None,
            panes: Panes::default(),
            waiting_for_devices: Vec::new(),
            tray_wait: None,
            start_minimised: false,
            reopening: false,
        }
    }

    /// Hands each whole line `ipc::watch_to` writes to a channel.
    struct Lines {
        partial: Vec<u8>,
        tx: Sender<String>,
    }

    impl Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.partial.extend_from_slice(bytes);
            while let Some(end) = self.partial.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.partial.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line).trim_end().to_owned();
                let _ = self.tx.send(line);
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// `fxsound --watch` with `args`, on a thread: its lines, and its exit code once the stream
    /// ends.
    fn watch(socket: &Path, args: &[&str]) -> (Receiver<String>, std::thread::JoinHandle<i32>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let socket = socket.to_path_buf();
        let argv: Vec<String> = std::iter::once("fxsound")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect();
        let meters = args.contains(&"--meters");
        let thread = std::thread::spawn(move || {
            let mut out = Lines {
                partial: Vec::new(),
                tx,
            };
            ipc::watch_to(
                &socket,
                &argv,
                Path::new("/"),
                meters,
                &mut out,
                &mut std::io::sink(),
            )
        });
        (rx, thread)
    }

    /// Tick until `lines` has one, as the window's loop would.
    fn next_line(runtime: &mut Runtime, lines: &Receiver<String>) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            runtime.tick();
            if let Ok(line) = lines.recv_timeout(Duration::from_millis(10)) {
                return line;
            }
            assert!(Instant::now() < deadline, "nothing arrived on the stream");
        }
    }

    /// Tick `ticks` times, and say what arrived meanwhile.
    fn quiet_ticks(runtime: &mut Runtime, lines: &Receiver<String>, ticks: usize) -> Vec<String> {
        for _ in 0..ticks {
            runtime.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        lines.try_iter().collect()
    }

    #[test]
    fn a_tick_hands_each_change_to_a_watcher_once_and_a_quiet_tick_hands_it_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let (lines, watcher) = watch(runtime.server.path(), &["--watch"]);
        assert!(next_line(&mut runtime, &lines).starts_with("status "));

        runtime.app.handle(&[UiAction::TogglePower]);
        assert_eq!(next_line(&mut runtime, &lines), "power on=false");
        assert_eq!(
            quiet_ticks(&mut runtime, &lines, 5),
            Vec::<String>::new(),
            "nothing changed, so nothing is said"
        );

        runtime.shutdown();
        assert_eq!(lines.recv().expect("the last line"), "quit");
        assert_eq!(watcher.join().expect("watcher"), 0, "the stream ended");
    }

    #[test]
    fn a_command_line_forwarded_to_the_instance_shows_up_on_the_stream() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let (lines, _watcher) = watch(runtime.server.path(), &["--watch", "--json"]);
        next_line(&mut runtime, &lines);

        let socket = runtime.server.path().to_path_buf();
        let forwarded = std::thread::spawn(move || {
            ipc::forward_to(
                &socket,
                &["fxsound".to_owned(), "--power=off".to_owned()],
                Path::new("/"),
                Duration::from_secs(5),
            )
        });
        let line = next_line(&mut runtime, &lines);
        let event: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(event["event"], "power", "{line}");
        assert_eq!(event["on"], false, "{line}");
        assert!(forwarded.join().expect("client").expect("answered").ok);
    }

    #[test]
    fn meters_reach_only_the_watcher_that_asked_for_them() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let (plain, _plain_watcher) = watch(runtime.server.path(), &["--watch"]);
        next_line(&mut runtime, &plain);
        assert!(!runtime.server.wants_meters());
        assert!(
            quiet_ticks(&mut runtime, &plain, 3).is_empty(),
            "no meters are gathered for a stream that did not ask"
        );

        let (metered, _metered_watcher) = watch(runtime.server.path(), &["--watch", "--meters"]);
        assert!(next_line(&mut runtime, &metered).starts_with("status "));
        assert!(runtime.server.wants_meters());
        assert!(next_line(&mut runtime, &metered).starts_with("input_meters "));
        assert!(
            quiet_ticks(&mut runtime, &plain, 3).is_empty(),
            "the plain stream still hears none of them"
        );
    }

    #[test]
    fn the_window_coming_and_going_reaches_the_watchers_through_the_same_consumers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let (lines, _watcher) = watch(runtime.server.path(), &["--watch"]);
        next_line(&mut runtime, &lines);

        runtime.announce(&AppEvent::Window { visible: false });
        assert_eq!(next_line(&mut runtime, &lines), "window visible=false");
    }

    /// A runtime whose window is on screen, as [`Runtime::run_window`] leaves it while
    /// `run_native` runs: `exit` back at `Hidden`, and `run_minimized` written `false` as
    /// `showMainWindow` writes it.
    fn with_a_window_up(runtime: &mut Runtime) {
        runtime.exit = WindowExit::Hidden;
        runtime.app.remember_window_hidden(false);
    }

    #[test]
    fn a_window_the_compositor_closed_with_a_tray_on_screen_starts_in_the_tray_next_time() {
        // 0.4.0 audit #35, the path the shell never sees: `killactive` (Super+Q) or Alt+F4 closes
        // the window without a frame of the shell's, so nothing but the start of the run has set
        // `exit` when `run_native` returns.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let (lines, _watcher) = watch(runtime.server.path(), &["--watch"]);
        next_line(&mut runtime, &lines);
        with_a_window_up(&mut runtime);

        assert_eq!(runtime.window_closed(true), WindowExit::Hidden);
        assert!(
            runtime.app.settings_run_minimized(),
            "quit from the tray now, FxSound starts in the tray"
        );
        assert!(
            runtime.app.tray_tip_shown(),
            "and the one-time tip was shown"
        );
        assert_eq!(next_line(&mut runtime, &lines), "window visible=false");
    }

    #[test]
    fn a_window_reopened_for_show_is_no_hide_and_its_successor_is_no_news() {
        // FA: on Wayland `--show` replaces a minimised window with a fresh one. That is not a
        // hide: no tip, no `run_minimized`, no `window visible=false` between the two.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        with_a_window_up(&mut runtime);
        runtime.exit = WindowExit::Reopen;

        assert_eq!(runtime.window_closed(true), WindowExit::Reopen);
        assert!(!runtime.app.settings_run_minimized());
        assert!(!runtime.app.tray_tip_shown());
        assert!(runtime.reopening, "the next window is not announced again");
    }

    #[test]
    fn a_start_in_the_tray_waits_for_a_tray_icon_unless_hide_or_activated_asked_for_it() {
        use WindowVisibility::{Hidden, Shown};
        // A line that only sets something, and the remembered tray state of a bare `fxsound`
        // from a launcher: both hidden with no `--hide`, and both would otherwise run with
        // nothing on screen on a desktop with no tray icon.
        let quiet = cold_start_visibility(false, false, false, true);
        let remembered = cold_start_visibility(false, false, true, false);
        assert_eq!((quiet, remembered), (Hidden, Hidden));
        assert!(waits_for_a_tray(quiet, false));
        assert!(waits_for_a_tray(remembered, false));
        // The autostart entry's `--hide` and the bus's `--activated` asked for no window.
        let asked = cold_start_visibility(true, false, false, false);
        assert!(!waits_for_a_tray(asked, true));
        assert!(!waits_for_a_tray(Shown, false));
    }

    #[test]
    fn a_start_in_the_tray_with_no_tray_icon_comes_up_minimised_once_its_wait_is_over() {
        // FA: with no tray icon a start from a line that only sets something would run with
        // nothing on screen to reach it by.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        runtime.tray_wait = Some(Instant::now() + Duration::from_millis(100));
        let started = Instant::now();
        assert_eq!(runtime.run_headless(), HeadlessExit::ShowMinimised);
        assert!(started.elapsed() >= Duration::from_millis(90), "it waited");
        assert!(runtime.tray_wait.is_none());
    }

    #[test]
    fn a_window_the_compositor_closed_with_no_tray_icon_is_not_remembered_as_hidden() {
        // GNOME without an AppIndicator extension: a start "in the tray" would be a start with
        // nothing on screen at all. The notice that names `fxsound --show` still goes out.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        assert!(
            !runtime.tray_visible(),
            "a runtime with no tray has no icon on screen"
        );
        with_a_window_up(&mut runtime);

        assert_eq!(runtime.window_closed(false), WindowExit::Hidden);
        assert!(!runtime.app.settings_run_minimized());
        assert!(runtime.app.tray_tip_shown(), "the no-tray notice was shown");
    }

    #[test]
    fn a_window_that_cannot_be_opened_leaves_fxsound_in_the_tray_rather_than_quitting() {
        // An instance the session bus started through the systemd user unit, with no
        // WAYLAND_DISPLAY in its environment: quitting would drop the sound, and the unit's
        // `Restart=on-failure` would start it hidden again, the same way every time the window
        // is asked for.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let (lines, _watcher) = watch(runtime.server.path(), &["--watch"]);
        next_line(&mut runtime, &lines);
        with_a_window_up(&mut runtime);
        runtime.settings_requested = true;
        runtime.start_minimised = true;

        let why = "neither WAYLAND_DISPLAY nor WAYLAND_SOCKET nor DISPLAY is set.";
        assert!(runtime.window_failed(&why, true), "it stays in the tray");
        assert!(
            !runtime.app.settings_run_minimized(),
            "the fresh start the notification asks for opens the window"
        );
        assert!(
            !runtime.settings_requested,
            "nothing asks for the window again on its own"
        );
        assert!(!runtime.start_minimised);
        assert_eq!(next_line(&mut runtime, &lines), "window visible=false");
        let notice = next_line(&mut runtime, &lines);
        assert!(
            notice.starts_with("notice message="),
            "and a status bar hears why: {notice}"
        );

        // The tray-only state it goes back to waits for someone to ask, rather than asking
        // for the window again at once.
        runtime.terminate.store(true, Ordering::Relaxed);
        assert_eq!(runtime.run_headless(), HeadlessExit::Quit);
        runtime.shutdown();
    }

    #[test]
    fn a_window_that_cannot_be_opened_with_no_tray_icon_ends_fxsound_as_before() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        with_a_window_up(&mut runtime);
        assert!(!runtime.window_failed(&"no display", false));
        assert!(
            !runtime.app.settings_run_minimized(),
            "nothing to start in next time"
        );
        runtime.shutdown();
    }

    #[test]
    fn a_window_closed_for_a_quit_leaves_the_start_up_preference_as_the_window_found_it() {
        // The window was showing when FxSound was told to quit, so the next start shows it; and a
        // quit is no hide, so no tray tip and no `window visible=false` either.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let (lines, _watcher) = watch(runtime.server.path(), &["--watch"]);
        next_line(&mut runtime, &lines);
        with_a_window_up(&mut runtime);
        runtime.exit = WindowExit::Quit;

        assert_eq!(runtime.window_closed(true), WindowExit::Quit);
        assert!(!runtime.app.settings_run_minimized());
        assert!(!runtime.app.tray_tip_shown());
        assert_eq!(quiet_ticks(&mut runtime, &lines, 5), Vec::<String>::new());
    }

    #[test]
    fn every_way_the_shell_hides_leaves_the_bookkeeping_to_the_close_that_follows() {
        // ✕, minimise into the tray, `--hide`, `--toggle-window` and the tray's left click only
        // close the window; what a hide leaves behind is `window_closed`'s, done once for them
        // and for the compositor's close request alike.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        with_a_window_up(&mut runtime);
        {
            let mut shell = Shell::new(&mut runtime, ThemeMode::Dark);
            let ctx = egui::Context::default();
            shell.apply_window_request(
                &ctx,
                WindowRequest {
                    hide: true,
                    ..WindowRequest::default()
                },
            );
            assert_eq!(shell.rt.exit, WindowExit::Hidden, "a hide, not a quit");
        }
        assert_eq!(runtime.window_closed(true), WindowExit::Hidden);
        assert!(runtime.app.settings_run_minimized());
    }

    #[test]
    fn what_the_last_tick_left_unsaid_goes_out_before_the_quit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let (lines, watcher) = watch(runtime.server.path(), &["--watch"]);
        next_line(&mut runtime, &lines);

        // Changed after the last tick, the way a tray Exit follows a tray pick in one tick.
        runtime.app.handle(&[UiAction::TogglePower]);
        runtime.shutdown();
        assert_eq!(watcher.join().expect("watcher"), 0);
        assert_eq!(
            lines.try_iter().collect::<Vec<_>>(),
            ["power on=false", "quit"]
        );
    }

    #[test]
    fn the_pump_looks_once_a_second_and_sooner_for_a_meters_stream_or_a_notice_due() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let now = Instant::now();
        assert_eq!(runtime.pump_interval(now), KEEPALIVE);

        runtime.app.raise_notice("Saved");
        let notice = runtime.pump_interval(Instant::now());
        assert_eq!(
            notice, KEEPALIVE,
            "four seconds away: the keepalive comes first"
        );
        runtime.app.state.notice_clock = runtime
            .app
            .state
            .notice_clock
            .take()
            .map(|(text, since)| (text, since - Duration::from_millis(3_700)));
        let due = runtime.pump_interval(Instant::now());
        assert!(
            due <= Duration::from_millis(300),
            "the notice goes when it is due, not at the next keepalive: {due:?}"
        );
        runtime.app.handle(&[UiAction::DismissNotice]);

        let (metered, _watcher) = watch(runtime.server.path(), &["--watch", "--meters"]);
        next_line(&mut runtime, &metered);
        assert_eq!(runtime.pump_interval(Instant::now()), ipc::METER_INTERVAL);
    }

    #[test]
    fn with_no_window_the_pump_sleeps_until_a_tray_click_wakes_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let (tray_tx, tray_rx) = crossbeam_channel::unbounded();
        runtime.tray_rx = tray_rx;
        let clicked = Instant::now();
        let tray = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            tray_tx
                .send(TrayCommand::Open)
                .expect("the pump is listening");
            tray_tx
        });
        runtime.wait(Duration::from_secs(10));
        assert!(
            clicked.elapsed() < Duration::from_secs(5),
            "the click, not the timeout, ended the wait"
        );
        let _tray_tx = tray.join().expect("tray");
        assert!(
            runtime.tick().show,
            "and the tick that follows carries it out"
        );
    }

    #[test]
    fn a_forwarded_command_line_wakes_the_sleeping_pump_and_is_answered() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let socket = runtime.server.path().to_path_buf();
        let asked = Instant::now();
        let forwarded = std::thread::spawn(move || {
            ipc::forward_to(
                &socket,
                &["fxsound".to_owned(), "--power=off".to_owned()],
                Path::new("/"),
                Duration::from_secs(5),
            )
        });
        runtime.wait(Duration::from_secs(10));
        assert!(asked.elapsed() < Duration::from_secs(5));
        runtime.tick();
        assert!(forwarded.join().expect("client").expect("answered").ok);
        assert!(!runtime.app.state.power);
    }

    #[test]
    fn with_nothing_arriving_the_pump_sleeps_out_its_interval_even_beside_a_closed_channel() {
        let dir = tempfile::tempdir().expect("temp dir");
        let runtime = runtime(dir.path());
        // `runtime()` dropped the tray's sender, as a tray that could not register does: its
        // channel is always ready, and must not be taken for an arrival.
        let started = Instant::now();
        runtime.wait(Duration::from_millis(300));
        assert!(started.elapsed() >= Duration::from_millis(290));
    }

    #[test]
    fn a_wake_up_with_no_channel_of_its_own_ends_the_wait() {
        let dir = tempfile::tempdir().expect("temp dir");
        let runtime = runtime(dir.path());
        let waker = runtime.waker.clone();
        let woken = Instant::now();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            waker.wake();
        });
        runtime.wait(Duration::from_secs(10));
        assert!(woken.elapsed() < Duration::from_secs(5));
    }

    /// Set, to a marker file's path, in the child copy of this binary that raises the signal.
    const SIGNAL_CHILD: &str = "FXSOUND_TEST_SIGNAL_CHILD";

    /// The signal is raised in a child copy of this test binary, never in this one: signal-hook
    /// keeps SIGTERM, SIGINT and SIGHUP once it has taken them (unregistering does not put the
    /// default action back), and a test process that shrugs off Ctrl+C is not one to leave behind.
    #[test]
    fn a_termination_signal_wakes_the_pump() {
        if let Some(marker) = std::env::var_os(SIGNAL_CHILD) {
            let waker = Waker::new();
            let signals = wake_on_signals(&waker).expect("the signal thread");
            // SIGHUP, the one of the three a test runner is not stopped by: the handler
            // registered above takes it.
            signal_hook::low_level::raise(signal_hook::consts::SIGHUP).expect("raised");
            let deadline = Instant::now() + Duration::from_secs(5);
            while waker.pending().is_empty() {
                assert!(Instant::now() < deadline, "the signal woke nobody");
                std::thread::sleep(Duration::from_millis(5));
            }
            signals.close();
            std::fs::write(marker, "woken").expect("the marker");
            return;
        }
        let dir = tempfile::tempdir().expect("temp dir");
        let marker = dir.path().join("woken");
        let (_, module) = module_path!()
            .split_once("::")
            .expect("a module of the crate");
        let name = format!("{module}::a_termination_signal_wakes_the_pump");
        let output = std::process::Command::new(std::env::current_exe().expect("this test binary"))
            .args([name.as_str(), "--exact", "--test-threads=1", "--nocapture"])
            .env(SIGNAL_CHILD, &marker)
            .output()
            .expect("the child ran");
        assert!(
            output.status.success(),
            "the child failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        // An `--exact` name that matched nothing passes too; the marker says the body ran.
        assert!(marker.exists(), "the child ran no test called {name}");
    }

    #[test]
    fn a_hidden_window_s_pane_and_column_face_are_there_when_it_is_shown_again() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        {
            let mut shell = Shell::new(&mut runtime, ThemeMode::Dark);
            shell.open_settings();
            shell.open_import();
            shell.scratch.column_face = shell.scratch.column_face.flipped();
            shell.scratch.window_drag = true;
            shell.menu.toggle();
        }
        let flipped = fxsound_ui::views::ColumnFace::default().flipped();
        let shell = Shell::new(&mut runtime, ThemeMode::Dark);
        assert!(shell.settings.is_some(), "Settings is still open");
        assert!(shell.import.is_some(), "and so is Import Presets");
        assert_eq!(shell.scratch.column_face, flipped);
        assert!(!shell.scratch.window_drag, "a gesture ends with its window");
        assert!(!shell.menu.open, "and so does the menu");
    }

    #[test]
    fn the_tray_s_settings_item_while_hidden_opens_the_pane_in_the_next_window() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        runtime.settings_requested = true;
        let shell = Shell::new(&mut runtime, ThemeMode::Dark);
        assert!(shell.settings.is_some());
        drop(shell);
        assert!(!runtime.settings_requested);
    }

    #[test]
    fn what_the_suspend_watcher_heard_reaches_the_controller_on_the_next_tick() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut runtime = runtime(dir.path());
        let (tx, rx) = crossbeam_channel::unbounded();
        runtime.sleep_rx = rx;
        tx.send(true).expect("the pump is listening");
        assert!(!runtime.app.is_system_sleeping(), "not before the tick");
        runtime.tick();
        assert!(runtime.app.is_system_sleeping());
        // Down and up again within one tick: the resume is the last word.
        tx.send(false).expect("the pump is listening");
        tx.send(true).expect("the pump is listening");
        tx.send(false).expect("the pump is listening");
        runtime.tick();
        assert!(!runtime.app.is_system_sleeping());
        runtime.shutdown();
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
mod calibration_tests {
    use super::*;
    use fxsound_app::audio_link::FakeEngine;
    use fxsound_core::messages::AudioToUi;
    use fxsound_core::{AudioDevice, AudioStatus, DeviceDirection, Settings};
    use fxsound_preset::{InputPresetStore, PresetStore};
    use fxsound_ui::dialogs::calibration::wizard;
    use fxsound_ui::dialogs::{CalibrationAction, CalibrationPhase};

    const MIC: &str = "alsa_input.usb-fifine";
    const MIC_NAME: &str = "fifine Microphone Analogue Stereo";

    /// A runtime serving the socket in `dir`, around a controller started against a stand-in
    /// engine whose input lane is attached to the fifine when `with_microphone`.
    fn runtime(dir: &std::path::Path, with_microphone: bool) -> Runtime {
        let Instance::Primary(listener) = Instance::acquire_in(dir).expect("acquire") else {
            panic!("expected to be primary");
        };
        let engine = FakeEngine::new();
        let mut settings = Settings::default();
        settings.set_lane_enabled(DeviceDirection::Input, true);
        let presets = PresetStore::with_dirs(Vec::new(), dir.join("presets"));
        let voices = InputPresetStore::with_dirs(Vec::new(), dir.join("presets").join("Input"));
        let mut app = App::start_for_tests(settings, presets, voices, &engine);
        engine.feed(AudioToUi::Devices(vec![AudioDevice {
            id: 7,
            name: MIC.to_owned(),
            description: MIC_NAME.to_owned(),
            is_default: true,
            direction: DeviceDirection::Input,
            form_factor: "microphone".to_owned(),
        }]));
        if with_microphone {
            engine.feed(AudioToUi::Attached {
                direction: DeviceDirection::Input,
                node_name: Some(MIC.to_owned()),
            });
            engine.feed(AudioToUi::Status {
                direction: DeviceDirection::Input,
                status: AudioStatus {
                    processing: true,
                    ..AudioStatus::default()
                },
            });
        }
        app.poll_audio();
        let (_tray_tx, tray_rx) = crossbeam_channel::unbounded();
        Runtime {
            app,
            dbus: None,
            server: listener.serve().expect("serve"),
            sleep: None,
            sleep_rx: crossbeam_channel::never(),
            tray: None,
            tray_rx,
            exit: WindowExit::Hidden,
            settings_requested: false,
            terminate: Arc::new(AtomicBool::new(false)),
            terminating: false,
            waker: Waker::new(),
            signals: None,
            panes: Panes::default(),
            waiting_for_devices: Vec::new(),
            tray_wait: None,
            start_minimised: false,
            reopening: false,
        }
    }

    #[test]
    fn calibrate_microphone_opens_the_controllers_wizard_over_the_pane_and_grows_the_window() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut rt = runtime(dir.path(), true);
        let mut shell = Shell::new(&mut rt, ThemeMode::Dark);
        shell.open_settings();
        shell.open_calibration();

        let view = shell.calibration.clone().expect("the wizard is open");
        assert_eq!(view.phase, CalibrationPhase::Intro);
        assert_eq!(view.device, MIC_NAME);
        assert!(
            view.can_start(),
            "the controller can drive the measurements, so Start is live"
        );
        assert!(shell.pane_open(), "the menu stays shut under the wizard");
        let size = shell.window_size();
        assert!(size.x >= wizard::WINDOW_SIZE.x && size.y >= wizard::WINDOW_SIZE.y);
        assert!(shell.rt.app.calibration_open());
    }

    #[test]
    fn without_a_microphone_on_the_input_lane_the_wizard_does_not_open() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut rt = runtime(dir.path(), false);
        let mut shell = Shell::new(&mut rt, ThemeMode::Dark);
        shell.open_settings();
        shell.open_calibration();
        assert!(shell.calibration.is_none());
        assert!(!shell.rt.app.calibration_open());
    }

    #[test]
    fn cancel_in_the_wizard_closes_it_in_the_controller_too() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut rt = runtime(dir.path(), true);
        let mut shell = Shell::new(&mut rt, ThemeMode::Dark);
        shell.open_calibration();
        shell.rt.app.handle_calibration(CalibrationAction::Start);
        assert!(shell.rt.app.calibration_is_live(), "Start began a run");
        shell.rt.app.handle_calibration(CalibrationAction::Cancel);
        assert!(!shell.rt.app.calibration_open());
        assert_eq!(shell.rt.app.calibration_view(), None);
    }
}

#[cfg(test)]
mod backdrop_tests {
    use super::*;
    use egui::{Event, PointerButton, Pos2, RawInput, Rect, pos2, vec2};

    const VIEW_BUTTON: Rect = Rect::from_min_max(pos2(40.0, 40.0), pos2(140.0, 80.0));
    const PANE_BUTTON: Rect = Rect::from_min_max(pos2(200.0, 40.0), pos2(300.0, 80.0));

    /// A view with a button, and when `pane` a pane's backdrop over it with a button of its own:
    /// move to `at`, press and release. Whether the view's button was ever hovered or clicked,
    /// and whether the pane's was clicked.
    fn press(at: Pos2, pane: bool) -> (bool, bool, bool) {
        let ctx = egui::Context::default();
        let (mut view_hovered, mut view_clicked, mut pane_clicked) = (false, false, false);
        let button = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        for events in [
            vec![Event::PointerMoved(at)],
            vec![Event::PointerMoved(at)],
            vec![Event::PointerMoved(at), button(true)],
            vec![button(false)],
        ] {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 200.0))),
                events,
                ..RawInput::default()
            };
            ctx.run_ui(input, |ui| {
                let view = ui.put(VIEW_BUTTON, egui::Button::new("knob"));
                view_hovered |= view.hovered();
                view_clicked |= view.clicked();
                if pane {
                    dim_backdrop(ui, ui.max_rect(), "test");
                    pane_clicked |= ui.put(PANE_BUTTON, egui::Button::new("close")).clicked();
                }
            })
            .drop_without_applying_deltas();
        }
        (view_hovered, view_clicked, pane_clicked)
    }

    #[test]
    fn without_a_pane_the_view_answers_the_pointer() {
        assert_eq!(press(VIEW_BUTTON.center(), false), (true, true, false));
    }

    #[test]
    fn a_pane_s_backdrop_keeps_the_pointer_from_the_view_under_it() {
        assert_eq!(
            press(VIEW_BUTTON.center(), true),
            (false, false, false),
            "no tooltip and no click through the pane"
        );
    }

    #[test]
    fn the_pane_s_own_controls_still_answer_the_pointer() {
        assert_eq!(press(PANE_BUTTON.center(), true), (false, false, true));
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::*;

    const PLAYING: Pacing = Pacing {
        pro: true,
        visualizer_settled: false,
        meters_moved: true,
        calibrating: false,
        out_of_sight: false,
    };

    #[test]
    fn the_pro_view_paints_sixty_a_second_while_sound_moves_through_its_visualizer() {
        assert_eq!(frame_interval(&PLAYING), Some(FRAME_INTERVAL));
        assert_eq!(FRAME_INTERVAL, Duration::from_millis(16));
    }

    #[test]
    fn a_settled_visualizer_or_meters_that_stand_still_ask_for_no_frames() {
        for pacing in [
            Pacing {
                visualizer_settled: true,
                ..PLAYING
            },
            Pacing {
                meters_moved: false,
                ..PLAYING
            },
        ] {
            assert_eq!(frame_interval(&pacing), None, "{pacing:?}");
        }
    }

    #[test]
    fn the_lite_view_and_a_window_minimised_out_of_sight_never_paint_for_the_meters() {
        assert_eq!(
            frame_interval(&Pacing {
                pro: false,
                ..PLAYING
            }),
            None
        );
        assert_eq!(
            frame_interval(&Pacing {
                out_of_sight: true,
                ..PLAYING
            }),
            None
        );
    }

    #[test]
    fn the_calibration_wizard_is_painted_while_it_runs_whatever_the_view() {
        let calibrating = Pacing {
            pro: false,
            visualizer_settled: true,
            meters_moved: false,
            calibrating: true,
            out_of_sight: false,
        };
        assert_eq!(frame_interval(&calibrating), Some(FRAME_INTERVAL));
    }
}

#[cfg(test)]
mod minimise_tests {
    use super::*;

    #[test]
    fn minimise_hides_to_the_tray_only_while_a_tray_icon_is_there() {
        // 0.4.0 audit #43: with no tray the window used to vanish, and only `fxsound --show` could
        // bring it back; the original minimises (`FxMainWindow.cpp:579-585`).
        assert_eq!(minimise_action(true), MinimiseAction::HideToTray);
        assert_eq!(minimise_action(false), MinimiseAction::Minimise);
    }

    #[test]
    fn show_brings_a_minimised_window_back_on_x11_and_maps_a_fresh_one_on_wayland() {
        // FA: `--show` sent a Focus alone, which winit ignores for an iconic X11 window and on
        // Wayland altogether, so a window minimised with no tray stayed minimised.
        assert_eq!(show_action(false, false), ShowAction::Focus);
        assert_eq!(show_action(false, true), ShowAction::Focus);
        assert_eq!(show_action(true, false), ShowAction::Restore);
        assert_eq!(show_action(true, true), ShowAction::Reopen);
    }

    #[test]
    fn a_session_is_wayland_as_winit_reads_it() {
        let vars = |set: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                set.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| std::ffi::OsString::from(value))
            }
        };
        assert!(wayland_session(vars(&[("WAYLAND_DISPLAY", "wayland-1")])));
        assert!(wayland_session(vars(&[("WAYLAND_SOCKET", "5")])));
        assert!(!wayland_session(vars(&[
            ("WAYLAND_DISPLAY", ""),
            ("DISPLAY", ":0")
        ])));
        assert!(!wayland_session(vars(&[("DISPLAY", ":0")])));
    }

    #[test]
    fn a_cold_start_from_a_line_that_only_sets_something_begins_in_the_tray() {
        // FA: R11 kept a running instance's window down for `fxsound --preset=Gaming`; a cold
        // start from the same keybinding opened and focused the window over the game.
        use WindowVisibility::{Hidden, Shown};
        // (hide, show, run_minimized, only_sets_things)
        assert_eq!(cold_start_visibility(false, false, false, true), Hidden);
        assert_eq!(cold_start_visibility(false, false, false, false), Shown);
        assert_eq!(cold_start_visibility(false, false, true, false), Hidden);
        assert_eq!(cold_start_visibility(false, true, true, false), Shown);
        assert_eq!(cold_start_visibility(true, false, false, false), Hidden);
        let line = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("fxsound").chain(args.iter().copied()))
                .expect("parses")
                .only_sets_things()
        };
        assert_eq!(
            cold_start_visibility(false, false, false, line(&["--preset=Gaming"])),
            Hidden
        );
        assert_eq!(
            cold_start_visibility(false, false, false, line(&["--toggle-power"])),
            Hidden
        );
        assert_eq!(cold_start_visibility(false, false, false, line(&[])), Shown);
        assert_eq!(
            cold_start_visibility(false, false, false, line(&["--view=2"])),
            Shown,
            "--view asks for the window"
        );
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

#[cfg(test)]
mod confirmation_tests {
    use super::*;
    use fxsound_app::audio_link::FakeEngine;
    use fxsound_core::{DeviceDirection, Effect, Preset, Settings};
    use fxsound_preset::{InputPresetStore, PresetStore};

    /// The question `confirm` puts in the message box, laid out with the app's fonts.
    fn asked(confirm: &Confirm) -> String {
        let ctx = egui::Context::default();
        ctx.set_fonts(theme::font_definitions());
        let mut question = String::new();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            question = confirm.question(ui.ctx());
        })
        .drop_without_applying_deltas();
        question
    }

    #[test]
    fn each_confirmation_question_fits_the_message_box_in_every_language() {
        // The questions before Delete Preset and Reset presets (0.4.0 audit #16, #21) on one
        // elided line lost "Saved presets are kept." in every language, and a long preset name
        // took the question mark with it. Measured with the app's own fonts, in the box's own
        // 410 x 52 message rect, in English and all 29 tables.
        use fxsound_core::i18n::{Catalogue, LANGUAGES};
        let ctx = egui::Context::default();
        ctx.set_fonts(theme::font_definitions());
        let names = [
            "Rock".to_owned(),
            "Rock Ballad Extended Night".to_owned(),
            "Rock Ballad Extended Night Mix For The Living Room Speakers 2026".to_owned(),
            // The widest a name can be: sixty-four capital Ws, and the 126 bytes of Cyrillic and
            // of CJK a Windows FxSound reads a name in.
            "W".repeat(64),
            "Ш".repeat(63),
            "音".repeat(42),
        ];
        ctx.run_ui(egui::RawInput::default(), |ui| {
            for language in &LANGUAGES {
                let table = Catalogue::for_language(language);
                let translate = |key: &str| table.get(key).unwrap_or(key).to_owned();
                for confirm in
                    std::iter::once(Confirm::ResetPresets).chain(names.iter().map(|name| {
                        Confirm::Delete {
                            lane: DeviceDirection::Output,
                            name: name.clone(),
                        }
                    }))
                {
                    let key = match confirm {
                        Confirm::Delete { .. } => DELETE_QUESTION,
                        Confirm::ResetPresets => RESET_QUESTION,
                    };
                    let question = confirm.question_from(ui.ctx(), &translate(key));
                    let galley =
                        dialogs::message::message_galley(ui.ctx(), &question, egui::Color32::WHITE);
                    let case = format!("{}: {question}", language.code);
                    assert!(!galley.elided, "{case}");
                    assert!(
                        galley.size().y <= dialogs::message::MESSAGE_HEIGHT + 0.01,
                        "{case}: {:?}",
                        galley.size()
                    );
                    let Confirm::Delete { name, .. } = &confirm else {
                        assert_eq!(question, translate(RESET_QUESTION), "{case}");
                        continue;
                    };
                    let template = translate(DELETE_QUESTION);
                    let (before, after) = template.split_once("%s").expect("a placeholder");
                    assert!(
                        question.starts_with(before) && question.ends_with(after),
                        "{case}: the question is whole"
                    );
                    if name.contains(' ') || name.len() < 8 {
                        assert!(
                            question.contains(name.as_str()),
                            "{case}: the name is whole"
                        );
                    }
                }
            }
        })
        .drop_without_applying_deltas();
    }

    /// A runtime over a controller whose speakers list the user presets `Mine` and `Other`, with
    /// `Mine` selected.
    fn runtime(dir: &std::path::Path) -> Runtime {
        let Instance::Primary(listener) = Instance::acquire_in(dir).expect("acquire") else {
            panic!("expected to be primary");
        };
        let mut presets = PresetStore::with_dirs(Vec::new(), dir.join("presets"));
        for name in ["Mine", "Other"] {
            let preset = Preset {
                name: name.to_owned(),
                ..Preset::default()
            };
            presets.save_as(&preset, name).expect("save");
        }
        let voices = InputPresetStore::with_dirs(Vec::new(), dir.join("presets").join("Input"));
        let mut settings = Settings::default();
        settings.output_preset = "Mine".to_owned();
        let app = App::start_for_tests(settings, presets, voices, &FakeEngine::new());
        let (_tray_tx, tray_rx) = crossbeam_channel::unbounded();
        Runtime {
            app,
            dbus: None,
            server: listener.serve().expect("serve"),
            sleep: None,
            sleep_rx: crossbeam_channel::never(),
            tray: None,
            tray_rx,
            exit: WindowExit::Hidden,
            settings_requested: false,
            terminate: Arc::new(AtomicBool::new(false)),
            terminating: false,
            waker: Waker::new(),
            signals: None,
            panes: Panes::default(),
            waiting_for_devices: Vec::new(),
            tray_wait: None,
            start_minimised: false,
            reopening: false,
        }
    }

    fn listed(shell: &Shell<'_>) -> Vec<String> {
        shell
            .rt
            .app
            .state
            .presets
            .iter()
            .map(|p| p.name.clone())
            .collect()
    }

    #[test]
    fn delete_preset_asks_first_and_deletes_only_on_yes() {
        // 0.4.0 audit #16: one click on the menu used to delete the preset for good.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut rt = runtime(dir.path());
        let mut shell = Shell::new(&mut rt, ThemeMode::Dark);
        assert_eq!(listed(&shell), ["Mine", "Other"]);

        shell.act_on_menu(MenuChoice::Delete);
        assert_eq!(
            listed(&shell),
            ["Mine", "Other"],
            "nothing happens before the answer"
        );
        assert_eq!(
            shell.confirm.as_ref().map(asked).as_deref(),
            Some("Move the preset Mine to the trash?")
        );
        assert!(
            shell.pane_open(),
            "the menu stays shut while the question is up"
        );
        assert!(shell.window_size().y >= dialogs::message::WINDOW_SIZE.y);

        shell.answer(ConfirmChoice::No);
        assert_eq!(listed(&shell), ["Mine", "Other"]);
        assert!(shell.confirm.is_none());

        shell.act_on_menu(MenuChoice::Delete);
        shell.answer(ConfirmChoice::Dismissed);
        assert_eq!(listed(&shell), ["Mine", "Other"], "closing the box is a no");

        shell.act_on_menu(MenuChoice::Delete);
        shell.answer(ConfirmChoice::Yes);
        assert_eq!(listed(&shell), ["Other"]);
        assert!(dir.path().join("presets/.Trash/files/Mine.fac").is_file());
    }

    #[test]
    fn a_yes_to_a_preset_that_is_no_longer_selected_deletes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut rt = runtime(dir.path());
        let mut shell = Shell::new(&mut rt, ThemeMode::Dark);
        shell.act_on_menu(MenuChoice::Delete);
        // A keybind moves the selection while the question is up.
        shell.rt.app.cycle_preset(true);
        assert_eq!(
            shell.rt.app.state.preset().map(|p| p.name.as_str()),
            Some("Other")
        );
        shell.answer(ConfirmChoice::Yes);
        assert_eq!(listed(&shell), ["Mine", "Other"]);
    }

    #[test]
    fn reset_presets_asks_first_and_discards_the_unsaved_changes_on_yes() {
        // 0.4.0 audit #21: the settings pane's button reset at once.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut rt = runtime(dir.path());
        let mut shell = Shell::new(&mut rt, ThemeMode::Dark);
        shell
            .rt
            .app
            .handle(&[UiAction::SetEffect(Effect::Bass, 7.0)]);
        shell.open_settings();
        // The pane's button, through the code `show_settings` runs on what the pane reports.
        let press_reset = |shell: &mut Shell<'_>| {
            let mut state = shell.settings.take().expect("the pane is open");
            assert!(state.can_reset_presets, "the button is live");
            let closed = shell.act_on_settings(&[SettingsAction::ResetPresets], &mut state);
            assert!(!closed, "the reset does not close the pane");
            shell.settings = Some(state);
        };
        press_reset(&mut shell);
        assert_eq!(
            shell.rt.app.lane_preset(DeviceDirection::Output),
            Some(("Mine", true)),
            "nothing is discarded before the answer"
        );
        assert!(
            shell.settings.as_ref().is_some_and(|s| s.can_reset_presets),
            "the pane still offers the reset"
        );
        assert_eq!(
            shell.confirm.as_ref().map(asked).as_deref(),
            Some("Discard the unsaved changes of every preset? Saved presets are kept.")
        );
        shell.answer(ConfirmChoice::No);
        assert_eq!(
            shell.rt.app.lane_preset(DeviceDirection::Output),
            Some(("Mine", true))
        );
        press_reset(&mut shell);
        shell.answer(ConfirmChoice::Dismissed);
        assert_eq!(
            shell.rt.app.lane_preset(DeviceDirection::Output),
            Some(("Mine", true)),
            "closing the box is a no"
        );
        press_reset(&mut shell);
        assert_eq!(
            shell.rt.app.lane_preset(DeviceDirection::Output),
            Some(("Mine", true))
        );
        shell.answer(ConfirmChoice::Yes);
        assert_eq!(
            shell.rt.app.lane_preset(DeviceDirection::Output),
            Some(("Mine", false))
        );
        assert!(shell.settings.is_some(), "the pane stays open");
        assert!(!shell.settings.as_ref().is_some_and(|s| s.can_reset_presets));
    }
}

#[cfg(test)]
mod name_editor_tests {
    use super::*;
    use fxsound_core::DeviceDirection::{Input, Output};

    #[test]
    fn an_editor_opened_for_one_lane_closes_when_the_window_moves_to_the_other() {
        // Save New Preset typed on the speakers, then `fxsound --edit=input` from a keybind: the
        // name was for the speakers' controls and list, and must not save the microphone's.
        let editor = NameEditor::new(EditorPurpose::SaveNew, Output, Some("A"));
        assert!(editor.still_applies(Output, Some("A"), true, true));
        assert!(!editor.still_applies(Input, Some("A"), true, true));
        let rename = NameEditor::new(EditorPurpose::Rename, Input, Some("A"));
        assert!(rename.still_applies(Input, Some("A"), false, true));
        assert!(!rename.still_applies(Output, Some("A"), false, true));
    }

    #[test]
    fn an_editor_whose_item_went_grey_closes() {
        let editor = NameEditor::new(EditorPurpose::SaveNew, Output, Some("A"));
        assert!(!editor.still_applies(Output, Some("A"), false, true));
        let rename = NameEditor::new(EditorPurpose::Rename, Output, Some("A"));
        assert!(!rename.still_applies(Output, Some("A"), true, false));
    }

    #[test]
    fn a_rename_editor_closes_when_another_preset_is_selected_under_it() {
        // FA: a headphone plugged in brought back its preset B while A's new name was typed, and
        // Enter renamed B.
        let rename = NameEditor::new(EditorPurpose::Rename, Output, Some("A"));
        assert!(rename.still_applies(Output, Some("A"), true, true));
        assert!(!rename.still_applies(Output, Some("B"), true, true));
        assert!(!rename.still_applies(Output, None, true, true));
        // Save New Preset saves the controls on screen, whichever preset they came from.
        let save = NameEditor::new(EditorPurpose::SaveNew, Output, Some("A"));
        assert!(save.still_applies(Output, Some("B"), true, true));
    }
}

#[cfg(test)]
mod forget_device_tests {
    //! A `--forget-device` (or D-Bus `ForgetDevice`, which travels the same channel) that reaches
    //! an instance PipeWire has not listed the devices to yet: bus activation starts FxSound for
    //! the call, and a script may run it right after `fxsound &` (0.4.0 audit #34, finisher F8).
    use super::*;
    use fxsound_app::audio_link::FakeEngine;
    use fxsound_core::messages::AudioToUi;
    use fxsound_core::{AudioDevice, DeviceDirection, Settings};
    use fxsound_preset::{InputPresetStore, PresetStore};
    use std::path::Path;
    use std::thread::JoinHandle;

    /// A pump whose engine has not listed a device yet, and whose settings remember the laptop's
    /// speakers and a dock that is not plugged in.
    fn young_runtime(dir: &Path) -> (Runtime, FakeEngine) {
        let Instance::Primary(listener) = Instance::acquire_in(dir).expect("acquire") else {
            panic!("expected to be primary");
        };
        let engine = FakeEngine::new();
        let mut settings = Settings::default();
        settings.remember_device_preset(
            "alsa_output.speakers",
            "Speakers",
            "",
            "",
            DeviceDirection::Output,
        );
        settings.remember_device_preset(
            "alsa_output.dock",
            "USB Dock",
            "",
            "",
            DeviceDirection::Output,
        );
        // Two presets for a line to pick, as a script and a keybind might.
        for name in ["Day", "Night"] {
            let preset = fxsound_core::Preset {
                name: name.to_owned(),
                ..fxsound_core::Preset::default()
            };
            fxsound_preset::save(&preset, &dir.join("presets").join(format!("{name}.fac")))
                .expect("a preset");
        }
        let mut presets = PresetStore::with_dirs(Vec::new(), dir.join("presets"));
        presets.rescan();
        let voices = InputPresetStore::with_dirs(Vec::new(), dir.join("presets").join("Input"));
        let app = App::start_for_tests(settings, presets, voices, &engine);
        let (_tray_tx, tray_rx) = crossbeam_channel::unbounded();
        let runtime = Runtime {
            app,
            dbus: None,
            server: listener.serve().expect("serve"),
            sleep: None,
            sleep_rx: crossbeam_channel::never(),
            tray: None,
            tray_rx,
            exit: WindowExit::Hidden,
            settings_requested: false,
            terminate: Arc::new(AtomicBool::new(false)),
            terminating: false,
            waker: Waker::new(),
            signals: None,
            panes: Panes::default(),
            waiting_for_devices: Vec::new(),
            tray_wait: None,
            start_minimised: false,
            reopening: false,
        };
        (runtime, engine)
    }

    fn speakers() -> AudioDevice {
        AudioDevice {
            id: 3,
            name: "alsa_output.speakers".to_owned(),
            description: "Speakers".to_owned(),
            is_default: true,
            direction: DeviceDirection::Output,
            form_factor: "speaker".to_owned(),
        }
    }

    /// `fxsound <args>` forwarded to `runtime`, on a thread: its answer.
    fn forward(runtime: &Runtime, args: &[&str]) -> JoinHandle<std::io::Result<ipc::Response>> {
        let socket = runtime.server.path().to_path_buf();
        let argv: Vec<String> = std::iter::once("fxsound")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect();
        std::thread::spawn(move || {
            ipc::forward_to(&socket, &argv, Path::new("/"), Duration::from_secs(5))
        })
    }

    /// Tick until the forwarded line is waiting for the device list.
    fn until_it_waits(runtime: &mut Runtime) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while runtime.waiting_for_devices.is_empty() {
            assert!(Instant::now() < deadline, "the line never arrived");
            runtime.wait(Duration::from_millis(50));
            runtime.tick();
        }
    }

    #[test]
    fn a_line_forwarded_after_one_that_waits_for_the_device_list_runs_after_it() {
        // FA: the keybind's `--preset=Day`, sent after a script's held line, ran at once, and the
        // held `--preset=Night` undid it when the device list came.
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut runtime, engine) = young_runtime(dir.path());
        let script = forward(&runtime, &["--forget-device=USB Dock", "--preset=Night"]);
        until_it_waits(&mut runtime);
        let keybind = forward(&runtime, &["--preset=Day"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while runtime.waiting_for_devices.len() < 2 {
            assert!(Instant::now() < deadline, "the second line never arrived");
            runtime.wait(Duration::from_millis(50));
            runtime.tick();
        }
        runtime.tick();
        assert!(!keybind.is_finished(), "it waits its turn");

        engine.feed(AudioToUi::Devices(vec![speakers()]));
        runtime.tick();
        assert!(runtime.waiting_for_devices.is_empty());
        for client in [script, keybind] {
            let answer = client.join().expect("client").expect("answered");
            assert!(answer.ok, "{}", answer.stderr);
        }
        assert_eq!(
            runtime.app.state.preset().map(|p| p.name.as_str()),
            Some("Day"),
            "the last word was the keybind's"
        );
    }

    #[test]
    fn a_line_behind_a_waiting_one_is_answered_once_that_ones_wait_is_over() {
        // No device list at all: the held line is refused when its wait is over, and the one
        // behind it runs right after, well within its own caller's patience.
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut runtime, _engine) = young_runtime(dir.path());
        let script = forward(&runtime, &["--forget-device=USB Dock"]);
        until_it_waits(&mut runtime);
        let keybind = forward(&runtime, &["--preset=Day"]);
        let deadline = Instant::now() + commands::DEVICE_LIST_WAIT + Duration::from_secs(2);
        while !(script.is_finished() && keybind.is_finished()) {
            assert!(Instant::now() < deadline, "never answered");
            runtime.wait(runtime.pump_interval(Instant::now()));
            runtime.tick();
        }
        assert!(!script.join().expect("client").expect("answered").ok);
        let answer = keybind.join().expect("client").expect("answered");
        assert!(answer.ok, "{}", answer.stderr);
        assert_eq!(
            runtime.app.state.preset().map(|p| p.name.as_str()),
            Some("Day")
        );
    }

    #[test]
    fn forget_device_before_the_first_device_list_waits_for_it_and_then_forgets() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut runtime, engine) = young_runtime(dir.path());
        let client = forward(&runtime, &["--forget-device=USB Dock"]);
        until_it_waits(&mut runtime);
        runtime.tick();
        assert!(!client.is_finished(), "held, not refused");

        engine.feed(AudioToUi::Devices(vec![speakers()]));
        runtime.tick();
        assert!(runtime.waiting_for_devices.is_empty());
        let answer = client.join().expect("client").expect("answered");
        assert!(answer.ok, "{}", answer.stderr);
        assert!(answer.stderr.is_empty(), "{}", answer.stderr);

        // Forgotten: asked again, the name is one neither list has.
        let again = forward(&runtime, &["--forget-device=USB Dock"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !again.is_finished() {
            assert!(Instant::now() < deadline, "never answered");
            runtime.wait(Duration::from_millis(50));
            runtime.tick();
        }
        let answer = again.join().expect("client").expect("answered");
        assert!(!answer.ok);
        assert!(
            answer
                .stderr
                .contains("neither device priority list has a device called"),
            "{}",
            answer.stderr
        );
    }

    #[test]
    fn a_connected_device_is_still_refused_once_the_list_is_in() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut runtime, engine) = young_runtime(dir.path());
        let client = forward(&runtime, &["--forget-device=Speakers"]);
        until_it_waits(&mut runtime);
        engine.feed(AudioToUi::Devices(vec![speakers()]));
        runtime.tick();
        let answer = client.join().expect("client").expect("answered");
        assert!(!answer.ok);
        assert!(
            answer.stderr.contains("unplug it first"),
            "{}",
            answer.stderr
        );
    }

    #[test]
    fn with_no_device_list_in_time_the_line_is_refused_before_the_caller_gives_up() {
        const { assert!(commands::DEVICE_LIST_WAIT.as_millis() < ipc::HANDLER_TIMEOUT.as_millis()) };
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut runtime, _engine) = young_runtime(dir.path());
        let client = forward(&runtime, &["--forget-device=USB Dock"]);
        until_it_waits(&mut runtime);

        // The pump wakes for the end of the wait rather than at the keepalive after it.
        let soon = Instant::now() + Duration::from_millis(100);
        runtime.waiting_for_devices[0].1 = soon;
        assert!(runtime.pump_interval(Instant::now()) <= Duration::from_millis(100));

        runtime.waiting_for_devices[0].1 = Instant::now();
        runtime.tick();
        assert!(runtime.waiting_for_devices.is_empty());
        let answer = client.join().expect("client").expect("answered");
        assert!(!answer.ok);
        assert!(
            answer
                .stderr
                .contains("has not listed the audio devices yet"),
            "{}",
            answer.stderr
        );
    }

    #[test]
    fn a_line_without_forget_device_does_not_wait_for_the_device_list() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut runtime, _engine) = young_runtime(dir.path());
        let client = forward(&runtime, &["--power=off"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client.is_finished() {
            assert!(Instant::now() < deadline, "never answered");
            runtime.wait(Duration::from_millis(50));
            runtime.tick();
            assert!(runtime.waiting_for_devices.is_empty());
        }
        assert!(client.join().expect("client").expect("answered").ok);
        assert!(!runtime.app.state.power);
    }

    #[test]
    fn a_line_waiting_for_the_device_list_hears_that_fxsound_is_shutting_down() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (mut runtime, _engine) = young_runtime(dir.path());
        let client = forward(&runtime, &["--forget-device=USB Dock"]);
        until_it_waits(&mut runtime);
        runtime.shutdown();
        let answer = client.join().expect("client").expect("answered");
        assert!(!answer.ok);
        assert!(answer.stderr.contains("shutting down"), "{}", answer.stderr);
    }
}
