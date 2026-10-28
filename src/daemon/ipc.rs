//! crownos-ipc integration: the [`crownpaper`] service on the calloop loop.
//!
//! The protocol itself is declared in this crate's library target, so the
//! daemon and every client are generated from one declaration.
//!
//! The listener and every accepted peer are registered as calloop sources
//! over dup'd fds; the single-owner [`Server`] lives in the [`Daemon`] that
//! every callback receives. Peer sources track the connection's poll
//! interest (read always, write only while replies or events are still
//! buffered) and reregister themselves when it changes.

use std::cell::Cell;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result, anyhow};
use calloop::generic::Generic;
use calloop::{EventSource, LoopHandle, Mode, Poll, PostAction, Readiness, Token, TokenFactory};
use crownos_ipc::{PeerId, RemoteError, Server, ServiceBuilder};

use super::Daemon;
use super::state::State;
use crate::decode;

pub struct Ipc {
    server: Server,
}

impl Ipc {
    pub fn bind() -> Result<Self> {
        let server = ServiceBuilder::new(crownpaper::SERVICE)
            .build()
            .context("binding the crownpaper service socket")?;
        Ok(Self { server })
    }

    pub fn emit_changed(&mut self, path: &Path) {
        let event = crownpaper::Changed {
            path: path.display().to_string(),
        };
        if let Err(e) = self.server.emit(&event) {
            log::warn!("failed to emit Changed event: {e}");
        }
    }
}

/// Wire the bound service socket into the event loop.
pub fn register(ipc: &Ipc, loop_handle: &LoopHandle<'static, Daemon>) -> Result<()> {
    let listener_fd = ipc
        .server
        .as_fd()
        .try_clone_to_owned()
        .context("duplicating the listener fd")?;
    let handle = loop_handle.clone();
    loop_handle
        .insert_source(
            Generic::new(listener_fd, calloop::Interest::READ, Mode::Level),
            move |_, _, daemon| {
                accept_pending(&handle, daemon);
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow!("registering the ipc listener source: {e}"))?;
    Ok(())
}

fn accept_pending(handle: &LoopHandle<'static, Daemon>, daemon: &mut Daemon) {
    loop {
        match daemon.ipc.server.accept() {
            Ok(Some(peer)) => {
                if let Err(e) = register_peer(handle, &daemon.ipc, peer) {
                    log::warn!("failed to register ipc peer: {e:#}");
                    daemon.ipc.server.drop_peer(peer);
                }
            }
            Ok(None) => return,
            Err(e) => {
                log::warn!("ipc accept failed: {e}");
                return;
            }
        }
    }
}

fn register_peer(handle: &LoopHandle<'static, Daemon>, ipc: &Ipc, peer: PeerId) -> Result<()> {
    let fd = ipc
        .server
        .peer_fd(peer)
        .context("peer vanished before registration")?
        .try_clone_to_owned()
        .context("duplicating the peer fd")?;
    let (source, interest) = PeerSource::new(fd);
    handle
        .insert_source(source, move |readiness, (), daemon| {
            pump_peer(daemon, peer, readiness, &interest)
        })
        .map_err(|e| anyhow!("registering an ipc peer source: {e}"))?;
    Ok(())
}

/// Pump one ready peer: flush writes, read frames, dispatch requests.
/// Returns `Remove` once the peer is gone so calloop drops its source.
fn pump_peer(
    daemon: &mut Daemon,
    peer: PeerId,
    readiness: Readiness,
    interest: &Cell<calloop::Interest>,
) -> PostAction {
    let server = &mut daemon.ipc.server;
    let handler = &mut Requests(&mut daemon.state);

    if readiness.writable && server.handle_writable(peer).is_err() {
        return PostAction::Remove;
    }
    if readiness.readable && server.handle_readable(peer).is_err() {
        return PostAction::Remove;
    }
    while let Some(msg) = server.next_request(peer) {
        if let Err(e) = crownpaper::dispatch(handler, server, peer, msg) {
            log::warn!("ipc dispatch failed: {e}");
        }
    }
    if server.reap(peer) {
        return PostAction::Remove;
    }
    if let Some(want) = server.peer_interest(peer) {
        interest.set(calloop::Interest {
            readable: true,
            writable: want.write,
        });
    }
    PostAction::Continue
}

struct Requests<'a>(&'a mut State);

impl crownpaper::Handler for Requests<'_> {
    fn set(&mut self, path: String, duration: f32) -> Result<(), RemoteError> {
        let path = canonical(&path);
        // Acknowledge as soon as the request is validated and queued; the
        // decode and transition proceed asynchronously.
        let kind = decode::probe(&path).map_err(|e| RemoteError::handler(format!("{e:#}")))?;
        self.0.request_set(path, kind, duration);
        Ok(())
    }

    fn preload(&mut self, paths: Vec<String>) {
        for path in paths {
            let path = canonical(&path);
            match decode::probe(&path) {
                Ok(kind) => self.0.request_preload(path, kind),
                Err(e) => log::warn!("skipping preload: {e:#}"),
            }
        }
    }

    fn unload(&mut self, paths: Vec<String>) {
        for path in paths {
            self.0.request_unload(&canonical(&path));
        }
    }

    fn preload_renderer(&mut self) {
        self.0.hold_renderer(true);
    }

    fn unload_renderer(&mut self) {
        self.0.hold_renderer(false);
    }

    fn ping(&mut self) -> Result<(), RemoteError> {
        Ok(())
    }

    fn quit(&mut self) {
        log::info!("quit requested over ipc");
        self.0.request_quit();
    }
}

/// Cache keys must be stable however a client spells the path. Falls back to
/// the raw path so `unload` still matches after the file was deleted.
fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// A connected peer as a calloop source. Wraps [`Generic`] so the registered
/// poll interest can follow the connection's needs: the pump callback stores
/// the wanted interest in the shared cell and the source reregisters itself
/// whenever that differs from what is currently programmed.
struct PeerSource {
    io: Generic<OwnedFd>,
    wanted: Rc<Cell<calloop::Interest>>,
}

impl PeerSource {
    fn new(fd: OwnedFd) -> (Self, Rc<Cell<calloop::Interest>>) {
        let wanted = Rc::new(Cell::new(calloop::Interest::READ));
        let io = Generic::new(fd, calloop::Interest::READ, Mode::Level);
        (
            Self {
                io,
                wanted: Rc::clone(&wanted),
            },
            wanted,
        )
    }
}

impl EventSource for PeerSource {
    type Event = Readiness;
    type Metadata = ();
    type Ret = PostAction;
    type Error = io::Error;

    fn process_events<F>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> io::Result<PostAction>
    where
        F: FnMut(Readiness, &mut ()) -> PostAction,
    {
        let mut action = PostAction::Continue;
        self.io.process_events(readiness, token, |r, _| {
            action = callback(r, &mut ());
            Ok(PostAction::Continue)
        })?;
        let wanted = self.wanted.get();
        if action == PostAction::Continue
            && (wanted.readable != self.io.interest.readable
                || wanted.writable != self.io.interest.writable)
        {
            self.io.interest = wanted;
            action = PostAction::Reregister;
        }
        Ok(action)
    }

    fn register(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        self.io.interest = self.wanted.get();
        self.io.register(poll, token_factory)
    }

    fn reregister(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        self.io.interest = self.wanted.get();
        self.io.reregister(poll, token_factory)
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.io.unregister(poll)
    }
}
