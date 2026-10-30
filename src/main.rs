//! crownpaper — the CrownOS GPU wallpaper daemon.
//!
//! `crownpaper` (or `crownpaper daemon`) shows a still or video wallpaper on every output and
//! animates changes with a blur→focus transition. The other subcommands are
//! thin clients for the daemon's `crownpaper` service on the CrownOS IPC bus.

mod cache;
mod daemon;
mod decode;
mod gpu;
mod video;
mod wallpaper;
mod wayland;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};

const IPC_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(
    name = "crownpaper",
    version,
    about = "Image and video wallpaper daemon for Wayland",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    daemon: DaemonArgs,
}

#[derive(Args)]
struct DaemonArgs {
    /// Wallpaper to show at startup [default: `wallpaper` from appearance.ron]
    #[arg(short, long)]
    image: Option<PathBuf>,
    /// Default transition duration in seconds
    #[arg(short, long, default_value_t = daemon::DEFAULT_TRANSITION_SECS)]
    duration: f32,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the wallpaper daemon (the default without a subcommand)
    Daemon(DaemonArgs),
    /// Transition to a new wallpaper (image or video)
    Set {
        path: PathBuf,
        /// Transition duration in seconds
        #[arg(short, long, default_value_t = daemon::DEFAULT_TRANSITION_SECS)]
        duration: f32,
    },
    /// Decode wallpapers into the GPU cache so a later `set` is instant
    Preload {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Drop preloaded wallpapers from the cache
    Unload {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Keep the GPU renderer warm so the next `set` animates at once
    PreloadRenderer,
    /// Let the renderer be released once the screen is at rest
    UnloadRenderer,
    /// Health-check the daemon
    Ping,
    /// Stop the daemon
    Quit,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,wgpu_core=warn,wgpu_hal=warn"),
    )
    .format_timestamp_millis()
    .init();

    let cli = Cli::parse();
    let cmd = cli.cmd.unwrap_or(Cmd::Daemon(cli.daemon));
    match cmd {
        Cmd::Daemon(DaemonArgs { image, duration }) => daemon::run(image, duration),
        Cmd::Set { path, duration } => {
            let path = canonicalize(&path)?;
            let mut client = connect()?;
            let pending = client.set(path.clone(), duration)?;
            client.wait(pending, Some(IPC_TIMEOUT))?;
            println!("wallpaper set to {path}");
            Ok(())
        }
        Cmd::Preload { paths } => {
            let paths = canonicalize_all(&paths)?;
            let count = paths.len();
            let mut client = connect()?;
            client.preload(paths)?;
            roundtrip(&mut client)?;
            println!("preloaded {count} wallpaper(s)");
            Ok(())
        }
        Cmd::Unload { paths } => {
            let paths = canonicalize_all(&paths)?;
            let count = paths.len();
            let mut client = connect()?;
            client.unload(paths)?;
            roundtrip(&mut client)?;
            println!("unloaded {count} wallpaper(s)");
            Ok(())
        }
        Cmd::PreloadRenderer => {
            let mut client = connect()?;
            client.preload_renderer()?;
            roundtrip(&mut client)?;
            println!("renderer held warm");
            Ok(())
        }
        Cmd::UnloadRenderer => {
            let mut client = connect()?;
            client.unload_renderer()?;
            roundtrip(&mut client)?;
            println!("renderer released");
            Ok(())
        }
        Cmd::Ping => {
            let mut client = connect()?;
            roundtrip(&mut client)?;
            println!("pong");
            Ok(())
        }
        Cmd::Quit => {
            connect()?.quit()?;
            println!("quit sent");
            Ok(())
        }
    }
}

fn connect() -> Result<crownpaper::Client> {
    crownpaper::Client::connect().context("connecting to crownpaper (is the daemon running?)")
}

/// A `ping` round trip. Requests are handled in order per connection, so this
/// also confirms that any notification sent just before it was processed.
fn roundtrip(client: &mut crownpaper::Client) -> Result<()> {
    let pending = client.ping()?;
    client.wait(pending, Some(IPC_TIMEOUT))?;
    Ok(())
}

/// The daemon may run with a different working directory, so paths are made
/// absolute before they go on the wire.
fn canonicalize(path: &Path) -> Result<String> {
    let absolute =
        std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    let text = absolute
        .to_str()
        .with_context(|| format!("{}: path is not valid UTF-8", absolute.display()))?;
    Ok(text.to_owned())
}

fn canonicalize_all(paths: &[PathBuf]) -> Result<Vec<String>> {
    ensure!(!paths.is_empty(), "no paths given");
    paths.iter().map(|p| canonicalize(p)).collect()
}
