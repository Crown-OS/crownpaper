//! crownos-ipc integration: the [`crownpaper`] service on the calloop loop.
//!
//! The protocol itself is declared in this crate's library target, so the
//! daemon and every client are generated from one declaration.
//!
//! The listener and every accepted peer are registered as calloop sources
//! over dup'd fds, because [`Server`] is single-owner and lives in an
//! `Rc<RefCell<_>>` shared by all callbacks. Peer sources track the
//! connection's poll interest (read always, write only while replies or
//! events are still buffered) and reregister themselves when it changes.

use std::cell::{Cell, RefCell};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{anyhow, Context, Result};
use crownos_ipc::{PeerId, RemoteError, Server, ServiceBuilder};
use crownshell::calloop::generic::Generic;
use crownshell::calloop::{
    self, EventSource, LoopHandle, Mode, Poll, PostAction, Readiness, Token, TokenFactory,
};
use crownshell::App;

use super::state::State;
use crate::decode;

pub struct Ipc {
    server: Server,
    handler: Handler,
}

impl Ipc {
    pub fn emit_changed(&mut self, path: &Path) {
        let event = crownpaper::Changed {
            path: path.display().to_string(),
        };
        if let Err(e) = self.server.emit(&event) {
            log::warn!("failed to emit Changed event: {e}");
        }
    }
}

/// Bind the service socket and wire it into the event loop.
pub fn register(
    loop_handle: &LoopHandle<'static, App>,
    state: Rc<RefCell<State>>,
) -> Result<Rc<RefCell<Ipc>>> {
    let server = ServiceBuilder::new(crownpaper::SERVICE)
        .build()
        .context("binding the crownpaper service socket")?;
    let ipc = Rc::new(RefCell::new(Ipc {
        server,
        handler: Handler { state },
    }));

    let listener_fd = ipc
        .borrow()
        .server
        .as_fd()
        .try_clone_to_owned()
        .context("duplicating the listener fd")?;
    let handle = loop_handle.clone();
    let ipc_for_listener = Rc::clone(&ipc);
    loop_handle
        .insert_source(
            Generic::new(listener_fd, calloop::Interest::READ, Mode::Level),
            move |_, _, _| {
                accept_pending(&handle, &ipc_for_listener);
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow!("registering the ipc listener source: {e}"))?;
    Ok(ipc)
}

fn accept_pending(handle: &LoopHandle<'static, App>, ipc: &Rc<RefCell<Ipc>>) {
    loop {
        // Bind first: a `borrow_mut` in the match scrutinee would live for
        // the whole match and collide with the borrows in `register_peer`.
        let accepted = ipc.borrow_mut().server.accept();
        match accepted {
            Ok(Some(peer)) => {
                if let Err(e) = register_peer(handle, ipc, peer) {
                    log::warn!("failed to register ipc peer: {e:#}");
                    ipc.borrow_mut().server.drop_peer(peer);
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

fn register_peer(
    handle: &LoopHandle<'static, App>,
    ipc: &Rc<RefCell<Ipc>>,
    peer: PeerId,
) -> Result<()> {
    let fd = {
        let ipc = ipc.borrow();
        let fd = ipc
            .server
            .peer_fd(peer)
            .context("peer vanished before registration")?;
        fd.try_clone_to_owned().context("duplicating the peer fd")?
    };
    let (source, interest) = PeerSource::new(fd);
    let ipc = Rc::clone(ipc);
    handle
        .insert_source(source, move |readiness, (), _app| {
            pump_peer(&ipc, peer, readiness, &interest)
        })
        .map_err(|e| anyhow!("registering an ipc peer source: {e}"))?;
    Ok(())
}

/// Pump one ready peer: flush writes, read frames, dispatch requests.
/// Returns `Remove` once the peer is gone so calloop drops its source.
fn pump_peer(
    ipc: &Rc<RefCell<Ipc>>,
    peer: PeerId,
    readiness: Readiness,
    interest: &Cell<calloop::Interest>,
) -> PostAction {
    let ipc = &mut *ipc.borrow_mut();
    let (server, handler) = (&mut ipc.server, &mut ipc.handler);

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

struct Handler {
    state: Rc<RefCell<State>>,
}

impl crownpaper::Handler for Handler {
    fn set(&mut self, path: String, duration: f32) -> Result<(), RemoteError> {
        let path = canonical(&path);
        // Acknowledge as soon as the request is validated and queued; the
        // decode and transition proceed asynchronously.
        decode::probe(&path).map_err(|e| RemoteError::handler(format!("{e:#}")))?;
        self.state.borrow_mut().request_set(path, duration);
        Ok(())
    }

    fn preload(&mut self, paths: Vec<String>) {
        let mut state = self.state.borrow_mut();
        for path in paths {
            let path = canonical(&path);
            match decode::probe(&path) {
                Ok(()) => state.request_preload(path),
                Err(e) => log::warn!("skipping preload: {e:#}"),
            }
        }
    }

    fn unload(&mut self, paths: Vec<String>) {
        let mut state = self.state.borrow_mut();
        for path in paths {
            state.request_unload(&canonical(&path));
        }
    }

    fn ping(&mut self) -> Result<(), RemoteError> {
        Ok(())
    }

    fn quit(&mut self) {
        log::info!("quit requested over ipc");
        self.state.borrow_mut().request_quit();
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
