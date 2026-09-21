//! The wire contract of the `crownpaper` service.
//!
//! Lives in the library target rather than next to the daemon's calloop
//! plumbing so that every client — this crate's own CLI, crownsettings, any
//! other CrownOS app that drives the desktop background — is generated from
//! the same declaration and cannot drift from the daemon.
//!
//! The daemon half of it (`Handler`, `dispatch`) costs nothing to a client
//! that only calls, so both halves are exported together.

crownos_ipc::protocol! {
    mod protocol: "crownpaper" {
        /// Transition the desktop to `path` over `duration` seconds.
        ///
        /// Replies as soon as the image is known to be decodable and the
        /// transition is queued — not when it finishes. The wallpaper that
        /// actually landed arrives as [`Changed`].
        fn set(path: String, duration: f32);
        /// Decode wallpapers into the GPU cache so a later `set` is instant.
        notify preload(paths: Vec<String>);
        /// Drop preloaded wallpapers from the cache.
        notify unload(paths: Vec<String>);
        /// Round trip that proves the daemon is alive and has drained
        /// everything sent before it.
        fn ping();
        /// Ask the daemon to shut down.
        notify quit();
        /// The wallpaper now on screen, pushed to every subscriber.
        event Changed { path: String };
    }
}

pub use protocol::*;
