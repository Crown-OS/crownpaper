//! Daemon wiring: crownshell app setup, per-output surfaces, IPC service,
//! decode worker and the channels that tie them into one calloop loop.

pub mod ipc;
pub mod state;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context, Result, anyhow};
use crownos_config::Appearance;
use crownshell::calloop::channel;
use crownshell::{Anchor, App, Layer, WindowConfig};

use crate::decode::{DecodeResult, Decoder};
use crate::surface::WallpaperSurface;
use ipc::Ipc;
use state::State;

/// Transition length when the caller does not specify one.
pub const DEFAULT_TRANSITION_SECS: f32 = 1.2;

pub fn run(image: Option<PathBuf>, duration: f32) -> Result<()> {
    let initial = startup_wallpaper(image)?;

    crownshell::run(move |app| {
        let loop_handle = app.loop_handle();

        // Wake channel: any state change that needs a repaint or has bus
        // events to flush pings this to make the loop turn promptly.
        let (wake_tx, wake_rx) = channel::channel::<()>();
        let (decode_tx, decode_rx) = channel::channel::<DecodeResult>();

        let state = Rc::new(RefCell::new(State::new(Decoder::spawn(decode_tx), wake_tx)));
        let ipc = ipc::register(&loop_handle, Rc::clone(&state))?;

        {
            let (ipc, state) = (Rc::clone(&ipc), Rc::clone(&state));
            loop_handle
                .insert_source(wake_rx, move |event, (), app| {
                    if let channel::Event::Msg(()) = event {
                        flush_bus_events(app, &ipc, &state);
                    }
                })
                .map_err(|e| anyhow!("registering the wake source: {e}"))?;
        }
        {
            let state = Rc::clone(&state);
            loop_handle
                .insert_source(decode_rx, move |event, (), _app| {
                    if let channel::Event::Msg(result) = event {
                        state.borrow_mut().on_decoded(result.path, result.result);
                    }
                })
                .map_err(|e| anyhow!("registering the decode source: {e}"))?;
        }

        if let Some(path) = initial {
            state.borrow_mut().request_set(path, duration);
        }

        app.on_output_added(move |app, output| {
            if app.window_on_output(output).is_some() {
                return; // hooks also fire for outputs handled during setup
            }
            let name = output_name(app, output);
            log::info!("creating wallpaper surface on output {name}");
            app.create_raw_window_on_output(
                WindowConfig {
                    namespace: "crownpaper".into(),
                    layer: Layer::Background,
                    anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                    exclusive_zone: -1,
                    ..Default::default()
                },
                WallpaperSurface::new(Rc::clone(&state)),
                output,
            );
        });
        // crownshell destroys the output's windows right after this hook.
        app.on_output_removed(|app, output| {
            log::info!("output {} removed", output_name(app, output));
        });
        Ok(())
    })
}

/// The wallpaper to show at startup, resolved before any Wayland setup so a
/// bad `--image` fails immediately.
///
/// `--image` wins. Without it the daemon follows `appearance.ron`'s
/// `wallpaper`, which crownsettings keeps pointed at whatever crownpaper last
/// reported — so a restarted daemon comes back with the wallpaper it had.
fn startup_wallpaper(image: Option<PathBuf>) -> Result<Option<PathBuf>> {
    match image {
        Some(path) => std::fs::canonicalize(&path)
            .map(Some)
            .with_context(|| format!("resolving initial wallpaper {}", path.display())),
        None => Ok(configured_wallpaper()),
    }
}

/// The `wallpaper` key of `appearance.ron`, if it names a file we can read.
///
/// Unset means "no wallpaper configured yet", and a path that no longer
/// resolves is a warning rather than an error: the daemon still has to come up
/// and serve `set`, and refusing to start over a stale config line would leave
/// the desktop with no way to fix it.
fn configured_wallpaper() -> Option<PathBuf> {
    let configured = crownos_config::load::<Appearance>(Appearance::SECTION).wallpaper;
    if configured.is_empty() {
        return None;
    }
    match std::fs::canonicalize(&configured) {
        Ok(path) => {
            log::info!("startup wallpaper from appearance.ron: {}", path.display());
            Some(path)
        }
        Err(e) => {
            log::warn!("ignoring wallpaper {configured} from appearance.ron: {e}");
            None
        }
    }
}

/// Emit queued `Changed` events and honor a pending quit. Runs whenever the
/// state wakes the loop, so bus subscribers see changes promptly no matter
/// which path (IPC, decode completion, cache hit) triggered them.
fn flush_bus_events(app: &mut App, ipc: &Rc<RefCell<Ipc>>, state: &Rc<RefCell<State>>) {
    let (changed, quit) = {
        let mut state = state.borrow_mut();
        (state.take_changed(), state.quit)
    };
    if !changed.is_empty() {
        let mut ipc = ipc.borrow_mut();
        for path in &changed {
            ipc.emit_changed(path);
        }
    }
    if quit {
        app.exit = true;
    }
}

fn output_name(app: &App, output: &crownshell::WlOutput) -> String {
    app.output_info(output)
        .and_then(|info| info.name)
        .unwrap_or_else(|| "<unnamed>".into())
}
