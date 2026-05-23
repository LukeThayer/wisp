# CLAUDE.md — wisp

Multiplayer FPS wizard game. Bevy 0.18 + avian3d 0.5 + lightyear 0.26 +
bevy_enhanced_input 0.25. RON-driven data layer for spells, cast engine
with custom handlers, server-authoritative replication for shared state
(players, props, lanterns, portals).

The big architectural arc was Phase 1 (data-driven spells) → Phase 2
(lightyear multiplayer). Both shipped. Stage Q (full server-authoritative
+ prediction + interpolation) is in progress: see the lightyear section
below.

## Build & run

```bash
nix develop --command cargo build           # full build
nix develop --command cargo run --bin client    # window client
nix develop --command cargo run --bin server    # headless server
```

The dev shell is required (cargo + linker + system libs). Don't run cargo
outside `nix develop`. **No `python3` in the dev shell** — use bash/jq,
not python scripts.

**Running binaries directly** (`./target/debug/server` and friends):
Bevy's `AssetPlugin` resolves the `assets/` directory relative to the
binary, not the cwd. So a direct binary launch looks for
`./target/debug/assets/`, not `./assets/`. `cargo run` papers over this
by setting `CARGO_MANIFEST_DIR`; the net-test harness invokes binaries
directly, so it doesn't. One-time symlink to make both paths work:

```bash
ln -sfn ../../assets target/debug/assets
```

If the server log shows `Failed to load folder. Path not found: spells`
on harness start-up, this symlink is missing or broken.

## Multiplayer test harness (`wisp-net-test`)

The single most important tool in this repo for verifying replication and
sync behavior. **Use it any time you touch netcode and want feedback
before reporting back to the user.** It runs a real server + N headless
observers and produces structured JSON you can diff and assert against.

### What it is

- `target/debug/server` — gameplay server, trace-enabled when
  `WISP_TRACE_FILE` is set.
- `target/debug/observer` — headless client (no window/render). Reads a
  script from `WISP_OBSERVER_SCRIPT`; emits trace events for every
  replicated entity received and every message sent.
- `src/trace.rs` — env-gated JSONL emitter shared by server/client/observer.
  No-op when `WISP_TRACE_FILE` is unset.
- `tools/net-test/run_session.sh` — orchestrator (build, spawn, drain,
  summarize).
- `tools/net-test/summarize.sh` — bash+jq summary builder.
- `tools/net-test/scripts/*.script` — observer scripts.
- `~/.claude/skills/wisp-net-test/SKILL.md` — schema + jq query patterns.

### When to invoke

Reach for the harness whenever a change might touch cross-peer state:

- Edits to `src/net/{protocol,server,client,replication}.rs`
- Spell modules that send/receive messages (lantern, convex_lens, iris, portal)
- Changes to `src/bin/{server,observer}.rs`
- `src/trace.rs` itself
- User reports a sync bug ("X happens locally but Y doesn't see it")

Don't reach for it for client-only UX changes (HUD, animation, input
binding wiring without a server roundtrip).

### How to invoke

```bash
cd /home/luke/src/wisp

# Two observers: one throws a lantern, one watches.
tools/net-test/run_session.sh /tmp/wisp-s1 2 \
    tools/net-test/scripts/throw_lantern.script \
    tools/net-test/scripts/idle.script

# Output: /tmp/wisp-s1/summary.json + per-process *.jsonl + *.log files
WISP_NET_TEST_TIMEOUT=10 tools/net-test/run_session.sh ...   # override 30s default
```

Then assert against `summary.json` with `jq`:

```bash
# Every observer received both portals?
jq '.portals[] | {net_id, slot, n_receivers: (.receivers | length)}' summary.json

# Any in-session replication latency > 50ms (excluding startup catch-up of props)?
jq '[.lanterns[].receivers[].latency_s, .portals[].receivers[].latency_s,
     .players[].receivers[].latency_s] | max * 1000' summary.json

# Any position divergence between observers (>0.1m per axis)?
jq '[.position_divergence | to_entries[] | select(.value.diverged)] | length' summary.json

# Parent → child cast chains (empty unless the script triggers a composed cast like fireball)
jq '.cast_chains' summary.json
```

