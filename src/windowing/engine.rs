//! The engine: a single actor owning one slot per resource. It enforces the
//! policy — grant, revoke handshake, waiter queues, timeouts — and pushes
//! Revoke/Grant control messages to the connection tasks, which own the
//! streams and write them on the wire.

use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

use simple_graphics_protocol::{PlaneRole, Resource};
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, timeout_at},
};
use tracing::{debug, info, warn};

use crate::types::ClientId;

use super::policy::{AcquireDecision, Policy, SlotState};

/// How long the engine waits for a revoked owner to Release before forcing
/// the resource free (the owner is NOT requeued in that case).
pub const REVOKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Messages the server pushes to a connection task (which owns the stream).
#[derive(Debug, Clone)]
pub enum ControlMessage {
    /// Evict the client: write `ServerMessage::Revoke` on the wire.
    Revoke { resource: Resource },
    /// The client is granted (after being queued or requeued): write
    /// `ServerMessage::Grant` with the matching fd on the wire.
    Grant { resource: Resource },
    /// The server's resource list changed (a device appeared or went away):
    /// write `ServerMessage::Advertise` with the current list. Server-level
    /// state rather than ownership, routed through the engine because the
    /// engine is what knows every connected client.
    Advertise { available_resources: Vec<Resource> },
}

/// Outcome of an `Acquire` request.
#[derive(Debug)]
pub enum AcquireOutcome {
    /// Granted immediately; the caller sends Grant + fds.
    Granted,
    /// Queued; a Grant will arrive later via the control channel.
    Queued,
    /// Denied; never granted. The reason is human-readable for `Deny`.
    Denied { reason: String },
}

/// A client waiting for a resource.
#[derive(Debug, Clone, Copy)]
struct Waiter {
    client: ClientId,
    /// Waiting as the app on the DISPLAY, taking a device from a client that has
    /// none. Such a claim is served before any other waiter and regardless of
    /// policy — `first-owner` never serves waiters, but it has to serve this one,
    /// or the app on screen would never get its device back.
    seat_claim: bool,
}

/// Per-resource slot: owner, waiters, and revoke bookkeeping.
struct Slot {
    policy: Policy,
    owner: Option<ClientId>,
    waiters: VecDeque<Waiter>,
    /// Set while a Revoke is in flight (awaiting the owner's Release).
    revoke_deadline: Option<Instant>,
    /// Set while the device behind this resource is absent (unplugged, gone
    /// from `/dev`): the owner KEEPS the resource — no revoke — and is handed a
    /// fresh fd when the device comes back. Nothing else can take it in the
    /// meantime.
    suspended: bool,
}

impl Slot {
    fn state(&self) -> SlotState {
        match (self.owner, self.revoke_deadline) {
            (None, _) => SlotState::Free,
            (Some(_), None) => SlotState::Granted,
            (Some(_), Some(_)) => SlotState::Revoking,
        }
    }

    /// Is this client already waiting? (dedup guard — no double entries)
    fn is_queued(&self, client: ClientId) -> bool {
        self.waiters.iter().any(|w| w.client == client)
    }

    /// Take the seat's claim out of the queue, if it has one: the app on the
    /// display is served before anyone else, whatever the policy's order is.
    fn pop_seat_claim(&mut self) -> Option<Waiter> {
        let index = self.waiters.iter().position(|w| w.seat_claim)?;
        self.waiters.remove(index)
    }

    fn pop_waiter(&mut self) -> Option<Waiter> {
        self.policy.pop_waiter(&mut self.waiters)
    }
}

/// Commands from connection tasks to the engine.
enum EngineCommand {
    /// Write an ordinary `Grant` for `resource` to `client`: how a client that
    /// kept a claim across a pipeline change is told it has the plane of the
    /// new pipeline. Deliberately not a special message — the client's normal
    /// grant path has to handle it.
    GrantTo {
        client: ClientId,
        resource: Resource,
    },
    /// The device behind `resource` is gone. Its holder keeps the resource (no
    /// revoke, no re-acquire when it returns).
    Suspend {
        resource: Resource,
    },
    /// The device behind `resource` is back: hand the holder a fresh fd.
    Resume {
        resource: Resource,
    },
    Register {
        client: ClientId,
        control: mpsc::UnboundedSender<ControlMessage>,
    },
    Acquire {
        client: ClientId,
        resource: Resource,
        reply: oneshot::Sender<AcquireOutcome>,
    },
    Release {
        client: ClientId,
        resource: Resource,
    },
    Disconnected {
        client: ClientId,
    },
    /// A resource appeared (a device was plugged in): add its slot with the
    /// server's policy so it can be acquired from now on.
    Offer {
        resource: Resource,
        policy: Policy,
    },
    /// Push a message to every connected client (the resource list changed).
    Broadcast {
        message: ControlMessage,
    },
}

