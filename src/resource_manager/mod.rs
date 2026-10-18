//! Open and register every available resource (video + input). Ownership is
//! NOT tracked here anymore — that is the policy engine's job.
//!
//! Backends are compile-time features (see docs/resource-manager.md):
//! `fbdev`, `drm`, and `input` each live in their own module behind a
//! `#[cfg(feature = ...)]`. Default features: `drm` + `input` — an unbuilt
//! backend is never opened, registered, or advertised; the engine denies
//! Acquires against it with "not registered". The protocol crate is
//! deliberately ungated (the wire format must not depend on the build).
//!
//! [`open_resources`] returns the advertised list in priority order:
//! clients read it top-down, so the first entry of a kind is the best
//! match (the first DRM card is the display card).

#[cfg(feature = "drm")]
mod cursor_art {
    //! The controller's cursor artwork, generated at build time.
    //!
    //! Produced by `tools/gen_cursors.py` into `OUT_DIR` (from `cursor-icons/*.png`
    //! when that directory exists, otherwise from the drawn set). The rules the
    //! generator enforces — opaque or transparent pixels only, a baked outline, a
    //! hotspot per shape — are documented there.
    include!(concat!(env!("OUT_DIR"), "/cursor_art.rs"));
}
pub(crate) mod cursor;
mod drm;
#[cfg(feature = "fbdev")]
mod fbdev;
#[cfg(feature = "input")]
pub mod hotplug;
#[cfg(feature = "input")]
mod input;
mod overlay_policy;
mod pointer;

// Both are only used by the debug/test-only consistency checker below: a release
// build does not compile it, and then has no use for them.
#[cfg(all(feature = "input", any(test, debug_assertions)))]
use std::collections::HashMap;
#[cfg(all(feature = "input", any(test, debug_assertions)))]
use std::path::PathBuf;
use std::sync::Arc;

use crate::types::ClientId;
use dashmap::DashMap;
use simple_graphics_protocol::{CursorShape, DrmGrant, PlaneRole, Resource};

use crate::types::ResourceRegistry;
// as above: only the consistency checker (and its tests) name this type
#[cfg(any(test, debug_assertions))]
use crate::types::AdvertisedResources;

use std::os::fd::OwnedFd;

#[cfg(feature = "drm")]
pub use drm::DrmRegistry;
#[cfg(feature = "input")]
pub use input::InputIndex;

/// Everything the daemon holds: the grant sources, and the input devices it
/// tracks so the reconciler can tell the node it opened from one re-created
/// under the same path.
///
/// Cloned per client connection (every field is a shared handle, so the clone is
/// cheap). The fields are private and every mutation goes through a method —
/// the grant side here, the input transitions in [`hotplug`] beside the
/// reconciler that calls them — because the order in which the fd table and the
/// input index move is what keeps them consistent. See
/// `docs/resource-manager.md`, "The invariants between them".
#[derive(Clone)]
pub struct Inventory {
    /// Static fds for `Fbdev` and `Input` (grants are dups of these).
    fds: ResourceRegistry,
    /// DRM lease factories: each grant creates a fresh lease fd.
    #[cfg(feature = "drm")]
    drm: DrmRegistry,
    /// The held input devices, keyed by devnode path.
    #[cfg(feature = "input")]
    inputs: InputIndex,
}

/// What a grant consists of: the fd to hand the client, and — for a DRM
/// plane — which plane the server chose for it.
#[derive(Debug)]
pub struct Granted {
    /// The fd the client receives over `SCM_RIGHTS`.
    pub fd: OwnedFd,
    /// The plane to draw on, when the resource is a DRM card.
    pub drm: Option<DrmGrant>,
    /// The client (if any) whose overlay claim was re-honoured by this grant
    /// and must be told with an ordinary Grant of its own.
    pub overlay_regranted: Option<ClientId>,
}