`cast_chains` is one entry per server-side `cast_dispatched` event with
a `caused_by_spell`. Each entry carries `parent_spell`, `parent_cast`,
`child_spell`, `child_cast`, `chain_depth`, `charge`, `t`. Driven by
`dispatch_child_cast`'s trace emission — see Spell composition section.

The skill at `~/.claude/skills/wisp-net-test/SKILL.md` has the full event
schema, script syntax, and richer query patterns. Read it before extending.

### Best practices

- **Start with an existing script.** Copy one from `tools/net-test/scripts/`
  rather than writing from scratch — the timing offsets that work are
  there for a reason.
- **Reproduce first, fix second.** If the user reports a sync bug,
  write or modify a script that reproduces it before touching code. Save
  the failing summary as the assertion target.
- **Assert on `net_id`, not local entity.** Local Bevy `Entity` ids
  differ per peer; `net_id` is server-assigned and shared. Every
  correlation and divergence check already keys on it.
- **Wall-clock t, not session-relative.** Trace `t` is unix epoch
  seconds (cross-process correlation works directly). Don't add a
  process-relative clock — it would re-introduce the skew bug the harness
  was rewritten to fix.
- **Tail logs when a session is silent.** If `summary.json` shows an
  observer didn't connect, check `session_dir/observer-N.log` for the
  cargo / link error — those go to stderr, not the trace stream.
- **Don't shell out python.** No python in the dev shell. Extend with
  bash + jq.
- **Don't trust startup latency.** Props spawn at server startup;
  observers connect ~1s later. Their first `prop_position` carries that
  ~1s latency. The `players`/`lanterns`/`portals` arrays are the in-session
  numbers worth caring about.
- **When changing the protocol, change the harness too.** A new
  replicated component → new trace event in `replication.rs`'s
  `ReplicationTracePlugin` → new correlation in `summarize.sh`. A new
  client→server message → new observer command in
  `src/bin/observer.rs::Command` + parse path + `script_*` trace event.

### When to extend the harness

Extend the harness *before* fixing a sync bug if the failure mode isn't
already representable. Specifically:

- **New replicated component** (e.g. `NetworkedBeam` for visible beam
  sync): add to `ReplicationTracePlugin` (arrival observer + position
  system if it has one), make the server include the `net_id`, then add
  a correlator block to `summarize.sh`.
- **New client→server message** (e.g. a charge-up cast): extend
  `Command` in `src/bin/observer.rs` with a parser branch, send the
  message in `advance_script`, emit a `script_*` trace event.
- **New validation pattern** (e.g. animation-state sync, latency budget
  alarms): add a top-level field in the `summarize.sh` output rather
  than asking jq queries to compute it ad-hoc.

Don't extend it for:

- One-off debug logging. Use `info!` / `tracing::debug!` instead — the
  trace stream is for cross-process correlation, not for "tell me what
  this function did."
- Per-frame state dumps. The trace files already grow ~200 events/sec on
  a 2-observer 5-second session. Filter on Changed<X> like the existing
  position systems do.
- Anything that requires running rendering or the full spell engine on
  the observer. Use the real client for those.

### Known limitations

- Observers bypass the cast engine + bei input layer. They send network
  messages directly. To exercise the input → cast → message pipeline,
  run the real client manually and let it write its own trace file:
  ```bash
  WISP_TRACE_FILE=/tmp/c.jsonl WISP_TRACE_SRC=client \
      nix develop --command ./target/debug/client
  ```
- Observers don't simulate physics. `goto`/`forward` set the broadcast
  pose; they don't collide with anything.
- Each session brings up a fresh server, so state can't be carried
  between sessions.

## Repo structure cheat sheet