/// Handle to the policy engine: a single actor task owning all state.
#[derive(Clone)]
pub struct PolicyEngine {
    tx: mpsc::Sender<EngineCommand>,
}

impl PolicyEngine {
    /// Spawn the engine with one slot per resource in `policies`.
    pub fn spawn(policies: HashMap<Resource, Policy>) -> Self {
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move { run_engine(rx, policies).await });
        Self { tx }
    }

    pub async fn register(&self, client: ClientId, control: mpsc::UnboundedSender<ControlMessage>) {
        let _ = self
            .tx
            .send(EngineCommand::Register { client, control })
            .await;
    }

    pub async fn acquire(&self, client: ClientId, resource: Resource) -> AcquireOutcome {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(EngineCommand::Acquire {
                client,
                resource,
                reply,
            })
            .await
            .is_err()
        {
            return AcquireOutcome::Denied {
                reason: "policy engine unavailable".into(),
            };
        }
        rx.await.unwrap_or(AcquireOutcome::Denied {
            reason: "policy engine unavailable".into(),
        })
    }

    /// Put `resource` into `client`'s hands with an ordinary Grant.
    pub async fn grant_to(&self, client: ClientId, resource: Resource) {
        let _ = self
            .tx
            .send(EngineCommand::GrantTo { client, resource })
            .await;
    }

    pub async fn release(&self, client: ClientId, resource: Resource) {
        let _ = self
            .tx
            .send(EngineCommand::Release { client, resource })
            .await;
    }

    pub async fn disconnected(&self, client: ClientId) {
        let _ = self.tx.send(EngineCommand::Disconnected { client }).await;
    }

    /// Start offering `resource`: its slot is created with `policy` so a
    /// connecting client can acquire it. Idempotent — offering a resource that
    /// already has a slot (a re-created device node) changes nothing.
    pub async fn offer(&self, resource: Resource, policy: Policy) {
        let _ = self
            .tx
            .send(EngineCommand::Offer { resource, policy })
            .await;
    }

    /// The device behind `resource` is gone: whoever holds it keeps it and is
    /// handed a fresh fd when it comes back (`resume`). No revoke: for a
    /// keyboard or a mouse that was unplugged, "you lost your device" is the
    /// wrong answer — the device is coming back.
    pub async fn suspend(&self, resource: Resource) {
        let _ = self.tx.send(EngineCommand::Suspend { resource }).await;
    }

    /// The device behind `resource` is back. A holder is re-granted the same
    /// resource with a fresh fd — it never lost it, so it has nothing to
    /// re-acquire.
    pub async fn resume(&self, resource: Resource) {
        let _ = self.tx.send(EngineCommand::Resume { resource }).await;
    }

    /// Send `message` to every registered client. Connections that have gone
    /// away are skipped: their task is cleaning up, and a failed send to a
    /// dead channel is not a server error.
    pub async fn broadcast(&self, message: ControlMessage) {
        let _ = self.tx.send(EngineCommand::Broadcast { message }).await;
    }
}

async fn run_engine(mut rx: mpsc::Receiver<EngineCommand>, policies: HashMap<Resource, Policy>) {
    let mut slots: HashMap<Resource, Slot> = policies
        .into_iter()
        .map(|(resource, policy)| {
            (
                resource,
                Slot {
                    policy,
                    owner: None,
                    waiters: VecDeque::new(),
                    revoke_deadline: None,
                    suspended: false,
                },
            )
        })
        .collect();
    let mut control_reg: HashMap<ClientId, mpsc::UnboundedSender<ControlMessage>> = HashMap::new();

    loop {
        // Sleep until the earliest revoke deadline, or forever if none.
        let deadline = slots.values().filter_map(|slot| slot.revoke_deadline).min();
        let cmd = match deadline {
            Some(d) => timeout_at(d, rx.recv()).await,
            None => Ok(rx.recv().await),
        };

        match cmd {
            Ok(Some(cmd)) => handle_command(cmd, &mut slots, &mut control_reg),
            Ok(None) => break, // all connection tasks gone; engine shuts down
            Err(_) => {
                // Revoke deadline(s) elapsed: force-reclaim silent owners.
                let mut ex_seats: Vec<ClientId> = Vec::new();
                for (resource, slot) in slots.iter_mut() {
                    if let Some(deadline) = slot.revoke_deadline
                        && deadline <= Instant::now()
                    {
                        warn!(
                            "Force-reclaiming {resource:?}: owner {} did not \
                                 Release after Revoke",
                            slot.owner.map(|o| o.to_string()).unwrap_or_default()
                        );
                        // A display taken away ends that client's seat, and the
                        // seat's inputs go with it.
                        if is_display(resource)
                            && let Some(ex_seat) = slot.owner
                        {
                            ex_seats.push(ex_seat);
                        }
                        force_reclaim(resource, slot, &control_reg);
                    }
                }
                for ex_seat in ex_seats {
                    release_seat_inputs(ex_seat, &mut slots, &control_reg);
                }
            }
        }
    }
}

