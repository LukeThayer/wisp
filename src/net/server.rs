//! Server-side network plugin. Layers lightyear's `ServerPlugins` plus our
//! shared `ProtocolPlugin`. On Startup it spawns a netcode server entity
//! bound to `default_server_addr()`. Observers log connections and
//! disconnections.

use core::time::Duration;
use std::net::SocketAddr;

use bevy::prelude::*;
use lightyear::prelude::server::{
    ClientOf, NetcodeConfig, NetcodeServer, ServerPlugins, ServerUdpIo,
};
use lightyear::prelude::{
    Connected, LinkOf, LinkStart, LocalAddr, MessageReceiver, NetworkTarget, PeerId, RemoteId,
    Replicate, ReplicationSender,
};

use avian3d::prelude::*;

use crate::net::protocol::{
    BeamCastBroadcast, BeamImpulseMessage, ChargeStateBroadcast, ChargeStateMessage,
    EquipWeaponsMessage, HandheldPortal, HoldPortalMessage, NetworkOwner, NetworkedHealth,
    NetworkedId, NetworkedLantern, NetworkedPlayer, NetworkedPortal, NetworkedPosition,
    NetworkedProp, PickupLanternMessage, PlacePortalMessage, PlayerInputMessage, PropShape,
    SpawnBodyMessage, TeleportSnap, TestCube, ThrowLanternMessage,
};
use crate::weapons::{ActiveWeaponSlot, EquippedWeapons, WeaponId};
use crate::spells::damage::{
    apply_damage_to_hurtbox, DeathEvent, Hurtbox, Team, PLAYER_MAX_HP,
};
use lightyear::prelude::MessageSender;
use crate::physics::GameLayer;
use crate::net::{default_server_addr, ProtocolPlugin, NETCODE_KEY, PROTOCOL_ID, TICK_HZ};
use crate::spells::portal::{disc_rotation, PORTAL_RADIUS};
use crate::trace;
use serde_json::json;
use std::collections::{HashMap, HashSet};

pub struct ServerNetPlugin;

impl Plugin for ServerNetPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ServerPlugins {
            tick_duration: Duration::from_secs_f32(1.0 / TICK_HZ as f32),
        })
        .add_plugins(ProtocolPlugin)
        .insert_resource(ServerBind {
            addr: default_server_addr(),
        })
        .init_resource::<NetworkedIdAlloc>()
        .init_resource::<ClientPlayerMap>()
        .init_resource::<HeldPortalRegistry>()
        .add_systems(Startup, (spawn_server, spawn_test_cube, spawn_arena))
        .add_systems(
            Update,
            (
                sync_networked_players,
                refresh_replicate_on_connect,
                drain_player_inputs,
                drain_customize_messages,
                apply_beam_impulses,
                relay_charge_states,
                drain_equip_messages,
                handle_throw_lantern,
                handle_pickup_lantern,
                handle_place_portal,
                handle_hold_portal,
                handle_spawn_body,
                update_networked_player_falling,
                update_networked_prop_falling,
                server_portal_teleport,
                update_prev_portal_pose.after(server_portal_teleport),
                sync_player_positions,
                sync_prop_positions,
                sync_lantern_positions,
                decay_beam_casts,
            ),
        )
        .add_systems(
            FixedUpdate,
            (apply_player_rotation, run_player_controller).chain(),
        )
        .add_systems(Update, despawn_disconnected_players)
        .add_observer(on_new_link)
        .add_observer(on_client_connected)
        .add_plugins(trace::TracePlugin);
    }
}

#[derive(Resource, Clone)]
pub struct ServerBind {
    pub addr: SocketAddr,
}

/// Lookup table: connected client id -> their `NetworkedPlayer` entity.
/// Populated by `sync_networked_players` on spawn and cleaned up by
/// `despawn_disconnected_players` on disconnect. Used for kill / damage
/// attribution: `handle_spawn_body` reads it to stamp the originating
/// caster onto `BodyTriggers`, so child casts (explosions) can credit
/// kills back to the player who threw the fireball.
#[derive(Resource, Default)]
pub struct ClientPlayerMap(pub HashMap<u64, Entity>);

/// Monotonic counter that assigns each replicated entity a peer-stable
/// `NetworkedId`. Starts at 1; 0 is reserved for "unset".
#[derive(Resource, Default)]
pub struct NetworkedIdAlloc {
    next: u64,
}

