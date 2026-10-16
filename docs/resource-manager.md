# Resource manager — backends, features, and DRM leases

The daemon's resource layer: how each backend (fbdev, DRM, input) is opened,
registered, granted, and reclaimed — and how backends are selected at build
time.

Source: `src/resource_manager/`. The arbitration of
*who owns what* is the policy engine ([policy-engine.md](policy-engine.md)),
which is orthogonal: it sees only the advertised resource list and never
touches device nodes. The wire format is specified in the protocol crate's
`docs/PROTOCOL.md` (repo: simple-graphics-protocol).

## Backends are compile-time features

Each backend is an optional Cargo feature. A build includes exactly the
backends it was compiled with; unbuilt backends do not exist in the binary —
no module, no dependency, no `/dev` probing.

| feature | module                    | dependency         | device              | registers                    |
| ------- | ------------------------- | ------------------ | ------------------- | ---------------------------- |
| `fbdev` | `resource_manager::fbdev` | -                  | `/dev/fb0`          | `Resource::Fbdev` in fds     |
| `drm`   | `resource_manager::drm`   | `drm` (optional)   | `/dev/dri/cardN`    | `Resource::drm(card)` — the card's primary plane — in drm registry |
| `input` | `resource_manager::input` | `evdev` (optional) | `/dev/input/event*` | `Resource::Input(_)` in fds |

Default features: **`drm` + `input`**. `fbdev` is opt-in (legacy path kept
for boards without DRM).

### The plane a request names

A card is offered per plane role (`simple_graphics_protocol::PlaneRole`).
`Resource::drm(card)` is the card's **primary** plane, the one an app on the
display scans out through; `Resource::drm_overlay(card)` is an overlay plane of
the same pipeline, for hardware compositing above whoever holds the primary.

Only the primary is granted today. The lease this server creates carries each
CRTC, its display connectors and its primary plane — overlay and cursor planes
stay with the server — so there is no overlay in it to hand over. An overlay
acquire is logged and denied with a reason, before the policy engine: a guest
plane must never queue for, or preempt, the app on the display. The role lives
in the protocol already so a later grant can grow into it without changing the
shape of a request.

```toml
[features]
default = ["drm", "input"]
fbdev   = []
drm     = ["dep:drm"]
input   = ["dep:evdev"]

[dependencies]
drm   = { version = "0.15", optional = true }
evdev = { version = "0.12", optional = true }
```

`dep:` syntax ties the optional dependency to the feature explicitly — the
`drm` crate is compiled and linked only when the `drm` feature is on, and the
same for `evdev`/`input`.

The protocol crate (`simple-graphics-protocol`) is deliberately **ungated**:
its `Resource` enum always has every variant, because the wire format must not
depend on how the server was built (a client cannot parse messages the server
was compiled unable to send, and vice versa). An unbuilt backend is simply
never *advertised* — the engine builds slots only from the advertised list, so
an `Acquire` for an unbuilt backend is denied with "not registered" exactly
like any other unknown resource. Gating lives at the module/dependency
boundary, never in the wire types.

## Module layout

```
resource_manager/
  mod.rs      Inventory, open_resources(), check_consistency()
  fbdev.rs    #[cfg(feature = "fbdev")]  open fbdev
  drm.rs      #[cfg(feature = "drm")]    DrmCard, DrmDevice, DrmRegistry
  input.rs    #[cfg(feature = "input")]  discovery + classification
```

```rust
/// Everything the daemon holds. Fields private: every mutation goes through a
/// method, because the order in which the fd table and the input index move is
/// what keeps them consistent.
#[derive(Clone)]
pub struct Inventory {
    fds: ResourceRegistry,          // static fds — grants are dups of these
    #[cfg(feature = "drm")]
    drm: DrmRegistry,               // lease factories — a fresh lease per grant
    #[cfg(feature = "input")]
    inputs: InputIndex,             // the held input devices, by devnode
}
```

`open_resources()` runs only the enabled openers and returns the `Inventory`
plus the `advertised` list in priority order (first
is best). The advertised list is the single source of truth for the engine's
slots and the policy map — never registry keys.