/// Is this a display resource — something the app on screen holds? A DRM card
/// lease or the framebuffer: whoever owns one of these IS the seat, and there is
/// exactly one seat (the daemon does not support several displays).
fn is_display(resource: &Resource) -> bool {
    match resource {
        // The PRIMARY plane is the seat. An overlay plane is hardware
        // compositing above whoever holds the display, so it is never the app
        // on screen (it is refused outright today, see `handle_acquire`).
        Resource::Drm { plane, .. } => *plane == PlaneRole::Primary,
        Resource::Fbdev => true,
        _ => false,
    }
}

/// Does `client` hold the display? Input belongs to the app on screen (see
/// [`acquire_one`]), so this is the test for whether a client may hold a device
/// at all.
fn holds_display(slots: &HashMap<Resource, Slot>, client: ClientId) -> bool {
    slots
        .iter()
        .any(|(resource, slot)| is_display(resource) && slot.owner == Some(client))
}

/// The seat changed hands or was vacated: whoever sat in it must not keep input
/// nobody can see. Ask every input the former seat holds for a Release (the same
/// handshake as a preemption — the device frees when it answers, and a waiter is
/// served normally).
fn release_seat_inputs(
    ex_seat: ClientId,
    slots: &mut HashMap<Resource, Slot>,
    control_reg: &HashMap<ClientId, mpsc::UnboundedSender<ControlMessage>>,
) {
    let held: Vec<Resource> = slots
        .iter()
        .filter(|(resource, slot)| {
            matches!(resource, Resource::Input(_)) && slot.owner == Some(ex_seat)
        })
        .map(|(resource, _)| resource.clone())
        .collect();

    for resource in held {
        let Some(control) = control_reg.get(&ex_seat) else {
            // The connection is already gone; its Disconnected handling frees
            // whatever it owned.
            continue;
        };
        let Some(slot) = slots.get_mut(&resource) else {
            continue;
        };
        let _ = control.send(ControlMessage::Revoke {
            resource: resource.clone(),
        });
        slot.revoke_deadline = Some(Instant::now() + REVOKE_TIMEOUT);
        info!("[client {ex_seat}] left the seat; revoking {resource:?} with it");
    }
}

/// Apply the policy for one resource: grant / queue with owner revoke /
/// deny. Returns the outcome; the caller replies.
fn acquire_one(
    client: ClientId,
    resource: Resource,
    slots: &mut HashMap<Resource, Slot>,
    control_reg: &mut HashMap<ClientId, mpsc::UnboundedSender<ControlMessage>>,
) -> AcquireOutcome {
    // Precomputed: the checks below need the slot mutably, and "who is the app
    // on screen" does not depend on the slot being asked for.
    let requester_is_seat = holds_display(slots, client);
    let holder = slots.get(&resource).and_then(|slot| slot.owner);
    let holder_is_seat = holder.is_some_and(|owner| holds_display(slots, owner));

    let Some(slot) = slots.get_mut(&resource) else {
        return AcquireOutcome::Denied {
            reason: format!("{resource:?} is not registered"),
        };
    };

    if slot.owner == Some(client) {
        return AcquireOutcome::Denied {
            reason: format!("{resource:?} is already owned by this client"),
        };
    }

    // The device is absent: the resource is out of the advertised list while it
    // is away, and its holder keeps it — there is nothing to take here, and
    // nothing to steal.
    if slot.suspended {
        return AcquireOutcome::Denied {
            reason: format!(
                "{resource:?} is not available while its device is away — its holder keeps it"
            ),
        };
    }

    // Input is owned by CLASS: the app on the display takes a device from a
    // client that has none, and a client with no display never takes one from
    // anybody. It may still hold a device nobody wants (free), but it never
    // queues for one: a low-priority waiter would be served ahead of the app on
    // screen the moment the device freed, and the app on screen would wait
    // behind a background client.
    if matches!(resource, Resource::Input(_))
        && !requester_is_seat
        && let Some(owner) = holder
    {
        let reason = format!(
            "{resource:?} is held by client {owner} — only the app on the display can take it"
        );
        warn!("[client {client}] denied {resource:?}: {reason}");
        return AcquireOutcome::Denied { reason };
    }

    // The seat's claim outranks a holder that is not on the display: it takes
    // the device whatever the policy says — `first-owner` does not protect a
    // background holder from the app on screen.
    let seat_steal = matches!(resource, Resource::Input(_))
        && requester_is_seat
        && holder.is_some()
        && !holder_is_seat;
    if seat_steal {
        info!(
            "[client {client}] takes {resource:?} from client {} — the app on the display outranks it",
            holder.map(|o| o.to_string()).unwrap_or_default()
        );
    }
    let decision = if seat_steal {
        AcquireDecision::RevokeAndQueue
    } else {
        slot.policy.decide(slot.state())
    };

    match decision {
        AcquireDecision::Grant => {
            slot.owner = Some(client);
            info!("[client {client}] acquired {resource:?}");
            AcquireOutcome::Granted
        }
        AcquireDecision::Deny => {
            let reason = match slot.owner {
                Some(owner) => format!("{resource:?} is owned by client {owner}"),
                None => format!("{resource:?} is not available"),
            };
            warn!("[client {client}] denied {resource:?}: {reason}");
            AcquireOutcome::Denied { reason }
        }
        AcquireDecision::RevokeAndQueue | AcquireDecision::Queue => {
            if slot.is_queued(client) {
                debug!("[client {client}] already queued for {resource:?}; keeping position");
                return AcquireOutcome::Queued;
            }
            slot.waiters.push_back(Waiter {
                client,
                seat_claim: seat_steal,
            });
            debug!(
                "[client {client}] queued for {resource:?} ({} waiting)",
                slot.waiters.len()
            );

            // First waiter: start the revoke of the current owner.
            if slot.revoke_deadline.is_none()
                && let Some(owner) = slot.owner
            {
                if let Some(control) = control_reg.get(&owner) {
                    let _ = control.send(ControlMessage::Revoke {
                        resource: resource.clone(),
                    });
                    slot.revoke_deadline = Some(Instant::now() + REVOKE_TIMEOUT);
                    info!(
                        "[client {client}] preempting client {owner} on \
                         {resource:?}; Revoke sent"
                    );
                } else {
                    // Owner vanished without a Disconnected; reclaim.
                    slot.owner = None;
                    grant_next(&resource, slot, control_reg);
                }
            }
            AcquireOutcome::Queued
        }
    }
}

