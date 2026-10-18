//! DRM backend: open display-capable cards (`/dev/dri/cardN`) as DRM master
//! and register each as `Resource::drm(card)` — the card's PRIMARY plane, the
//! one an app on the display draws through — and `Resource::drm_overlay(card)`
//! — one of that pipeline's overlay planes, stacked above the primary.
//!
//! Compiled only with the `drm` feature (default). The master fd never
//! leaves the server: grants hand the client a lease fd (see
//! [`DrmDevice::grant_lease`]) and revocation is kernel-enforced.
//! Registration order IS the advertised priority order (first is best): the
//! card selection logic lives here with the open.
//!
//! # One lease per pipeline, shared by its clients
//!
//! The kernel forbids two leases over the same CRTC (`EBUSY`) and rejects a
//! lease without a CRTC, a connector and a plane (`EINVAL`), so a display
//! pipeline can only ever be leased once — and a lease is a `drm_master`, not
//! a process, so several clients can hold dups of that one lease. The lease is
//! therefore created for the pipeline's owner and *reused* for a client that
//! asks for an overlay, which is what makes a plane grantable at all: the
//! client programs the granted plane itself, through the same lease.
//!
//! A client that asks for the overlay role is granted one overlay plane (the
//! highest one that can drive the pipeline's CRTC, raised above the primary)
//! and owns it until it releases the resource; the daemon clears the plane on
//! release, because a dropped grant is not a cleared plane — the plane state
//! keeps its own reference to the client's framebuffer, so the last frame
//! keeps scanning out until somebody programs that plane again. Only the
//! master can do that once the client is gone.
//!
//! What this is not: a sandbox. Every client holding the shared lease can
//! program every plane in it, so which plane a client uses is policy the
//! daemon keeps, not something the kernel enforces between them. The parts
//! that ARE kernel-enforced are the lease itself (torn down with the pipeline
//! owner, taking every grant with it) and what the lease leaves out: cursor
//! planes, so a client can never reprogram the server's pointer, and any other
//! plane the server keeps for itself by simply not leasing it.
//!
//! A card's life is a type-state machine — [`DrmDeviceState::Locked`]
//! (master held, leaseable) or [`DrmDeviceState::Leased`] (master + one
//! live lease, its owner, and any overlay grant of it). Transitions consume
//! the state and return `Result<Next, Self>`: an ioctl failure hands back the
//! state you were in, so a card can never be lost or double-leased — the
//! failure path is a retry, not a leak. The state lives behind
//! `Mutex<Option<...>>` because the engine can push a Revoke for one task
//! while another task runs a grant on the same card (see
//! docs/resource-manager.md).