impl Inventory {
    /// The grant sources, with no input devices behind them. The daemon always
    /// goes through `open_resources()`; this is how a test builds a server's
    /// inventory.
    #[cfg(test)]
    #[cfg(feature = "drm")]
    pub(crate) fn new(fds: ResourceRegistry, drm: DrmRegistry) -> Self {
        Self {
            fds,
            drm,
            #[cfg(feature = "input")]
            inputs: input::new_index(),
        }
    }

    /// The grant sources for a build without DRM.
    #[cfg(test)]
    #[cfg(not(feature = "drm"))]
    pub(crate) fn new(fds: ResourceRegistry) -> Self {
        Self {
            fds,
            #[cfg(feature = "input")]
            inputs: input::new_index(),
        }
    }

    /// The fd to grant for one resource, and what to draw on: a card's PRIMARY
    /// role creates (or reuses) the pipeline's lease, its OVERLAY role takes a
    /// plane of the running pipeline — through that same lease, since a lease
    /// is the only handle a client can program a plane with. Anything else is
    /// a dup of the registered fd.
    ///
    /// `client` is what makes the overlay grant an ownership: the plane stays
    /// with this client until it releases the resource or disconnects.
    pub fn grant_fd(&self, resource: &Resource, client: ClientId) -> anyhow::Result<Granted> {
        match resource {
            #[cfg(feature = "drm")]
            Resource::Drm { plane, .. } => {
                let device = self
                    .drm_device(resource)
                    .ok_or_else(|| anyhow::anyhow!("card of {resource:?} is not registered"))?;

                match plane {
                    PlaneRole::Primary => device
                        .grant_lease(client)
                        .map(|lease| Granted {
                            fd: lease.fd,
                            drm: None,
                            overlay_regranted: lease.overlay_regranted,
                        })
                        .map_err(|e| anyhow::anyhow!("failed to lease {resource:?}: {e}")),
                    PlaneRole::Overlay => {
                        // The plane is chosen first (it is what the request is
                        // for and what can refuse), then the lease fd it is
                        // programmed through. A failure to dup that fd takes
                        // the plane back, so a refused request cannot leave a
                        // plane assigned to nobody.
                        let drm = device
                            .grant_overlay(client)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        match device.lease_fd() {
                            Ok(fd) => Ok(Granted {
                                fd,
                                drm: Some(drm),
                                overlay_regranted: None,
                            }),
                            Err(e) => {
                                device.release_overlay(client);
                                Err(anyhow::anyhow!("failed to grant {resource:?}: {e}"))
                            }
                        }
                    }
                }
            }
            _ => self
                .fds
                .get(resource)
                .ok_or_else(|| anyhow::anyhow!("resource {resource:?} is not registered"))?
                .try_clone()
                .map(|fd| Granted {
                    fd,
                    drm: None,
                    overlay_regranted: None,
                })
                .map_err(|e| anyhow::anyhow!("failed to dup fd for {resource:?}: {e}")),
        }
    }

    /// The device behind any DRM resource of a card, whatever role it names.
    ///
    /// The registry is keyed by the resource a client acquires, and the DRM
    /// resource that is registered is the card's PRIMARY plane — one entry per
    /// card, one device behind it. An overlay resource is the same card's, so
    /// it resolves through the card rather than by its own key, which is how a
    /// lookup by the overlay's own resource would find nothing.
    #[cfg(feature = "drm")]
    /// The card whose pipeline `client` holds, if any. A cursor request names no
    /// card (there is one pointer), so the daemon finds the lease behind it.
    /// Record the cursor shape for whichever card `client` holds the lease of.
    ///
    /// A cursor request names no card — there is one pointer — so the daemon
    /// finds the lease behind it. Returns whether any card accepted it (i.e.
    /// whether the client is a display owner at all).
    pub(crate) fn record_cursor_shape(
        &self,
        client: ClientId,
        shape: CursorShape,
        visible: bool,
    ) -> Option<drm::CursorRequest> {
        for entry in self.drm.iter() {
            let device = entry.value();
            if device.record_cursor_shape(client, shape, visible) {
                return device.cursor_request();
            }
        }
        None
    }