/// Handle a Release: validate ownership, free the slot (requeueing the
/// revoked owner for one more turn when a revoke was in flight), and hand
/// the resource to the next waiter.
fn release_one(
    client: ClientId,
    resource: &Resource,
    slots: &mut HashMap<Resource, Slot>,
    control_reg: &mut HashMap<ClientId, mpsc::UnboundedSender<ControlMessage>>,
) {
    let Some(slot) = slots.get_mut(resource) else {
        warn!("[client {client}] release of unregistered {resource:?} ignored");
        return;
    };
    if slot.owner != Some(client) {
        warn!("[client {client}] cannot release {resource:?}: not the owner");
        return;
    }

    let revoking = slot.revoke_deadline.is_some();
    slot.owner = None;
    slot.revoke_deadline = None;
    if revoking && !slot.waiters.is_empty() {
        // Clean revoke-ack: the preempted owner gets one more turn (not as a
        // seat claim — it is not the app on screen).
        slot.policy.requeue_waiter(
            &mut slot.waiters,
            Waiter {
                client,
                seat_claim: false,
            },
        );
        info!("[client {client}] released {resource:?} after Revoke; requeued");
    } else {
        // Spontaneous release, or the revoke's preemptor(s) are gone:
        // no requeue — requeueing with an empty queue would regrant
        // the resource straight back to the releaser.
        info!("[client {client}] released {resource:?}");
    }
    grant_next(resource, slot, control_reg);
}