impl NetworkedIdAlloc {
    pub fn next(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
}

/// Maps `(client_id, PortalSlot) → handheld portal Entity`. Lets
/// `handle_hold_portal` route per-tick updates to the existing portal
/// (rather than respawning each frame) and gives `despawn_disconnected_players`
/// an O(1) cleanup target.
#[derive(Resource, Default)]
pub struct HeldPortalRegistry {
    by_owner_slot: HashMap<(u64, crate::spells::portal::PortalSlot), Entity>,
}

/// Server-side previous-position tracker for portal-traveler entities
/// (`NetworkedProp`, `NetworkedLantern`). Local to the server; not
/// replicated. Mirrors `spells::portal::PrevPos` from the client.
#[derive(Component, Default)]
struct ServerPrevPos(Option<Vec3>);

/// Server-side anti-bounce set for portal-traveler entities. Once a
/// traveler teleports through a portal pair, it can't re-trigger either
/// portal until it moves far enough away. Mirrors the client's
/// `spells::portal::PortalLockout`.
#[derive(Component, Default)]
struct ServerPortalLockout(HashSet<Entity>);

/// Previous-frame world pose of a `NetworkedPortal`. Required for the
/// "portal sweeps over stationary traveler" case in
/// `server_portal_teleport`: without this, prev/curr crossing math
/// both use the current portal pose, so a moving handheld portal
/// passing across a body produces zero along-axis delta and the
/// teleport never fires. Refreshed at the end of each tick by
/// `update_prev_portal_pose`; initial value is set to the spawn pose
/// (so the first frame can't trigger a phantom crossing).
#[derive(Component)]
struct ServerPrevPortalPose {
    translation: Vec3,
    rotation: Quat,
}

fn spawn_server(mut commands: Commands, bind: Res<ServerBind>) {
    let config = NetcodeConfig::default()
        .with_protocol_id(PROTOCOL_ID)
        .with_key(NETCODE_KEY);
    info!("wisp server listening on {}", bind.addr);
    let entity = commands
        .spawn((
            NetcodeServer::new(config),
            ServerUdpIo::default(),
            LocalAddr(bind.addr),
        ))
        .id();
    commands.trigger(LinkStart { entity });
}

/// Each new client connection gets its own `LinkOf` entity on the server.
/// We attach a `ReplicationSender` to it so replication actually flows.
fn on_new_link(trigger: On<Add, LinkOf>, mut commands: Commands) {
    commands.entity(trigger.entity).insert((
        Name::new("ClientLink"),
        ReplicationSender::default(),
    ));
}

fn on_client_connected(
    trigger: On<Add, Connected>,
    clients: Query<&RemoteId, With<ClientOf>>,
) {
    let Ok(RemoteId(peer_id)) = clients.get(trigger.entity) else {
        return;
    };
    info!("Client connected: {:?}", peer_id);
    let id = match peer_id {
        PeerId::Netcode(id) | PeerId::Steam(id) | PeerId::Local(id) | PeerId::Entity(id) => {
            Some(*id)
        }
        _ => None,
    };
    trace::event("client_connected", json!({"client_id": id}));
}

/// When the set of connected clients changes, refresh `Replicate` on every
/// `NetworkedPlayer` with an explicit `manual(senders)` list rebuilt from
/// the current set of `ClientOf` LinkOf entities. `NetworkTarget::All`'s
/// on-insert resolution apparently snapshots the sender list at spawn time
/// and doesn't widen on subsequent connects; manual mode fed each frame
/// avoids that.
/// Maintain each `NetworkedPlayer`'s `Replicate::manual` sender list so it
/// always equals the currently-connected client set. Detects a change via
/// connection-count delta to avoid re-inserting every frame.
///
/// `NetworkTarget::All` with `SingleServer` mode snapshots the sender list
/// at spawn time and doesn't widen it when new clients connect later;
/// `manual` mode lets us explicitly refresh.
fn refresh_replicate_on_connect(
    senders: Query<Entity, (With<ClientOf>, With<Connected>)>,
    targets: Query<
        Entity,
        Or<(
            With<NetworkedPlayer>,
            With<NetworkedLantern>,
            With<NetworkedProp>,
            With<NetworkedPortal>,
        )>,
    >,
    mut commands: Commands,
    mut prev_count: Local<usize>,
) {
    let current: Vec<Entity> = senders.iter().collect();
    if current.len() == *prev_count {
        return;
    }
    *prev_count = current.len();
    for entity in &targets {
        commands
            .entity(entity)
            .insert(Replicate::manual(current.clone()));
    }
}

/// Despawn NetworkedPlayer entities whose owning client has disconnected,
/// so stale wizards don't accumulate across reconnects.
fn despawn_disconnected_players(
    connections: Query<&RemoteId, (With<ClientOf>, With<Connected>)>,
    players: Query<(Entity, &NetworkOwner)>,
    mut commands: Commands,
    mut client_map: ResMut<ClientPlayerMap>,
    mut held_portals: ResMut<HeldPortalRegistry>,
) {
    let alive: HashSet<u64> = connections
        .iter()
        .filter_map(|RemoteId(peer_id)| match peer_id {
            PeerId::Netcode(id) => Some(*id),
            PeerId::Steam(id) => Some(*id),
            PeerId::Local(id) => Some(*id),
            PeerId::Entity(id) => Some(*id),
            _ => None,
        })
        .collect();
    for (entity, owner) in &players {
        if !alive.contains(&owner.0) {
            commands.entity(entity).despawn();
            client_map.0.remove(&owner.0);
            trace::event(
                "client_disconnected",
                json!({"client_id": owner.0, "entity": format!("{:?}", entity)}),
            );
        }
    }
    // Drop any held portals owned by clients that just left so they
    // don't dangle. Iterate by collecting keys first because the loop
    // borrows the map immutably while we want to remove from it.
    let stale: Vec<_> = held_portals
        .by_owner_slot
        .keys()
        .copied()
        .filter(|(client_id, _)| !alive.contains(client_id))
        .collect();
    for key in stale {
        if let Some(entity) = held_portals.by_owner_slot.remove(&key) {
            commands.entity(entity).despawn();
        }
    }
}

/// Polls each frame to ensure exactly one `NetworkedPlayer` per connected
/// client. Runs as a regular system rather than as an observer on
/// `Add<Connected>` to avoid timing issues with `Replicate`'s on-insert
/// hook resolving senders before the connection lifecycle is fully
/// settled.
fn sync_networked_players(
    connections: Query<(Entity, &RemoteId), (With<ClientOf>, With<Connected>)>,
    existing: Query<&NetworkOwner>,
    mut commands: Commands,
    mut id_alloc: ResMut<NetworkedIdAlloc>,
    mut client_map: ResMut<ClientPlayerMap>,
) {
    let existing_ids: HashSet<u64> = existing.iter().map(|o| o.0).collect();
    let senders: Vec<Entity> = connections.iter().map(|(e, _)| e).collect();
    for (_, RemoteId(peer_id)) in &connections {
        let client_id = match peer_id {
            PeerId::Netcode(id) => *id,
            PeerId::Steam(id) => *id,
            PeerId::Local(id) => *id,
            PeerId::Entity(id) => *id,
            _ => continue,
        };
        if existing_ids.contains(&client_id) {
            continue;
        }
        info!(
            "Spawning NetworkedPlayer for client {client_id} (senders={})",
            senders.len()
        );
        // Initial position: spread along X by id, lifted to (1.5, z=8).
        let offset_x = (client_id % 8) as f32 * 2.0 - 6.0;
        let initial = Vec3::new(offset_x, 1.5, 8.0);
        let net_id = id_alloc.next();
        trace::event(
            "player_spawned",
            json!({
                "client_id": client_id,
                "net_id": net_id,
                "pos": [initial.x, initial.y, initial.z],
            }),
        );
        let player_entity = commands.spawn((
            (
                Name::new(format!("NetworkedPlayer({client_id})")),
                NetworkedPlayer,
                NetworkedId(net_id),
                BeamCastTimer { last_update: 0.0, active: false },
                NetworkOwner(client_id),
                NetworkedPosition::from_vec3(initial),
                PlayerInputState::default(),
                crate::net::protocol::PlayerCustomization::default(),
                // Server-only Hurtbox + replicated NetworkedHealth.
                // Clients render the HUD from NetworkedHealth; the
                // server reads Hurtbox for the damage path.
                Hurtbox::new(PLAYER_MAX_HP, Team::Players),
                NetworkedHealth { hp: PLAYER_MAX_HP, max_hp: PLAYER_MAX_HP },
                EquippedWeapons::starter(),
                ActiveWeaponSlot::default(),
            ),
            // Server-authoritative dynamic body (Stage Q option b).
            // Same params as the client-side rig in `src/player/mod.rs`
            // so the local prediction matches what the server simulates.
            Transform::from_translation(initial),
            Position(initial),
            LinearVelocity::default(),
            (
                RigidBody::Dynamic,
                Collider::capsule(0.4, 1.2),
                LockedAxes::ROTATION_LOCKED,
                Mass(80.0),
                Friction::new(0.0),
                LinearDamping(0.5),
                Restitution::new(0.0),
            ),
            CollisionLayers::new(GameLayer::Player, LayerMask::ALL),
            ServerPrevPos::default(),
            ServerPortalLockout::default(),
            Replicate::manual(senders.clone()),
        )).id();
        client_map.0.insert(client_id, player_entity);
    }
}

/// Drains `CustomizeMessage`s and stamps the latest customization onto
/// the matching `NetworkedPlayer`'s `PlayerCustomization` component.
/// Component replication propagates the change to every connected
/// client; on each client, the recolor system reads the component off
/// the NetworkedPlayer entity and updates the material tints.
fn drain_customize_messages(
    mut receivers: Query<
        (&RemoteId, &mut MessageReceiver<crate::net::protocol::CustomizeMessage>),
        With<ClientOf>,
    >,
    mut players: Query<
        (&NetworkOwner, &mut crate::net::protocol::PlayerCustomization),
        With<NetworkedPlayer>,
    >,
) {
    for (RemoteId(peer_id), mut receiver) in &mut receivers {
        let client_id = match peer_id {
            PeerId::Netcode(id) => *id,
            PeerId::Steam(id) => *id,
            PeerId::Local(id) => *id,
            PeerId::Entity(id) => *id,
            _ => continue,
        };
        let mut latest = None;
        for msg in receiver.receive() {
            latest = Some(msg);
        }
        let Some(msg) = latest else { continue };
        for (owner, mut customization) in &mut players {
            if owner.0 == client_id {
                *customization = msg.customization;
            }
        }
    }
}

/// Per-player record of the most-recent client input. Refreshed every
/// `Update` by `drain_player_inputs`; consumed every `FixedUpdate` by
/// `run_player_controller`.
#[derive(Component, Default, Clone, Copy)]
struct PlayerInputState {
    movement: Vec2,
    yaw: f32,
    pitch: f32,
    jump: bool,
    casting: bool,
}

/// Drains `PlayerInputMessage`s from each connected client and writes the
/// latest input onto that client's `PlayerInputState`. The controller in
/// `FixedUpdate` consumes it. Stage Q option (b): server is the
/// movement authority; clients send only inputs.
fn drain_player_inputs(
    mut receivers: Query<
        (&RemoteId, &mut MessageReceiver<PlayerInputMessage>),
        With<ClientOf>,
    >,
    mut players: Query<(&NetworkOwner, &mut PlayerInputState), With<NetworkedPlayer>>,
) {
    for (RemoteId(peer_id), mut receiver) in &mut receivers {
        let client_id = match peer_id {
            PeerId::Netcode(id) => *id,
            PeerId::Steam(id) => *id,
            PeerId::Local(id) => *id,
            PeerId::Entity(id) => *id,
            _ => continue,
        };
        let mut latest: Option<PlayerInputMessage> = None;
        for msg in receiver.receive() {
            latest = Some(msg);
        }
        let Some(msg) = latest else { continue };
        for (owner, mut input) in &mut players {
            if owner.0 == client_id {
                input.movement = Vec2::new(msg.movement[0], msg.movement[1]);
                input.yaw = msg.yaw;
                input.pitch = msg.pitch;
                input.jump = msg.jump;
                input.casting = msg.casting;
            }
        }
    }
}

/// Server-side player controller (FixedUpdate). Ports
/// `src/player/controller.rs::apply_movement` + `apply_jump` +
/// `ground_check`: reads the last received `PlayerInputState`, builds a
/// desired ground velocity, applies the acceleration force, and applies
/// a vertical impulse if Jump is freshly held + the body is grounded.
/// Avian integrates the dynamic body and the resulting Position
/// replicates to clients via `sync_player_positions`.
fn apply_player_rotation(
    mut q: Query<(&PlayerInputState, &mut Rotation), With<NetworkedPlayer>>,
) {
    for (input, mut rot) in &mut q {
        rot.0 = Quat::from_axis_angle(Vec3::Y, input.yaw);
    }
}

fn run_player_controller(
    time: Res<Time>,
    spatial: SpatialQuery,
    mut players: Query<
        (Entity, &PlayerInputState, &Transform, Forces),
        With<NetworkedPlayer>,
    >,
    mut prev_jump: Local<std::collections::HashMap<Entity, bool>>,
) {
    const MAX_SPEED: f32 = 4.2;
    const GROUND_ACCEL: f32 = 60.0;
    const AIR_ACCEL: f32 = 10.0;
    const JUMP_IMPULSE: f32 = 5.0;
    const MASS: f32 = 80.0;
    let dt = time.delta_secs().max(1e-5);

    for (entity, input, tf, mut forces) in &mut players {
        // Ground check: short ray below the capsule. Exclude THIS
        // player (filter is per-iteration, not the global "first
        // player" snapshot the old code used — see commentary in
        // `sync_player_positions`).
        let ray_origin =
            tf.translation + Vec3::new(0.0, -0.6 - 0.4, 0.0); // half-height - radius
        let grounded = spatial
            .cast_ray(
                ray_origin,
                Dir3::NEG_Y,
                0.2,
                true,
                &SpatialQueryFilter::from_excluded_entities([entity]),
            )
            .is_some();

        // Movement: input.movement is WASD axis in *local* frame —
        // movement.x = strafe (+right), movement.y = forward (+forward
        // is -Z in body frame).
        let local = Vec3::new(input.movement.x, 0.0, -input.movement.y);
        let world_dir = Quat::from_axis_angle(Vec3::Y, input.yaw) * local;
        let desired_ground = world_dir.normalize_or_zero() * MAX_SPEED;
        let lv = forces.linear_velocity();
        let current = Vec3::new(lv.x, 0.0, lv.z);
        let delta = desired_ground - current;
        let accel_cap = if grounded { GROUND_ACCEL } else { AIR_ACCEL };
        let accel = delta.clamp_length_max(accel_cap * dt) / dt;
        forces.apply_force(accel * MASS);

        // Jump on rising edge.
        let was_jumping = *prev_jump.get(&entity).unwrap_or(&false);
        if input.jump && !was_jumping && grounded {
            forces.apply_linear_impulse(Vec3::Y * (MASS * JUMP_IMPULSE));
        }
        prev_jump.insert(entity, input.jump);
    }
}

/// Each Update, copy each player's avian-driven Transform back into the
/// replicated `NetworkedPosition` so clients see the authoritative pose.
/// Replaces the old write-from-message path in `apply_position_updates`.
fn sync_player_positions(
    mut q: Query<
        (Entity, &Transform, &PlayerInputState, &mut NetworkedPosition),
        (With<NetworkedPlayer>, Changed<Transform>),
    >,
    spatial: SpatialQuery,
) {
    for (entity, tf, input, mut netpos) in &mut q {
        netpos.x = tf.translation.x;
        netpos.y = tf.translation.y;
        netpos.z = tf.translation.z;
        netpos.yaw = input.yaw;
        netpos.pitch = input.pitch;
        netpos.casting = input.casting;
        // Derive airborne server-side instead of trusting client. The
        // ray-filter exclusion has to be THIS player (not "any
        // player" — that was the previous bug; with multiple peers,
        // `players.iter().next()` always returned the first one, so
        // the second player's ray got blocked by its own capsule and
        // `airborne` was stuck false for them).
        let ray_origin = tf.translation + Vec3::new(0.0, -1.0, 0.0);
        let grounded = spatial
            .cast_ray(
                ray_origin,
                Dir3::NEG_Y,
                0.2,
                true,
                &SpatialQueryFilter::from_excluded_entities([entity]),
            )
            .is_some();
        netpos.airborne = !grounded;
    }
}

/// Server-side authoritative arena: ground collider + the prop cubes /
/// spheres. Server simulates physics on the props; positions stream to
/// clients via NetworkedPosition.
fn spawn_arena(mut commands: Commands, mut id_alloc: ResMut<NetworkedIdAlloc>) {
    // Ground (static plane, no replication — clients spawn their own
    // ground visual in world.rs).
    let ground_size = Vec3::new(50.0, 0.5, 50.0);
    commands.spawn((
        Name::new("ServerGround"),
        Transform::from_xyz(0.0, -0.25, 0.0),
        RigidBody::Static,
        Collider::cuboid(ground_size.x, ground_size.y, ground_size.z),
        CollisionLayers::new(GameLayer::Ground, LayerMask::ALL),
    ));

    // Initial spawn positions only — server keeps them static and does
    // NOT stream ongoing position updates. Each client simulates the prop
    // dynamics locally so spells (still local-only for now) can push them.
    // Multiplayer sync of interaction lands when client→server spell
    // casts are wired.
    let props: [(Vec3, PropShape, f32, f32); 6] = [
        (Vec3::new(-2.5, 0.5, 0.0), PropShape::Cube { size: 0.8 }, 2.0, 0.1),
        (Vec3::new(0.0, 0.5, 0.0), PropShape::Sphere { radius: 0.5 }, 3.5, 0.4),
        (Vec3::new(2.5, 0.5, -0.5), PropShape::Cube { size: 0.8 }, 1.0, 0.7),
        (Vec3::new(-1.2, 0.5, -2.5), PropShape::Sphere { radius: 0.5 }, 1.5, 1.0),
        (Vec3::new(1.5, 0.5, -3.0), PropShape::Cube { size: 0.8 }, 4.0, 1.5),
        (Vec3::new(0.3, 0.5, -5.0), PropShape::Sphere { radius: 0.5 }, 2.5, 2.1),
    ];

    for (pos, shape, mass, seed) in props {
        let (collider, name) = match shape {
            PropShape::Cube { size } => (Collider::cuboid(size, size, size), "Prop-Cube"),
            PropShape::Sphere { radius } => (Collider::sphere(radius), "Prop-Sphere"),
            PropShape::Cuboid { x, y, z } => (Collider::cuboid(x, y, z), "Prop-Cuboid"),
        };
        let net_id = id_alloc.next();
        trace::event(
            "prop_spawned",
            json!({"net_id": net_id, "pos": [pos.x, pos.y, pos.z]}),
        );
        commands.spawn((
            Name::new(name),
            NetworkedProp { shape, tint_seed: seed, mass, tint: [0.0, 0.0, 0.0] },
            NetworkedId(net_id),
            NetworkedPosition::from_vec3(pos),
            Transform::from_translation(pos),
            RigidBody::Dynamic,
            collider,
            Mass(mass),
            Friction::new(0.4),
            Restitution::new(0.1),
            CollisionLayers::new(GameLayer::Default, LayerMask::ALL),
            ServerPrevPos::default(),
            ServerPortalLockout::default(),
            Replicate::to_clients(NetworkTarget::All),
        ));
    }
}

/// Copy each prop's avian-driven Transform back into its replicated
/// NetworkedPosition each frame so clients see ongoing physics motion.
fn sync_prop_positions(
    mut q: Query<(&Transform, &mut NetworkedPosition), (With<NetworkedProp>, Changed<Transform>)>,
) {
    for (tf, mut pos) in &mut q {
        pos.x = tf.translation.x;
        pos.y = tf.translation.y;
        pos.z = tf.translation.z;
    }
}

/// Server-side cap on how many lanterns a single client can have placed.
const MAX_LANTERNS_PER_CLIENT: usize = 3;
const LANTERN_RADIUS: f32 = 0.12;
const LANTERN_PICKUP_RADIUS: f32 = 1.8;

fn handle_throw_lantern(
    mut receivers: Query<
        (&RemoteId, &mut MessageReceiver<ThrowLanternMessage>),
        With<ClientOf>,
    >,
    senders: Query<Entity, (With<ClientOf>, With<Connected>)>,
    existing: Query<&NetworkOwner, With<NetworkedLantern>>,
    mut commands: Commands,
    mut id_alloc: ResMut<NetworkedIdAlloc>,
) {
    let current_senders: Vec<Entity> = senders.iter().collect();
    for (RemoteId(peer_id), mut receiver) in &mut receivers {
        let client_id = match peer_id {
            PeerId::Netcode(id) => *id,
            PeerId::Steam(id) => *id,
            PeerId::Local(id) => *id,
            PeerId::Entity(id) => *id,
            _ => continue,
        };
        for msg in receiver.receive() {
            let count = existing.iter().filter(|o| o.0 == client_id).count();
            if count >= MAX_LANTERNS_PER_CLIENT {
                continue;
            }
            let origin = Vec3::new(msg.origin[0], msg.origin[1], msg.origin[2]);
            let velocity = Vec3::new(msg.velocity[0], msg.velocity[1], msg.velocity[2]);
            if !origin.is_finite() || !velocity.is_finite() {
                continue;
            }
            let net_id = id_alloc.next();
            trace::event(
                "lantern_spawned",
                json!({
                    "client_id": client_id,
                    "net_id": net_id,
                    "pos": [origin.x, origin.y, origin.z],
                    "vel": [velocity.x, velocity.y, velocity.z],
                }),
            );
            commands.spawn((
                Name::new(format!("Lantern({client_id})")),
                NetworkedLantern,
                NetworkedId(net_id),
                NetworkOwner(client_id),
                NetworkedPosition::from_vec3(origin),
                // Set both Transform and Position; avian uses Position
                // as the canonical body state and would otherwise start
                // the lantern at (0, 0, 0) before its first sync.
                Transform::from_translation(origin),
                Position(origin),
                LinearVelocity(velocity),
                (
                    RigidBody::Dynamic,
                    Collider::sphere(LANTERN_RADIUS),
                    Mass(0.4),
                    Friction::new(1.0),
                    // Float: ignore gravity (lantern hangs where it
                    // was thrown), heavy linear damping so any push
                    // bleeds off quickly instead of drifting forever.
                    GravityScale(0.0),
                    LinearDamping(2.5),
                    AngularDamping(1.5),
                    Restitution::new(0.3),
                ),
                // Lanterns pass through player capsules so they don't get
                // stuck inside the caster on spawn.
                CollisionLayers::new(
                    GameLayer::Default,
                    [GameLayer::Default, GameLayer::Ground],
                ),
                ServerPrevPos::default(),
                ServerPortalLockout::default(),
                Replicate::manual(current_senders.clone()),
            ));
        }
    }
}

fn handle_pickup_lantern(
    mut receivers: Query<&mut MessageReceiver<PickupLanternMessage>, With<ClientOf>>,
    lanterns: Query<(Entity, &Transform), With<NetworkedLantern>>,
    mut commands: Commands,
) {
    for mut receiver in &mut receivers {
        for msg in receiver.receive() {
            let player_pos = Vec3::new(
                msg.player_position[0],
                msg.player_position[1],
                msg.player_position[2],
            );
            if !player_pos.is_finite() {
                continue;
            }
            for (entity, tf) in &lanterns {
                if player_pos.distance(tf.translation) <= LANTERN_PICKUP_RADIUS {
                    commands.entity(entity).despawn();
                    trace::event(
                        "lantern_picked_up",
                        json!({
                            "entity": format!("{:?}", entity),
                            "pos": [tf.translation.x, tf.translation.y, tf.translation.z],
                        }),
                    );
                }
            }
        }
    }
}

fn sync_lantern_positions(
    mut q: Query<
        (&Transform, &mut NetworkedPosition),
        (With<NetworkedLantern>, Changed<Transform>),
    >,
) {
    for (tf, mut pos) in &mut q {
        pos.x = tf.translation.x;
        pos.y = tf.translation.y;
        pos.z = tf.translation.z;
    }
}

/// Drains `BeamImpulseMessage`s from each connected client and applies the
/// impulse to whichever `NetworkedProp` lies on the ray. The server is
/// authoritative; clients see the resulting motion via the replicated
/// `NetworkedPosition`.
fn apply_beam_impulses(
    spatial: SpatialQuery,
    mut receivers: Query<(&RemoteId, &mut MessageReceiver<BeamImpulseMessage>), With<ClientOf>>,
    props: Query<Entity, With<NetworkedProp>>,
    lanterns: Query<Entity, With<NetworkedLantern>>,
    mut forces: Query<Forces>,
    time: Res<Time>,
    mut beam_owners: Query<(&NetworkOwner, &mut BeamCastTimer), With<NetworkedPlayer>>,
    mut broadcast_senders: Query<&mut MessageSender<BeamCastBroadcast>, With<ClientOf>>,
    client_map: Res<ClientPlayerMap>,
    mut hurtboxes: Query<(&mut Hurtbox, Option<&mut NetworkedHealth>)>,
    mut deaths: ResMut<Messages<DeathEvent>>,
) {
    let now = time.elapsed_secs();
    for (RemoteId(peer_id), mut receiver) in &mut receivers {
        let client_id = match peer_id {
            PeerId::Netcode(id) | PeerId::Steam(id) | PeerId::Local(id) | PeerId::Entity(id) => {
                *id
            }
            _ => continue,
        };
        // Resolve the shooter's player entity so we can (a) exclude
        // them from the raycast (no self-hit) and (b) attribute kill
        // credit when the beam takes someone down. Prior to this the
        // exclude list dropped *every* player, which silently
        // prevented lens-family beams from ever hitting another peer.
        let shooter = client_map.0.get(&client_id).copied();
        let mut excluded: Vec<Entity> = lanterns.iter().collect();
        if let Some(s) = shooter {
            excluded.push(s);
        }
        for msg in receiver.receive() {
            let origin = Vec3::new(msg.origin[0], msg.origin[1], msg.origin[2]);
            let dir_vec = Vec3::new(msg.direction[0], msg.direction[1], msg.direction[2]);
            let Ok(dir) = Dir3::new(dir_vec) else { continue };
            let filter = SpatialQueryFilter::from_excluded_entities(excluded.clone());
            let hit = spatial.cast_ray(origin, dir, msg.range, true, &filter);
            let beam_length = hit.map(|h| h.distance).unwrap_or(msg.range);
            // Mark this client's beam as active server-side so the decay
            // system can flip it off after ~150ms of silence.
            for (owner, mut timer) in &mut beam_owners {
                if owner.0 == client_id {
                    timer.last_update = now;
                    timer.active = true;
                    break;
                }
            }
            // Broadcast to every connected client (including the caster —
            // local visuals already draw their own beam, but we filter
            // out self on receive). This is one BeamCastBroadcast per
            // beam tick the client sent, ~60 Hz max.
            let broadcast = BeamCastBroadcast {
                client_id,
                active: true,
                origin: msg.origin,
                direction: msg.direction,
                length: beam_length,
            };
            for mut sender in &mut broadcast_senders {
                let _ = sender.send::<crate::net::protocol::PlayerInputChannel>(broadcast);
            }
            if let Some(hit) = hit {
                // Impulse: only NetworkedProps are dynamic, so push
                // them only. (Players are kinematic on the server-
                // authoritative path and shouldn't be punted around
                // by impulse anyway.)
                if props.contains(hit.entity) {
                    if let Ok(mut f) = forces.get_mut(hit.entity) {
                        f.apply_linear_impulse(*dir * msg.magnitude);
                        trace::event(
                            "impulse_applied",
                            json!({
                                "target": format!("{:?}", hit.entity),
                                "magnitude": msg.magnitude,
                                "dir": [dir.x, dir.y, dir.z],
                            }),
                        );
                    }
                }
                // Damage: anything with a Hurtbox takes a hit. Iris
                // sends a one-shot burst, convex_lens sends per-frame
                // ticks — both populate `msg.damage` so this branch is
                // generic.
                if msg.damage > 0.0 {
                    if let Ok((mut hurtbox, net_hp)) = hurtboxes.get_mut(hit.entity) {
                        apply_damage_to_hurtbox(
                            hit.entity,
                            &mut hurtbox,
                            net_hp,
                            msg.damage,
                            shooter,
                            &mut deaths,
                        );
                    }
                }
            }
        }
    }
}

/// Server-side per-player beam state: last update time + last broadcast
/// `active` value. When 150ms pass with no further beam impulse, we
/// broadcast `active=false` once to every client so other peers stop
/// rendering the caster's beam.
#[derive(Component)]
struct BeamCastTimer {
    last_update: f32,
    active: bool,
}

fn decay_beam_casts(
    time: Res<Time>,
    mut q: Query<(&NetworkOwner, &mut BeamCastTimer), With<NetworkedPlayer>>,
    mut senders: Query<&mut MessageSender<BeamCastBroadcast>, With<ClientOf>>,
) {
    let now = time.elapsed_secs();
    for (owner, mut timer) in &mut q {
        if timer.active && now - timer.last_update > 0.15 {
            timer.active = false;
            for mut sender in &mut senders {
                let _ = sender.send::<crate::net::protocol::PlayerInputChannel>(
                    BeamCastBroadcast {
                        client_id: owner.0,
                        active: false,
                        origin: [0.0; 3],
                        direction: [0.0; 3],
                        length: 0.0,
                    },
                );
            }
        }
    }
}

/// Server-authoritative portal placement. Clients raycast locally and send
/// `PlacePortalMessage`; the server despawns any existing portal in the
/// requested slot (globally — portals are shared) and spawns a replicated
/// `NetworkedPortal` so every connected client materializes it.
fn handle_place_portal(
    mut receivers: Query<&mut MessageReceiver<PlacePortalMessage>, With<ClientOf>>,
    senders: Query<Entity, (With<ClientOf>, With<Connected>)>,
    existing: Query<(Entity, &NetworkedPortal)>,
    mut commands: Commands,
    mut id_alloc: ResMut<NetworkedIdAlloc>,
) {
    let current_senders: Vec<Entity> = senders.iter().collect();
    for mut receiver in &mut receivers {
        for msg in receiver.receive() {
            let pos = Vec3::new(msg.position[0], msg.position[1], msg.position[2]);
            let normal = Vec3::new(msg.normal[0], msg.normal[1], msg.normal[2]);
            if !pos.is_finite() || !normal.is_finite() {
                continue;
            }
            for (e, np) in &existing {
                if np.slot == msg.slot {
                    commands.entity(e).despawn();
                }
            }
            let net_id = id_alloc.next();
            trace::event(
                "portal_placed",
                json!({
                    "slot": format!("{:?}", msg.slot),
                    "net_id": net_id,
                    "pos": [pos.x, pos.y, pos.z],
                    "normal": msg.normal,
                }),
            );
            let normal_vec = Vec3::new(msg.normal[0], msg.normal[1], msg.normal[2]);
            let rotation = disc_rotation(normal_vec);
            commands.spawn((
                Name::new(format!("Portal({:?})", msg.slot)),
                NetworkedPortal {
                    slot: msg.slot,
                    normal: msg.normal,
                },
                NetworkedId(net_id),
                NetworkedPosition::from_vec3(pos),
                Transform::from_translation(pos).with_rotation(rotation),
                ServerPrevPortalPose {
                    translation: pos,
                    rotation,
                },
                Replicate::manual(current_senders.clone()),
            ));
        }
    }
}

/// Server-authoritative per-tick state for the `handheld_portal` spell.
/// First `active: true` message in a `(client_id, slot)` spawns a
/// `HandheldPortal`-tagged `NetworkedPortal` (despawning any existing
/// portal in that slot — handheld portals replace placed ones for
/// their slot, matching the existing portal placement semantics).
/// Subsequent messages update the existing portal's pose in place so
/// every observer sees a continuous slide rather than spawn/despawn
/// churn. `active: false` despawns.
///
/// Pose handling: `Transform.translation/rotation` AND `Position`
/// (avian canonical) AND `NetworkedPosition.{x,y,z,yaw,pitch}` are all
/// written each tick. Translation flows to clients via the existing
/// portal `sync_networked_positions` system; rotation flows via
/// `yaw`/`pitch` on `NetworkedPosition` (client-side
/// `sync_handheld_portal_rotation` in `spells::handheld_portal`
/// re-derives `Transform.rotation` from them — `NetworkedPortal.normal`
/// updates are unreliable per CLAUDE.md, so we route via the reliable
/// per-tick channel instead).
fn handle_hold_portal(
    mut receivers: Query<
        (&RemoteId, &mut MessageReceiver<HoldPortalMessage>),
        With<ClientOf>,
    >,
    senders: Query<Entity, (With<ClientOf>, With<Connected>)>,
    existing: Query<(Entity, &NetworkedPortal), Without<HandheldPortal>>,
    // Portals are Transform-only (no RigidBody / avian Position), same
    // as `handle_place_portal`'s spawn — querying for `&mut Position`
    // here would silently miss every update.
    mut held: Query<
        (&mut Transform, &mut NetworkedPosition, &mut NetworkedPortal),
        With<HandheldPortal>,
    >,
    mut registry: ResMut<HeldPortalRegistry>,
    mut commands: Commands,
    mut id_alloc: ResMut<NetworkedIdAlloc>,
) {
    let current_senders: Vec<Entity> = senders.iter().collect();
    for (RemoteId(peer_id), mut receiver) in &mut receivers {
        let client_id = match peer_id {
            PeerId::Netcode(id)
            | PeerId::Steam(id)
            | PeerId::Local(id)
            | PeerId::Entity(id) => *id,
            _ => continue,
        };
        for msg in receiver.receive() {
            let pos = Vec3::new(msg.position[0], msg.position[1], msg.position[2]);
            if !pos.is_finite() || !msg.yaw.is_finite() || !msg.pitch.is_finite() {
                continue;
            }
            let key = (client_id, msg.slot);
            if !msg.active {
                if let Some(entity) = registry.by_owner_slot.remove(&key) {
                    commands.entity(entity).despawn();
                }
                continue;
            }

            let forward = forward_from_yaw_pitch(msg.yaw, msg.pitch);
            let normal = -forward;
            let rotation = disc_rotation(normal);
            let normal_arr = [normal.x, normal.y, normal.z];

            if let Some(&entity) = registry.by_owner_slot.get(&key) {
                if let Ok((mut tf, mut netpos, mut np)) = held.get_mut(entity) {
                    tf.translation = pos;
                    tf.rotation = rotation;
                    netpos.x = pos.x;
                    netpos.y = pos.y;
                    netpos.z = pos.z;
                    netpos.yaw = msg.yaw;
                    netpos.pitch = msg.pitch;
                    np.normal = normal_arr;
                }
                // If `get_mut` failed here, the most common cause is
                // that we spawned the entity earlier in *this same
                // system call* via `commands.spawn(...)` and Commands
                // haven't flushed yet — the entity is real but not
                // visible to the query. Skipping is safe: the next
                // message (next frame) finds it. Falling through to
                // respawn would leak the just-spawned entity (it
                // carries `HandheldPortal`, so the
                // `Without<HandheldPortal>` conflict scan below
                // wouldn't despawn it).
                continue;
            }

            // Despawn any conflicting portal in this slot — handheld
            // portals replace placed ones. Placed portals match via the
            // `Without<HandheldPortal>` query above; conflicting handheld
            // portals from other carriers are dropped from the registry
            // and despawned.
            for (e, np) in &existing {
                if np.slot == msg.slot {
                    commands.entity(e).despawn();
                }
            }
            let other_holds: Vec<_> = registry
                .by_owner_slot
                .iter()
                .filter(|((_, slot), _)| *slot == msg.slot)
                .map(|(k, _)| *k)
                .collect();
            for k in other_holds {
                if let Some(e) = registry.by_owner_slot.remove(&k) {
                    commands.entity(e).despawn();
                }
            }

            let net_id = id_alloc.next();
            let mut netpos = NetworkedPosition::from_vec3(pos);
            netpos.yaw = msg.yaw;
            netpos.pitch = msg.pitch;
            trace::event(
                "handheld_portal_spawned",
                json!({
                    "client_id": client_id,
                    "slot": format!("{:?}", msg.slot),
                    "net_id": net_id,
                    "pos": [pos.x, pos.y, pos.z],
                }),
            );
            let entity = commands
                .spawn((
                    Name::new(format!(
                        "HandheldPortal({client_id},{:?})",
                        msg.slot
                    )),
                    NetworkedPortal {
                        slot: msg.slot,
                        normal: normal_arr,
                    },
                    HandheldPortal,
                    NetworkOwner(client_id),
                    NetworkedId(net_id),
                    netpos,
                    Transform::from_translation(pos).with_rotation(rotation),
                    ServerPrevPortalPose {
                        translation: pos,
                        rotation,
                    },
                    Replicate::manual(current_senders.clone()),
                ))
                .id();
            registry.by_owner_slot.insert(key, entity);
        }
    }
}

/// Camera forward direction (world -Z, then pitch around X, then yaw
/// around Y) for the carrier of a handheld portal. Mirrors the
/// client's `apply_rotation` + camera-child pitch chain so the server
/// derives the same forward vector the client sees.
fn forward_from_yaw_pitch(yaw: f32, pitch: f32) -> Vec3 {
    let yaw_q = Quat::from_axis_angle(Vec3::Y, yaw);
    let pitch_q = Quat::from_axis_angle(Vec3::X, pitch);
    (yaw_q * pitch_q) * Vec3::NEG_Z
}

/// Generic server-spawn for `DeliveryDef::SpawnBody` / `Place`. Every
/// data-driven spell that wants to launch a physical body sends a
/// `SpawnBodyMessage` from its delivery; the server materializes a
/// `NetworkedProp` with the requested physics and replicates it to all
/// clients. No bespoke message + handler per spell.
///
/// If the message carries a `parent_cast`, the server looks up that cast
/// in its `SpellCatalog` and — if the cast's `PayloadDef::SpawnBody` has
/// `on_event` hooks — attaches a `BodyTriggers` component so collisions
/// and timers fire child casts via the trigger plugin.
fn handle_spawn_body(
    mut receivers: Query<
        (&RemoteId, &mut MessageReceiver<SpawnBodyMessage>),
        With<ClientOf>,
    >,
    senders: Query<Entity, (With<ClientOf>, With<Connected>)>,
    catalog: Option<Res<crate::spells::catalog::SpellCatalog>>,
    body_catalog: Option<Res<crate::spells::catalog::BodyCatalog>>,
    client_map: Res<ClientPlayerMap>,
    mut commands: Commands,
    mut id_alloc: ResMut<NetworkedIdAlloc>,
) {
    let current_senders: Vec<Entity> = senders.iter().collect();
    for (RemoteId(peer_id), mut receiver) in &mut receivers {
        let sender_client_id = match peer_id {
            PeerId::Netcode(id) | PeerId::Steam(id) | PeerId::Local(id) | PeerId::Entity(id) => {
                Some(*id)
            }
            _ => None,
        };
        let original_caster: Option<Entity> = sender_client_id
            .and_then(|id| client_map.0.get(&id).copied());
        for msg in receiver.receive() {
            let origin = Vec3::new(msg.origin[0], msg.origin[1], msg.origin[2]);
            let velocity =
                Vec3::new(msg.velocity[0], msg.velocity[1], msg.velocity[2]);
            if !origin.is_finite() || !velocity.is_finite() {
                continue;
            }
            let collider = match msg.shape {
                PropShape::Cube { size } => Collider::cuboid(size, size, size),
                PropShape::Sphere { radius } => Collider::sphere(radius),
                PropShape::Cuboid { x, y, z } => Collider::cuboid(x, y, z),
            };
            let net_id = id_alloc.next();
            trace::event(
                "spawn_body",
                json!({
                    "net_id": net_id,
                    "pos": [origin.x, origin.y, origin.z],
                    "vel": [velocity.x, velocity.y, velocity.z],
                }),
            );
            // Resolve parent_cast → BodyTriggers (if any).
            let triggers = msg.parent_cast.as_ref().and_then(|p| {
                let catalog = catalog.as_ref()?;
                let spell = catalog
                    .get(&crate::spells::SpellId(p.spell_id.clone()))?;
                let cast = spell.casts.iter().find(|c| c.id.0 == p.cast_id)?;
                let crate::spells::data::PayloadDef::SpawnBody { on_event, .. } =
                    &cast.payload
                else {
                    return None;
                };
                if on_event.is_empty() {
                    return None;
                }
                Some(crate::spells::triggers::BodyTriggers {
                    events: on_event.clone(),
                    // Resolved from the sending client's RemoteId via
                    // `ClientPlayerMap`. `None` means the caster
                    // disconnected before the body triggered, or the
                    // peer was an unsupported PeerId variant.
                    original_caster,
                    captured_charge: p.captured_charge,
                    chain_depth: p.chain_depth,
                    caused_by: Some(crate::spells::data::TriggerSpec {
                        spell_id: crate::spells::SpellId(p.spell_id.clone()),
                        cast_id: crate::spells::data::CastId(p.cast_id.clone()),
                    }),
                    fired: false,
                    elapsed: 0.0,
                })
            });
            let mut entity = commands.spawn((
                Name::new(format!("SpawnedBody({net_id})")),
                NetworkedProp {
                    shape: msg.shape,
                    tint_seed: msg.tint_seed,
                    mass: msg.mass,
                    tint: msg.tint,
                },
                NetworkedId(net_id),
                NetworkedPosition::from_vec3(origin),
                Transform::from_translation(origin),
                Position(origin),
                LinearVelocity(velocity),
                (
                    RigidBody::Dynamic,
                    collider,
                    Mass(msg.mass),
                    Friction::new(msg.friction),
                    LinearDamping(msg.linear_damping),
                    AngularDamping(msg.angular_damping),
                    Restitution::new(msg.restitution),
                    CollisionLayers::new(GameLayer::Default, LayerMask::ALL),
                ),
                ServerPrevPos::default(),
                ServerPortalLockout::default(),
                Replicate::manual(current_senders.clone()),
            ));
            if let Some(triggers) = triggers {
                entity.insert(triggers);
            }

            // Attach server-side body markers (e.g. `RollingGlacier`,
            // `FrostSpike`) so the bespoke ice / projectile systems can
            // find what they need to drive. Resolved from the parent
            // cast's `SpawnBody.template` via the body catalog so the
            // wire format doesn't need to learn about marker variants.
            let markers: Option<Vec<crate::spells::data::MarkerKind>> = msg
                .parent_cast
                .as_ref()
                .and_then(|p| {
                    let cat = catalog.as_ref()?;
                    let spell =
                        cat.get(&crate::spells::SpellId(p.spell_id.clone()))?;
                    let cast = spell.casts.iter().find(|c| c.id.0 == p.cast_id)?;
                    let crate::spells::data::PayloadDef::SpawnBody { template, .. } =
                        &cast.payload
                    else {
                        return None;
                    };
                    let body_cat = body_catalog.as_ref()?;
                    let body = body_cat.get(template)?;
                    Some(body.markers.clone())
                });
            if let Some(markers) = markers {
                for marker in markers {
                    match marker {
                        crate::spells::data::MarkerKind::Lantern => {}
                        crate::spells::data::MarkerKind::PortalTraveler => {
                            // Already in the portal-traveler set via the
                            // `Or<(NetworkedProp, NetworkedLantern, …)>`
                            // filter in `server_portal_teleport`; no
                            // explicit marker required.
                        }
                        crate::spells::data::MarkerKind::RollingGlacier => {
                            entity.insert(crate::spells::markers::RollingGlacier {
                                caster: original_caster,
                                ..Default::default()
                            });
                        }
                        crate::spells::data::MarkerKind::FrostSpike => {
                            // See `spells::bodies::spawn_body` — this path
                            // is dead today; `frost_spire` spawns its
                            // own spike directly. Placeholder zeros.
                            entity.insert(crate::spells::markers::FrostSpike {
                                lifetime: 180.0,
                                rise_remaining: 0.0,
                                damage: 0.0,
                                caster: original_caster,
                            });
                        }
                    }
                }
            }
        }
    }
}

/// Server-authoritative portal teleport for replicated dynamic bodies
/// (props + lanterns). Ported from `spells::portal::process_traveler_teleports`
/// — same crossing math, but iterates `NetworkedPortal` + `NetworkedProp`
/// / `NetworkedLantern` instead of `Portal` / `PortalTraveler`. Without
/// this, server physics pushes a prop straight through the portal plane
/// and every observer sees it emerge on the wrong side.
fn server_portal_teleport(
    portals: Query<(Entity, &NetworkedPortal, &Transform, &ServerPrevPortalPose)>,
    mut travelers: Query<
        (
            Entity,
            &mut Transform,
            &mut Position,
            &mut LinearVelocity,
            &mut ServerPrevPos,
            &mut ServerPortalLockout,
        ),
        (
            Without<NetworkedPortal>,
            // Players are travelers too as of Stage Q.5b — when the
            // local rig walks into a portal, the server (which now
            // owns the authoritative player position) needs to do the
            // teleport, otherwise `sync_local_player_from_server`
            // snaps the local rig back next frame.
            Or<(
                With<NetworkedProp>,
                With<NetworkedLantern>,
                With<NetworkedPlayer>,
            )>,
        ),
    >,
    // Player-specific writeback: server-side rotation gets clobbered by
    // `apply_player_rotation` each FixedUpdate (it sets Rotation from
    // PlayerInputState.yaw), so the teleport rotation needs to flow
    // through input.yaw too. And the client's local `Facing.yaw`
    // (camera direction + next input) needs a snap message to match.
    mut player_inputs: Query<(&NetworkOwner, &mut PlayerInputState), With<NetworkedPlayer>>,
    mut snap_senders: Query<&mut MessageSender<TeleportSnap>, With<ClientOf>>,
) {
    const LOCKOUT_RADIUS: f32 = PORTAL_RADIUS * 1.5;

    // Each portal contributes (entity, current_tf, prev_tf). Prev pose
    // is used for the prev_along crossing math so a moving portal
    // (handheld) sweeping past a stationary traveler still registers as
    // a crossing — without prev_tf, both sides of the comparison use
    // the same current portal pose and the delta is just the traveler's
    // motion.
    let mut primary: Option<(Entity, Transform, Transform)> = None;
    let mut secondary: Option<(Entity, Transform, Transform)> = None;
    for (e, np, tf, prev_pose) in &portals {
        let mut tf = *tf;
        // `handle_place_portal` already sets rotation, but defend against
        // entities older than that change.
        if tf.rotation == Quat::IDENTITY {
            tf.rotation = disc_rotation(Vec3::from(np.normal));
        }
        let prev_tf = Transform {
            translation: prev_pose.translation,
            rotation: prev_pose.rotation,
            scale: Vec3::ONE,
        };
        match np.slot {
            crate::spells::portal::PortalSlot::Primary => {
                primary = Some((e, tf, prev_tf))
            }
            crate::spells::portal::PortalSlot::Secondary => {
                secondary = Some((e, tf, prev_tf))
            }
        }
    }

    let pair = match (primary, secondary) {
        (Some(p), Some(s)) => Some((p, s)),
        _ => None,
    };

    for (entity, mut tf, mut avian_pos, mut velocity, mut prev, mut lockout) in
        &mut travelers
    {
        let pos = tf.translation;

        // Clear lockouts for portals this traveler has moved away from
        // (or that no longer exist).
        lockout.0.retain(|portal_e| match portals.get(*portal_e) {
            Ok((_, _, ptf, _)) => (pos - ptf.translation).length() <= LOCKOUT_RADIUS,
            Err(_) => false,
        });

        let Some((
            (primary_e, primary_tf, primary_prev),
            (secondary_e, secondary_tf, secondary_prev),
        )) = pair
        else {
            prev.0 = Some(pos);
            continue;
        };

        let pairs = [
            (
                primary_e,
                secondary_e,
                primary_tf,
                secondary_tf,
                primary_prev,
            ),
            (
                secondary_e,
                primary_e,
                secondary_tf,
                primary_tf,
                secondary_prev,
            ),
        ];

        let prev_pos = prev.0.unwrap_or(pos);
        let mut teleported_to: Option<Vec3> = None;

        for (entry_entity, exit_entity, entry, exit, prev_entry) in pairs {
            if lockout.0.contains(&entry_entity) {
                continue;
            }

            let entry_normal = (entry.rotation * Vec3::Y).normalize();
            let prev_entry_normal = (prev_entry.rotation * Vec3::Y).normalize();
            // prev_along uses the *previous* entry pose so portal motion
            // is part of the relative delta; curr_along uses the current
            // pose so we always teleport from the up-to-date disc.
            let prev_along =
                (prev_pos - prev_entry.translation).dot(prev_entry_normal);
            let curr_along = (pos - entry.translation).dot(entry_normal);
            let crossed = (prev_along > 0.0) != (curr_along > 0.0);
            if !crossed {
                continue;
            }

            let denom = curr_along - prev_along;
            let t = if denom.abs() > 1e-6 {
                (-prev_along / denom).clamp(0.0, 1.0)
            } else {
                0.0
            };
            // Radial check in the *relative* (traveler − entry) frame so
            // a portal sweeping over a stationary traveler still passes:
            // both relative positions resolve to the traveler's offset
            // from each disc, lerped at crossing time. Using absolute
            // world positions would lerp a fixed point that the moving
            // portal can sit far away from.
            let prev_rel = prev_pos - prev_entry.translation;
            let curr_rel = pos - entry.translation;
            let cross_rel = prev_rel.lerp(curr_rel, t);
            let cross_radial =
                (cross_rel - entry_normal * cross_rel.dot(entry_normal)).length();
            if cross_radial > PORTAL_RADIUS {
                continue;
            }

            // Pick the basis position AND the entry pose it was
            // measured against together. With a moving (handheld)
            // portal, the same world point can be on opposite sides of
            // prev_entry and curr_entry — using basis_pos with the
            // *current* entry pose would compute local_pos in a frame
            // where the traveler is on the wrong side of the disc's
            // normal, so the exit-side math places them below the
            // exit disc and they immediately collide with whatever's
            // behind it (the floor, for ground portals).
            let (basis_pos, basis_entry) = if prev_along > 0.0 {
                (prev_pos, prev_entry)
            } else {
                (pos, entry)
            };
            let basis_entry_inv = basis_entry.rotation.inverse();
            // Anti-parallel portal pairs (opposite walls facing each
            // other) get an X_local flip on position/rotation/velocity
            // to cancel the natural world-X mirror that comes from
            // entry vs exit walls having opposite X_local world
            // directions. Parallel / perpendicular pairs (ground-to-
            // ground, wall-to-floor, etc.) don't have that mirror in
            // the first place, so applying the same flip would
            // introduce one — which is what broke ground portals.
            let needs_x_flip = {
                let na = (basis_entry.rotation * Vec3::Y).normalize_or_zero();
                let nb = (exit.rotation * Vec3::Y).normalize_or_zero();
                na.dot(nb) < -0.5
            };
            let rot_flip = Quat::from_rotation_z(core::f32::consts::PI);
            // For velocity: Z-rot π flips X_local + Y_local (anti-parallel
            // cancel + "into → out of"). For non-anti-parallel pairs use
            // X-rot π which only flips Y_local + Z_local so the world-X
            // velocity component is preserved.
            let vel_flip = if needs_x_flip {
                rot_flip
            } else {
                Quat::from_rotation_x(core::f32::consts::PI)
            };
            let q_vel = exit.rotation * vel_flip * basis_entry_inv;

            let is_player = player_inputs.contains(entity);
            let (new_pos, new_rot) = if is_player {
                let basis_tf = Transform {
                    translation: basis_pos,
                    rotation: tf.rotation,
                    scale: Vec3::ONE,
                };
                let virtual_tf = crate::spells::portal::portal_virtual_transform(
                    basis_tf,
                    &basis_entry,
                    &exit,
                );
                (virtual_tf.translation, virtual_tf.rotation)
            } else {
                let mut local_pos =
                    basis_entry_inv * (basis_pos - basis_entry.translation);
                if needs_x_flip {
                    local_pos.x = -local_pos.x;
                }
                // One-sided ground exit: the -Y side of the disc is
                // buried in the floor, so the only accessible side is
                // +Y. Whichever side of the entry the body came from,
                // emerge above the floor. Without this, walking into a
                // floating portal from behind drops the body below the
                // ground exit and it collides with the floor from the
                // wrong side.
                let exit_normal_world =
                    (exit.rotation * Vec3::Y).normalize_or_zero();
                if exit_normal_world.y
                    >= crate::spells::portal::HORIZONTAL_NORMAL_DOT_Y
                {
                    local_pos.y = local_pos.y.abs();
                }
                let new_pos = exit.translation + exit.rotation * local_pos;
                let q_rot = if needs_x_flip {
                    exit.rotation * rot_flip * basis_entry_inv
                } else {
                    exit.rotation * basis_entry_inv
                };
                (new_pos, q_rot * tf.rotation)
            };

            // Write avian's Position (canonical under
            // LightyearAvianPlugin's sync) AND Transform so any system
            // reading either within the same frame sees the post-teleport
            // pose. NetworkedPlayer's `sync_player_positions` will pick
            // up the new Transform and ship it via NetworkedPosition,
            // which `sync_local_player_from_server` then snaps to the
            // local rig — so the teleport actually sticks on the local
            // client too.
            tf.translation = new_pos;
            tf.rotation = new_rot;
            avian_pos.0 = new_pos;
            velocity.0 = q_vel * velocity.0;

            // Player teleport rotation handoff. Two consumers need to
            // know the new yaw: (1) the server's own
            // `apply_player_rotation` (FixedUpdate) — without an
            // updated PlayerInputState.yaw, the rotation we just wrote
            // gets clobbered next tick back to the pre-teleport
            // direction; (2) the local client's `Facing.yaw` — drives
            // the camera direction and what yaw the next input
            // message carries. Both fed from the post-teleport
            // forward vector via the same atan2 the retired
            // local-only `process_teleports` used.
            if let Ok((owner, mut input)) = player_inputs.get_mut(entity) {
                let new_fwd = tf.rotation * Vec3::NEG_Z;
                let horiz_len_sq = new_fwd.x * new_fwd.x + new_fwd.z * new_fwd.z;
                if horiz_len_sq > 1e-6 {
                    let inv = horiz_len_sq.sqrt().recip();
                    let new_yaw = (-new_fwd.x * inv).atan2(-new_fwd.z * inv);
                    input.yaw = new_yaw;
                    let snap = TeleportSnap {
                        client_id: owner.0,
                        new_yaw,
                    };
                    for mut sender in &mut snap_senders {
                        let _ =
                            sender.send::<crate::net::protocol::PlayerInputChannel>(snap);
                    }
                }
            }

            lockout.0.insert(entry_entity);
            lockout.0.insert(exit_entity);
            teleported_to = Some(new_pos);
            trace::event(
                "prop_teleported",
                json!({
                    "entity": format!("{:?}", entity),
                    "from_portal": format!("{:?}", entry_entity),
                    "to_pos": [new_pos.x, new_pos.y, new_pos.z],
                }),
            );
            break;
        }

        prev.0 = Some(teleported_to.unwrap_or(pos));
    }
}

/// At the end of each Update, copy every portal's current Transform
/// into its `ServerPrevPortalPose` so the next tick's
/// `server_portal_teleport` sees the right "previous" pose. Scheduled
/// `.after(server_portal_teleport)` to guarantee that the teleport
/// pass reads the value from the prior frame, not the one we just
/// wrote.
fn update_prev_portal_pose(
    mut q: Query<(&Transform, &mut ServerPrevPortalPose), With<NetworkedPortal>>,
) {
    for (tf, mut prev) in &mut q {
        prev.translation = tf.translation;
        prev.rotation = tf.rotation;
    }
}

/// Spawns one `TestCube` at startup with replication enabled to all
/// clients. Stage-N proof that the wire actually carries components.
fn spawn_test_cube(mut commands: Commands) {
    commands.spawn((
        TestCube,
        Replicate::to_clients(NetworkTarget::All),
    ));
    info!("Spawned replicated TestCube.");
}

/// Drains `ChargeStateMessage` from each connected client and rebroadcasts
/// it as `ChargeStateBroadcast` (stamped with the sender's `client_id`) to
/// every other client so they can render the caster's third-person charge
/// orb. Pure relay — the server doesn't run the cast engine, so this never
/// reads or mutates server-side gameplay state.
fn relay_charge_states(
    mut receivers: Query<
        (&RemoteId, &mut MessageReceiver<ChargeStateMessage>),
        With<ClientOf>,
    >,
    mut broadcast_senders: Query<&mut MessageSender<ChargeStateBroadcast>, With<ClientOf>>,
) {
    for (RemoteId(peer_id), mut receiver) in &mut receivers {
        let client_id = match peer_id {
            PeerId::Netcode(id) | PeerId::Steam(id) | PeerId::Local(id) | PeerId::Entity(id) => {
                *id
            }
            _ => continue,
        };
        for msg in receiver.receive() {
            let broadcast = ChargeStateBroadcast {
                client_id,
                active: msg.active,
                ratio: msg.ratio,
                tint: msg.tint,
            };
            for mut sender in &mut broadcast_senders {
                let _ = sender.send::<crate::net::protocol::PlayerInputChannel>(broadcast);
            }
        }
    }
}

/// Server-side mirror of `spells::portal::update_player_falling`: when a
/// `NetworkedPlayer` is standing on top of a horizontal portal disc,
/// drop the `Ground` layer from their collision filter so avian's
/// gravity actually pulls them through. Without this the floor collider
/// keeps the server's authoritative player above the disc and the
/// teleport plane is never crossed — ground portals just look like
/// painted spots on the floor.
///
/// Client-side `update_player_falling` runs in `PortalPlugin` which the
/// server doesn't load (no `SpellsPlugin` on the server). And since
/// Stage Q.5b the local client rig is kinematic, so flipping its
/// collision layers wouldn't help — the position comes from the server
/// via `NetworkedPosition` regardless.
fn update_networked_player_falling(
    mut commands: Commands,
    portals: Query<(Entity, &NetworkedPortal, &Transform), Without<NetworkedPlayer>>,
    mut players: Query<
        (
            Entity,
            &Transform,
            &CollisionLayers,
            &ServerPortalLockout,
        ),
        With<NetworkedPlayer>,
    >,
) {
    use crate::spells::portal::{
        PortalSlot, HORIZONTAL_DISC_DEPTH, HORIZONTAL_NORMAL_DOT_Y, PORTAL_RADIUS,
    };
    // Only drop the Ground layer when *both* portal slots exist —
    // otherwise the player falls through the floor with nowhere to
    // emerge. With a single portal placed, leave the floor solid so
    // the disc reads as a painted spot rather than a hole into the
    // void.
    let mut has_primary = false;
    let mut has_secondary = false;
    for (_, np, _) in &portals {
        match np.slot {
            PortalSlot::Primary => has_primary = true,
            PortalSlot::Secondary => has_secondary = true,
        }
    }
    let pair_exists = has_primary && has_secondary;

    for (player_entity, player_tf, layers, lockout) in &mut players {
        let mut in_threshold = false;
        if pair_exists {
            for (e, _, portal_tf) in &portals {
                if lockout.0.contains(&e) {
                    continue;
                }
                let normal = (portal_tf.rotation * Vec3::Y).normalize_or_zero();
                if normal.y.abs() < HORIZONTAL_NORMAL_DOT_Y {
                    continue;
                }
                let rel = player_tf.translation - portal_tf.translation;
                let along = rel.dot(normal);
                let radial = (rel - normal * along).length();
                if radial < PORTAL_RADIUS && along.abs() < HORIZONTAL_DISC_DEPTH {
                    in_threshold = true;
                    break;
                }
            }
        }

        let target = if in_threshold {
            CollisionLayers::new(GameLayer::Player, [GameLayer::Default, GameLayer::Player])
        } else {
            CollisionLayers::new(GameLayer::Player, LayerMask::ALL)
        };

        if *layers != target {
            commands.entity(player_entity).insert(target);
        }
    }
}

/// Server-side mirror for replicated props + lanterns (the things that
/// can roll/throw onto ground portals OR be hurled at surface portals).
/// Same idea as `update_networked_player_falling`, but also has to
/// handle the *surface* case: a fast prop (fireball, glacier) thrown
/// at a wall-mounted portal would otherwise collide with the wall
/// behind the portal (the portal sits at wall + `SURFACE_INSET` ≈ 2
/// cm) one physics step *before* `server_portal_teleport` sees the
/// sign-flip — and any `OnCollision`-triggered body explodes on the
/// wrong wall before it ever crosses.
///
/// Per-portal threshold ⇒ which layer to drop:
/// - horizontal (ground) portal, prop within `HORIZONTAL_DISC_DEPTH` ⇒
///   drop `Ground` so gravity pulls it through the floor.
/// - surface (wall) portal, prop within `SURFACE_DISC_DEPTH` along the
///   wall normal ⇒ drop `Default` so the prop sails through the wall
///   and reaches the portal plane intact.
///
/// Both can be true at once (a prop wedged where two portals overlap);
/// the target filter ANDs both drops.
fn update_networked_prop_falling(
    mut commands: Commands,
    portals: Query<
        (Entity, &NetworkedPortal, &Transform),
        (Without<NetworkedProp>, Without<NetworkedLantern>),
    >,
    mut props: Query<
        (
            Entity,
            &Transform,
            &CollisionLayers,
            &ServerPortalLockout,
        ),
        (
            Or<(With<NetworkedProp>, With<NetworkedLantern>)>,
            Without<NetworkedPortal>,
        ),
    >,
) {
    use crate::spells::portal::{
        PortalSlot, HORIZONTAL_DISC_DEPTH, HORIZONTAL_NORMAL_DOT_Y, PORTAL_RADIUS,
        SURFACE_DISC_DEPTH,
    };
    // Same pair-gate as `update_networked_player_falling`: with only
    // one slot placed, a prop near the disc has nowhere to emerge, so
    // keep its collision filter intact and let it sit on top of the
    // disc rather than fall through into the void.
    let mut has_primary = false;
    let mut has_secondary = false;
    for (_, np, _) in &portals {
        match np.slot {
            PortalSlot::Primary => has_primary = true,
            PortalSlot::Secondary => has_secondary = true,
        }
    }
    let pair_exists = has_primary && has_secondary;

    for (prop_entity, prop_tf, layers, lockout) in &mut props {
        let mut near_horizontal = false;
        let mut near_surface = false;
        if pair_exists {
            for (e, _, portal_tf) in &portals {
                if lockout.0.contains(&e) {
                    continue;
                }
                let normal = (portal_tf.rotation * Vec3::Y).normalize_or_zero();
                let rel = prop_tf.translation - portal_tf.translation;
                let along = rel.dot(normal);
                let radial = (rel - normal * along).length();
                if radial >= PORTAL_RADIUS {
                    continue;
                }
                if normal.y.abs() >= HORIZONTAL_NORMAL_DOT_Y {
                    if along.abs() < HORIZONTAL_DISC_DEPTH {
                        near_horizontal = true;
                    }
                } else if along.abs() < SURFACE_DISC_DEPTH {
                    near_surface = true;
                }
                if near_horizontal && near_surface {
                    break;
                }
            }
        }

        let target = match (near_horizontal, near_surface) {
            (false, false) => CollisionLayers::new(GameLayer::Default, LayerMask::ALL),
            (true, false) => {
                // Drop Ground so the prop falls through the floor.
                CollisionLayers::new(GameLayer::Default, [GameLayer::Default, GameLayer::Player])
            }
            (false, true) => {
                // Drop Default so the prop passes through the wall.
                CollisionLayers::new(GameLayer::Default, [GameLayer::Ground, GameLayer::Player])
            }
            (true, true) => {
                // Drop both — overlapping portals of different kinds.
                CollisionLayers::new(GameLayer::Default, [GameLayer::Player])
            }
        };

        if *layers != target {
            commands.entity(prop_entity).insert(target);
        }
    }
}

/// Drain `EquipWeaponsMessage` from each connected client and stamp the
/// loadout onto that client's `NetworkedPlayer.{EquippedWeapons,
/// ActiveWeaponSlot}`. Replication then carries the change to every
/// observer. No validation here — weapon ids are authored data, and a
/// bad id just shows up as an empty slot in each observer's
/// `WeaponCatalog` lookup.
fn drain_equip_messages(
    mut receivers: Query<
        (&RemoteId, &mut MessageReceiver<EquipWeaponsMessage>),
        With<ClientOf>,
    >,
    mut players: Query<
        (&NetworkOwner, &mut EquippedWeapons, &mut ActiveWeaponSlot),
        With<NetworkedPlayer>,
    >,
) {
    for (RemoteId(peer_id), mut receiver) in &mut receivers {
        let client_id = match peer_id {
            PeerId::Netcode(id) | PeerId::Steam(id) | PeerId::Local(id) | PeerId::Entity(id) => {
                *id
            }
            _ => continue,
        };
        let mut latest: Option<EquipWeaponsMessage> = None;
        for msg in receiver.receive() {
            latest = Some(msg);
        }
        let Some(msg) = latest else { continue };
        for (owner, mut equipped, mut active) in &mut players {
            if owner.0 != client_id {
                continue;
            }
            let new_equipped = [
                msg.slots[0].clone().map(WeaponId::new),
                msg.slots[1].clone().map(WeaponId::new),
            ];
            if equipped.0 != new_equipped {
                equipped.0 = new_equipped;
            }
            if active.0 != msg.active {
                active.0 = msg.active;
            }
            break;
        }
    }
}
