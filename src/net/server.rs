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
    BeamCastBroadcast, BeamImpulseMessage, NetworkOwner, NetworkedId, NetworkedLantern,
    NetworkedPlayer, NetworkedPortal, NetworkedPosition, NetworkedProp,
    PickupLanternMessage, PlacePortalMessage, PlayerInputMessage, PropShape,
    SpawnBodyMessage, TestCube, ThrowLanternMessage,
};
use lightyear::prelude::MessageSender;
use crate::physics::GameLayer;
use crate::net::{default_server_addr, ProtocolPlugin, NETCODE_KEY, PROTOCOL_ID, TICK_HZ};
use crate::spells::portal::{disc_rotation, PORTAL_RADIUS};
use crate::trace;
use serde_json::json;
use std::collections::HashSet;

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
        .add_systems(Startup, (spawn_server, spawn_test_cube, spawn_arena))
        .add_systems(
            Update,
            (
                sync_networked_players,
                refresh_replicate_on_connect,
                drain_player_inputs,
                drain_customize_messages,
                apply_beam_impulses,
                handle_throw_lantern,
                handle_pickup_lantern,
                handle_place_portal,
                handle_spawn_body,
                server_portal_teleport,
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
) {
    use std::collections::HashSet;
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
            trace::event(
                "client_disconnected",
                json!({"client_id": owner.0, "entity": format!("{:?}", entity)}),
            );
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
) {
    use std::collections::HashSet;
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
        commands.spawn((
            (
                Name::new(format!("NetworkedPlayer({client_id})")),
                NetworkedPlayer,
                NetworkedId(net_id),
                BeamCastTimer { last_update: 0.0, active: false },
                NetworkOwner(client_id),
                NetworkedPosition::from_vec3(initial),
                PlayerInputState::default(),
                crate::net::protocol::PlayerCustomization::default(),
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
        ));
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
        };
        let net_id = id_alloc.next();
        trace::event(
            "prop_spawned",
            json!({"net_id": net_id, "pos": [pos.x, pos.y, pos.z]}),
        );
        commands.spawn((
            Name::new(name),
            NetworkedProp { shape, tint_seed: seed, mass },
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
                    LinearDamping(0.4),
                    AngularDamping(0.5),
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
    players: Query<Entity, With<NetworkedPlayer>>,
    lanterns: Query<Entity, With<NetworkedLantern>>,
    mut forces: Query<Forces>,
    time: Res<Time>,
    mut beam_owners: Query<(&NetworkOwner, &mut BeamCastTimer), With<NetworkedPlayer>>,
    mut broadcast_senders: Query<&mut MessageSender<BeamCastBroadcast>, With<ClientOf>>,
) {
    let excluded: Vec<Entity> = players.iter().chain(lanterns.iter()).collect();
    let now = time.elapsed_secs();
    for (RemoteId(peer_id), mut receiver) in &mut receivers {
        let client_id = match peer_id {
            PeerId::Netcode(id) | PeerId::Steam(id) | PeerId::Local(id) | PeerId::Entity(id) => {
                *id
            }
            _ => continue,
        };
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
                if !props.contains(hit.entity) {
                    continue;
                }
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
                Replicate::manual(current_senders.clone()),
            ));
        }
    }
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
    mut receivers: Query<&mut MessageReceiver<SpawnBodyMessage>, With<ClientOf>>,
    senders: Query<Entity, (With<ClientOf>, With<Connected>)>,
    catalog: Option<Res<crate::spells::catalog::SpellCatalog>>,
    mut commands: Commands,
    mut id_alloc: ResMut<NetworkedIdAlloc>,
) {
    let current_senders: Vec<Entity> = senders.iter().collect();
    for mut receiver in &mut receivers {
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
                    // The originating client is identified by the
                    // `ClientOf` connection; mapping that to the player
                    // entity is the responsibility of attribution-aware
                    // child casts (deferred — for now they get None and
                    // run as orphans, which is fine for the explosion
                    // case).
                    original_caster: None,
                    captured_charge: p.captured_charge,
                    chain_depth: p.chain_depth,
                    caused_by: Some(crate::spells::data::TriggerSpec {
                        spell_id: crate::spells::SpellId(p.spell_id.clone()),
                        cast_id: crate::spells::data::CastId(p.cast_id.clone()),
                    }),
                    fired: false,
                })
            });
            let mut entity = commands.spawn((
                Name::new(format!("SpawnedBody({net_id})")),
                NetworkedProp {
                    shape: msg.shape,
                    tint_seed: msg.tint_seed,
                    mass: msg.mass,
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
    portals: Query<(Entity, &NetworkedPortal, &Transform)>,
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
) {
    const LOCKOUT_RADIUS: f32 = PORTAL_RADIUS * 1.5;

    let mut primary: Option<(Entity, Transform)> = None;
    let mut secondary: Option<(Entity, Transform)> = None;
    for (e, np, tf) in &portals {
        let mut tf = *tf;
        // `handle_place_portal` already sets rotation, but defend against
        // entities older than that change.
        if tf.rotation == Quat::IDENTITY {
            tf.rotation = disc_rotation(Vec3::from(np.normal));
        }
        match np.slot {
            crate::spells::portal::PortalSlot::Primary => primary = Some((e, tf)),
            crate::spells::portal::PortalSlot::Secondary => secondary = Some((e, tf)),
        }
    }

    let pair = match (primary, secondary) {
        (Some(p), Some(s)) => Some((p, s)),
        _ => None,
    };

    for (_entity, mut tf, mut avian_pos, mut velocity, mut prev, mut lockout) in
        &mut travelers
    {
        let pos = tf.translation;

        // Clear lockouts for portals this traveler has moved away from
        // (or that no longer exist).
        lockout.0.retain(|portal_e| match portals.get(*portal_e) {
            Ok((_, _, ptf)) => (pos - ptf.translation).length() <= LOCKOUT_RADIUS,
            Err(_) => false,
        });

        let Some(((primary_e, primary_tf), (secondary_e, secondary_tf))) = pair else {
            prev.0 = Some(pos);
            continue;
        };

        let pairs = [
            (primary_e, secondary_e, primary_tf, secondary_tf),
            (secondary_e, primary_e, secondary_tf, primary_tf),
        ];

        let prev_pos = prev.0.unwrap_or(pos);
        let mut teleported_to: Option<Vec3> = None;

        for (entry_entity, exit_entity, entry, exit) in pairs {
            if lockout.0.contains(&entry_entity) {
                continue;
            }

            let entry_normal = (entry.rotation * Vec3::Y).normalize();
            let prev_along = (prev_pos - entry.translation).dot(entry_normal);
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
            let cross_pos = prev_pos.lerp(pos, t);
            let to_cross = cross_pos - entry.translation;
            let cross_radial =
                (to_cross - entry_normal * to_cross.dot(entry_normal)).length();
            if cross_radial > PORTAL_RADIUS {
                continue;
            }

            let basis_pos = if prev_along > 0.0 { prev_pos } else { pos };
            let to_basis = basis_pos - entry.translation;
            let entry_inv = entry.rotation.inverse();
            let q_pos = exit.rotation * entry_inv;
            let vel_flip = Quat::from_rotation_x(core::f32::consts::PI);
            let q_vel = exit.rotation * vel_flip * entry_inv;
            // Write avian's Position (canonical under
            // LightyearAvianPlugin's sync) AND Transform so any system
            // reading either within the same frame sees the post-teleport
            // pose. NetworkedPlayer's `sync_player_positions` will pick
            // up the new Transform and ship it via NetworkedPosition,
            // which `sync_local_player_from_server` then snaps to the
            // local rig — so the teleport actually sticks on the local
            // client too.
            let new_pos = exit.translation + q_pos * to_basis;
            tf.translation = new_pos;
            tf.rotation = q_pos * tf.rotation;
            avian_pos.0 = new_pos;
            velocity.0 = q_vel * velocity.0;

            lockout.0.insert(entry_entity);
            lockout.0.insert(exit_entity);
            teleported_to = Some(new_pos);
            trace::event(
                "prop_teleported",
                json!({
                    "entity": format!("{:?}", _entity),
                    "from_portal": format!("{:?}", entry_entity),
                    "to_pos": [new_pos.x, new_pos.y, new_pos.z],
                }),
            );
            break;
        }

        prev.0 = Some(teleported_to.unwrap_or(pos));
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