    pub(crate) fn drm_device(
        &self,
        resource: &Resource,
    ) -> Option<dashmap::mapref::one::Ref<'_, Resource, drm::DrmDevice>> {
        let Resource::Drm { card, .. } = resource else {
            return None;
        };
        self.drm.get(&Resource::drm(*card))
    }

    /// Everything `client` held that the policy engine does not know about —
    /// and the pipeline it owned.
    ///
    /// The engine cleans up after itself on disconnect (it tracks the resources
    /// it granted), but an overlay grant never went through it, so without this
    /// a killed client would leave its plane granted to nobody, still scanning
    /// out its last frame. And a lease outliving its owner is worse than a
    /// leak: the CRTC keeps its mode, so the pipeline still looks alive and the
    /// next overlay request would be granted a plane of a display nobody is
    /// driving.
    pub fn release_client(&self, client: ClientId) {
        #[cfg(feature = "drm")]
        for device in self.drm.iter() {
            device.release_overlay(client);
            device.revoke_lease_if_owned_by(client);
        }
        #[cfg(not(feature = "drm"))]
        let _ = client;
    }

    /// Give up what a resource holds, now.
    ///
    /// A PRIMARY release revokes the lease — kernel-enforced, and every
    /// overlay grant of that pipeline goes with it. An OVERLAY release frees
    /// only its plane: the pipeline belongs to the client still on the
    /// display, and that client keeps drawing.
    pub fn revoke_lease(&self, resource: &Resource, client: ClientId) {
        #[cfg(feature = "drm")]
        if let Resource::Drm { plane, .. } = resource
            && let Some(device) = self.drm_device(resource)
        {
            match plane {
                PlaneRole::Primary => device.revoke_lease(),
                PlaneRole::Overlay => device.release_overlay(client),
            }
        }
        #[cfg(not(feature = "drm"))]
        let _ = (resource, client);
    }
}

/// Open and register every available resource.
///
/// Returns the devices the daemon holds, plus the resources in advertised order
/// (priority order — first is best). Backends that are not compiled in
/// contribute nothing.
/// The artwork for `shape`, in the size this build carries.
///
/// A shape the artwork does not carry falls back to the default pointer: a
/// wrong-shaped cursor is a visible bug, a missing one a usability bug, and the
/// second is worse.
pub(crate) fn cursor_art(shape: CursorShape) -> &'static cursor_art::Art {
    use CursorShape::*;
    match shape {
        Default => &cursor_art::DEFAULT,
        Text => &cursor_art::TEXT,
        Pointer => &cursor_art::POINTER,
        Crosshair => &cursor_art::CROSSHAIR,
        Move => &cursor_art::MOVE,
        ResizeVertical => &cursor_art::RESIZE_VERTICAL,
        ResizeHorizontal => &cursor_art::RESIZE_HORIZONTAL,
        ResizeTopLeft => &cursor_art::RESIZE_TOP_LEFT,
        ResizeTopRight => &cursor_art::RESIZE_TOP_RIGHT,
        Wait => &cursor_art::WAIT,
        Progress => &cursor_art::PROGRESS,
        NotAllowed => &cursor_art::NOT_ALLOWED,
        Help => &cursor_art::HELP,
        Grab => &cursor_art::GRAB,
        Grabbing => &cursor_art::GRABBING,
    }
}

/// One line describing the cursor artwork this build carries, for the startup
/// log — an operator should see what the pointer can look like without digging
/// into the build directory.
pub(crate) fn cursor_art_summary() -> String {
    let names: Vec<&str> = cursor_art::ALL.iter().map(|(name, _)| *name).collect();
    // The default pointer is the one an app gets before it asks for anything,
    // so its size and hotspot are what the startup line should state.
    let default = cursor_art(CursorShape::Default);
    format!(
        "{} cursor shapes, {} px each: {} (default {}x{}, hotspot {},{})",
        names.len(),
        default.pixels.len(),
        names.join(", "),
        default.width,
        default.height,
        default.hotspot.0,
        default.hotspot.1
    )
}

