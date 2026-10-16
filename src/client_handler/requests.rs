//! Client request semantics: Acquire (ask the engine, reply Grant/Deny or
//! stay silent for a queue), Release (forward to the engine), Ack (clear
//! the grant-ack timer).

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

    // The overlay role is part of the protocol but not of what this server
    // grants yet: a lease over primary planes only has no overlay in it to hand
    // over. Refuse it here, before the policy engine — an overlay must never
    // queue for, or preempt, the app on the display.
    if let Resource::Drm {
        card,
        plane: PlaneRole::Overlay,
    } = resource
    {
        let reason = format!(
            "overlay planes are not granted yet (card {card}: this server leases the primary \
             plane only)"
        );
        warn!("[client {client_id} (pid {client_pid})] Denied {resource:?}: {reason}");
        let resp = serialize_framed(&ServerMessage::Deny { reason })?;
        stream.write_all(&resp).await.map_err(ServerError::Write)?;
        return Ok(());
    }

    match engine.acquire(client_id, resource.clone()).await {
        AcquireOutcome::Granted => {
            // The engine marked this client the owner. Produce the fd and
            // send the Grant; if the fd cannot be produced (e.g.
            // create_lease failed on an un-leasable card), roll the
            // ownership back so the next waiter can be served, reply Deny,
            // and keep the connection alive.
            if let Err(e) =
                send_grant(stream, client_id, client_pid, resource.clone(), inventory).await
            {
                warn!("[client {client_id} (pid {client_pid})] Grant failed for {resource:?}: {e}");
                engine.release(client_id, resource.clone()).await;
                let resp = serialize_framed(&ServerMessage::Deny {
                    reason: format!("grant failed: {e}"),
                })?;
                stream.write_all(&resp).await.map_err(ServerError::Write)?;
                return Ok(());
            }
            *ack_deadline = Some(Instant::now() + GRANT_ACK_TIMEOUT);
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
) -> ServerResult<()> {
    // The granted fd must stay open until AFTER send_with_fd: SCM_RIGHTS
    // dups the fd at send time, and sending a number whose fd was already
    // closed fails with EBADF. Binding it here (not inside the match arm)
    // keeps it alive across the send.
    let granted = inventory.grant_fd(&resource)?;
    let fd = granted.as_raw_fd();

    info!("[client {client_id} (pid {client_pid})] Granted {resource:?} (fd {fd})");
    let response = serialize_framed(&ServerMessage::Grant { resource })?;
    stream
        .send_with_fd(&response, &[fd])
        .map_err(ServerError::Write)?;
    Ok(())
}

/// Forward a Release to the engine. The engine validates ownership (a
/// Release from a non-owner, or one that completes a revoke, is its call —
/// it warns and requeues accordingly). A `Drm` lease is revoked
/// immediately: the resource must be truly free before the next grant,
/// kernel-enforced, regardless of what the client does with its fd.
pub(super) async fn handle_release(
    client_id: ClientId,
    resource: Resource,
    engine: &PolicyEngine,
    inventory: &Inventory,
) {
    inventory.revoke_lease(&resource);
    engine.release(client_id, resource).await;
}
