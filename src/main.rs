//! Binary entry: wire up logging, resources, and the windowing policy, then
//! hand everything to the [`server`] accept loop.

use std::{collections::HashMap, sync::Arc};

#[cfg(feature = "input")]
use crate::resource_manager::hotplug;
use crate::{
    resource_manager::open_resources,
    types::AdvertisedResources,
    windowing::{Policy, PolicyEngine},
};
use anyhow::Context;
use simple_graphics_protocol::Resource;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

mod client_handler;
mod error;
mod resource_manager;
mod server;
mod types;
mod windowing;

#[cfg(test)]
mod integration_tests;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // init logger
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("Starting simple-graphics-controller");
    let (inventory, advertised) = open_resources();

    info!("{}", resource_manager::cursor_art_summary());
    // Advertised list: shared and mutable, because the input reconciler adds
    // and drops resources as devices come and go.
    let advertised = Arc::new(AdvertisedResources::new(advertised));
    debug!(
        "Advertised resources (priority order): {:?}",
        advertised.snapshot()
    );

    // Window policy: SGC_POLICY env, default fair-queue. One policy for all
    // registered resources (per-resource override map is a future knob).
    let policy: Policy = std::env::var("SGC_POLICY")
        .ok()
        .map(|value| value.parse())
        .transpose()
        .context("invalid SGC_POLICY (expected first-owner | latest-owner | fair-queue)")?
        .unwrap_or(Policy::FairQueue);
    info!("Windowing policy: {policy:?}");

    // One policy per advertised resource (covers Fbdev, Drm, and Input).
    let policies: HashMap<Resource, Policy> = advertised
        .snapshot()
        .into_iter()
        .map(|resource| (resource, policy))
        .collect();
    let engine = PolicyEngine::spawn(policies);

    // Input devices are not a boot-time snapshot: adopt the ones plugged in
    // while the server runs, and suspend the ones that go away. Resources
    // adopted later are offered to the engine with the same policy.
    #[cfg(feature = "input")]
    tokio::spawn(hotplug::run(
        inventory.clone(),
        advertised.clone(),
        engine.clone(),
        policy,
    ));

    // The inventory (with the DRM masters inside it) stays alive for the whole
    // run: closing a master destroys its leases.
    server::run(engine, inventory, advertised).await
}