```
src/
  net/
    protocol.rs        # replicated components + messages; channel + direction registration
    server.rs          # ServerNetPlugin; spawn, handlers, NetworkedIdAlloc
    client.rs          # ClientNetPlugin; connect, position send
    replication.rs     # ReplicationLocalPlugin (visuals), ReplicationTracePlugin (events)
  spells/
    data.rs            # SpellId / CastId / HandlerId / ChargingDef / OriginDef / TargetDef / PayloadDef
    engine.rs          # CastEngine: Idle/Charging/Channeling/Releasing/Cooldown + dispatch_child_cast
    handlers.rs        # HandlerRegistry (named Custom handlers) + CastContext
    triggers.rs        # BodyTriggers + on-collision → child cast (server-only)
    explosion.rs       # "explosion.impulse" AoE handler (server-only)
    {convex_lens, iris, portal, lantern}.rs   # spell modules + handlers
    catalog.rs         # RON loader → SpellCatalog / BodyCatalog
  trace.rs             # JSONL emitter (env-gated)
  bin/{client,server,observer}.rs

assets/
  spells/*.spell.ron   # SpellDef per spell (incl. fireball + explosion_small)
  bodies/*.body.ron    # BodyDef per spawnable (incl. fireball)

tools/net-test/        # orchestration, summarize, example scripts
```

## Spell composition (child casts)

A spell can spawn a body that triggers another spell. Canonical example:
fireball.spell.ron throws a `fireball` body whose `on_event:
OnCollision(explosion_small.boom)` runs `explosion_small.boom` at the
collision point. The fireball test is the integration shape; design the
same way when adding new "spell triggers spell" gameplay.

**Wire flow** (client cast → server-spawned body → child cast):
1. Client's cast engine dispatches `fireball.throw`. `PayloadDef::SpawnBody`
   calls `deliveries::spawn_body`, which builds a `SpawnBodyMessage` with
   `parent_cast: Some(ParentCastInfo { spell_id, cast_id, captured_charge,
   chain_depth })` and ships it to the server.
2. Server's `handle_spawn_body` resolves `parent_cast` against its
   `SpellCatalog`, reads the cast's `PayloadDef::SpawnBody.on_event`
   triggers, and attaches a `BodyTriggers` component (with
   `CollisionEventsEnabled`) to the spawned `NetworkedProp`.
3. Avian emits `CollisionStart` on impact. `triggers::enqueue_pending_collision_triggers`
   marks the body fired (one-shot), despawns it, and writes a
   `PendingChildCast` message per trigger.
4. `triggers::dispatch_pending_child_casts` (exclusive world) drains the
   queue and calls `engine::dispatch_child_cast` with
   `OriginDef::Impact(collision_point)` overriding the cast's authored
   origin, plus the propagated `captured_charge`, `chain_depth`, and
   `caused_by` parent reference.
5. `dispatch_child_cast` resolves the cast in the catalog, **skips**
   PreCast/Cost/Cooldown/Charging (child casts are mechanical — parent
   already paid), and runs `run_payload` + `run_effect` like a normal
   dispatch. Hard cap `MAX_CHILD_CAST_CHAIN_DEPTH = 4` prevents cycle-bombs.

**Server-side requirements**: the server now needs the spell catalog and
handler registry. `src/bin/server.rs` loads `CatalogPlugin`,
`HandlerRegistry`, `BodyTriggersPlugin`, and `ExplosionPlugin`. Catalog
validation is soft on missing handlers — the server doesn't register
client-only handlers (iris.burst, portal.place_*, etc.) and warns rather
than failing.

**Where things live**:
- Trigger types in RON: `data.rs::{BodyEventTrigger, TriggerSpec}`,
  embedded in `PayloadDef::SpawnBody.on_event`.
- Wire: `SpawnBodyMessage.parent_cast: Option<ParentCastInfo>` in
  `src/net/protocol.rs`.
- Server runtime: `src/spells/triggers.rs` (component + 3-step plugin).
- `dispatch_child_cast` + `ChildCastContext` in `src/spells/engine.rs`.

**Adding a new composed spell**: write the RON files (parent body +
parent spell + child spell + any child handler), register any
server-side custom handler. No new wire types or message handlers
needed.

