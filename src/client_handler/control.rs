//! Engine-pushed control messages: write `Revoke` / `Grant`-from-queue on
//! the wire. A `Grant` carries the resource's fd and arms the ack timer.

use crate::{
    error::{ServerError, ServerResult},
    resource_manager::Inventory,
    types::ClientId,
    windowing::{ControlMessage, PolicyEngine},
};
use nix::libc::pid_t;
use simple_graphics_protocol::{ServerMessage, serialize_framed};
use tokio::{io::AsyncWriteExt, net::UnixStream, time::Instant};
use tracing::{info, warn};

use super::GRANT_ACK_TIMEOUT;
use super::requests::send_grant;

pub(super) async fn process_control_message(
    stream: &mut UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    engine: &PolicyEngine,
    msg: ControlMessage,
    inventory: &Inventory,
    ack_deadline: &mut Option<Instant>,
) -> ServerResult<()> {
    match msg {
        ControlMessage::Revoke { resource } => {
            // The wire Revoke is an ASK, not a kill: the client keeps a
            // valid lease (DRM) or fd (fbdev/input) through the grace
            // window (REVOKE_TIMEOUT) so it can finish its frame and hand
            // off. The kernel revoke for DRM happens at the HANDOFF — the
            // owner's Release (requests.rs), or the next grant's
            // stale-revoke after force-reclaim. Revoking first would kill
            // the client's next modeset ioctl mid-frame on an invalid fd.
            info!("[client {client_id} (pid {client_pid})] Revoking {resource:?}");
            let resp = serialize_framed(&ServerMessage::Revoke { resource })?;
            stream.write_all(&resp).await.map_err(ServerError::Write)?;
        }
        ControlMessage::Pointer { x, y } => {
            // Per-frame path: no log, no ack. The position is the controller's,
            // clamped to the mode, so the app never integrates deltas itself and
            // cannot drift from the drawn cursor.
            let resp = serialize_framed(&ServerMessage::Pointer { x, y })?;
            stream.write_all(&resp).await.map_err(ServerError::Write)?;
        }
        ControlMessage::Advertise {
            available_resources,
        } => {
            // Server-level push: a device appeared or went away. The client
            // replaces its list; no fd and no ack are involved. A client that
            // predates this push reads it as an unexpected message and ignores
            // it, which is why the same `Advertise` variant is reused instead of
            // introducing a new one.
            info!(
                "[client {client_id} (pid {client_pid})] Pushing Advertise: {available_resources:?}"
            );
            let resp = serialize_framed(&ServerMessage::Advertise {
                available_resources,
            })?;
            stream.write_all(&resp).await.map_err(ServerError::Write)?;
        }
        ControlMessage::Grant { resource } => {
            // The engine granted this client the slot (from the queue or a
            // revoke-ack). Produce the fd and send the Grant. If the fd
            // cannot be produced (e.g. create_lease failed on an
            // un-leasable card), roll the ownership back so the next waiter
            // can be served, reply Deny, and keep the connection alive — a
            // failed grant is a resource problem, not a protocol error.
            if let Err(e) =
                send_grant(stream, client_id, client_pid, resource.clone(), inventory).await
            {
                warn!("[client {client_id}] grant failed for {resource:?}: {e}");
                engine.release(client_id, resource.clone()).await;
                let resp = serialize_framed(&ServerMessage::Deny {
                    reason: format!("grant failed: {e}"),
                })?;
                stream.write_all(&resp).await.map_err(ServerError::Write)?;
                return Ok(());
            }
            *ack_deadline = Some(Instant::now() + GRANT_ACK_TIMEOUT);
        }
    }
    Ok(())
}