fn handle_command(
    cmd: EngineCommand,
    slots: &mut HashMap<Resource, Slot>,
    control_reg: &mut HashMap<ClientId, mpsc::UnboundedSender<ControlMessage>>,
) {
    match cmd {
        EngineCommand::Register { client, control } => {
            control_reg.insert(client, control);
            debug!("[client {client}] registered with policy engine");
        }
        EngineCommand::GrantTo { client, resource } => match control_reg.get(&client) {
            Some(control) => {
                debug!("Grant to client {client} for {resource:?} (held claim)");
                let _ = control.send(ControlMessage::Grant { resource });
            }
            None => warn!("Grant to client {client} for {resource:?}: no connection"),
        },
        EngineCommand::Disconnected { client } => {
            control_reg.remove(&client);
            for (resource, slot) in slots.iter_mut() {
                let before = slot.waiters.len();
                slot.waiters.retain(|w| w.client != client);
                if slot.waiters.len() != before {
                    debug!("[client {client}] removed from {resource:?} waiters");
                }
                if slot.owner == Some(client) {
                    info!("[client {client}] disconnected, releasing {resource:?}");
                    slot.owner = None;
                    slot.revoke_deadline = None;
                    grant_next(resource, slot, control_reg);
                }
            }
        }
        EngineCommand::Acquire {
            client,
            resource,
            reply,
        } => {
            let outcome = acquire_one(client, resource, slots, control_reg);
            let _ = reply.send(outcome);
        }
        EngineCommand::Release { client, resource } => {
            // Releasing the display is leaving the seat, and the seat's inputs
            // go with it (computed before the release, while the client still
            // owns the slot).
            let left_seat = is_display(&resource)
                && slots.get(&resource).and_then(|slot| slot.owner) == Some(client);
            release_one(client, &resource, slots, control_reg);
            if left_seat {
                release_seat_inputs(client, slots, control_reg);
            }
        }
        EngineCommand::Broadcast { message } => {
            let text = format!("{message:?}");
            let mut delivered = 0usize;
            for control in control_reg.values() {
                if control.send(message.clone()).is_ok() {
                    delivered += 1;
                }
            }
            debug!("Pushed to {delivered} client(s): {text}");
        }
        EngineCommand::Suspend { resource } => {
            let Some(slot) = slots.get_mut(&resource) else {
                debug!("Suspend of unregistered {resource:?} ignored");
                return;
            };
            slot.suspended = true;
            // Nothing can be handed over while the device is away: drop a
            // revoke that was in flight, or its deadline would reclaim a
            // resource nobody could use.
            slot.revoke_deadline = None;
            match slot.owner {
                Some(owner) => info!(
                    "Suspended {resource:?}: client {owner} keeps it until the device is back"
                ),
                None => info!("Suspended {resource:?}: it is unavailable while its device is away"),
            }
        }
        EngineCommand::Resume { resource } => {
            let Some(slot) = slots.get_mut(&resource) else {
                debug!("Resume of unregistered {resource:?} ignored");
                return;
            };
            if !slot.suspended {
                debug!("Resume of {resource:?} that was not suspended; ignored");
                return;
            }
            slot.suspended = false;
            match slot.owner {
                // The holder never lost the resource: same client, same name, a
                // fresh fd for the device that came back.
                Some(owner) => match control_reg.get(&owner) {
                    Some(control) => {
                        info!("Re-granting {resource:?} to client {owner}: the device is back");
                        let _ = control.send(ControlMessage::Grant {
                            resource: resource.clone(),
                        });
                    }
                    None => {
                        slot.owner = None;
                        warn!("{resource:?} came back but client {owner} is gone; it stays free");
                    }
                },
                None => info!("{resource:?} is back and free"),
            }
        }
        EngineCommand::Offer { resource, policy } => {
            // Idempotent: a re-created device node keeps its slot (and with it
            // any waiter that is still queued for the name).
            if slots.contains_key(&resource) {
                debug!("{resource:?} offered again; slot kept");
                return;
            }
            slots.insert(
                resource.clone(),
                Slot {
                    policy,
                    owner: None,
                    waiters: VecDeque::new(),
                    revoke_deadline: None,
                    suspended: false,
                },
            );
            info!("Offering {resource:?} ({policy:?})");
        }
    }
}

/// Hand the resource to the next waiter (per the slot's policy).
///
/// The engine does not own streams: it pushes a `Grant` into the winner's
/// control channel; that connection's task writes it (with fds) on the wire.
/// A waiter whose channel is gone (raced disconnect) is dropped and the next
/// one is tried.
fn grant_next(
    resource: &Resource,
    slot: &mut Slot,
    control_reg: &HashMap<ClientId, mpsc::UnboundedSender<ControlMessage>>,
) {
    loop {
        // A seat claim outranks the queue order (and `first-owner`'s refusal to
        // serve waiters at all): the app on the display is never left waiting
        // behind a client that has no display.
        let waiter = match slot.pop_seat_claim() {
            Some(waiter) => waiter,
            None => match slot.pop_waiter() {
                Some(waiter) => waiter,
                None => return,
            },
        };
        match control_reg.get(&waiter.client) {
            Some(control)
                if control
                    .send(ControlMessage::Grant {
                        resource: resource.clone(),
                    })
                    .is_ok() =>
            {
                slot.owner = Some(waiter.client);
                info!("[client {}] granted {resource:?} from queue", waiter.client);
                return;
            }
            Some(_) => {
                warn!(
                    "[client {}] grant delivery failed; dropping waiter",
                    waiter.client
                );
            }
            None => {
                warn!(
                    "[client {}] no control channel; dropping waiter",
                    waiter.client
                );
            }
        }
    }
}

