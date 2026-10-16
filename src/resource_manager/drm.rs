//! DRM backend: open display-capable cards (`/dev/dri/cardN`) as DRM master
//! and register each as `Resource::drm(card)` — the card's PRIMARY plane, the
//! one an app on the display draws through. The overlay role of the same card
//! is part of the protocol and refused here for now (see
//! [`Resource::drm_overlay`] and `docs/resource-manager.md`).
//!
//! Compiled only with the `drm` feature (default). The master fd never
//! leaves the server: every grant creates a FRESH lease over the card's
//! objects (see [`DrmDevice::grant_lease`]) and revocation is
//! kernel-enforced. Registration order IS the advertised priority order
//! (first is best): the card selection logic lives here with the open.
//!
//! A card's life is a type-state machine — [`DrmDeviceState::Locked`]
//! (master held, leaseable) or [`DrmDeviceState::Leased`] (master + one
//! live lease). Transitions consume the state and return `Result<Next,
//! Self>`: an ioctl failure hands back the state you were in, so a card can
//! never be lost or double-leased — the failure path is a retry, not a
//! leak. The state lives behind `Mutex<Option<...>>` because the engine can
//! push a Revoke for one task while another task runs a grant on the same
//! card (see docs/resource-manager.md).

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
use drm::control::{
    Device as ControlDevice, LeaseId, RawResourceHandle, ResourceHandles, connector,
};
use nix::fcntl::OFlag;
use simple_graphics_protocol::Resource;
use tracing::{debug, error, info};

/// The server's DRM master handle for one card: a plain fd wrapper that
/// implements the `drm` crate's `Device` traits (the crate deliberately
/// does not open device nodes for you).
///
/// The master is what creates leases. It is held by the server for the
/// whole run: closing it would destroy every lease it created.
#[derive(Debug)]
pub struct DrmCard(File);

impl AsFd for DrmCard {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for DrmCard {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl drm::Device for DrmCard {}
impl drm::control::Device for DrmCard {}

impl DrmCard {
    /// Open a DRM card node the way a display server should: `O_RDWR` for
    /// modesetting, `O_CLOEXEC` so the fd never leaks into children, and
    /// `O_NONBLOCK` so event reads on the fd can never block.
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .custom_flags((OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).bits())
            .open(path)?;
        Ok(Self(file))
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
}

/// Master held + one live lease.
#[derive(Debug)]
pub struct LeasedDrmDevice {
    card: DrmCard,
    lease: DrmLease,
}

impl LockedDrmDevice {
    /// Query the card's objects fresh and lease them all. Returns `Err` with
    /// the still-`Locked` device when probing or `create_lease` fails, so
    /// the caller can restore the state unchanged.
    pub fn create_lease(self) -> Result<LeasedDrmDevice, (Self, io::Error)> {
        let card = self.card;

        let objects: Vec<RawResourceHandle> = match lease_objects(&card) {
            Ok(objects) => objects,
            Err(e) => return Err((Self { card }, e)),
        };

        match card.create_lease(&objects, 0) {
            Ok((id, fd)) => {
                info!("Created lease {id} ({} objects)", objects.len());
                Ok(LeasedDrmDevice {
                    card,
                    lease: DrmLease { id, fd },
                })
            }
            Err(e) => Err((Self { card }, e)),
        }
    }
}

impl LeasedDrmDevice {
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
                Ok(LockedDrmDevice { card })
            }
            Err(e) => Err((
                LeasedDrmDevice {
                    card,
                    lease: DrmLease {
                        id: lease.id,
                        fd: lease.fd,
                    },
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
}

impl DrmDevice {
    /// Create a fresh lease for a grant and return its fd. Any still-active
    /// lease is revoked first (a previous owner may have died without
    /// releasing): the kernel cannot lease the same objects twice.
    pub fn grant_lease(&self) -> io::Result<OwnedFd> {
        let mut guard = self.state.lock().unwrap();
        let current = guard.take().expect("state always present");

        // Ensure the card is Locked: revoke any live lease first.
        let locked = match current {
            DrmDeviceState::Locked(dev) => dev,
            DrmDeviceState::Leased(dev) => match dev.revoke_lease() {
                Ok(dev) => {
                    debug!("Revoked stale lease on card{}", self.index);
                    dev
                }
                Err((dev, e)) => {
                    error!("Failed to revoke stale lease on card{}: {e}", self.index);
                    *guard = Some(DrmDeviceState::Leased(dev));
                    return Err(e);
                }
            },
        };

        match locked.create_lease() {
            Ok(leased) => {
                // Hand the client a dup; the server keeps its own copy in
                // the Leased state (see DrmLease).
                let fd = leased.lease.fd.try_clone();
                match fd {
                    Ok(fd) => {
                        *guard = Some(DrmDeviceState::Leased(leased));
                        Ok(fd)
                    }
                    Err(e) => {
                        error!("Failed to dup lease fd on card{}: {e}", self.index);
                        // Lease is alive but unsendable; put it back Leased
                        // so a later grant revokes and retries it.
                        *guard = Some(DrmDeviceState::Leased(leased));
                        Err(e)
                    }
                }
            }
            Err((dev, e)) => {
                *guard = Some(DrmDeviceState::Locked(dev));
                Err(e)
            }
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

/// The card's leaseable objects as `RawResourceHandle`s, queried fresh:
/// every CRTC and plane, and every display connector (writeback connectors
/// are capture-only and cannot present a display). A connector whose probe
/// failed is kept rather than silently dropped.
fn lease_objects(card: &DrmCard) -> io::Result<Vec<RawResourceHandle>> {
    let handles = card.resource_handles()?;
    let planes = card.plane_handles()?;

    let connectors = display_connectors(card, &handles);

    let mut objects: Vec<RawResourceHandle> = handles
        .crtcs
        .iter()
        .copied()
        .map(RawResourceHandle::from)
        .chain(connectors.iter().copied().map(RawResourceHandle::from))
        .chain(planes.iter().copied().map(RawResourceHandle::from))
        .collect();
    objects.shrink_to_fit();
    Ok(objects)
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

        let card = match DrmCard::open(&path) {
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
                    card: opened.card,
                }))),
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