`Inventory` also carries the grant side of the request path (`grant_fd`,
`revoke_lease`), and the input transitions live in `impl Inventory` in
`hotplug.rs`, next to the reconciler that calls them.

## What owns what

Four structures describe the same devices from four different angles, and each
fact has exactly one owner:

| structure | owns | keyed by |
| --- | --- | --- |
| `PolicyEngine` | ownership and arbitration — who holds what, what is queued, what is suspended *to other clients* | `Resource` |
| `Inventory.fds` | the grant *source* for Fbdev and Input: one canonical fd per resource, duplicated on grant | `Resource` |
| `Inventory.drm` | the grant *source* for DRM: one lease factory per card, a fresh kernel lease per grant | `Resource` |
| `InputIndex` | device identity (`dev`, `ino`, sysfs) and whether the devnode we opened is still ours | devnode path |
| `AdvertisedResources` | a *derived view* — what the daemon offers, in priority order | — |

`InputIndex` is not a grant source and holds no fd: it is the reconciler's
bookkeeping, which is why an input device is described in two places and why
`HeldInput.resource` is the only link between them.

## The invariants between them

Four rules tie those structures together. They are asserted in
`check_consistency()` — called by unit tests that build states by hand, and at
the end of every reconcile pass in debug builds, where it logs rather than
panics (a violation is a bug in the code above, not a reason to drop a board's
session):

1. one index entry per resource, keyed by the devnode it sits on;
2. a LIVE entry (`device_gone` false) is advertised, and its resource has an fd to
   grant;
3. a `device_gone` entry is neither advertised nor grantable — the holder keeps the
   name, the daemon keeps no way to hand it out;
4. every advertised input has an index entry, and every input fd belongs to an
   advertised resource.

The two flags are deliberately not the same word, because they are not the
same fact: the engine's `suspended` means "unavailable to other clients", while
the index's `HeldInput::device_gone` means "the devnode we opened is not there".
One transition sets both, which is why they always agree — but only the engine's
decides who may acquire.

The transitions keep them true by their order, which is why that order is
load-bearing: `suspend()` removes the resource from the advertised list and tells
the engine *before* it drops the fd, so no grant can land in between;
`resume()` registers the fd *before* the engine re-grants, because the grant is
a dup of that registry entry.

## Two registry kinds

- **fds registry** (`Arc<DashMap<Resource, OwnedFd>>`) — fbdev and input.
  The server opens the device once and keeps the fd; a grant is a
  `try_clone()` dup of that fd. The client's fd is the server's fd: same
  semantics, cooperative close.
- **drm registry** (`Arc<DashMap<Resource, DrmDevice>>`) — DRM cards. The
  server's own fd (the master) must never reach a client — it would hand out
  DRM master. Grants are *fresh lease fds*, created by the kernel on demand
  and revoked (kernel-enforced) on reclaim.

## DRM card lifecycle — a type-state machine

A card's usable life is two states, and the type system makes every other
transition unrepresentable:

```mermaid
stateDiagram-v2
    [*] --> Locked: probe ok<br/>(master acquired, display-capable)
    Locked --> Leased: grant_lease()<br/>create_lease over crtcs+connectors+planes
    Leased --> Locked: revoke_lease()<br/>kernel revoke, then drop lease fd
    Locked --> [*]: server shutdown<br/>(master fd closes, leases die)
```

Cards that fail the probe (no master, no display connector, un-leasable)
never enter the registry — there is no `Available` state in the map, because
an unregistered card is indistinguishable from a skipped one and the engine
denies it. Discovery order *is* advertised priority (connected first, then
lowest index).

```rust
/// Master fd wrapper (O_RDWR | O_CLOEXEC | O_NONBLOCK).
pub struct DrmCard(File);
impl drm::Device for DrmCard {}
impl drm::control::Device for DrmCard {}

/// A live kernel lease: id for revoke, fd for the client (and as a handle
/// that keeps the lease alive while the server holds it).
pub struct DrmLease {
    id: LeaseId,
    fd: OwnedFd,
}

/// Master held, leaseable.
pub struct LockedDrmDevice {
    card: DrmCard,
}

/// Master held + one live lease.
pub struct LeasedDrmDevice {
    card: DrmCard,
    lease: DrmLease,
}

impl LockedDrmDevice {
    /// Query the card's objects fresh and lease them all.
    pub fn create_lease(self) -> Result<LeasedDrmDevice, Self>;
}

impl LeasedDrmDevice {
    /// Kernel revoke; on success drop the lease fd and return to Locked.
    /// On failure return Err(Self) — id and fd are preserved, so the
    /// reclaim can be retried later (never lose the lease on an ioctl error).
    pub fn revoke_lease(self) -> Result<LockedDrmDevice, Self>;
}
```