**Open caveat**: `BodyTriggers.original_caster` is hardcoded `None`
today — the server doesn't map `ClientOf` connections to
`NetworkedPlayer` entities at attach time, so damage attribution from
child casts can't credit the originating player. Add that mapping when
attribution becomes load-bearing.

## Lightyear architecture (what we use, how to extend)

We use lightyear 0.26.4 with features `["netcode", "udp", "avian3d",
"input_bei", "interpolation"]`. The full surface is large; we use a
focused subset. The migration to lightyear-canonical patterns is the
ongoing "Stage Q" arc — see below.

### Plug-in layers (top of file map)

```
src/bin/{client,server}.rs       ← composes lightyear plugins via wisp::{build_shared, add_avian_with_lightyear}
src/lib.rs                        ← build_shared (gameplay) + add_avian_with_lightyear (physics + lightyear-avian)
src/net/mod.rs                    ← ClientNetPlugin / ServerNetPlugin re-exports
src/net/protocol.rs               ← all replicated components + messages + channels (shared by both peers)
src/net/{client,server}.rs        ← per-peer plugins (lightyear's ClientPlugins/ServerPlugins are added here)
src/net/replication.rs            ← receive-side observers (visuals, traces, anim, beam-cast state)
```

### Reference examples (CRITICAL — match these patterns)

`/tmp/lightyear-0.26.4/examples/` is a worktree of lightyear's git
tagged at `0.26.4` (same as our crate). Read these BEFORE inventing new
patterns:

- `avian_3d_character/` — the 3D character + avian + leafwing input
  reference. Showcases `LightyearAvianPlugin`, `AvianReplicationMode::Position`,
  `add_prediction`, `add_should_rollback`, `add_linear_correction_fn`,
  `add_linear_interpolation`, and shared controller in `FixedUpdate`.
- `bevy_enhanced_inputs/` — bei input replication. Shows `InputPlugin::<Context>`
  with `rebroadcast_inputs: true`, `register_input_action::<A>()`, and
  the server observer pattern (`On<Fire<Action>>`).
- `simple_box/` — minimal native-input predicted character.
- `fps/` — leafwing + `LagCompensationPlugin` + `LagCompensationSpatialQuery`.
- `projectiles/` — server-spawned projectiles with separate
  `PredictionTarget` / `InterpolationTarget` selectors.

If a pattern in this codebase diverges from these examples, there's
usually a documented reason (in code comments or this file). When in
doubt, port from the example.

Refresh the worktree when the lightyear crate version bumps:

```bash
cd /home/luke/src/lightyear && git fetch origin
git worktree remove /tmp/lightyear-0.26.4 --force && git worktree add /tmp/lightyear-<new-tag> <new-tag>
```

### What's wired today

**Physics + transport**:
- `LightyearAvianPlugin { replication_mode: AvianReplicationMode::Position }`
  via `wisp::add_avian_with_lightyear` (called by both bins).
- `PhysicsTransformPlugin`, `PhysicsInterpolationPlugin`, `IslandPlugin`,
  `IslandSleepingPlugin` disabled — lightyear owns Transform↔Position
  sync, sleeping bodies misbehave under rollback.
- `ClientPlugins` in `ClientNetPlugin`, `ServerPlugins` in `ServerNetPlugin`.
- UDP transport via `lightyear_udp` feature; netcode handshake via
  `lightyear_netcode` feature.

**Replicated components** (all registered in `src/net/protocol.rs`):
| Component | Notes |
|--|--|
| `NetworkedPlayer` | marker, server-spawned per client |
| `NetworkOwner(u64)` | client_id (could be `HasAuthority` later) |
| `NetworkedId(u64)` | server-assigned stable cross-peer key |
| `NetworkedPosition` | hand-rolled pose + flags; uses lightyear's `add_interpolation_with(lerp_networked_position)` |
| `NetworkedProp` / `NetworkedLantern` / `NetworkedPortal` | shape + state markers |
| `avian::{Position, Rotation, LinearVelocity}` | registered with `add_prediction` + `add_should_rollback`, **disabled by default** via `ComponentReplicationConfig { disable: true, .. }`. Dormant until Stage Q wires per-entity overrides. |