/// Force-free a slot whose owner never confirmed the Revoke. No requeue: a
/// wedged client does not get a queue spot.
fn force_reclaim(
    resource: &Resource,
    slot: &mut Slot,
    control_reg: &HashMap<ClientId, mpsc::UnboundedSender<ControlMessage>>,
) {
    slot.owner = None;
    slot.revoke_deadline = None;
    grant_next(resource, slot, control_reg);
}
#[cfg(test)]
mod engine_tests {
    use super::*;
    use simple_graphics_protocol::{InputResource, Resource};
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    fn fbdev() -> Resource {
        Resource::Fbdev
    }

    fn keyboard() -> Resource {
        Resource::Input(InputResource::Keyboard(0))
    }

    fn cid(n: u64) -> ClientId {
        ClientId::new(n)
    }

    fn engine(policy: Policy) -> PolicyEngine {
        PolicyEngine::spawn(HashMap::from([(fbdev(), policy)]))
    }

    /// Register a fake connection and return its control receiver.
    async fn connect(engine: &PolicyEngine, id: u64) -> mpsc::UnboundedReceiver<ControlMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        engine.register(cid(id), tx).await;
        rx
    }

    /// Wait (real time) for the next control message.
    async fn next_control(rx: &mut mpsc::UnboundedReceiver<ControlMessage>) -> ControlMessage {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("expected a control message")
            .expect("control channel closed")
    }

    /// Poll for a control message without real-time timeout (paused time).
    async fn poll_control(rx: &mut mpsc::UnboundedReceiver<ControlMessage>) -> ControlMessage {
        for _ in 0..100 {
            if let Ok(msg) = rx.try_recv() {
                return msg;
            }
            tokio::task::yield_now().await;
        }
        panic!("no control message after 100 polls");
    }

    #[tokio::test]
    async fn immediate_grant_when_free() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Granted
        ));
        // Immediate grants are sent by the task itself — nothing on control.
        assert!(a.try_recv().is_err());
    }

    #[tokio::test]
    async fn preempt_revokes_owner_then_grants_after_release() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;

        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Granted
        ));
        assert!(matches!(
            engine.acquire(cid(2), fbdev()).await,
            AcquireOutcome::Queued
        ));
        // A is told to leave.
        assert!(matches!(
            next_control(&mut a).await,
            ControlMessage::Revoke { resource } if resource == fbdev()
        ));
        // A's revoke-ack Release hands the resource to B.
        engine.release(cid(1), fbdev()).await;
        assert!(matches!(
            next_control(&mut b).await,
            ControlMessage::Grant { resource } if resource == fbdev()
        ));
    }

    #[tokio::test]
    async fn revoked_owner_is_requeued_and_gets_one_more_turn() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;

        engine.acquire(cid(1), fbdev()).await;
        engine.acquire(cid(2), fbdev()).await; // B queued, revoke sent to A
        let _ = next_control(&mut a).await; // Revoke
        engine.release(cid(1), fbdev()).await; // revoke-ack -> A requeued
        let _ = next_control(&mut b).await; // B granted
        engine.release(cid(2), fbdev()).await; // B done -> A gets one more turn
        assert!(matches!(
            next_control(&mut a).await,
            ControlMessage::Grant { .. }
        ));
    }

    #[tokio::test]
    async fn fair_queue_serves_oldest_waiter() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;
        let mut c = connect(&engine, 3).await;

        engine.acquire(cid(1), fbdev()).await;
        engine.acquire(cid(2), fbdev()).await; // queued
        engine.acquire(cid(3), fbdev()).await; // queued
        let _ = next_control(&mut a).await; // Revoke A
        engine.release(cid(1), fbdev()).await;
        // B arrived before C -> B is granted first.
        assert!(matches!(
            next_control(&mut b).await,
            ControlMessage::Grant { .. }
        ));
        assert!(c.try_recv().is_err());
    }

    #[tokio::test]
    async fn latest_owner_serves_newest_waiter() {
        let engine = engine(Policy::LatestOwner);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;
        let mut c = connect(&engine, 3).await;

        engine.acquire(cid(1), fbdev()).await;
        engine.acquire(cid(2), fbdev()).await; // queued
        engine.acquire(cid(3), fbdev()).await; // queued
        let _ = next_control(&mut a).await; // Revoke A
        engine.release(cid(1), fbdev()).await;
        // C arrived after B -> C is granted first (LIFO).
        assert!(matches!(
            next_control(&mut c).await,
            ControlMessage::Grant { .. }
        ));
        assert!(b.try_recv().is_err());
    }

    #[tokio::test]
    async fn first_owner_denies_new_acquires() {
        let engine = engine(Policy::FirstOwner);
        let _a = connect(&engine, 1).await;
        let _b = connect(&engine, 2).await;

        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Granted
        ));
        assert!(matches!(
            engine.acquire(cid(2), fbdev()).await,
            AcquireOutcome::Denied { .. }
        ));
    }

    #[tokio::test]
    async fn double_acquire_by_owner_is_denied() {
        let engine = engine(Policy::FairQueue);
        let _a = connect(&engine, 1).await;
        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Granted
        ));
        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Denied { reason } if reason.contains("already owned")
        ));
    }

    #[tokio::test]
    async fn unregistered_resource_is_denied() {
        let engine = PolicyEngine::spawn(HashMap::new());
        let _a = connect(&engine, 1).await;
        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Denied { reason } if reason.contains("not registered")
        ));
    }

    #[tokio::test]
    async fn disconnect_while_queued_removes_waiter() {
        let engine = engine(Policy::FairQueue);
        let _a = connect(&engine, 1).await;
        let _b = connect(&engine, 2).await;
        let mut c = connect(&engine, 3).await;

        engine.acquire(cid(1), fbdev()).await;
        assert!(matches!(
            engine.acquire(cid(2), fbdev()).await,
            AcquireOutcome::Queued
        ));
        // B gives up and disconnects while queued.
        engine.disconnected(cid(2)).await;
        engine.release(cid(1), fbdev()).await;
        // No waiter left: a fresh Acquire is granted immediately.
        assert!(matches!(
            engine.acquire(cid(3), fbdev()).await,
            AcquireOutcome::Granted
        ));
        assert!(c.try_recv().is_err());
    }

    #[tokio::test]
    async fn owner_disconnect_grants_queued_waiter() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;

        engine.acquire(cid(1), fbdev()).await;
        engine.acquire(cid(2), fbdev()).await; // queued, revoke sent
        let _ = next_control(&mut a).await; // Revoke
        // A dies instead of releasing.
        engine.disconnected(cid(1)).await;
        assert!(matches!(
            next_control(&mut b).await,
            ControlMessage::Grant { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn silent_owner_is_force_reclaimed_after_timeout() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;

        engine.acquire(cid(1), fbdev()).await;
        engine.acquire(cid(2), fbdev()).await; // queued, revoke sent
        let _ = poll_control(&mut a).await; // Revoke

        // A never releases. Advance past the revoke deadline.
        tokio::time::advance(REVOKE_TIMEOUT + Duration::from_secs(1)).await;
        let msg = poll_control(&mut b).await;
        assert!(matches!(msg, ControlMessage::Grant { .. }));
    }

    #[tokio::test]
    async fn offered_resource_becomes_acquirable() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let keyboard = keyboard();

        // Nothing registered it yet.
        assert!(matches!(
            engine.acquire(cid(1), keyboard.clone()).await,
            AcquireOutcome::Denied { .. }
        ));

        engine.offer(keyboard.clone(), Policy::FairQueue).await;
        // Offering twice is what a re-created device node looks like: the slot
        // is kept, not duplicated.
        engine.offer(keyboard.clone(), Policy::FairQueue).await;
        // Input goes with the display, so the display comes first.
        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Granted
        ));
        assert!(matches!(
            engine.acquire(cid(1), keyboard.clone()).await,
            AcquireOutcome::Granted
        ));
        // The immediate grants go out on the client's own wire write.
        assert!(a.try_recv().is_err());
    }

    #[tokio::test]
    async fn input_without_a_display_is_low_priority() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;
        let keyboard = keyboard();
        // first-owner on the device, so the seat's claim has to beat that too.
        engine.offer(keyboard.clone(), Policy::FirstOwner).await;

        // A has no display: it may hold a device nobody wants...
        assert!(matches!(
            engine.acquire(cid(1), keyboard.clone()).await,
            AcquireOutcome::Granted
        ));
        // ...but another client with no display cannot take it, or even wait for
        // it: a queued low-priority waiter would be served ahead of the seat.
        assert!(matches!(
            engine.acquire(cid(2), keyboard.clone()).await,
            AcquireOutcome::Denied { reason } if reason.contains("only the app on the display")
        ));
        assert!(b.try_recv().is_err());

        // B takes the display, so B is the app on screen: it takes the device
        // from A whatever the resource's policy says.
        assert!(matches!(
            engine.acquire(cid(2), fbdev()).await,
            AcquireOutcome::Granted
        ));
        assert!(matches!(
            engine.acquire(cid(2), keyboard.clone()).await,
            AcquireOutcome::Queued
        ));
        assert!(matches!(
            next_control(&mut a).await,
            ControlMessage::Revoke { resource } if resource == keyboard
        ));
        engine.release(cid(1), keyboard.clone()).await;
        assert!(matches!(
            next_control(&mut b).await,
            ControlMessage::Grant { resource } if resource == keyboard
        ));
    }

    #[tokio::test]
    async fn leaving_the_seat_releases_its_inputs() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;
        let keyboard = keyboard();
        engine.offer(keyboard.clone(), Policy::FairQueue).await;

        // A is the app on screen: display, then its device.
        assert!(matches!(
            engine.acquire(cid(1), fbdev()).await,
            AcquireOutcome::Granted
        ));
        assert!(matches!(
            engine.acquire(cid(1), keyboard.clone()).await,
            AcquireOutcome::Granted
        ));

        // A gives the display up: nobody keeps input off-screen, so A is asked
        // for the keyboard with it.
        engine.release(cid(1), fbdev()).await;
        assert!(matches!(
            next_control(&mut a).await,
            ControlMessage::Revoke { resource } if resource == keyboard
        ));

        // B becomes the app on screen and can take the device once A answers.
        assert!(matches!(
            engine.acquire(cid(2), fbdev()).await,
            AcquireOutcome::Granted
        ));
        engine.release(cid(1), keyboard.clone()).await;
        assert!(matches!(
            engine.acquire(cid(2), keyboard.clone()).await,
            AcquireOutcome::Granted
        ));
        assert!(b.try_recv().is_err());
    }

    #[tokio::test]
    async fn preempted_seat_loses_its_inputs() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let mut b = connect(&engine, 2).await;
        let keyboard = keyboard();
        engine.offer(keyboard.clone(), Policy::FairQueue).await;

        engine.acquire(cid(1), fbdev()).await;
        engine.acquire(cid(1), keyboard.clone()).await;

        // B takes the screen: A is asked for the display...
        assert!(matches!(
            engine.acquire(cid(2), fbdev()).await,
            AcquireOutcome::Queued
        ));
        assert!(matches!(
            next_control(&mut a).await,
            ControlMessage::Revoke { resource } if resource == Resource::Fbdev
        ));

        // ...and when A hands it over, the input goes with the seat.
        engine.release(cid(1), fbdev()).await;
        assert!(matches!(
            next_control(&mut b).await,
            ControlMessage::Grant { resource } if resource == Resource::Fbdev
        ));
        assert!(matches!(
            next_control(&mut a).await,
            ControlMessage::Revoke { resource } if resource == keyboard
        ));
        engine.release(cid(1), keyboard.clone()).await;
        assert!(matches!(
            engine.acquire(cid(2), keyboard.clone()).await,
            AcquireOutcome::Granted
        ));
    }

    /// A device that is unplugged is NOT revoked: its holder keeps the
    /// resource, nobody else can take it, and the client is handed a fresh fd
    /// when the device comes back — no re-acquire, no gap for a race.
    #[tokio::test]
    async fn an_unplugged_device_keeps_its_holder() {
        let engine = engine(Policy::FairQueue);
        let mut a = connect(&engine, 1).await;
        let keyboard = keyboard();

        engine.offer(keyboard.clone(), Policy::FairQueue).await;
        engine.acquire(cid(1), fbdev()).await; // A is the app on screen
        assert!(matches!(
            engine.acquire(cid(1), keyboard.clone()).await,
            AcquireOutcome::Granted
        ));

        // The device goes away.
        engine.suspend(keyboard.clone()).await;
        // Nobody is revoked — there is nothing on A's control channel.
        assert!(a.try_recv().is_err());
        // And nothing else can take the name while the device is away.
        assert!(matches!(
            engine.acquire(cid(2), keyboard.clone()).await,
            AcquireOutcome::Denied { reason } if reason.contains("device is away")
        ));

        // The device is back: same client, same resource, fresh fd.
        engine.resume(keyboard.clone()).await;
        assert!(matches!(
            next_control(&mut a).await,
            ControlMessage::Grant { resource } if resource == keyboard
        ));
        // Still A's: only A releasing it can change that.
        assert!(matches!(
            engine.acquire(cid(1), keyboard.clone()).await,
            AcquireOutcome::Denied { reason } if reason.contains("already owned")
        ));
        engine.release(cid(1), keyboard.clone()).await;
        assert!(matches!(
            engine.acquire(cid(2), keyboard.clone()).await,
            AcquireOutcome::Granted
        ));
    }

    /// A suspended device whose holder went away comes back to nobody: it is
    /// simply available again (the resource layer re-advertises it).
    #[tokio::test]
    async fn a_suspended_device_outlives_its_holder() {
        let engine = engine(Policy::FairQueue);
        engine.offer(keyboard(), Policy::FairQueue).await;
        engine.acquire(cid(1), keyboard()).await;
        engine.suspend(keyboard()).await;
        engine.disconnected(cid(1)).await;
        engine.resume(keyboard()).await;

        assert!(matches!(
            engine.acquire(cid(2), keyboard()).await,
            AcquireOutcome::Granted
        ));
    }
}