Consuming transitions that return `Result<Next, Self>` are the point: an
ioctl failure hands back the state you were in, so a card can never be lost
or double-leased — the failure path is a retry, not a leak.

The objects for the lease are queried fresh at `create_lease` time
(`resource_handles()` + `plane_handles()`), not cached: probe-time handles
are used for discovery/ordering only, and the lease always reflects the card
as it is now. (Probing again under the state lock is a few cheap ioctls.)

### Why the state lives behind a Mutex

The engine can push `Revoke` for an owner while a queued client's task is
concurrently running a grant on the same card — two tasks, one `DrmDevice`.
The pure remove/transition/insert dance over a shared map would race. So each
registry entry serializes its transitions:

```rust
pub struct DrmDevice {
    /// Invariant: always Some outside a transition.
    state: Mutex<Option<DrmDeviceState>>,
}

pub enum DrmDeviceState {
    Locked(LockedDrmDevice),
    Leased(LeasedDrmDevice),
}

impl DrmDevice {
    /// Grant path: stale-revoke any previous lease first (the kernel cannot
    /// re-lease the same objects until the old lease dies), then lease.
    pub fn grant_lease(&self) -> io::Result<OwnedFd> {
        let mut guard = self.state.lock().unwrap();
        let current = guard.take().expect("state always present");
        let next = match current {
            DrmDeviceState::Locked(dev) => dev.create_lease(),
            DrmDeviceState::Leased(dev) => match dev.revoke_lease() {
                Ok(dev) => dev.create_lease(),
                Err(dev) => return Err(/* lease could not be reclaimed */),
            },
        };
        match next {
            Ok(dev) => { *guard = Some(DrmDeviceState::Leased(dev)); /* hand a dup of the lease fd */ }
            Err(dev) => { *guard = Some(DrmDeviceState::Locked(dev)); Err(/* create_lease failed */) }
        }
    }

    /// Reclaim path: revoke a live lease; a failed revoke keeps the Leased
    /// state so the next reclaim can retry. No-op when Locked.
    pub fn revoke_lease(&self) {
        let mut guard = self.state.lock().unwrap();
        let current = guard.take().expect("state always present");
        *guard = Some(match current {
            DrmDeviceState::Leased(dev) => match dev.revoke_lease() {
                Ok(dev) => DrmDeviceState::Locked(dev),
                Err(dev) => DrmDeviceState::Leased(dev), // retry next time
            },
            state => state,
        });
    }
}
```

`take()`/`replace()` is the standard "move a value out from behind a Mutex"
idiom; the `Option` is only ever `None` inside a transition. The state lock
is a plain `std::sync::Mutex` — no `await` happens while it is held (lease
ioctls are fast), so the policy engine's async machinery is untouched.

Note the server keeps its copy of the lease fd inside `LeasedDrmDevice` (it
is sent to the client via `SCM_RIGHTS` as a dup, never moved). Consequence:
a client that dies without releasing does *not* free the objects by itself —
its fd closing leaves the server's copy. That is fine and deterministic: the
card is reclaimed on the next reclaim path (see below), and a lingering lease
never blocks a grant, because the grant path stale-revokes first.

## Revoke handoff — ask first, revoke at handoff

Preempting a DRM client is a two-step handoff, and the order matters. If the
server kernel-revoked the lease without warning, the client's next modeset
ioctl would fail mid-frame and it would die on an invalid lease fd before it
could finish drawing or tear down. So the wire `Revoke` is an *ask*, and the
kernel `revoke_lease()` ioctl happens only at the handoff — when the client
releases, or when the grace window expires:

1. Engine decides to preempt owner A for waiter B -> A's task writes
   `ServerMessage::Revoke` on the wire and arms `REVOKE_TIMEOUT` (5s, see
   policy-engine.md). No ioctl yet: A's lease stays fully valid, so A can
   finish its current frame and hand off gracefully.
2. Whichever comes first:
   - **A releases on time**: the revoke ioctl runs at the handoff (a no-op
     if A's fd close already killed the lease — tolerated), the engine
     requeues A for one more turn, and the next waiter is granted.
   - **5s pass with no Release**: the engine force-reclaims the slot; the
     next grant's stale-revoke ioctl kills A's lease on the spot — enforced
     even though A's fd is still open — and creates a fresh lease for B.
     A is NOT requeued (wedged clients don't get queue spots).
3. The next grant always creates a FRESH lease over the card's objects
   (per-grant model); the client never receives a previous client's lease.

| event                         | DRM action                                   | fbdev/input |
| ----------------------------- | -------------------------------------------- | ----------- |
| engine-pushed `Revoke`        | wire `Revoke` only — the ask; lease stays valid for the 5s grace window | wire `Revoke` only (cooperative) |
| client `Release` (on time)    | revoke ioctl at handoff -> requeue releaser -> grant next waiter a fresh lease | drop ownership, requeue |
| 5s deadline, no `Release`     | force-reclaim -> next grant's stale-revoke kills the lease -> fresh lease; no requeue | force-reclaim (no kernel stop) |
| client disconnect             | engine frees ownership; lease lingers (server copy) -> reclaimed by the next grant's stale-revoke | ownership freed |
| grant while a lease is active | stale-revoke inside `grant_lease()` then create | n/a |

A failed `revoke_lease` ioctl keeps the `Leased` state (id + fd preserved),
so the reclaim is retried on the next path that touches the card — the state
machine never throws away the one handle that can still reclaim the objects.

## Input hot-plug — the reconciler

fbdev and DRM cards are fixed by what the kernel exposes at boot; input devices
are not. `resource_manager::hotplug` reconciles the server's devices with
`/dev/input` — an inotify watch wakes it, a 60 s pass backstops it — and makes the
server match what it finds:

| observed | action |
| --- | --- |
| a device node with no index entry | open it, register the fd, `AdvertisedResources::insert`, `engine.offer(resource, policy)` — unless it is the device a SUSPENDED resource of the same class is waiting for, which it resumes instead |
| the node was re-created for the SAME device (same `/sys/class/input/eventN/device`) | replace the server's fd only: no revoke, no offer — the holder's dup still works, and the next grant gets a path that resolves |
| a DIFFERENT device now owns the node, and the old device still exists | replace the server's fd only — the holder's dup still works |
| a DIFFERENT device of the SAME class owns the node and the old device is gone (replaced within one pass) | resume: the holder keeps the resource and is handed the new device's fd |
| the node is absent, but its device still exists (udev mid-re-creation) | nothing — it is not a removal; the node is re-opened when it returns |
| the node and its device are gone (unplug) | `engine.suspend`: the resource leaves the advertised list and nothing else changes — its holder KEEPS it; the index entry stays, marked suspended |
| the device of a suspended resource comes back (the same node AND its class, or another node of the same class) | `engine.resume`: register the fresh fd, re-advertise, and re-grant the holder |

Details that matter:

- **Two identities**: the inode (`dev, ino`, from `fstat` on the fd we opened,
  re-checked against the path) says whether the node is still the one we opened;
  the sysfs device (`/sys/class/input/eventN/device` resolved) says whether it is
  still the same DEVICE. A udev trigger re-creates `eventN` for a device that
  never moved — revoking a holder then would take working input away for nothing
  — while an unplug changes both. Never revoke on the inode alone.
- **Why the fd is replaced anyway**: a deleted inode resolves to
  `eventN (deleted)`, and that is the string a client's libinput tries to open
  on the next grant (the backend resolves the fd through `/proc/self/fd`). The
  server's fd must therefore follow the node; the holders' dups need not.
- **A resume requires the class, on either path**: a suspended entry waits for a
  device of its OWN class, because the class is part of what the resource means —
  `Keyboard(2)` handed a mouse gives its holder a device its name says it is not.
  A device of another class on that node is a new device: the claim keeps its name
  by moving off the node (a placeholder key, so `suspended_peer` still finds it by
  class when its own device returns, wherever that is), and the newcomer is
  adopted as a new resource.
- **Index reuse**: an adopted device takes the lowest free index of its class, so
  a replug lands on the name it had before instead of shifting every later device.
- **What wakes it**: an inotify watch on `/dev/input` — `IN_CREATE`,
  `IN_DELETE`, `IN_MOVED_TO`, and deliberately NOT `IN_ATTRIB`: udev chmods the
  node right after the kernel creates it, and that carries no information the
  reconciler acts on. The watch is installed before the first pass, so a device
  that appears while the daemon starts is not missed. inotify is a syscall, so
  there is nothing to add: the `input` feature already pulls `evdev` with
  `default-features = false` to keep libudev out of the build, and the only user
  is `nix` (a dependency already) behind its `inotify` feature. `AsyncFd` gives
  the wake; nix's `Inotify` implements `AsFd` while `AsyncFd` wants `AsRawFd`, so
  the fd is duped for readiness and events are read through the `Inotify` handle
  (same queue).
- **Why the backstop passes exist**: a burst is left to settle for 200 ms and
  then reconciled once (a plug fires several events — the node, udev's chmod, a
  trigger touching neighbours), but events can be missed: an overflowed queue, or
  a watch that could not be installed at all (no `/dev/input` yet — the reconciler
  then polls every 2 s, which is also its behaviour before the watch existed). A
  stale view has nothing else to correct it, so a pass runs every 60 s
  regardless. A node that is announced but not yet openable is retried after
  500 ms (up to 5 times) rather than waiting for the next tick.
- **Every change is pushed to the clients that are already connected**: the
  engine broadcasts the new list (`ControlMessage::Advertise` →
  `ServerMessage::Advertise`) — the whole list, not a delta, so a missed push
  costs nothing and an old client that predates the push simply reads it as an
  unexpected message. Adoption is offered to the engine BEFORE the push goes out,
  so a client that acts on the list at once has its Acquire accepted.
- **A device that goes away is SUSPENDED, never revoked**: unplugging a mouse
  must not cost a running app its mouse, and it must not cost it a re-acquire
  that a client with no display could win the race for. `PolicyEngine::suspend`
  leaves the slot and its holder exactly as they are and only takes the resource
  out of the advertised list (it cannot be granted while its device is away, and
  a later Acquire is refused with "not available while its device is away").
  Nothing is sent to the holder; its own fd dies, which is what libinput reports
  to it as `DEVICE_REMOVED`.
- **A device that comes back resumes the SAME resource**: the index entry that
  was kept holds the resource name, so the returning device is matched to it —
  by the node it had, or, if it returned on another node, by class (lowest
  index) — the fresh fd is registered, the resource is re-advertised, and
  `PolicyEngine::resume` sends the holder an unsolicited `Grant` for the
  resource it never lost. Order matters: the fd is registered before the engine
  is told, because the grant is a dup of that registry entry.
- **The cost, stated plainly**: while a device is away its resource stays
  reserved for the client that held it — nobody else can take it, and if the
  device never comes back the name stays reserved for as long as that client
  lives. That is the price of "a grant survives the hardware leaving" (the
  alternative, revoking, is what forces an app to re-acquire and races with
  other clients).
- **A display the server loses is a different matter**: nothing withdraws a
  resource any more, so the only ways a slot stops being the holder's are the
  client's own Release, a policy preemption, and a disconnect.

## What this design does not touch

- **Wire protocol**: ungated, unchanged — `Advertise` lists what this build
  actually registered.
- **Policy engine**: slots come from `advertised`, plus one `Offer` per resource
  the input reconciler adopts at runtime; a backend that isn't compiled is
  simply never a slot.
- **Client crates**: `libsgc-rs` and the demo clients talk to the protocol
  crate only; they build identically against any server build.
- **fbdev/input grants**: still plain dups of the server's registered fd;
  the fds registry is untouched.