pub fn open_resources() -> (Inventory, Vec<Resource>) {
    let resource_reg: ResourceRegistry = Arc::new(DashMap::new());
    // With no backend features the list is never pushed to; the mut keeps
    // the body identical across all feature combinations.
    #[allow(unused_mut)]
    let mut advertised = Vec::new();

    #[cfg(feature = "drm")]
    let drm_registry: DrmRegistry = Arc::new(DashMap::new());

    // One value the backends register into, so no backend can hold a grant
    // source without the input index that goes with it.
    let inventory = Inventory {
        fds: resource_reg.clone(),
        #[cfg(feature = "drm")]
        drm: drm_registry,
        #[cfg(feature = "input")]
        inputs: input::new_index(),
    };

    #[cfg(feature = "fbdev")]
    fbdev::open(&inventory, &mut advertised);

    #[cfg(feature = "drm")]
    drm::open_devices(&inventory, &mut advertised);

    #[cfg(feature = "input")]
    input::open_devices(&inventory, &mut advertised);

    (inventory, advertised)
}

/// Check the invariants that tie the three structures together: one line per
/// violation, empty when the state is coherent.
///
/// These are the rules the module maintains by hand today, and the reason the
/// transitions (suspend / resume / adopt) live behind `Inventory` methods: a
/// violation means some path updated one structure without the others.
///
/// 1. one index entry per resource, keyed by the devnode it sits on;
/// 2. a LIVE entry (its device is there) is advertised, and its resource has an fd to
///    grant;
/// 3. a `device_gone` entry is neither advertised nor grantable — the holder keeps
///    the name, the daemon keeps no way to hand it out;
/// 4. every advertised input has an index entry, and every input fd belongs to
///    an advertised resource (never a grant source without a device behind it).
///
/// Built for the two callers that exist: the `debug_assertions` check hotplug
/// runs after suspend/adopt, and the tests below. A release build has neither,
/// which is why the gate is not just the `input` feature - it was dead code
/// there, and warned about it.
#[cfg(all(feature = "input", any(test, debug_assertions)))]
pub fn check_consistency(
    fds: &ResourceRegistry,
    index: &InputIndex,
    advertised: &AdvertisedResources,
) -> Vec<String> {
    let list = advertised.snapshot();
    let mut problems = Vec::new();
    let mut seen: HashMap<Resource, PathBuf> = HashMap::new();

    for entry in index.iter() {
        let path = entry.key().clone();
        let held = entry.value();

        if let Some(previous) = seen.insert(held.resource.clone(), path.clone()) {
            problems.push(format!(
                "{:?} is in the index twice: {} and {}",
                held.resource,
                previous.display(),
                path.display()
            ));
        }

        let has_fd = fds.contains_key(&held.resource);
        let advertised = list.contains(&held.resource);
        if held.device_gone {
            if has_fd {
                problems.push(format!(
                    "{:?} is marked device_gone but still has an fd",
                    held.resource
                ));
            }
            if advertised {
                problems.push(format!(
                    "{:?} is marked device_gone but still advertised",
                    held.resource
                ));
            }
        } else {
            if !has_fd {
                problems.push(format!(
                    "{:?} ({}) is live but has no fd",
                    held.resource,
                    path.display()
                ));
            }
            if !advertised {
                problems.push(format!(
                    "{:?} ({}) is live but not advertised",
                    held.resource,
                    path.display()
                ));
            }
        }
    }

    for resource in &list {
        if matches!(resource, Resource::Input(_)) && !seen.contains_key(resource) {
            problems.push(format!("{resource:?} is advertised with no index entry"));
        }
    }

    for entry in fds.iter() {
        if matches!(entry.key(), Resource::Input(_)) && !list.contains(entry.key()) {
            problems.push(format!("{:?} has an fd but is not advertised", entry.key()));
        }
    }

    problems
}