**Messages** (still hand-rolled per spell):
| Message | Direction | Purpose |
|--|--|--|
| `PlayerPositionMessage` | C→S | per-tick player pose (will go away in Stage Q) |
| `BeamImpulseMessage` | C→S | beam/iris ray impulse |
| `ThrowLanternMessage` / `PickupLanternMessage` | C→S | lantern lifecycle |
| `PlacePortalMessage` | C→S | portal placement intent |
| `SpawnBodyMessage` | C→S | generic `SpawnBody`/`Place` delivery (stone_toss, future spells) |
| `BeamCastBroadcast` | S→C | beam visual state for remote clients |

**Replication strategy**: `Replicate::manual(senders)` + per-frame
`refresh_replicate_on_connect`. `NetworkTarget::All` snapshots senders at
insert and breaks for late joiners — the manual approach is required.

### How to extend (without inventing new patterns)

**To replicate a new component**:
1. Add `#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]` (Default may be required by message registration; check).
2. Register in `ProtocolPlugin::build` via `app.register_component::<T>()`.
3. If it changes per-tick on the wire, also chain `.add_interpolation_with(lerp_fn)` (define a `fn(T, T, f32) -> T`).
4. If it should be predicted on the client, chain `.add_prediction()` + `.add_should_rollback(|a, b| …)`. For now, also chain `.with_replication_config(ComponentReplicationConfig { disable: true, .. })` so it doesn't replicate until you explicitly enable per-entity.

**To add a new client→server message**:
1. Declare the struct with serde derives.
2. Register in `ProtocolPlugin::build` via `app.register_message::<M>().add_direction(NetworkDirection::ClientToServer)`.
3. Server side: query `&mut MessageReceiver<M>` on `ClientOf`-tagged entities, drain `receiver.receive()`.
4. Client side: query `Single<&mut MessageSender<M>>`, call `sender.send::<PlayerInputChannel>(msg)`.
5. Both sides of the channel must be declared — the existing
   `PlayerInputChannel` is bidirectional (both `ClientToServer` and
   `ServerToClient` directions). Reuse it; don't proliferate channels.

**To add a new server-spawn projectile or placeable** (the generic path):
1. Author `assets/bodies/<name>.body.ron`.
2. Author `assets/spells/<name>.spell.ron` with `delivery: SpawnBody(template: "<name>", launch: Throw(forward, up))` or `delivery: Place(...)`.
3. Done. `deliveries::spawn_body` / `::place` route through `SpawnBodyMessage` → server `handle_spawn_body` → `NetworkedProp` replication automatically.

### Stage Q: server-authoritative + prediction + interpolation

**Goal**: server runs the player simulation, client predicts the local
player locally and reconciles on server confirmation, other players are
interpolated.

**Progress**:
- ✅ Q.1: `LightyearAvianPlugin` + disabled conflicting avian plugins.
- ✅ Q.2: `Position` / `Rotation` / `LinearVelocity` registered for
  prediction + interpolation (DISABLED by default — no entity uses them
  yet; existing `NetworkedPosition` path still drives the wire).
- 🚧 Q.3 BLOCKED: bei version mismatch (see below).
- ✅ Q.4b: hand-rolled server-authoritative controller — option (b)
  from the blocker decision. `PlayerInputMessage` ships WASD axis + yaw
  + jump + casting-flag from client each tick; server stores it in a
  `PlayerInputState` component; `run_player_controller` in `FixedUpdate`
  applies forces to the now-Dynamic `NetworkedPlayer` body; avian
  integrates; `sync_player_positions` ships the resulting pose back via
  the existing `NetworkedPosition` wire. `apply_player_rotation` is a
  separate FixedUpdate system because avian's `Forces` SystemParam
  internally borrows `Rotation` — same query can't also write it.
