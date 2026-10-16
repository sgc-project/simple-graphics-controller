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
mod drm;
#[cfg(feature = "fbdev")]
mod fbdev;
#[cfg(feature = "input")]
pub mod hotplug;
#[cfg(feature = "input")]
mod input;

// Both are only used by the debug/test-only consistency checker below: a release
// build does not compile it, and then has no use for them.
#[cfg(all(feature = "input", any(test, debug_assertions)))]
use std::collections::HashMap;
#[cfg(all(feature = "input", any(test, debug_assertions)))]
use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use simple_graphics_protocol::{PlaneRole, Resource};

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

    /// The fd to grant for one resource: a fresh lease for a card's PRIMARY
    /// plane, otherwise a dup of the registered fd.
    ///
    /// The overlay role of a card is not granted yet: the lease this server
    /// creates carries the primary planes only, so there is nothing to hand a
    /// client that asks for an overlay. The request is refused (see
    /// `handle_acquire`, which logs and denies before it gets here) and this
    /// arm is what makes the refusal total.
    pub fn grant_fd(&self, resource: &Resource) -> anyhow::Result<OwnedFd> {
        match resource {
            #[cfg(feature = "drm")]
            Resource::Drm { card, plane } => match plane {
                PlaneRole::Primary => {
                    let device = self.drm.get(resource).ok_or_else(|| {
                        anyhow::anyhow!("resource {resource:?} is not registered")
                    })?;
                    device
                        .grant_lease()
                        .map_err(|e| anyhow::anyhow!("failed to lease {resource:?}: {e}"))
                }
                PlaneRole::Overlay => Err(anyhow::anyhow!(
                    "overlay planes are not granted yet (card {card}: this server leases the \
                     primary plane only)"
                )),
            },
            _ => self
                .fds
                .get(resource)
                .ok_or_else(|| anyhow::anyhow!("resource {resource:?} is not registered"))?
                .try_clone()
                .map_err(|e| anyhow::anyhow!("failed to dup fd for {resource:?}: {e}")),
        }
    }

    /// Give up a `Drm` lease now, so the resource is free for the next grant
    /// whatever the client does with the fd it was handed.
    pub fn revoke_lease(&self, resource: &Resource) {
        #[cfg(feature = "drm")]
        if matches!(
            resource,
            Resource::Drm {
                plane: PlaneRole::Primary,
                ..
            }
        ) && let Some(device) = self.drm.get(resource)
        {
            device.revoke_lease();
        }
        #[cfg(not(feature = "drm"))]
        let _ = resource;
    }
}

/// Open and register every available resource.
///
/// Returns the devices the daemon holds, plus the resources in advertised order
/// (priority order — first is best). Backends that are not compiled in
/// contribute nothing.
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