#[cfg(all(test, feature = "input"))]
mod tests {
    use super::*;
    use crate::resource_manager::input::{HeldInput, new_index};
    use simple_graphics_protocol::InputResource;

    fn fd() -> std::os::fd::OwnedFd {
        std::fs::File::open("/dev/null").expect("/dev/null").into()
    }

    fn entry(resource: &Resource, device_gone: bool) -> HeldInput {
        HeldInput {
            resource: resource.clone(),
            dev: 1,
            ino: 2,
            device: None,
            device_gone,
        }
    }

    fn keyboard() -> Resource {
        Resource::Input(InputResource::Keyboard(0))
    }

    fn mouse() -> Resource {
        Resource::Input(InputResource::Mouse(0))
    }

    /// Startup plus one device that went away while somebody held it: the
    /// keyboard is live and advertised, the mouse's device is gone and it is neither.
    fn coherent() -> (ResourceRegistry, InputIndex, AdvertisedResources) {
        let fds: ResourceRegistry = Arc::new(DashMap::new());
        fds.insert(Resource::Fbdev, fd());
        fds.insert(keyboard(), fd());

        let index = new_index();
        index.insert("/dev/input/event0".into(), entry(&keyboard(), false));
        index.insert("/dev/input/event6".into(), entry(&mouse(), true));

        let advertised = AdvertisedResources::new(vec![Resource::Fbdev, keyboard()]);
        (fds, index, advertised)
    }