- ⏳ Q.4b reconciliation (deferred): local client still runs its own
  controller (snappy visual response); we do NOT reconcile local
  prediction against server confirmation. Cheaters could drift their
  local view, but every other peer sees the server's authoritative
  pose. To add reconciliation later: periodically snap or lerp the
  local rig's Transform toward the replicated `NetworkedPosition` for
  the locally-owned `NetworkedPlayer` (currently hidden from the local
  view by `hide_self_wizard_body`). Don't do this without testing
  carefully — naive snap correction will feel awful at any latency.
- ⏳ Q.5 (partial): the old `PlayerPositionMessage` is gone (replaced
  by `PlayerInputMessage`). What remains is unifying the local Player
  rig with the replicated NetworkedPlayer entity so the camera attaches
  to the predicted server-authoritative entity — requires the bei input
  layer to ship from a per-replicated-entity context, which is blocked
  on Q.3. Not necessary for current "server authoritative for what
  others see" model.

### Stage Q blocker: bei version mismatch

**The problem**:
- wisp depends on `bevy_enhanced_input = "0.25"` (Cargo.toml).
- `lightyear_inputs_bei = "0.26.4"` (pulled in by the `input_bei`
  feature) depends on `bevy_enhanced_input = "0.22"`.
- Cargo accepts both crates in the dep graph, but they're separate
  types. `lightyear_inputs_bei::register_input_action::<A>()` requires
  `A: bevy_enhanced_input::v0.22.2::InputAction`, but our `Movement`,
  `Fire`, etc. impl the `v0.25.0::InputAction` trait. Compile error:
  ```
  the trait `lightyear::input::bei::prelude::InputAction` is not implemented for `Pickup`
  note: there are multiple different versions of crate `bevy_enhanced_input` in the dependency graph
  ```
- Latest lightyear `main` is also on bei 0.24, still incompatible with
  0.25. (Verified by checking
  `/home/luke/src/lightyear/lightyear_inputs_bei/Cargo.toml`.)