use std::collections::HashMap;
use std::{
    fs::{File, read_dir},
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use dashmap::DashMap;
use drm::Device;
use drm::control::atomic::AtomicModeReq;
use drm::control::{
    AtomicCommitFlags, Device as ControlDevice, LeaseId, RawResourceHandle, ResourceHandles,
    connector, crtc, plane, property,
};
use nix::fcntl::OFlag;
use simple_graphics_protocol::CursorShape;
use simple_graphics_protocol::OverlayGeometry;
use simple_graphics_protocol::{DrmGrant, PlaneRole, Resource};
use thiserror::Error;
use tracing::{debug, error, info, trace, warn};

use crate::resource_manager::cursor::CursorSurface;
use crate::resource_manager::overlay_policy::{
    self, OverlayClaim, OverlayDecision, OverlayGrantView, OverlayPolicy,
};
use crate::types::ClientId;

/// The server's DRM master handle for one card: a plain fd wrapper that
/// implements the `drm` crate's `Device` traits (the crate deliberately
/// does not open device nodes for you).
///
/// The master is what creates leases. It is held by the server for the
/// whole run: closing it would destroy every lease it created.
#[derive(Debug)]
pub struct DrmCard {
    file: File,
    /// The card's index, as in `/dev/dri/cardN` — what every message about
    /// this card names it by.
    index: u8,
}

impl AsFd for DrmCard {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl AsRawFd for DrmCard {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl drm::Device for DrmCard {}
impl drm::control::Device for DrmCard {}

impl DrmCard {
    /// Open a DRM card node the way a display server should: `O_RDWR` for
    /// modesetting, `O_CLOEXEC` so the fd never leaks into children, and
    /// `O_NONBLOCK` so event reads on the fd can never block.
    fn open(path: &Path, index: u8) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .custom_flags((OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).bits())
            .open(path)?;
        Ok(Self { file, index })
    }

    /// Planes the server programs from the master fd. A driver without atomic
    /// mode setting is still a usable display server — the z-order step is the
    /// only part that needs it, and it degrades to a warning.
    fn enable_plane_capabilities(&self) {
        for (capability, missing) in [
            (
                drm::ClientCapability::UniversalPlanes,
                "planes will not be leased (a lease still needs a CRTC and a connector)",
            ),
            (
                drm::ClientCapability::Atomic,
                "the overlay plane cannot be raised above the primary",
            ),
        ] {
            if let Err(e) = self.set_client_capability(capability, true) {
                warn!(
                    "card{}: {capability:?} unavailable: {e} — {missing}",
                    self.index
                );
            }
        }
    }
}

/// One opened card, probed and ready to be registered. Probe data is used
/// for discovery/ordering only; the lease itself re-queries the card's
/// objects fresh at `create_lease` time.
struct OpenedCard {
    index: u8,
    card: DrmCard,
    /// Display connectors (writeback excluded) — a card with none is not
    /// display-capable and is not registered.
    connectors: Vec<connector::Handle>,
    any_connected: bool,
}

/// The server's DRM cards, keyed by their resource. The master fd inside
/// each device creates leases on demand; clients never see it.
pub type DrmRegistry = Arc<DashMap<Resource, DrmDevice>>;

/// A live kernel lease: the id for `revoke_lease`, and the fd that keeps
/// the lease alive while the server holds it (grants hand the client a dup
/// of this fd — the server's copy is what makes revoke deterministic).
#[derive(Debug)]
pub struct DrmLease {
    id: LeaseId,
    fd: OwnedFd,
}

/// Master held, leaseable.
#[derive(Debug)]
pub struct LockedDrmDevice {
    card: DrmCard,
    /// The client whose overlay of this card outlived the pipeline it was
    /// granted on. The plane went with the pipeline (its state was cleared
    /// before the lease was revoked); what survives is the client's right to
    /// have the next pipeline's plane, which is honoured in `grant_lease`.
    ///
    /// This is why an overlay does not have to ask again: it never lost its
    /// grant, only the plane under it.
    overlay_hold: Option<ClientId>,
}

/// The shape the app on the display wants the pointer to take.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CursorRequest {
    /// The client that asked — the lease owner at the time.
    pub client: ClientId,
    pub shape: CursorShape,
    pub visible: bool,
}

/// What a lease grant hands back: the client's fd, and the client (if any)
/// whose overlay claim was re-honoured on the new pipeline and must be told
/// about it with an ordinary Grant.
pub struct LeaseGrant {
    pub fd: OwnedFd,
    pub overlay_regranted: Option<ClientId>,
}

/// An overlay plane granted out of a pipeline: which plane, which CRTC and
/// connector it drives, and the client that owns it until it releases.
#[derive(Debug)]
struct OverlayGrant {
    plane: plane::Handle,
    crtc: crtc::Handle,
    connector: Option<connector::Handle>,
    owner: ClientId,
}

impl OverlayGrant {
    /// What the owning client is told it may draw on.
    fn detail(&self) -> DrmGrant {
        DrmGrant {
            plane: PlaneRole::Overlay,
            plane_id: u32::from(self.plane),
            crtc_id: u32::from(self.crtc),
            connector_id: self.connector.map(u32::from),
        }
    }
}

/// Why an overlay request could not be served. Every arm is a sentence the
/// client can act on, because that is what it is sent as.
#[derive(Debug, Error)]
pub enum OverlayError {
    /// Nobody holds the display, so there is no pipeline to put a plane on.
    #[error("no client holds the display (card {0}: acquire its primary plane first)")]
    NoPipeline(u8),
    /// Another client holds the overlay; it is released when that client is done.
    #[error("the overlay plane of card {0} is held by client {1}")]
    HeldByOther(u8, ClientId),
    /// Nowhere to put a plane: the pipeline has no usable overlay.
    #[error("card {0} has no overlay plane that can drive crtc {1}")]
    NoFreePlane(u8, u32),
    /// The card could not be queried or programmed.
    #[error("card {0}: {1}")]
    Io(u8, io::Error),
}

/// Master held + one live lease: its owner and any overlay grant of it.
#[derive(Debug)]
pub struct LeasedDrmDevice {
    card: DrmCard,
    lease: DrmLease,
    /// A client this card's overlay is promised to when the pipeline comes
    /// back. Set only for the moment between the new lease and the re-grant.
    overlay_hold: Option<ClientId>,
    /// The client the lease was created for — the app on the display. The
    /// lease dies with it, and every overlay grant dies with the lease.
    owner: ClientId,
    overlay: Option<OverlayGrant>,
}

impl LockedDrmDevice {
    /// Query the card's objects fresh and lease them all. Returns `Err` with
    /// the still-`Locked` device when probing or `create_lease` fails, so
    /// the caller can restore the state unchanged.
    pub fn create_lease(self, owner: ClientId) -> Result<LeasedDrmDevice, (Self, io::Error)> {
        let overlay_hold = self.overlay_hold;
        let card = self.card;

        let objects: Vec<RawResourceHandle> = match lease_objects(&card) {
            Ok(objects) => objects,
            Err(e) => return Err((Self { card, overlay_hold }, e)),
        };

        match card.create_lease(&objects, 0) {
            Ok((id, fd)) => {
                info!(
                    "Created lease {id} for client {owner} ({} objects)",
                    objects.len()
                );
                Ok(LeasedDrmDevice {
                    card,
                    lease: DrmLease { id, fd },
                    owner,
                    overlay_hold,
                    overlay: None,
                })
            }
            Err(e) => Err((Self { card, overlay_hold }, e)),
        }
    }
}

impl LeasedDrmDevice {
    /// A dup of the lease's fd, for the client being granted something of it.
    fn fd(&self) -> io::Result<OwnedFd> {
        self.lease.fd.try_clone()
    }

    /// Grant this pipeline's overlay plane to `owner`, or hand back the grant
    /// it already holds. Which plane: the highest one that can drive the
    /// pipeline's CRTC, so a driver that orders its planes overlay-first (or
    /// stacks several) gives up its topmost one first — the plane most likely
    /// to already sit above the primary. Idempotent, because a client asking
    /// again for what it holds must not be handed a second plane.
    fn grant_overlay(
        &mut self,
        owner: ClientId,
        cursor_plane: Option<plane::Handle>,
    ) -> Result<DrmGrant, OverlayError> {
        let index = self.card.index;

        // The policy owns who gets the plane; this function owns how a plane is
        // picked and programmed once the policy says yes.
        let held: Vec<OverlayGrantView> = self
            .overlay
            .as_ref()
            .map(|grant| OverlayGrantView {
                client: grant.owner,
                plane: u32::from(grant.plane),
                crtc: u32::from(grant.crtc),
            })
            .into_iter()
            .collect();
        // What the decision was made from, so a refusal can be read back
        // without guessing which plane the asker lost to.
        let claim = OverlayClaim {
            client: owner,
            card: index,
        };
        trace!(
            "card{}: client {owner} asks for the overlay; held: [{}]",
            claim.card,
            held.iter()
                .map(|grant| format!(
                    "client {} on plane {} (crtc {})",
                    grant.client, grant.plane, grant.crtc
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        match overlay_policy::NoPreemption.decide(&claim, &held) {
            OverlayDecision::Grant => {}
            OverlayDecision::Refuse { holder } => {
                return Err(OverlayError::HeldByOther(index, holder));
            }
        }

        // Allowed, and already this client's: the same grant, restated — not a
        // second plane picked for a client that holds one.
        if let Some(grant) = &self.overlay {
            return Ok(grant.detail());
        }

        let io_error = |e: io::Error| OverlayError::Io(index, e);
        let (crtc, connector) = live_pipeline(&self.card).ok_or(OverlayError::NoPipeline(index))?;
        // The cursor's plane is the controller's own: handing it to a client
        // would take the pointer off the screen.
        let plane = overlay_candidates(&self.card, crtc)
            .map_err(io_error)?
            .into_iter()
            .filter(|(plane, _)| Some(*plane) != cursor_plane)
            .max_by_key(|(_, zpos)| *zpos)
            .map(|(plane, _)| plane)
            .ok_or_else(|| OverlayError::NoFreePlane(index, u32::from(crtc)))?;

        match raise_plane_above_primary(&self.card, plane) {
            ZposOutcome::AlreadyAbove { overlay, primary } => info!(
                "card{index}: overlay plane {} is above the primary (zpos {overlay} vs \
                 {primary})",
                u32::from(plane)
            ),
            ZposOutcome::Raised { from, to, primary } => info!(
                "card{index}: overlay plane {} raised above the primary (zpos {from} -> {to}, \
                 primary {primary})",
                u32::from(plane)
            ),
            ZposOutcome::CannotRaise { reason } => warn!(
                "card{index}: overlay plane {} may be HIDDEN behind the primary: {reason}",
                u32::from(plane)
            ),
        }

        let grant = OverlayGrant {
            plane,
            crtc,
            connector,
            owner,
        };
        let detail = grant.detail();
        info!(
            "card{index}: granted overlay plane {} (crtc {}) to client {owner}",
            detail.plane_id, detail.crtc_id
        );
        self.overlay = Some(grant);
        Ok(detail)
    }

    /// Take the overlay grant back: free the plane and program it off the
    /// pipeline. Clearing is the point — the plane state holds its own
    /// reference to the client's framebuffer, so a client that is gone would
    /// leave its last frame scanning out of a pipeline nothing else touches,
    /// and only the master can program that plane now.
    ///
    /// Only the holder gets to do this. A Release from anybody else — a client
    /// that was refused the plane, or one that opened a connection and left —
    /// must not take a plane it never had, which is why the owner is checked
    /// before anything is cleared.
    fn release_overlay(&mut self, owner: ClientId) {
        let Some(grant) = &self.overlay else {
            return;
        };
        if grant.owner != owner {
            warn!(
                "card{}: client {owner} tried to release the overlay plane held by client {} \
                 — leaving it with its owner",
                self.card.index, grant.owner
            );
            return;
        }
        if let Some(plane) = self.take_overlay() {
            info!(
                "card{}: overlay plane {plane} released by client {owner}",
                self.card.index
            );
        }
    }

    /// Drop the overlay grant and program its plane off the pipeline.
    /// Returns the plane that was cleared, if there was one.
    ///
    /// Clearing is the point: the plane state holds its own reference to the
    /// client's framebuffer, so the last frame keeps scanning out until
    /// something programs that plane again — and once the grant is dropped
    /// (its owner gone), only the master can.
    /// The client that holds the granted plane, if one is granted.
    fn overlay_owner(&self) -> Option<ClientId> {
        self.overlay.as_ref().map(|grant| grant.owner)
    }

    fn take_overlay(&mut self) -> Option<u32> {
        let grant = self.overlay.take()?;
        clear_plane(&self.card, grant.plane, grant.crtc);
        Some(u32::from(grant.plane))
    }

    /// Kernel-revoke the lease. On success the lease is dead (even if the
    /// client keeps its fd open) and we drop our fd copy, returning to
    /// `Locked`. On failure return `Err(Self)` — id and fd are preserved,
    /// so the reclaim can be retried later; never lose the lease on an
    /// ioctl error.
    pub fn revoke_lease(self) -> Result<LockedDrmDevice, (Self, io::Error)> {
        let card = self.card;
        let lease = self.lease;

        match card.revoke_lease(lease.id) {
            Ok(()) => {
                drop(lease.fd);
                Ok(LockedDrmDevice {
                    card,
                    overlay_hold: None,
                })
            }
            Err(e) => Err((
                LeasedDrmDevice {
                    card,
                    lease: DrmLease {
                        id: lease.id,
                        fd: lease.fd,
                    },
                    owner: self.owner,
                    overlay: self.overlay,
                    overlay_hold: self.overlay_hold,
                },
                e,
            )),
        }
    }
}

/// The state machine: a card is either leaseable or carrying one live
/// lease. `DrmDevice` wraps it in a `Mutex<Option<_>>` (invariant: `Some`
/// outside a transition) so cross-task revokes and grants serialize on the
/// take/replace idiom.
#[derive(Debug)]
enum DrmDeviceState {
    Locked(LockedDrmDevice),
    Leased(LeasedDrmDevice),
}

/// A display-capable card the server owns as DRM master.
///
/// Clients never get the master fd. Each grant creates a fresh lease over
/// the card's objects and hands the client a dup of the lease fd; the lease
/// is revoked (kernel-enforced) when the resource is released or revoked,
/// so the server can reclaim the card at any time regardless of client
/// cooperation.
#[derive(Debug)]
pub struct DrmDevice {
    index: u8,
    state: Mutex<Option<DrmDeviceState>>,
    /// Where each client says its overlay is. Client-reported, so it is what
    /// the client *believes*, which is the part a stacking or focus policy
    /// needs: the plane's programmed rectangle only reflects the last commit
    /// and cannot say whether the client considers itself visible.
    ///
    /// Own mutex, never taken together with `state`: the record survives the
    /// pipeline it was made on, which is the whole point of it being here.
    overlay_geometries: Mutex<HashMap<ClientId, OverlayGeometry>>,
    /// What the app on the display last asked the cursor to look like.
    cursor_request: Mutex<Option<CursorRequest>>,
    /// The surface the controller draws the pointer on, once a pipeline exists.
    cursor_surface: Mutex<Option<CursorSurface>>,
    /// Where the pointer was last drawn. A shape can change while the pointer
    /// stands still (a drag starts, a button appears under it), and the tracker
    /// only commits on motion — so the redraw after a shape change needs the
    /// position from here, not from the reader.
    cursor_position: Mutex<Option<(i32, i32)>>,
}

impl DrmDevice {
    /// The pipeline's lease fd for `owner`, creating the lease when the card
    /// is free. The lease is shared, not per client: the same owner asking
    /// again gets another dup of it (a re-acquire, or the overlay role of the
    /// pipeline it already holds), and a *different* client is a handover —
    /// the old lease, and every overlay grant of it, is revoked first, because
    /// the kernel cannot lease the same objects twice.
    pub fn grant_lease(&self, owner: ClientId) -> io::Result<LeaseGrant> {
        let mut guard = self.state.lock().unwrap();
        let current = guard.take().expect("state always present");

        let mut leased = match current {
            DrmDeviceState::Leased(dev) if dev.owner == owner => dev,
            DrmDeviceState::Locked(dev) => match dev.create_lease(owner) {
                Ok(leased) => leased,
                Err((dev, e)) => {
                    error!("Failed to lease card{}: {e}", self.index);
                    *guard = Some(DrmDeviceState::Locked(dev));
                    return Err(e);
                }
            },
            DrmDeviceState::Leased(mut dev) => {
                // A handover is a pipeline change, not the overlay's business:
                // clear the plane of the old pipeline and keep the holder's
                // claim, so it is honoured on the new one below.
                let overlay_holder = dev.overlay_owner();
                if let Some(plane) = dev.take_overlay() {
                    info!(
                        "card{}: overlay plane {plane} cleared — the display changed hands",
                        self.index
                    );
                }
                match dev.revoke_lease() {
                    Ok(mut locked) => {
                        if let Some(holder) = overlay_holder {
                            info!(
                                "card{}: client {holder} keeps its overlay claim across the \
                                 handover — it is re-granted on the new pipeline",
                                self.index
                            );
                            locked.overlay_hold = Some(holder);
                        }
                        match locked.create_lease(owner) {
                            Ok(leased) => {
                                debug!("Revoked the previous lease on card{}", self.index);
                                leased
                            }
                            Err((dev, e)) => {
                                error!(
                                    "Failed to lease card{} for client {owner}: {e}",
                                    self.index
                                );
                                *guard = Some(DrmDeviceState::Locked(dev));
                                return Err(e);
                            }
                        }
                    }
                    Err((dev, e)) => {
                        error!("Failed to revoke stale lease on card{}: {e}", self.index);
                        *guard = Some(DrmDeviceState::Leased(dev));
                        return Err(e);
                    }
                }
            }
        };

        // A client whose overlay outlived the last pipeline gets the new
        // pipeline's plane now, under the same lock that made the lease: there
        // is no window in which another client could take it. It is told with
        // an ordinary Grant, so the client needs no special path for it.
        let overlay_regranted = match leased.overlay_hold.take() {
            Some(holder) => match leased.grant_overlay(holder, self.cursor_plane()) {
                Ok(grant) => {
                    info!(
                        "card{}: overlay plane {} (crtc {}) re-granted to client {holder} \
                         — the pipeline is back",
                        self.index, grant.plane_id, grant.crtc_id
                    );
                    Some(holder)
                }
                Err(e) => {
                    warn!(
                        "card{}: could not re-grant the overlay to client {holder} yet: {e} \
                         — it stays promised",
                        self.index
                    );
                    leased.overlay_hold = Some(holder);
                    None
                }
            },
            None => None,
        };

        // Hand the client a dup; the server keeps its own copy in the Leased
        // state (see DrmLease).
        match leased.fd() {
            Ok(fd) => {
                *guard = Some(DrmDeviceState::Leased(leased));
                Ok(LeaseGrant {
                    fd,
                    overlay_regranted,
                })
            }
            Err(e) => {
                error!("Failed to dup lease fd on card{}: {e}", self.index);
                // Lease is alive but unsendable; put it back Leased so a later
                // grant revokes and retries it.
                *guard = Some(DrmDeviceState::Leased(leased));
                Err(e)
            }
        }
    }

    /// Grant this card's overlay plane to `owner` and say what to draw on. The
    /// client programs it through the pipeline's lease, so no fd comes back —
    /// [`Self::lease_fd`] hands out that dup. A card nobody holds has no
    /// pipeline, and no pipeline has no planes to put on screen.
    pub fn grant_overlay(&self, owner: ClientId) -> Result<DrmGrant, OverlayError> {
        // Read before the state lock: the order is state, then cursor surface.
        let cursor_plane = self.cursor_plane();
        let mut guard = self.state.lock().unwrap();
        let current = guard.take().expect("state always present");

        let (next, result) = match current {
            DrmDeviceState::Locked(dev) => (
                DrmDeviceState::Locked(dev),
                Err(OverlayError::NoPipeline(self.index)),
            ),
            DrmDeviceState::Leased(mut dev) => {
                let result = dev.grant_overlay(owner, cursor_plane);
                (DrmDeviceState::Leased(dev), result)
            }
        };
        *guard = Some(next);
        result
    }

    /// The pipeline's owner is gone (its connection ended, or it was killed):
    /// revoke the lease, and every overlay grant of it with it. The granted
    /// plane is cleared first — the state that records it is what is about to
    /// go, and a client that is gone cannot clear its own plane.
    ///
    /// Without this a lease would outlive the app it was made for: the CRTC
    /// keeps its mode, so the pipeline still looks alive, and the next client
    /// asking for an overlay would be granted a plane of a display nobody is
    /// driving.
    pub fn revoke_lease_if_owned_by(&self, owner: ClientId) {
        let mut guard = self.state.lock().unwrap();
        let current = guard.take().expect("state always present");

        *guard = Some(match current {
            DrmDeviceState::Leased(mut dev) if dev.owner == owner => {
                let overlay_holder = dev.overlay_owner();
                if let Some(plane) = dev.take_overlay() {
                    info!(
                        "card{}: overlay plane {plane} cleared — the display owner is gone",
                        self.index
                    );
                }
                match dev.revoke_lease() {
                    Ok(mut locked) => {
                        info!(
                            "card{}: lease revoked — client {owner} held the display and is gone",
                            self.index
                        );
                        if let Some(holder) = overlay_holder {
                            // The overlay's claim survives the pipeline: the
                            // plane went with it, the promise did not.
                            info!(
                                "card{}: client {holder} keeps its overlay claim across the \
                                 pipeline change — it is re-granted when a display is back",
                                self.index
                            );
                            locked.overlay_hold = Some(holder);
                        }
                        DrmDeviceState::Locked(locked)
                    }
                    Err((dev, e)) => {
                        error!("Failed to revoke the lease of card{}: {e}", self.index);
                        DrmDeviceState::Leased(dev)
                    }
                }
            }
            other => other,
        });
    }

    /// Remember the shape the app on the display asked for. Only the client
    /// holding the lease may set it: a background window must not change the
    /// pointer the user is looking at.
    ///
    /// Returns whether the request was accepted, so the caller can log a
    /// refusal instead of pretending the shape changed.
    pub fn record_cursor_shape(&self, client: ClientId, shape: CursorShape, visible: bool) -> bool {
        if !self.is_leased_by(client) {
            return false;
        }
        *self.cursor_request.lock().unwrap() = Some(CursorRequest {
            client,
            shape,
            visible,
        });
        true
    }

    /// Whether `client` holds this card's pipeline lease.
    pub fn is_leased_by(&self, client: ClientId) -> bool {
        let guard = self.state.lock().unwrap();
        matches!(
            guard.as_ref().expect("state always present"),
            DrmDeviceState::Leased(dev) if dev.owner == client
        )
    }

    /// The last cursor request accepted for this card, if any.
    pub(crate) fn cursor_request(&self) -> Option<CursorRequest> {
        *self.cursor_request.lock().unwrap()
    }

    /// The plane the cursor is drawn on, if it is drawn at all.
    fn cursor_plane(&self) -> Option<plane::Handle> {
        self.cursor_surface
            .lock()
            .unwrap()
            .as_ref()
            .map(CursorSurface::plane)
    }

    /// What the display owner asked the cursor to look like, if it asked.
    pub fn cursor_shape(&self) -> Option<CursorRequest> {
        *self.cursor_request.lock().unwrap()
    }

    /// The mode of the pipeline this card is driving, if it is driving one.
    /// The pointer is clamped to it: the kernel refuses a cursor at the edge,
    /// so a stale mode is a cursor that vanishes in a corner.
    pub fn pipeline_mode(&self) -> Option<(u32, u32)> {
        let guard = self.state.lock().unwrap();
        let DrmDeviceState::Leased(dev) = guard.as_ref().expect("state always present") else {
            tracing::debug!("card{}: not leased — no cursor", self.index);
            return None;
        };
        let Some((crtc, _)) = live_pipeline(&dev.card) else {
            tracing::debug!("card{}: no CRTC reports a mode", self.index);
            return None;
        };
        let info = match dev.card.get_crtc(crtc) {
            Ok(info) => info,
            Err(e) => {
                tracing::debug!("card{}: get_crtc {crtc:?}: {e}", self.index);
                return None;
            }
        };
        let Some(mode) = info.mode() else {
            tracing::debug!("card{}: crtc {crtc:?} reports no mode", self.index);
            return None;
        };
        Some((mode.size().0 as u32, mode.size().1 as u32))
    }

    /// Draw the pointer at `position` with `shape`, on this card's pipeline.
    ///
    /// No pipeline means no cursor: there is nothing on screen to point at, and
    /// a plane commit on a CRTC with no mode would be refused anyway.
    /// Draw the pointer at `position` with `shape`, on this card's pipeline.
    ///
    /// `mode` is passed in because the caller already knows it: the steady-state
    /// path must be one `set_plane` and nothing else. Re-asking the pipeline for
    /// its CRTC and mode costs three ioctls, and at pointer rates that is enough
    /// competing traffic with the app's own commits to make the cursor stutter.
    pub fn show_cursor(&self, shape: CursorShape, position: (i32, i32), mode: (u32, u32)) {
        // Lock order: the card state, then the cursor surface. Never the other
        // way round.
        let state = self.state.lock().unwrap();
        let DrmDeviceState::Leased(dev) = state.as_ref().expect("state always present") else {
            return;
        };

        let mut guard = self.cursor_surface.lock().unwrap();
        let first = guard.is_none();
        if first {
            // Picking a plane is the one thing that needs the pipeline, and it
            // happens once per pipeline, not once per move.
            let Some((crtc, _)) = live_pipeline(&dev.card) else {
                return;
            };
            // The CRTC's own cursor plane first: it is the server's alone
            // (`lease_objects` keeps every cursor plane out of every lease) and
            // sits above the primary by construction, so it costs a client no
            // plane and needs no zpos. A driver with none falls back to an
            // overlay no client has been promised, which then has to be raised.
            let (plane, raised) = match cursor_plane(&dev.card, crtc) {
                Some(plane) => (Some(plane), false),
                None => {
                    let taken = dev.overlay.as_ref().map(|grant| grant.plane);
                    let plane = overlay_candidates(&dev.card, crtc)
                        .ok()
                        .and_then(|candidates| {
                            candidates
                                .into_iter()
                                .filter(|(plane, _)| Some(*plane) != taken)
                                .max_by_key(|(_, zpos)| *zpos)
                                .map(|(plane, _)| plane)
                        });
                    (plane, true)
                }
            };
            let Some(plane) = plane else {
                warn!(
                    "card{}: no plane left for the cursor (no cursor plane, every overlay granted)",
                    self.index
                );
                return;
            };
            if raised
                && let ZposOutcome::CannotRaise { reason } =
                    raise_plane_above_primary(&dev.card, plane)
            {
                warn!(
                    "card{}: cursor plane {} may be HIDDEN behind the primary: {reason}",
                    self.index,
                    u32::from(plane),
                );
            }
            info!(
                "card{}: cursor bound to {} plane {} of crtc {}",
                self.index,
                if raised { "an overlay" } else { "the cursor" },
                u32::from(plane),
                u32::from(crtc)
            );
            *guard = Some(CursorSurface::new(plane, crtc));
        }
        let surface = guard.as_mut().expect("created above");
        let drawn = surface.show_at(&dev.card, shape, position, mode);
        if drawn.is_ok() {
            *self.cursor_position.lock().unwrap() = Some(position);
        }
        match drawn {
            Ok(()) if first => info!(
                "card{}: drawing the cursor at {position:?} (shape {shape:?}, mode {mode:?})",
                self.index
            ),
            Ok(()) => debug!("card{}: cursor moved to {position:?}", self.index),
            Err(e) => {
                // The pipeline moved under the surface (a new CRTC, or a new
                // pipeline): forget it, so the next draw picks a plane again
                // rather than committing into a dead one forever.
                *guard = None;
                warn!(
                    "card{}: could not draw the cursor at {position:?}: {e}",
                    self.index
                );
            }
        }
    }

    /// Redraw the pointer at the position it was last drawn at.
    ///
    /// A shape can change with the pointer standing still — a drag starts, a
    /// control appears under it — and the tracker commits only on motion, so
    /// without this the shape would stay stale until the user moved.
    pub fn redraw_cursor(&self) {
        let Some(position) = *self.cursor_position.lock().unwrap() else {
            return;
        };
        // Rare path (a shape change, not a move), so asking the pipeline for the
        // mode here costs nothing.
        let Some(mode) = self.pipeline_mode() else {
            return;
        };
        match self.cursor_shape() {
            Some(request) if !request.visible => self.hide_cursor(),
            Some(request) => self.show_cursor(request.shape, position, mode),
            None => self.show_cursor(CursorShape::Default, position, mode),
        }
    }

    /// Take the pointer off screen (no pipeline, or the app hid it).
    pub fn hide_cursor(&self) {
        let state = self.state.lock().unwrap();
        let card = match state.as_ref().expect("state always present") {
            DrmDeviceState::Leased(dev) => &dev.card,
            DrmDeviceState::Locked(dev) => &dev.card,
        };
        let mut guard = self.cursor_surface.lock().unwrap();
        if let Some(surface) = guard.as_mut()
            && let Err(e) = surface.hide(card)
        {
            warn!("card{}: could not hide the cursor: {e}", self.index);
        }
    }

    /// Record where a client says its overlay is, and say whether the plane
    /// agrees. A disagreement is not an error on its own — a client reports the
    /// geometry it asked for, and a commit may not have landed yet — but it is
    /// exactly what one wants to see when a notification ends up somewhere
    /// nobody expected.
    pub fn record_overlay_geometry(
        &self,
        client: ClientId,
        resource: &Resource,
        geometry: OverlayGeometry,
    ) {
        self.overlay_geometries
            .lock()
            .unwrap()
            .insert(client, geometry);

        if geometry.is_hidden() {
            info!(
                "card{}: client {client} reports its overlay hidden (0,0,0,0) — it keeps its \
                 grant",
                self.index
            );
        } else {
            info!(
                "card{}: client {client} reports its overlay at {},{} {}x{}",
                self.index, geometry.x, geometry.y, geometry.width, geometry.height
            );
        }
        // The kernel's own rectangle is not readable through the legacy plane
        // interface (drm's `plane::Info` carries no rect), so the record is
        // checked against the plane state on the board — `crtc-pos` in the
        // kernel's state dump — rather than guessed at here.
        let _ = resource;
    }

    /// Give the overlay plane back: the client owns it until it does, and it
    /// is free for another client afterwards. The lease itself is untouched —
    /// this client only held a plane of it.
    pub fn release_overlay(&self, owner: ClientId) {
        self.overlay_geometries.lock().unwrap().remove(&owner);
        let mut guard = self.state.lock().unwrap();
        let current = guard.take().expect("state always present");

        *guard = Some(match current {
            DrmDeviceState::Leased(mut dev) => {
                dev.release_overlay(owner);
                if dev.overlay_hold == Some(owner) {
                    dev.overlay_hold = None;
                }
                DrmDeviceState::Leased(dev)
            }
            DrmDeviceState::Locked(mut dev) => {
                if dev.overlay_hold == Some(owner) {
                    info!(
                        "card{}: client {owner} gave up its overlay claim while no display is \
                         driven",
                        self.index
                    );
                    dev.overlay_hold = None;
                }
                DrmDeviceState::Locked(dev)
            }
        });
    }

    /// A dup of the live lease's fd, for a client that holds something of the
    /// pipeline without being its owner. `Err` when the card has no lease:
    /// [`Self::grant_overlay`] is what decides whether a client may have one.
    pub fn lease_fd(&self) -> io::Result<OwnedFd> {
        let guard = self.state.lock().unwrap();
        match guard.as_ref().expect("state always present") {
            DrmDeviceState::Leased(dev) => dev.fd(),
            DrmDeviceState::Locked(_) => Err(io::Error::other("card has no live lease")),
        }
    }

    /// Revoke the granted lease right now, without waiting for the client
    /// to close its fd. A no-op when Locked; a failed revoke keeps the
    /// `Leased` state (id + fd preserved) so the next reclaim can retry.
    pub fn revoke_lease(&self) {
        let mut guard = self.state.lock().unwrap();
        let current = guard.take().expect("state always present");

        *guard = Some(match current {
            DrmDeviceState::Leased(dev) => match dev.revoke_lease() {
                Ok(dev) => {
                    info!("Revoked lease on card{}", self.index);
                    DrmDeviceState::Locked(dev)
                }
                Err((dev, e)) => {
                    error!("Failed to revoke lease on card{}: {e}", self.index);
                    DrmDeviceState::Leased(dev)
                }
            },
            state => state,
        });
    }
}

/// The pipeline this card is driving: the CRTC that has a mode set, and the
/// connector its encoder drives when the kernel reports one. `None` while
/// nothing is programmed — a client acquires the display first and sets the
/// mode, so an overlay request that arrives before that has no pipeline to
/// join.
fn live_pipeline(card: &DrmCard) -> Option<(crtc::Handle, Option<connector::Handle>)> {
    let handles = card.resource_handles().ok()?;
    let crtc = handles.crtcs.iter().copied().find(|handle| {
        card.get_crtc(*handle)
            .is_ok_and(|info| info.mode().is_some())
    })?;

    let connector = handles.connectors.iter().copied().find(|handle| {
        card.get_connector(*handle, false)
            .ok()
            .and_then(|info| info.current_encoder())
            .and_then(|encoder| card.get_encoder(encoder).ok())
            .and_then(|info| info.crtc())
            .is_some_and(|driven| driven == crtc)
    });

    Some((crtc, connector))
}

/// The card's overlay planes that can drive `crtc`, paired with their current
/// z-position so the caller can take the topmost.
fn overlay_candidates(card: &DrmCard, crtc: crtc::Handle) -> io::Result<Vec<(plane::Handle, u64)>> {
    let handles = card.resource_handles()?;
    let mut candidates = Vec::new();

    for plane in card.plane_handles()? {
        if plane_kind(card, plane) != Some(PlaneKind::Overlay) {
            continue;
        }
        let info = card.get_plane(plane)?;
        if !handles.filter_crtcs(info.possible_crtcs()).contains(&crtc) {
            continue;
        }
        let zpos = zpos(card, plane).map_or(0, |(_, value, _)| value);
        candidates.push((plane, zpos));
    }

    Ok(candidates)
}

/// The CRTC's own cursor plane, if the driver has one.
///
/// vc4 does — four of them, one per CRTC — and `lease_objects` keeps every
/// cursor plane out of every lease, so this plane is always free for the
/// pointer. Using it rather than an overlay costs a client nothing and needs no
/// zpos: a cursor plane is above the primary by construction.
fn cursor_plane(card: &DrmCard, crtc: crtc::Handle) -> Option<plane::Handle> {
    let handles = card.resource_handles().ok()?;
    card.plane_handles().ok()?.into_iter().find(|plane| {
        plane_kind(card, *plane) == Some(PlaneKind::Cursor)
            && card
                .get_plane(*plane)
                .is_ok_and(|info| handles.filter_crtcs(info.possible_crtcs()).contains(&crtc))
    })
}

/// The card's primary plane, by its `type` property: never "the first plane
/// that can drive the CRTC", because a driver may order its pipeline
/// overlay-first (sun4i does).
fn primary_plane(card: &DrmCard) -> io::Result<Option<plane::Handle>> {
    Ok(card
        .plane_handles()?
        .into_iter()
        .find(|plane| plane_kind(card, *plane) == Some(PlaneKind::Primary)))
}

/// A named property of an object: its handle and its raw value.
fn find_property<H: drm::control::ResourceHandle>(
    card: &DrmCard,
    object: H,
    name: &str,
) -> Option<(property::Handle, property::RawValue)> {
    for (handle, raw) in card.get_properties(object).ok()? {
        let info = card.get_property(handle).ok()?;
        if info.name().to_bytes() == name.as_bytes() {
            return Some((handle, raw));
        }
    }
    None
}

/// A plane's `zpos`: its property handle, current value, and the range the
/// driver accepts. A driver that declares zpos immutable reports a range of
/// one value — the range is how you tell, not the property's flags.
fn zpos(card: &DrmCard, plane: plane::Handle) -> Option<(property::Handle, u64, (u64, u64))> {
    let (handle, raw) = find_property(card, plane, "zpos")?;
    let range = match card.get_property(handle).ok()?.value_type() {
        property::ValueType::UnsignedRange(lo, hi) => (*lo, *hi),
        _ => (raw, raw),
    };
    Some((handle, raw, range))
}

/// What has to move for a granted overlay to sit above the primary.
#[derive(Debug, PartialEq, Eq)]
struct StackingPlan {
    /// The overlay's new z-position.
    overlay: u64,
    /// The primary's new z-position, when it has to move down to make room.
    primary: Option<u64>,
}

/// Where the two planes have to move, or `None` when they need not — and
/// `None` must mean "name neither of them in a commit": a plane whose zpos is
/// immutable rejects any request that mentions it, even with the value it
/// already holds, which fails the whole commit and leaves the grant where it
/// was.
///
/// `overlay` and `primary` are each `(current value, accepted range)`.
fn stacking_plan(overlay: (u64, (u64, u64)), primary: (u64, (u64, u64))) -> Option<StackingPlan> {
    let (overlay_zpos, overlay_range) = overlay;
    let (primary_zpos, primary_range) = primary;

    // Already above: touching either plane would only risk the commit.
    if overlay_zpos > primary_zpos {
        return None;
    }
    // Room above the primary inside the overlay's own range.
    if overlay_range.1 > primary_zpos {
        return Some(StackingPlan {
            overlay: primary_zpos + 1,
            primary: None,
        });
    }
    // The overlay cannot reach any higher: move the primary down one instead,
    // which needs the primary's own range to allow it.
    let primary_can_move = primary_range.0 != primary_range.1 && primary_zpos > primary_range.0;
    if primary_can_move {
        return Some(StackingPlan {
            overlay: overlay_range.1,
            primary: Some(primary_zpos - 1),
        });
    }
    None
}

/// Program the z-order so `granted` sits above the primary. Best effort: a
/// failure only means the plane may be occluded, and the grant is worth having
/// either way.
/// What happened when the daemon tried to put a granted overlay above the
/// primary plane.
///
/// The client cannot see z-order: it draws into a plane and the compositor
/// decides what is on top. So this outcome is the only place the truth exists,
/// and silence is how an overlay ends up granted, drawing, and invisible behind
/// the app on the display — which is the failure mode on boards that stack
/// overlays below the primary (Qualcomm among them).
#[derive(Debug)]
pub(super) enum ZposOutcome {
    /// Already above: nothing was committed, because touching either plane
    /// would only risk a modeset for no visible change.
    AlreadyAbove { overlay: u64, primary: u64 },
    /// A commit was made and the kernel now holds `to` — read back after the
    /// commit, not assumed from the request.
    Raised { from: u64, to: u64, primary: u64 },
    /// The plane cannot be put above the primary here, and why.
    CannotRaise { reason: String },
}

fn raise_plane_above_primary(card: &DrmCard, granted: plane::Handle) -> ZposOutcome {
    let Some(primary) = primary_plane(card).ok().flatten() else {
        return ZposOutcome::CannotRaise {
            reason: "the card reports no primary plane".to_string(),
        };
    };
    let Some((overlay_prop, overlay_zpos, overlay_range)) = zpos(card, granted) else {
        return ZposOutcome::CannotRaise {
            reason: "the driver exposes no zpos property".to_string(),
        };
    };
    let Some((primary_prop, primary_zpos, primary_range)) = zpos(card, primary) else {
        return ZposOutcome::CannotRaise {
            reason: "the primary plane has no zpos property".to_string(),
        };
    };

    if overlay_zpos > primary_zpos {
        return ZposOutcome::AlreadyAbove {
            overlay: overlay_zpos,
            primary: primary_zpos,
        };
    }

    let Some(plan) = stacking_plan((overlay_zpos, overlay_range), (primary_zpos, primary_range))
    else {
        return ZposOutcome::CannotRaise {
            reason: format!(
                "no z-order above the primary is available (overlay zpos {overlay_zpos} in \
                 {}..={}, primary zpos {primary_zpos} in {}..={})",
                overlay_range.0, overlay_range.1, primary_range.0, primary_range.1
            ),
        };
    };

    let mut request = AtomicModeReq::new();
    if let Some(value) = plan.primary {
        request.add_property(primary, primary_prop, property::Value::UnsignedRange(value));
    }
    request.add_property(
        granted,
        overlay_prop,
        property::Value::UnsignedRange(plan.overlay),
    );

    // ALLOW_MODESET because a z-order change is a modeset-shaped change on some
    // drivers; sun4i accepts it without, and vc4 does not offer it at all
    // (immutable zpos), which the plan above already accounts for.
    if let Err(e) = card.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, request) {
        return ZposOutcome::CannotRaise {
            reason: format!("the z-order commit failed: {e}"),
        };
    }

    // Read it back: the log has to state what the kernel holds, not what was
    // asked for. A driver that accepts the commit and keeps its own order is
    // exactly the case that hides an overlay.
    match zpos(card, granted).map(|(_, value, _)| value) {
        Some(after) if after > primary_zpos => ZposOutcome::Raised {
            from: overlay_zpos,
            to: after,
            primary: primary_zpos,
        },
        Some(after) => ZposOutcome::CannotRaise {
            reason: format!(
                "the commit landed but the kernel still reports zpos {after} against the \
                 primary's {primary_zpos}"
            ),
        },
        None => ZposOutcome::CannotRaise {
            reason: "the zpos property disappeared after the commit".to_string(),
        },
    }
}

/// Program a plane off the pipeline. A dropped grant is not a cleared plane:
/// the plane state holds its own reference to the framebuffer, so the last
/// frame keeps scanning out until something programs that plane again — and
/// once the grant holder is gone, only the master can.
fn clear_plane(card: &DrmCard, plane: plane::Handle, crtc: crtc::Handle) {
    if let Err(e) = card.set_plane(plane, crtc, None, 0, (0, 0, 0, 0), (0, 0, 0, 0)) {
        warn!(
            "card{}: failed to clear plane {}: {e}",
            card.index,
            u32::from(plane)
        );
    }
}

/// The card's leaseable objects as `RawResourceHandle`s, queried fresh: every
/// CRTC, every display connector (writeback connectors are capture-only and
/// cannot present a display) and every plane the clients may use.
///
/// # What the lease deliberately leaves out
///
/// Cursor planes. A lease is the only thing that can keep a client away from a
/// plane, so the server's pointer plane is kept out of it: on a card that has
/// cursor planes (vc4, for one) a client holding the lease could otherwise
/// reprogram the server's pointer. A driver that reports no plane type, or a
/// plane whose type cannot be read, stays in the lease — the status quo is
/// "your own planes", and guessing wrong would silently take planes away.
///
/// The same mechanism is how the server reserves any plane for itself: leave
/// it out of the object list.
fn lease_objects(card: &DrmCard) -> io::Result<Vec<RawResourceHandle>> {
    let handles = card.resource_handles()?;
    let planes = card.plane_handles()?;

    let connectors = display_connectors(card, &handles);

    let mut objects: Vec<RawResourceHandle> = Vec::new();
    objects.extend(handles.crtcs.iter().copied().map(RawResourceHandle::from));
    objects.extend(connectors.iter().copied().map(RawResourceHandle::from));
    for plane in planes {
        match plane_kind(card, plane) {
            Some(PlaneKind::Cursor) => {
                debug!(
                    "card{}: keeping cursor plane {} with the server",
                    card.index,
                    u32::from(plane)
                );
            }
            _ => objects.push(RawResourceHandle::from(plane)),
        }
    }
    objects.shrink_to_fit();
    Ok(objects)
}

/// The kinds of plane the server distinguishes, from the plane's immutable
/// `type` property. `None` when the driver reports no type (or reading it
/// failed): an unknown plane is treated as usable rather than withheld.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaneKind {
    Primary,
    Overlay,
    Cursor,
}

fn plane_kind(card: &DrmCard, plane: plane::Handle) -> Option<PlaneKind> {
    let properties = card.get_properties(plane).ok()?;
    for (handle, raw) in properties {
        let info = card.get_property(handle).ok()?;
        if info.name().to_bytes() != b"type" {
            continue;
        }
        let property::ValueType::Enum(values) = info.value_type() else {
            return None;
        };
        let name = values
            .get_value_from_raw_value(raw)?
            .name()
            .to_bytes()
            .to_vec();
        return match name.as_slice() {
            b"Primary" => Some(PlaneKind::Primary),
            b"Overlay" => Some(PlaneKind::Overlay),
            b"Cursor" => Some(PlaneKind::Cursor),
            _ => None,
        };
    }
    None
}

/// Open every DRM card that can present a display and register it as
/// `Resource::drm(card)` — each physical `/dev/dri/cardN` becomes one
/// resource, registered in priority order (first is best). Render nodes
/// (`renderDNN`) are skipped — they cannot modeset.
///
/// The server opens each card as DRM master and KEEPS the master fd for
/// the server's lifetime. It creates no lease at startup: a fresh lease is
/// created per grant (see [`DrmDevice::grant_lease`]) and revoked when the
/// client releases the resource, so the server can reclaim the card at any
/// time — enforced by the kernel, no client cooperation needed.
use super::Inventory;

pub(super) fn open_devices(inventory: &Inventory, advertised: &mut Vec<Resource>) {
    let entries = match read_dir("/dev/dri/") {
        Ok(entries) => entries,
        Err(e) => {
            error!("Failed to read /dev/dri: {e}");
            return;
        }
    };

    let mut indices = entries
        .filter_map(Result::ok)
        .filter_map(|entry| card_index(&entry.path()))
        .collect::<Vec<u8>>();
    indices.sort_unstable();

    // Open and probe every card node (master + object handles), then order
    // the display-capable ones by priority. Probing needs the open fd, so
    // the ordering happens after all opens, not while listing.
    let mut opened = Vec::new();
    for index in indices {
        let path = PathBuf::from(format!("/dev/dri/card{index}"));

        let card = match DrmCard::open(&path, index) {
            Ok(card) => card,
            Err(e) => {
                error!("Failed to open {}: {e}", path.display());
                continue;
            }
        };

        // The kernel hands master to the first O_RDWR opener, but only an
        // explicit acquire fails loudly when another process (e.g. a real
        // display server) already holds it — do not fight it, skip.
        if let Err(e) = card.acquire_master_lock() {
            error!("Failed to acquire DRM master on {}: {e}", path.display());
            continue;
        }

        let handles = match card.resource_handles() {
            Ok(handles) => handles,
            Err(e) => {
                error!("Failed to query resources on {}: {e}", path.display());
                continue;
            }
        };

        let connectors = display_connectors(&card, &handles);
        if !connectors.is_empty() {
            // Only a card that can present a display needs planes to program:
            // warning about a render node (v3d, panfrost) would be noise about
            // a card the server is about to drop.
            card.enable_plane_capabilities();
        }
        let any_connected = connectors.iter().any(|handle| {
            card.get_connector(*handle, false)
                .is_ok_and(|info| info.state() == connector::State::Connected)
        });

        opened.push(OpenedCard {
            index,
            card,
            connectors,
            any_connected,
        });
    }

    // Priority order: a card with a connected display first, then the
    // lowest index. Cards without a display connector are not
    // display-capable and drop out below.
    opened.sort_by(|a, b| {
        b.any_connected
            .cmp(&a.any_connected)
            .then(a.index.cmp(&b.index))
    });

    for opened in opened {
        let path = format!("/dev/dri/card{}", opened.index);
        if opened.connectors.is_empty() {
            debug!("Skipping {path}: no display connectors");
            continue;
        }

        let resource = Resource::drm(opened.index);
        let fd = opened.card.as_raw_fd();
        info!("Opened {path} ({resource:?}) (master fd {fd})");
        inventory.drm.insert(
            resource.clone(),
            DrmDevice {
                index: opened.index,
                state: Mutex::new(Some(DrmDeviceState::Locked(LockedDrmDevice {
                    overlay_hold: None,
                    card: opened.card,
                }))),
                overlay_geometries: Mutex::new(HashMap::new()),
                cursor_request: Mutex::new(None),
                cursor_surface: Mutex::new(None),
                cursor_position: Mutex::new(None),
            },
        );
        advertised.push(resource);
    }
}

/// The card's display connectors (writeback connectors are capture-only
/// and cannot present a display). A connector whose probe failed is kept
/// rather than silently dropped.
fn display_connectors(card: &DrmCard, handles: &ResourceHandles) -> Vec<connector::Handle> {
    handles
        .connectors
        .iter()
        .copied()
        .filter(|handle| match card.get_connector(*handle, false) {
            Ok(info) => info.interface() != connector::Interface::Writeback,
            Err(_) => true,
        })
        .collect()
}

/// Parse the card index out of a `/dev/dri/cardN` path (`card0` -> `0`).
fn card_index(path: &Path) -> Option<u8> {
    let name = path.file_name()?.to_str()?;
    name.strip_prefix("card")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::StackingPlan;
    use super::stacking_plan;

    /// A driver that declares zpos mutable over the whole stack (sun4i: 0..2
    /// over primary + two overlays).
    const MUTABLE: (u64, u64) = (0, 2);
    /// vc4: the primary's zpos is immutable, the overlays carry fixed values.
    const IMMUTABLE: (u64, u64) = (0, 0);

    #[test]
    fn an_overlay_already_above_the_primary_is_not_touched() {
        // Naming either plane in a commit can fail the commit on a driver whose
        // zpos is immutable, so "nothing to do" has to mean "name nothing".
        assert_eq!(stacking_plan((2, MUTABLE), (1, MUTABLE)), None);
    }

    #[test]
    fn an_overlay_below_the_primary_moves_just_above_it() {
        // The overlay has room: it moves, the primary stays out of the commit.
        assert_eq!(
            stacking_plan((0, MUTABLE), (1, MUTABLE)),
            Some(StackingPlan {
                overlay: 2,
                primary: None
            })
        );
    }

    #[test]
    fn a_primary_pinned_at_the_top_makes_room_by_moving_down() {
        // Overlay and primary both at the top of the range: the overlay cannot
        // climb, so the primary steps down one.
        assert_eq!(
            stacking_plan((1, MUTABLE), (2, MUTABLE)),
            Some(StackingPlan {
                overlay: 2,
                primary: Some(1)
            })
        );
    }

    #[test]
    fn an_immutable_zpos_is_never_programmed() {
        assert_eq!(stacking_plan((0, IMMUTABLE), (0, IMMUTABLE)), None);
    }

    #[test]
    fn a_primary_that_cannot_move_leaves_the_overlay_alone() {
        // The overlay's range tops out at the primary's position, and the
        // primary's zpos is immutable: nothing can be programmed.
        assert_eq!(stacking_plan((1, (1, 1)), (1, IMMUTABLE)), None);
    }

    #[test]
    fn a_drivers_default_stack_needs_no_commit() {
        // The H618's default: overlay 0, primary 1, overlay 2 — the granted
        // plane is picked by z-position, so the top overlay needs no commit at
        // all and the bottom one moves to just above the primary.
        assert_eq!(stacking_plan((2, MUTABLE), (1, MUTABLE)), None);
        assert_eq!(
            stacking_plan((0, MUTABLE), (1, MUTABLE)),
            Some(StackingPlan {
                overlay: 2,
                primary: None
            })
        );
    }
}
