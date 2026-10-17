//! Client request semantics: Acquire (ask the engine, reply Grant/Deny or
//! stay silent for a queue), Release (forward to the engine), Ack (clear
//! the grant-ack timer).
//!
//! The overlay role is granted without the engine, because it is not a claim
//! on the display: it takes no slot, queues behind nobody and preempts
//! nothing.

use std::os::fd::AsRawFd;

use crate::{
    error::{ServerError, ServerResult},
    resource_manager::Inventory,
    types::ClientId,
    windowing::{AcquireOutcome, PolicyEngine},
};
use nix::libc::pid_t;
use sendfd::SendWithFd;
use simple_graphics_protocol::{
    ClientRequest, PlaneRole, Resource, ServerMessage, serialize_framed,
};
use tokio::{io::AsyncWriteExt, net::UnixStream, time::Instant};
use tracing::{debug, info, warn};

use super::GRANT_ACK_TIMEOUT;

/// Handle one parsed client request.
pub(super) async fn dispatch_request(
    req: ClientRequest,
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    engine: &PolicyEngine,
    inventory: &Inventory,
    ack_deadline: &mut Option<Instant>,
) -> ServerResult<()> {
    match req {
        ClientRequest::Acquire { resource } => {
            handle_acquire(
                stream,
                client_id,
                client_pid,
                resource,
                engine,
                inventory,
                ack_deadline,
            )
            .await?;
        }
        ClientRequest::Release { resource } => {
            handle_release(client_id, resource, engine, inventory).await;
        }
        ClientRequest::OverlayGeometry { resource, geometry } => {
            // Client-reported placement: recorded, not acted on. An overlay
            // that hides says so here and keeps its grant.
            if let Resource::Drm { .. } = resource {
                if let Some(device) = inventory.drm_device(&resource) {
                    device.record_overlay_geometry(client_id, &resource, geometry);
                }
            } else {
                debug!(
                    "[client {client_id} (pid {client_pid})] geometry for a non-DRM \
                     resource {resource:?} ignored"
                );
            }
        }
        ClientRequest::Ack => {
            info!("[client {client_id} (pid {client_pid})] Grant acknowledged");
            *ack_deadline = None;
        }
    }
    Ok(())
}

/// Handle an Acquire: ask the engine, then act on its decision.
///
/// A queued Acquire gets no reply here — the Grant arrives later through the
/// control channel (the requeued/queued grant path).
pub(super) async fn handle_acquire(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    resource: Resource,
    engine: &PolicyEngine,
    inventory: &Inventory,
    ack_deadline: &mut Option<Instant>,
) -> ServerResult<()> {
    info!("[client {client_id} (pid {client_pid})] Acquire: {resource:?}");

    // The overlay role is granted outside the policy engine, on purpose: a
    // plane of the pipeline is not a claim on the display, so an overlay
    // request must never queue for, be queued behind, or preempt the app on
    // it. It is granted if it is free and refused if another client holds it —
    // that client releases it, or disconnects.
    if let Resource::Drm {
        plane: PlaneRole::Overlay,
        ..
    } = resource
    {
        debug!(
            "[client {client_id} (pid {client_pid})] Acquire: {resource:?} (no policy: an \
             overlay never queues for the display)"
        );
        return match send_grant(stream, client_id, client_pid, resource.clone(), inventory).await {
            Ok(_) => {
                *ack_deadline = Some(Instant::now() + GRANT_ACK_TIMEOUT);
                Ok(())
            }
            Err(e) => {
                warn!("[client {client_id} (pid {client_pid})] Denied {resource:?}: {e}");
                let resp = serialize_framed(&ServerMessage::Deny {
                    reason: e.to_string(),
                })?;
                stream.write_all(&resp).await.map_err(ServerError::Write)?;
                Ok(())
            }
        };
    }

    match engine.acquire(client_id, resource.clone()).await {
        AcquireOutcome::Granted => {
            // The engine marked this client the owner. Produce the fd and
            // send the Grant; if the fd cannot be produced (e.g.
            // create_lease failed on an un-leasable card), roll the
            // ownership back so the next waiter can be served, reply Deny,
            // and keep the connection alive.
            let overlay_regranted = match send_grant(
                stream,
                client_id,
                client_pid,
                resource.clone(),
                inventory,
            )
            .await
            {
                Ok(regranted) => regranted,
                Err(e) => {
                    warn!(
                        "[client {client_id} (pid {client_pid})] Grant failed for \
                             {resource:?}: {e}"
                    );
                    engine.release(client_id, resource.clone()).await;
                    let resp = serialize_framed(&ServerMessage::Deny {
                        reason: format!("grant failed: {e}"),
                    })?;
                    stream.write_all(&resp).await.map_err(ServerError::Write)?;
                    return Ok(());
                }
            };
            *ack_deadline = Some(Instant::now() + GRANT_ACK_TIMEOUT);

            // A client that kept an overlay claim across the pipeline change is
            // told now, by an ordinary Grant: the plane it was promised is the
            // new pipeline's, under the same resource name it already holds.
            if let (Some(holder), Resource::Drm { card, .. }) = (overlay_regranted, resource) {
                engine.grant_to(holder, Resource::drm_overlay(card)).await;
            }
        }
        AcquireOutcome::Queued => {
            debug!(
                "[client {client_id} (pid {client_pid})] Queued for {resource:?}; \
                 grant will arrive via control channel"
            );
        }
        AcquireOutcome::Denied { reason } => {
            info!("[client {client_id} (pid {client_pid})] Denied {resource:?}: {reason}");
            let resp = serialize_framed(&ServerMessage::Deny { reason })?;
            stream.write_all(&resp).await.map_err(ServerError::Write)?;
        }
    }
    Ok(())
}