**Options** (also recorded in task #55):

(a) **Downgrade wisp bei to 0.22**. Invasive — bei 0.22→0.25 changed
    the action context macro (`actions!()` → `actions_spawn!()` IIRC),
    the `ActionSettings` shape, and several enum names. Probably 4–6
    hours of careful porting across `src/input.rs`, every spell module
    that observes `On<Fire<...>>`, the radial menu, and the cast
    engine's `phase_advance` action-snapshot code.

(b) **Hand-roll the input wire** (recommended pragmatic option).
    Extend `PlayerPositionMessage` (or add a new `PlayerInputMessage`)
    to carry per-tick input state (movement vec, jump pressed, fire
    pressed, etc.). Server reads it and runs the controller. Skip
    lightyear-native rollback; if prediction is wanted, hand-roll it
    by running the same controller locally and reconciling against
    the replicated `Position`. Keeps bei 0.25 locally for ergonomics.

(c) **Wait** for a lightyear release that bumps `bevy_enhanced_input`
    to 0.25+. No timeline visible in the lightyear repo.

(d) **Switch off bei entirely** and use lightyear's `input_native`
    plugin (defines a single replicated `Inputs` enum, no bei
    integration). Largest refactor; reference
    `/tmp/lightyear-0.26.4/examples/simple_box/`.

The harness baseline is still healthy on Q.2: `diverged: 0`,
`watcher_received_all: true`, `max_latency_ms ~0.6`. Pause Stage Q
here pending the bei decision.

**Steps to resume Q.4**:
1. Server: in `sync_networked_players`, spawn the player with:
   - `RigidBody::Dynamic` + `Collider::capsule(0.4, 1.2)` +
     `LockedAxes` (lock all rotations, see avian_3d_character's
     `CharacterPhysicsBundle`)
   - `Position(initial)`, `Rotation::default()`, `LinearVelocity::default()`
   - `ActionState::<Movement>::default()` (and per other action)
   - `Replicate::to_clients(NetworkTarget::All)` — at this point we can
     finally drop `Replicate::manual` for the player
   - `PredictionTarget::to_clients(NetworkTarget::All)`
   - `ControlledBy { owner: trigger.entity, lifetime: Default::default() }`
2. Move `apply_movement` / `apply_jump` from `src/player/controller.rs`
   to a shared module (`src/player/shared_controller.rs`?) and have it
   read from `ActionState` instead of bei `Action<A>` observers. Reference:
   `/tmp/lightyear-0.26.4/examples/avian_3d_character/src/shared.rs::apply_character_action`.
3. Add a server system that runs the controller in `FixedUpdate` on
   entities `Without<Predicted>`. Add the same system on the client gated
   on `With<Predicted>`.

**Steps to resume Q.5**:
1. Client observer:
   ```rust
   fn on_predicted_player(
       trigger: On<Add, Predicted>,
       players: Query<&NetworkOwner, (With<NetworkedPlayer>, With<Predicted>)>,
       …,
   ) {
       // Attach Camera3d + PlayerCamera + lens_anchor + body SceneRoot.
       // Add bei context: actions!(PlayerContext[...]).
       // Add LocalPlayer marker.
   }
   ```
2. Delete `PlayerPositionMessage`, `send_local_player_position`,
   `apply_position_updates`. The `airborne` and `casting` bool flags on
   `NetworkedPosition` will need a new replication path (probably their
   own small component or merged into the bei input stream).
3. Delete `src/player/mod.rs::spawn_player` — server is now the
   canonical spawn site.

**Risks / open questions**:
- bei action enums are currently local-only (`#[derive(InputAction)]` from
  bevy_enhanced_input). lightyear_inputs_bei may require additional
  derives (`Serialize`, `Deserialize`, `Reflect`?). Check `bevy_enhanced_inputs`
  example's action declarations.
- Casting state (`CastState` / `LocalBeamState` / animation blend state)
  currently lives on the local Player entity. With Stage Q, this entity
  becomes the `Predicted` copy of a replicated entity. Confirm that
  attaching non-replicated components to `Predicted` doesn't break
  rollback (the prediction registry only tracks registered components).
- `cancel_on_spell_change` and other systems use `Query<…, With<Player>>`.
  After Stage Q the `Player` marker still works because we attach it to
  the predicted entity; nothing should change. Verify in the harness.

### Keeping the lightyear layer generic

When adding a new spell, prefer the data-driven paths:
- Projectile / placeable → `SpawnBody` / `Place` delivery, no new
  message or server handler needed.
- Hitscan / impulse → reuse `BeamImpulseMessage` (it's generic over
  origin/direction/range/magnitude).
- New component class → check if it's per-tick state (use
  interpolation), one-shot (Message broadcast), or initial-value-only
  (default replication).

Avoid bespoke messages for things that fit the existing patterns. If
the existing patterns don't fit, document the *why* in a comment near
the new message declaration (see `BeamCastBroadcast`'s justification in
`src/net/protocol.rs` for the canonical example).

## Conventions worth knowing

- Don't add `python3` dependencies — not in the dev shell.
- Match versions to `Cargo.toml` exactly. `avian3d = "0.5"` is pinned by
  lightyear's avian feature; don't bump.
- Server uses `MinimalPlugins` + manual `AssetPlugin`/`MeshPlugin`/`ScenePlugin`
  to satisfy avian's collider-cache. Don't move it back to `DefaultPlugins`.
- Server is authoritative for shared state. Spells that affect the
  world send a message (`BeamImpulseMessage`, `ThrowLanternMessage`,
  `PlacePortalMessage`, etc.); the server applies and replicates back.
- `Replicate::manual(senders)` + refresh-on-connect, not
  `Replicate::to_clients(NetworkTarget::All)` — the latter snapshots
  senders at insert time and breaks for late-joining clients.
- Trace observers in `ReplicationTracePlugin` defer the arrival event
  via `PendingTraceArrival` so they see both `NetworkedId` and (optional)
  `NetworkOwner` in one shot. Don't fold them back into the on-Add
  observer or `net_id` will be missing.