    #[test]
    fn a_coherent_state_reports_nothing() {
        let (fds, index, advertised) = coherent();
        assert_eq!(
            check_consistency(&fds, &index, &advertised),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_live_input_without_a_grant_source_is_reported() {
        let (fds, index, advertised) = coherent();
        fds.remove(&keyboard());
        let problems = check_consistency(&fds, &index, &advertised);
        assert!(
            problems.iter().any(|p| p.contains("live but has no fd")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_device_gone_input_that_is_still_offered_is_reported() {
        let (fds, index, advertised) = coherent();
        advertised.insert(mouse());
        fds.insert(mouse(), fd());
        let problems = check_consistency(&fds, &index, &advertised);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("device_gone but still advertised")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("device_gone but still has an fd")),
            "{problems:?}"
        );
    }
}

/// Read the input devices for the controller's own pointer tracking, and draw
/// the cursor where they take it.
///
/// The controller opens the devices itself, and reads **its own** descriptors —
/// never the ones it hands to clients. `SCM_RIGHTS` shares a file description,
/// not just a descriptor, so a reader here and a reader in the client on the
/// same description would each get part of the event stream: a pointer that
/// jumps under load, with no error anywhere. Two independent `open()` calls
/// give two descriptions, and both see every event.
///
/// Only devices with relative axes are read (a pointer, not a keyboard), and
/// the set is rediscovered every second so a device plugged in later is picked
/// up and an unplugged one is dropped. Absolute devices are out of scope until
/// the projection work gives them an axis range to map.
pub(crate) fn spawn_pointer_tracking(drm: DrmRegistry, engine: crate::windowing::PolicyEngine) {
    use std::collections::HashMap;
    use std::os::fd::AsRawFd;

    std::thread::spawn(move || {
        let mut pointer = pointer::Pointer::new((0, 0));
        let mut devices: HashMap<std::path::PathBuf, evdev::Device> = HashMap::new();
        let mut last_discovery = std::time::Instant::now();
        // The mode the cursor was last drawn on. A new pipeline (the first
        // client, a resume) draws the cursor at once instead of waiting for the
        // user to move the mouse: an invisible cursor on a fresh screen looks
        // like the cursor is broken.
        let mut drawn_on: Option<(u32, u32)> = None;

        loop {
            if last_discovery.elapsed() >= std::time::Duration::from_secs(1) {
                last_discovery = std::time::Instant::now();
                for (path, device) in evdev::enumerate() {
                    if device.supported_relative_axes().iter().next().is_some() {
                        // Non-blocking, and this is not an optimisation:
                        // `fetch_events` reads until EAGAIN, so on a blocking fd
                        // the read after the last queued event never returns —
                        // the reader would sit in the kernel forever, having
                        // handled one batch and drawn nothing.
                        if !make_nonblocking(&device) {
                            continue;
                        }
                        // `or_insert`: a device already being read keeps its
                        // position in the queue; a new one starts here.
                        devices.entry(path).or_insert(device);
                    }
                }
                devices.retain(|path, _| path.exists());
            }

            // Wake at least once a second: the rediscovery above must run even
            // when nothing moves.
            let mut fds: Vec<nix::poll::PollFd> = devices
                .values()
                .map(|device| {
                    // SAFETY: the descriptor belongs to `device`, which outlives
                    // this borrow: the BorrowedFd is only used for this poll.
                    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(device.as_raw_fd()) };
                    nix::poll::PollFd::new(fd, nix::poll::PollFlags::POLLIN)
                })
                .collect();
            let _ = nix::poll::poll(&mut fds, 1000u16);

            let mut moved = false;
            for device in devices.values_mut() {
                let Ok(events) = device.fetch_events() else {
                    continue;
                };
                for event in events {
                    if pointer.handle(event.event_type(), event.code(), event.value()) {
                        moved = true;
                    }
                }
            }
            // The clamp belongs to the display, which belongs to a card.
            let mut mode = None;
            for card in drm.iter() {
                mode = card.pipeline_mode().or(mode);
            }
            let Some(mode) = mode else {
                // No pipeline: forget what the cursor was drawn on, so the next
                // one draws at once even if it uses the same mode.
                if drawn_on.is_some() {
                    tracing::debug!("no pipeline: the cursor has nowhere to be drawn");
                }
                drawn_on = None;
                continue;
            };
            let fresh_pipeline = drawn_on != Some(mode);
            if !moved && !fresh_pipeline {
                continue;
            }
            if fresh_pipeline {
                // A new pipeline starts the pointer in the middle of its mode.
                // A relative pointer has no absolute position to inherit, and a
                // cursor parked in a corner on a fresh screen reads as broken.
                pointer = pointer::Pointer::new(mode);
            }
            pointer.set_mode(mode);
            drawn_on = Some(mode);
            let position = pointer.position();
            // One integrator, one truth: the app on the display is told where
            // the pointer is rather than summing deltas itself, which is the only
            // way its idea of the pointer cannot drift from the drawn cursor —
            // two integrators clamping at an edge lose different deltas.
            engine.pointer(position.0, position.1);
            for card in drm.iter() {
                match card.cursor_shape() {
                    Some(request) if !request.visible => card.hide_cursor(),
                    Some(request) => card.show_cursor(request.shape, position),
                    None => {
                        card.show_cursor(simple_graphics_protocol::CursorShape::Default, position)
                    }
                }
            }
        }
    });
}

/// Put an evdev device into non-blocking mode.
///
/// `evdev::Device` has no such method (only its `SyncStream` wrapper does), so
/// this is the fcntl underneath. See the caller for why it is not optional.
fn make_nonblocking(device: &evdev::Device) -> bool {
    use std::os::fd::AsRawFd;

    use nix::fcntl::{FcntlArg, OFlag, fcntl};

    let fd = device.as_raw_fd();
    match fcntl(fd, FcntlArg::F_GETFL) {
        Ok(flags) => fcntl(
            fd,
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
        )
        .is_ok(),
        Err(_) => false,
    }
}

impl Inventory {
    /// Redraw every card's pointer at the shape it now holds, at the position it
    /// was last drawn at.
    pub(crate) fn redraw_cursor(&self) {
        for card in self.drm.iter() {
            card.redraw_cursor();
        }
    }

    /// The cards, for the pointer reader (which outlives any one request).
    pub fn drm_registry(&self) -> DrmRegistry {
        self.drm.clone()
    }
}