/// Send `ServerMessage::Grant` for one resource, with its fd.
///
/// For `Drm` the fd is a FRESH lease, created for this grant: the client
/// becomes the lessee (never DRM master), and the server keeps the ability
/// to revoke it at any time. Other resources grant a dup of the server's
/// registered fd.
pub(super) async fn send_grant(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    resource: Resource,
    inventory: &Inventory,
) -> ServerResult<Option<ClientId>> {
    // The granted fd must stay open until AFTER send_with_fd: SCM_RIGHTS
    // dups the fd at send time, and sending a number whose fd was already
    // closed fails with EBADF. Binding it here (not inside the match arm)
    // keeps it alive across the send.
    let granted = inventory.grant_fd(&resource, client_id)?;
    let fd = granted.fd.as_raw_fd();

    match &granted.drm {
        Some(drm) => info!(
            "[client {client_id} (pid {client_pid})] Granted {resource:?} (plane {} crtc {} fd \
             {fd})",
            drm.plane_id, drm.crtc_id
        ),
        None => info!("[client {client_id} (pid {client_pid})] Granted {resource:?} (fd {fd})"),
    }
    let response = serialize_framed(&ServerMessage::Grant {
        resource,
        drm: granted.drm,
    })?;
    stream
        .send_with_fd(&response, &[fd])
        .map_err(ServerError::Write)?;
    Ok(granted.overlay_regranted)
}

/// Forward a Release to the engine. The engine validates ownership (a
/// Release from a non-owner, or one that completes a revoke, is its call —
/// it warns and requeues accordingly). A `Drm` lease is revoked
/// immediately: the resource must be truly free before the next grant,
/// kernel-enforced, regardless of what the client does with its fd.
///
/// The overlay role is the exception: it never took a slot in the engine, so
/// releasing it frees its plane and stops there. The pipeline belongs to the
/// client still on the display, and that client keeps drawing.
pub(super) async fn handle_release(
    client_id: ClientId,
    resource: Resource,
    engine: &PolicyEngine,
    inventory: &Inventory,
) {
    inventory.revoke_lease(&resource, client_id);

    if matches!(
        resource,
        Resource::Drm {
            plane: PlaneRole::Overlay,
            ..
        }
    ) {
        return;
    }

    engine.release(client_id, resource).await;
}