- **Component replication propagates initial values reliably; updates
  to existing replicated components have been unreliable in our setup.**
  For per-tick state (beam casts, anything else changing more than once),
  use a Message broadcast (server→clients) instead of mutating a
  replicated component. `NetworkedBeamCast` was originally a replicated
  component; it never propagated past the initial sync. We switched to
  `BeamCastBroadcast` (Message) + `LocalBeamState` (local component
  filled by the broadcast drain). See `src/net/server.rs::apply_beam_impulses`
  + `src/net/replication.rs::drain_beam_broadcasts`. Initial protocol
  state (player/prop/lantern/portal spawn) still uses component
  replication — that path is solid.
- Channels are direction-tagged at registration. For a channel that
  carries server→client messages too, append a second
  `.add_direction(NetworkDirection::ServerToClient)` on the
  `add_channel::<C>(...)` chain. Forgetting this silently drops the
  outbound side without an error.
- When broadcasting a per-tick message back from server to all clients
  including the original sender, the caster receives loopback too. In
  `drain_beam_broadcasts` we filter by `LocalId` so the local player's
  own beam isn't double-rendered (local `cast_beam` + remote-beam mesh).
- **Under `LightyearAvianPlugin`, write avian's `Position` / `Rotation`,
  not `Transform`.** With `PhysicsTransformPlugin` disabled, lightyear
  owns the sync — and it syncs **from** avian's components **to**
  Transform every tick. A direct `Transform.rotation = …` or
  `Transform.translation = …` write gets clobbered on the next sync
  and looks like "the system ran but had no effect". This bit:
  - `apply_rotation` (`src/player/controller.rs`) — writes `Rotation.0`
    for body yaw; camera pitch still writes Transform because the
    camera child isn't a physics body.
  - `process_teleports` + `process_traveler_teleports`
    (`src/spells/portal.rs`) — write `Position.0` (and Transform for
    same-frame readers) when teleporting bodies through portals.
  If you find yourself writing Transform on an entity with `RigidBody`,
  switch to the avian component instead.

## Iteration workflow

Touched netcode/replication? Run the harness before reporting back:

```bash
WISP_NET_TEST_TIMEOUT=15 tools/net-test/run_session.sh /tmp/wisp-check 2 \
    tools/net-test/scripts/multi_spell.script \
    tools/net-test/scripts/idle_long.script

jq '{
    diverged: ([.position_divergence | to_entries[] | select(.value.diverged)] | length),
    watcher_received_all: (
        (.event_counts."observer-2".replicated_player == 2) and
        (.event_counts."observer-2".replicated_lantern == 1) and
        (.event_counts."observer-2".replicated_portal == 2) and
        (.event_counts."observer-2".replicated_prop == 6)
    ),
    max_latency_ms: (([.lanterns[].receivers[].latency_s, .portals[].receivers[].latency_s, .players[].receivers[].latency_s] | max) * 1000)
}' /tmp/wisp-check/summary.json
```

Expected baseline: `diverged: 0`, `watcher_received_all: true`,
`max_latency_ms < 5`. Anything else is a regression worth investigating
before claiming the work is done.

Touched child-cast dispatch, body triggers, or anything in
`src/spells/{engine,triggers,explosion}.rs`? Add the fireball
integration check too:

```bash
WISP_NET_TEST_TIMEOUT=15 tools/net-test/run_session.sh /tmp/wisp-fireball 2 \
    tools/net-test/scripts/throw_fireball.script \
    tools/net-test/scripts/idle_long.script

jq '{
    n_chains: (.cast_chains | length),
    fireball_chain_present: (
        [.cast_chains[] |
            select(.parent_cast == "fireball.throw"
                   and .child_cast == "explosion_small.boom"
                   and .chain_depth == 1)] | length
    ),
    explosion_applied: .event_counts.server.explosion_applied
}' /tmp/wisp-fireball/summary.json
```

Expected: `n_chains: 1`, `fireball_chain_present: 1`,
`explosion_applied: 1`. The chain entry confirms `dispatch_child_cast`
fired with `caused_by` correlated to the parent cast; the
`explosion_applied` count confirms the server-side AoE handler ran.
