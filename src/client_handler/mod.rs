//! Per-connection task: owns the client stream and drives the select loop —
//! wire bytes (client requests), engine control messages (Revoke/Grant),
//! and the grant-ack timeout — then unregisters the client on disconnect.
//!
//! Submodules: [`wire`] (frame assembly), [`requests`] (client request
//! semantics), [`control`] (engine-pushed messages).

mod control;
mod requests;
mod wire;

use std::{sync::Arc, time::Duration};

use crate::error::{ServerError, ServerResult};
use crate::resource_manager::Inventory;
use crate::types::{AdvertisedResources, ClientId};
use crate::windowing::PolicyEngine;
use nix::libc::pid_t;
use simple_graphics_protocol::{ServerMessage, serialize_framed};
use tokio::{io::AsyncWriteExt, net::UnixStream, sync::mpsc, time::Instant};
use tracing::{debug, info, warn};

/// How long the server waits for a client to acknowledge a Grant before
/// declaring the grant unconfirmed. Delivery signal only — it never gates
/// ownership or the queue (the fd was already duplicated to the client).
const GRANT_ACK_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn handle_connection(
    mut stream: UnixStream,
    client_id: ClientId,
    client_pid: pid_t,
    engine: PolicyEngine,
    inventory: Inventory,
    advertised: Arc<AdvertisedResources>,
) -> ServerResult<()> {
    info!("[client {client_id} (pid {client_pid})] New client connected");

    // Control channel: the engine pushes Revoke/Grant here; this task writes
    // them on the wire (with fds for Grant). Unregistered on disconnect.
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    engine.register(client_id, control_tx).await;

    // Send available resources immediately (no Hello handshake needed). The
    // list carries the discovery priority order — first is best.
    let available_resources = advertised.snapshot();
    let res = serialize_framed(&ServerMessage::Advertise {
        available_resources: available_resources.clone(),
    })?;
    stream.write_all(&res).await.map_err(ServerError::Write)?;
    debug!("[client {client_id} (pid {client_pid})] Sent Advertise: {available_resources:?}");

    let mut ack_deadline: Option<Instant> = None;
    // Bytes read from the wire but not yet assembled into a complete frame.
    // Persists across select! iterations: a frame split across reads must
    // survive even when the control channel wins the next select.
    let mut frame_buf: Vec<u8> = Vec::new();

    // Wrap the loop so engine.disconnected() ALWAYS runs, even when an arm
    // propagates an error (e.g. EPIPE writing to a client that just died) —
    // otherwise the engine would keep a dead client as owner forever.
    let result: ServerResult<()> = async {
        loop {
            tokio::select! {
                // Wire: a client request (never blocks on a partial frame —
                // see wire::process_wire_bytes; the control channel stays live).
                _ = stream.readable() => {
                    if !wire::process_wire_bytes(
                        &mut stream,
                        &mut frame_buf,
                        client_id,
                        client_pid,
                        &engine,
                        &inventory,
                        &mut ack_deadline,
                    ).await? {
                        break;
                    }
                }
                // Engine: server-pushed Revoke / Grant-from-queue.
                Some(msg) = control_rx.recv() => {
                    control::process_control_message(
                        &mut stream,
                        client_id,
                        client_pid,
                        &engine,
                        msg,
                        &inventory,
                        &mut ack_deadline,
                    ).await?;
                }
                // Grant delivery ack timeout (log-only; never gates ownership).
                // NOTE: select! evaluates this future EXPRESSION eagerly,
                // before any if-guard would be checked — so it must not
                // panic on None. Pending forever while no grant awaits an ack.
                _ = async {
                    if let Some(deadline) = ack_deadline {
                        tokio::time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    warn!("[client {client_id} (pid {client_pid})] Grant not acknowledged within {GRANT_ACK_TIMEOUT:?}");
                    ack_deadline = None;
                }
            }
        }
        Ok(())
    }
    .await;

    info!("[client {client_id} (pid {client_pid})] Client disconnected");
    // The engine's own bookkeeping first (the resources it granted), then what
    // never went through it: the overlay planes, which a client owns directly.
    engine.disconnected(client_id).await;
    inventory.release_client(client_id);
    result?;

    Ok(())
}
