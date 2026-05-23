//! Client-only observers + systems that materialize local presentation for
//! replicated gameplay entities.
//!
//! Today: TestCube smoke mesh, NetworkedPlayer wizard body + per-frame
//! `NetworkedPosition → Transform` sync, and animation clip selection
//! driven by inferred velocity. Stage Q+ adds bei-input integration.

use std::collections::HashMap;

use bevy::prelude::*;
use lightyear::prelude::Replicated;

use avian3d::prelude::*;

use crate::net::protocol::{
    BeamCastBroadcast, NetworkOwner, NetworkedId, NetworkedLantern, NetworkedPlayer,
    NetworkedPortal, NetworkedPosition, NetworkedProp, PropShape, TestCube,
};
use lightyear::prelude::MessageReceiver;
use crate::physics::GameLayer;
use crate::player::visuals::{
    apply_locomotion_blend, step_airborne_blend, step_casting_blend, WizardAssets,
};
use crate::spells::markers::Lantern;
use crate::trace;
use serde_json::json;

pub struct ReplicationLocalPlugin;

impl Plugin for ReplicationLocalPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(on_test_cube_replicated);
        app.add_observer(on_networked_player_replicated);
        app.add_observer(on_networked_prop_replicated);
        app.add_observer(on_networked_lantern_replicated);
        app.add_systems(
            Update,
            (
                // Smoothing pipeline: capture each replicated sample, then
                // every frame lerp Transform between the last two samples.
                // Chained so capture observes the latest replicated value
                // before smooth_networked_transforms reads it; both come
                // before sync_networked_positions (portal-only direct
                // write) to keep schedule order explicit.
                capture_networked_position_samples,
                smooth_networked_transforms,
                sync_networked_positions,
                drive_remote_animations,
                drive_remote_beam_visuals,
                cleanup_orphan_beam_meshes,
                hide_self_wizard_body,
            )
                .chain(),
        );
    }
}

/// Per-entity sample buffer for `NetworkedPosition`. The server replicates
/// `NetworkedPosition` once per tick (~16ms at 60Hz); without smoothing
/// the client's Transform only updates on those tick boundaries, which
/// looks fine for slow-moving bodies but jitters for anything fast
/// (sliding boxes, fireballs at 18 m/s).
///
/// We store the two most-recent samples and render at `now - span` so
/// the render time is always bracketed by two real samples. That costs
/// one tick of effective latency and produces artifact-free smoothing:
/// no extrapolation overshoot when samples arrive late.
#[derive(Component, Default, Debug)]
pub struct NetworkedPositionSmoothing {
    /// Previous sample (older). Until a second sample arrives, equals cur.
    prev_pos: Vec3,
    prev_yaw: f32,
    prev_time: f32,
    /// Latest received sample.
    cur_pos: Vec3,
    cur_yaw: f32,
    cur_time: f32,
    /// False until first sample captured. Skipped frames before then
    /// leave Transform at whatever the spawn observer set (usually
    /// identity), matching pre-smoothing behavior.
    initialized: bool,
}

/// Shift `cur → prev` and stamp the new sample whenever
/// `NetworkedPosition` changes. Portals are excluded — they're placed
/// statically and don't need smoothing.
fn capture_networked_position_samples(
    time: Res<Time>,
    mut q: Query<
        (&NetworkedPosition, &mut NetworkedPositionSmoothing),
        (Changed<NetworkedPosition>, Without<NetworkedPortal>),
    >,
) {
    let now = time.elapsed_secs();
    for (np, mut s) in &mut q {
        let pos = np.to_vec3();
        if !s.initialized {
            // First sample: prev = cur so smoothing is a no-op until a
            // second sample arrives. Body sits at its initial position
            // rather than animating from origin.
            s.prev_pos = pos;
            s.prev_yaw = np.yaw;
            s.prev_time = now;
            s.cur_pos = pos;
            s.cur_yaw = np.yaw;
            s.cur_time = now;
            s.initialized = true;
        } else {
            s.prev_pos = s.cur_pos;
            s.prev_yaw = s.cur_yaw;
            s.prev_time = s.cur_time;
            s.cur_pos = pos;
            s.cur_yaw = np.yaw;
            s.cur_time = now;
        }
    }
}

/// Per-frame Transform interpolation. Lerps prev → cur with `t` based on
/// `now - span`, clamped to `[0, 1]`. The render-delay trick (subtract
/// one sample span) keeps us interpolating between two real samples
/// instead of extrapolating past the latest one.
///
/// **Writes both `Transform` AND avian's `Position`/`Rotation` when they're
/// present.** This is critical for any entity that has `RigidBody` on the
/// client (props, lanterns): without writing `Position`, avian's
/// `transform_to_position` (RunFixedMainLoop) → `position_to_transform`
/// (PostUpdate) cycle overwrites our smoothed `Transform` with the
/// *previous* frame's value, producing a 2-frame stutter that's
/// imperceptible for slow motion but obvious for fast bodies. Writing
/// `Position` here trips `position_changed`, which makes
/// `transform_to_position` skip; then `position_to_transform` syncs
/// `Transform` to the value we just wrote — consistent, no fight.
fn smooth_networked_transforms(
    time: Res<Time>,
    mut q: Query<
        (
            &NetworkedPositionSmoothing,
            &mut Transform,
            Option<&mut avian3d::prelude::Position>,
            Option<&mut avian3d::prelude::Rotation>,
        ),
        Without<NetworkedPortal>,
    >,
) {
    use core::f32::consts::PI;
    let now = time.elapsed_secs();
    for (s, mut tf, pos_opt, rot_opt) in &mut q {
        if !s.initialized {
            continue;
        }
        let span = (s.cur_time - s.prev_time).max(1e-3);
        let render_time = now - span;
        let t = ((render_time - s.prev_time) / span).clamp(0.0, 1.0);
        let pos = s.prev_pos.lerp(s.cur_pos, t);
        let mut dyaw = s.cur_yaw - s.prev_yaw;
        if dyaw > PI {
            dyaw -= 2.0 * PI;
        } else if dyaw < -PI {
            dyaw += 2.0 * PI;
        }
        let yaw = s.prev_yaw + dyaw * t;
        let rot = Quat::from_axis_angle(Vec3::Y, yaw);
        tf.translation = pos;
        tf.rotation = rot;
        if let Some(mut p) = pos_opt {
            p.0 = pos.into();
        }
        if let Some(mut r) = rot_opt {
            *r = avian3d::prelude::Rotation::from(rot);
        }
    }
}

/// Hide the wizard body on the local client's *own* replicated
/// `NetworkedPlayer`. The server replicates every player back to every
/// client including the originator; without this, the local player ends
/// up rendering their own third-person body at their own position and
/// the camera sits inside it.
fn hide_self_wizard_body(
    parents: Query<&ChildOf>,
    owners: Query<&NetworkOwner, With<NetworkedPlayer>>,
    mut bodies: Query<(Entity, &mut Visibility), With<RemoteWizardBody>>,
    local_ids: Query<
        &lightyear::prelude::LocalId,
        Without<lightyear::prelude::server::ClientOf>,
    >,
) {
    let Some(my_id) =
        local_ids.iter().next().and_then(|local_id| match local_id.0 {
            lightyear::prelude::PeerId::Netcode(id)
            | lightyear::prelude::PeerId::Steam(id)
            | lightyear::prelude::PeerId::Local(id)
            | lightyear::prelude::PeerId::Entity(id) => Some(id),
            _ => None,
        })
    else {
        return;
    };
    for (body_entity, mut vis) in &mut bodies {
        let mut current = body_entity;
        loop {
            if let Ok(owner) = owners.get(current) {
                let target = if owner.0 == my_id {
                    Visibility::Hidden
                } else {
                    Visibility::Inherited
                };
                if *vis != target {
                    *vis = target;
                }
                break;
            }
            let Ok(parent) = parents.get(current) else { break };
            current = parent.parent();
        }
    }
}

/// Drain incoming `BeamCastBroadcast` messages and write them onto the
/// matching player's `LocalBeamState`. Skips messages for the local
/// player — their own beam is rendered locally by `cast_beam`; without
/// the filter the loopback broadcast would render a second beam from the
/// remote presentation entity.
fn drain_beam_broadcasts(
    mut receivers: Query<&mut MessageReceiver<BeamCastBroadcast>>,
    mut players: Query<(&NetworkOwner, &mut LocalBeamState), With<NetworkedPlayer>>,
    local_ids: Query<
        &lightyear::prelude::LocalId,
        Without<lightyear::prelude::server::ClientOf>,
    >,
) {
    let my_id = local_ids.iter().next().and_then(|local_id| {
        match local_id.0 {
            lightyear::prelude::PeerId::Netcode(id)
            | lightyear::prelude::PeerId::Steam(id)
            | lightyear::prelude::PeerId::Local(id)
            | lightyear::prelude::PeerId::Entity(id) => Some(id),
            _ => None,
        }
    });
    for mut receiver in &mut receivers {
        for msg in receiver.receive() {
            if my_id == Some(msg.client_id) {
                continue;
            }
            for (owner, mut state) in &mut players {
                if owner.0 == msg.client_id {
                    state.active = msg.active;
                    state.origin = msg.origin;
                    state.direction = msg.direction;
                    state.length = msg.length;
                    break;
                }
            }
        }
    }
}

/// Marker on the wizard body child of a `NetworkedPlayer`. Used by
/// `hide_self_wizard_body` to suppress the local player's own avatar
/// from their first-person view (otherwise the camera sits inside the
/// mesh and the player sees themselves from the inside).
#[derive(Component)]
struct RemoteWizardBody;

/// Beam mesh rendered for a remote player. Stored as a root entity (NOT
/// a child of the player) so its world-coordinate `origin`/`direction`
/// from the latest `BeamCastBroadcast` can be applied directly to its
/// `Transform`. The `player` link lets `drive_remote_beam_visuals` find
/// the right state.
#[derive(Component)]
struct RemoteBeamMesh {
    player: Entity,
}

/// Local-only mirror of a remote player's current beam cast, refilled
/// from `BeamCastBroadcast` messages each frame. Not replicated — we get
/// the state via Message broadcast, not component sync.
#[derive(Component, Default, Clone, Copy)]
struct LocalBeamState {
    active: bool,
    origin: [f32; 3],
    direction: [f32; 3],
    length: f32,
}

/// Trace-only mirror of [`ReplicationLocalPlugin`]. Emits structured JSON
/// events to the file pointed at by `WISP_TRACE_FILE` (no-op when unset).
/// Wired into both the regular client and the headless observer binary so
/// the test harness can correlate "what each peer saw" without rendering.
pub struct ReplicationTracePlugin;

impl Plugin for ReplicationTracePlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(trace_networked_player_arrival);
        app.add_observer(trace_networked_prop_arrival);
        app.add_observer(trace_networked_lantern_arrival);
        app.add_observer(trace_networked_portal_arrival);
        app.add_observer(trace_networked_lantern_despawn);
        app.add_observer(trace_networked_portal_despawn);
        app.add_observer(trace_networked_player_despawn);
        app.add_systems(
            Update,
            (
                resolve_pending_arrivals,
                ensure_local_beam_state,
                drain_beam_broadcasts,
                trace_player_positions,
                trace_prop_positions,
                trace_lantern_positions,
                trace_portal_positions,
                trace_beam_cast_changes,
            )
                .chain(),
        );
    }
}

/// Ensure every replicated `NetworkedPlayer` has a `LocalBeamState`.
/// `LocalBeamState` lives in this crate (not in `protocol`) so it isn't
/// part of the replicated set, but every peer that wants to render or
/// trace beams needs one — including the headless observer that has no
/// visuals. Runs every frame and is a no-op once every player has the
/// component.
fn ensure_local_beam_state(
    q: Query<Entity, (With<NetworkedPlayer>, Without<LocalBeamState>)>,
    mut commands: Commands,
) {
    for e in &q {
        commands.entity(e).insert(LocalBeamState::default());
    }
}

fn trace_beam_cast_changes(
    q: Query<
        (&NetworkedId, &LocalBeamState),
        (With<NetworkedPlayer>, Changed<LocalBeamState>),
    >,
) {
    for (id, cast) in &q {
        let kind = if cast.active { "beam_start" } else { "beam_end" };
        trace::event(
            kind,
            json!({
                "net_id": id.0,
                "active": cast.active,
                "origin": cast.origin,
                "direction": cast.direction,
                "length": cast.length,
            }),
        );
    }
}

fn trace_networked_lantern_despawn(
    trigger: On<Remove, NetworkedLantern>,
    q: Query<&NetworkedId>,
) {
    let id = q.get(trigger.entity).ok().map(|i| i.0);
    trace::event(
        "despawned_lantern",
        json!({"entity": format!("{:?}", trigger.entity), "net_id": id}),
    );
}

fn trace_networked_portal_despawn(
    trigger: On<Remove, NetworkedPortal>,
    q: Query<&NetworkedId>,
) {
    let id = q.get(trigger.entity).ok().map(|i| i.0);
    trace::event(
        "despawned_portal",
        json!({"entity": format!("{:?}", trigger.entity), "net_id": id}),
    );
}

fn trace_networked_player_despawn(
    trigger: On<Remove, NetworkedPlayer>,
    q: Query<&NetworkedId>,
) {
    let id = q.get(trigger.entity).ok().map(|i| i.0);
    trace::event(
        "despawned_player",
        json!({"entity": format!("{:?}", trigger.entity), "net_id": id}),
    );
}

/// The replicated `NetworkedId` and (optionally) `NetworkOwner` may arrive
/// in any order with the marker component. Defer the arrival emission to
/// a system that fires once the entity has both, so we never log
/// `net_id: null`.
#[derive(Component)]
struct PendingTraceArrival {
    kind: TraceArrivalKind,
}

enum TraceArrivalKind {
    Player,
    Prop,
    Lantern,
    Portal,
}

fn trace_networked_player_arrival(
    trigger: On<Add, NetworkedPlayer>,
    mut commands: Commands,
) {
    commands.entity(trigger.entity).insert(PendingTraceArrival {
        kind: TraceArrivalKind::Player,
    });
}

fn trace_networked_prop_arrival(
    trigger: On<Add, NetworkedProp>,
    mut commands: Commands,
) {
    commands.entity(trigger.entity).insert(PendingTraceArrival {
        kind: TraceArrivalKind::Prop,
    });
}

fn trace_networked_lantern_arrival(
    trigger: On<Add, NetworkedLantern>,
    mut commands: Commands,
) {
    commands.entity(trigger.entity).insert(PendingTraceArrival {
        kind: TraceArrivalKind::Lantern,
    });
}

fn trace_networked_portal_arrival(
    trigger: On<Add, NetworkedPortal>,
    mut commands: Commands,
) {
    commands.entity(trigger.entity).insert(PendingTraceArrival {
        kind: TraceArrivalKind::Portal,
    });
}

/// Resolve pending arrivals once `NetworkedId` is present. Emits the
/// arrival trace event and removes the marker so the entity is logged
/// exactly once.
fn resolve_pending_arrivals(
    q: Query<(
        Entity,
        &PendingTraceArrival,
        &NetworkedId,
        Option<&NetworkOwner>,
        Option<&NetworkedProp>,
        Option<&NetworkedPortal>,
    )>,
    mut commands: Commands,
) {
    for (entity, pending, id, owner, prop, portal) in &q {
        let kind = match pending.kind {
            TraceArrivalKind::Player => "replicated_player",
            TraceArrivalKind::Prop => "replicated_prop",
            TraceArrivalKind::Lantern => "replicated_lantern",
            TraceArrivalKind::Portal => "replicated_portal",
        };
        let mut payload = json!({
            "entity": format!("{:?}", entity),
            "net_id": id.0,
        });
        if let Some(o) = owner {
            payload["owner"] = json!(o.0);
        }
        if let Some(p) = prop {
            payload["shape"] = match p.shape {
                PropShape::Cube { size } => json!({"kind": "cube", "size": size}),
                PropShape::Sphere { radius } => json!({"kind": "sphere", "radius": radius}),
            };
        }
        if let Some(p) = portal {
            payload["slot"] = json!(format!("{:?}", p.slot));
            payload["normal"] = json!(p.normal);
        }
        trace::event(kind, payload);
        commands.entity(entity).remove::<PendingTraceArrival>();
    }
}

fn trace_player_positions(
    q: Query<
        (&NetworkedId, &NetworkedPosition),
        (With<NetworkedPlayer>, Changed<NetworkedPosition>),
    >,
) {
    for (id, pos) in &q {
        trace::event(
            "player_position",
            json!({
                "net_id": id.0,
                "pos": [pos.x, pos.y, pos.z],
                "yaw": pos.yaw,
            }),
        );
    }
}

fn trace_prop_positions(
    q: Query<
        (&NetworkedId, &NetworkedPosition),
        (With<NetworkedProp>, Changed<NetworkedPosition>),
    >,
) {
    for (id, pos) in &q {
        trace::event(
            "prop_position",
            json!({"net_id": id.0, "pos": [pos.x, pos.y, pos.z]}),
        );
    }
}

fn trace_lantern_positions(
    q: Query<
        (&NetworkedId, &NetworkedPosition),
        (With<NetworkedLantern>, Changed<NetworkedPosition>),
    >,
) {
    for (id, pos) in &q {
        trace::event(
            "lantern_position",
            json!({"net_id": id.0, "pos": [pos.x, pos.y, pos.z]}),
        );
    }
}

fn trace_portal_positions(
    q: Query<
        (&NetworkedId, &NetworkedPosition, &NetworkedPortal),
        (With<NetworkedPortal>, Changed<NetworkedPosition>),
    >,
) {
    for (id, pos, np) in &q {
        trace::event(
            "portal_position",
            json!({
                "net_id": id.0,
                "slot": format!("{:?}", np.slot),
                "pos": [pos.x, pos.y, pos.z],
            }),
        );
    }
}

/// Attach mesh + light + local `Lantern` marker when a server-spawned
/// lantern arrives. The marker is what the local lens-power computation
/// uses to find sources, so adding it makes replicated lanterns power any
/// player's beam (including the thrower's own).
fn on_networked_lantern_replicated(
    trigger: On<Add, NetworkedLantern>,
    replicated: Query<(), (With<NetworkedLantern>, With<lightyear::prelude::Replicated>)>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if replicated.get(trigger.entity).is_err() {
        return;
    }
    commands
        .entity(trigger.entity)
        .insert((
            Name::new("RemoteLantern"),
            Lantern,
            Mesh3d(meshes.add(Sphere::new(0.12))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(1.0, 0.95, 0.7),
                emissive: LinearRgba::rgb(8.0, 6.5, 2.5),
                ..default()
            })),
            Transform::default(),
            Visibility::default(),
            // Static client-side body, but excludes the Player layer so
            // the local player capsule passes through it (matches the
            // server's collision filter and avoids snag-on-throw).
            RigidBody::Static,
            Collider::sphere(0.12),
            CollisionLayers::new(
                GameLayer::Default,
                [GameLayer::Default, GameLayer::Ground],
            ),
            NetworkedPositionSmoothing::default(),
        ))
        .with_children(|c| {
            c.spawn((
                Name::new("LanternLight"),
                PointLight {
                    color: Color::srgb(1.0, 0.92, 0.65),
                    intensity: 60_000.0,
                    range: 16.0,
                    shadows_enabled: false,
                    ..default()
                },
                Transform::default(),
            ));
        });
}

/// Attach a mesh + static collider when a replicated `NetworkedProp`
/// arrives. The server is authoritative for the prop's position (driven by
/// avian on the server); clients render and use a static collider so the
/// local player doesn't walk through. Real physics interaction lands when
/// client→server spell casts are wired.
fn on_networked_prop_replicated(
    trigger: On<Add, NetworkedProp>,
    replicated: Query<&NetworkedProp, With<lightyear::prelude::Replicated>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let Ok(prop) = replicated.get(trigger.entity) else {
        return;
    };
    let seed = prop.tint_seed;
    let material = materials.add(StandardMaterial {
        base_color: Color::srgb(
            0.4 + 0.5 * (seed * 1.3).fract(),
            0.4 + 0.5 * (seed * 2.7).fract(),
            0.4 + 0.5 * (seed * 5.1).fract(),
        ),
        perceptual_roughness: 0.6,
        ..default()
    });
    let (mesh, collider) = match prop.shape {
        PropShape::Cube { size } => (
            meshes.add(Cuboid::new(size, size, size)),
            Collider::cuboid(size, size, size),
        ),
        PropShape::Sphere { radius } => {
            (meshes.add(Sphere::new(radius)), Collider::sphere(radius))
        }
    };
    let _ = prop.mass; // unused; server is the physics authority now.
    commands.entity(trigger.entity).insert((
        Name::new("RemoteProp"),
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::default(),
        Visibility::default(),
        // Static body so the local player can walk around the prop, but
        // its motion is driven by the server's `NetworkedPosition` via
        // `smooth_networked_transforms`, not by local avian dynamics.
        RigidBody::Static,
        collider,
        CollisionLayers::new(GameLayer::Default, LayerMask::ALL),
        NetworkedPositionSmoothing::default(),
    ));
}

/// Per-entity state used to compute velocity from successive replicated
/// positions, so we can pick the right walk-direction clip. We keep the
/// last *changed* position and the wall-clock time of that change rather
/// than the per-frame position — `NetworkedPosition` only changes when
/// the server emits a tick, and frames that fall between ticks would
/// otherwise report velocity=0 and flicker the animation back to idle.
/// `smoothed_velocity` then exponentially smooths the per-event samples
/// so small network jitter doesn't oscillate the clip choice either.
#[derive(Component, Default)]
struct RemoteAnimState {
    last_pos: Vec3,
    last_pos_time: f32,
    smoothed_velocity: Vec3,
    /// Eased follower of the replicated `airborne` flag (0 = grounded,
    /// 1 = airborne). Without this, jump start / landing would pop the
    /// blend instantly between the falling and locomotion clips.
    airborne_blend: f32,
    /// Eased follower of the replicated `casting` flag. 0 = plain
    /// locomotion clips, 1 = casting variants.
    casting_blend: f32,
    initialized: bool,
}

/// Attach a wizard body + a (initially hidden) beam mesh when a
/// server-spawned `NetworkedPlayer` arrives. The beam mesh is driven by
/// the parent's replicated `NetworkedBeamCast` via
/// [`drive_remote_beam_visuals`].
fn on_networked_player_replicated(
    trigger: On<Add, NetworkedPlayer>,
    replicated: Query<&NetworkOwner, With<Replicated>>,
    asset_server: Res<AssetServer>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let Ok(owner) = replicated.get(trigger.entity) else {
        return;
    };
    info!("NetworkedPlayer arrived (owner={}); attaching wizard body.", owner.0);
    let beam_material = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 0.85, 0.5, 0.6),
        emissive: LinearRgba::rgb(2.5, 1.5, 0.5),
        alpha_mode: AlphaMode::Add,
        unlit: true,
        ..default()
    });
    commands
        .entity(trigger.entity)
        .insert((
            Name::new(format!("RemotePlayer({})", owner.0)),
            Transform::default(),
            Visibility::default(),
            RemoteAnimState::default(),
            LocalBeamState::default(),
            NetworkedPositionSmoothing::default(),
        ))
        .with_children(|c| {
            c.spawn((
                Name::new("WizardBody"),
                RemoteWizardBody,
                SceneRoot(
                    asset_server.load(GltfAssetLabel::Scene(0).from_asset("wizard.glb")),
                ),
                Transform::from_xyz(0.0, -1.0, 0.0)
                    .with_rotation(Quat::from_rotation_y(std::f32::consts::PI)),
                Visibility::Inherited,
            ));
        });
    // The beam is a root entity (not a child) so its world coords from
    // `NetworkedBeamCast` apply directly. We point it back at the player
    // entity to fetch the cast each frame.
    commands.spawn((
        Name::new(format!("RemoteBeam({})", owner.0)),
        RemoteBeamMesh {
            player: trigger.entity,
        },
        Mesh3d(meshes.add(Cylinder::new(0.06, 1.0))),
        MeshMaterial3d(beam_material),
        Transform::default(),
        Visibility::Hidden,
    ));
}

/// Per-frame: read each remote player's `LocalBeamState` (refilled by
/// `drain_beam_broadcasts`) and update the associated `RemoteBeamMesh`.
/// The beam is a root entity so we can write its world Transform
/// directly from the world-space cast origin and direction.
fn drive_remote_beam_visuals(
    players: Query<&LocalBeamState, With<NetworkedPlayer>>,
    mut beams: Query<(&RemoteBeamMesh, &mut Transform, &mut Visibility)>,
) {
    for (link, mut tf, mut vis) in &mut beams {
        let Ok(cast) = players.get(link.player) else {
            // Player despawned; cleanup handled by cleanup_orphan_beam_meshes.
            *vis = Visibility::Hidden;
            continue;
        };
        if !cast.active {
            *vis = Visibility::Hidden;
            continue;
        }
        let origin = Vec3::from(cast.origin);
        let dir_v = Vec3::from(cast.direction);
        let Ok(dir) = Dir3::new(dir_v) else {
            *vis = Visibility::Hidden;
            continue;
        };
        // Cylinder mesh is centered on its origin and runs along Y from
        // -0.5 to 0.5. Translate forward by length/2 so its base sits at
        // `origin`; rotate +Y → beam direction; scale Y to beam length.
        let length = cast.length.max(0.01);
        let midpoint = origin + *dir * (length * 0.5);
        let rotation = Quat::from_rotation_arc(Vec3::Y, *dir);
        *tf = Transform::from_translation(midpoint)
            .with_rotation(rotation)
            .with_scale(Vec3::new(1.0, length, 1.0));
        *vis = Visibility::Visible;
    }
}

/// Despawn beam meshes whose linked player has gone away.
fn cleanup_orphan_beam_meshes(
    players: Query<(), With<NetworkedPlayer>>,
    beams: Query<(Entity, &RemoteBeamMesh)>,
    mut commands: Commands,
) {
    for (beam_entity, link) in &beams {
        if players.get(link.player).is_err() {
            commands.entity(beam_entity).despawn();
        }
    }
}

/// Portal-only `NetworkedPosition` → `Transform` copy. Portals are
/// placed statically (their rotation comes from the surface normal on
/// receive and stays put), so they don't need the per-frame smoothing
/// that movable entities go through via
/// [`smooth_networked_transforms`].
fn sync_networked_positions(
    mut portal: Query<
        (&NetworkedPosition, &mut Transform),
        (Changed<NetworkedPosition>, With<NetworkedPortal>),
    >,
) {
    for (pos, mut tf) in &mut portal {
        tf.translation = pos.to_vec3();
    }
}

/// For each `NetworkedPlayer`, infer velocity from the position delta since
/// last frame, pick the matching animation clip, and apply it to the
/// AnimationPlayer somewhere in the entity's descendants. The local
/// player's AnimationPlayer is driven by `player::visuals::drive_animation`
/// — the ancestor walk in this system terminates without a match for that
/// player (its root is `Player`, not `NetworkedPlayer`), so the two
/// systems don't conflict.
fn drive_remote_animations(
    time: Res<Time>,
    wizard: Res<WizardAssets>,
    mut anim_state_q: Query<
        (Entity, &NetworkedPosition, &mut RemoteAnimState),
        With<NetworkedPlayer>,
    >,
    parents: Query<&ChildOf>,
    mut anim_players: Query<(Entity, &mut AnimationPlayer)>,
) {
    if !wizard.ready() {
        return;
    }
    // Exponential smoothing factor. Larger = snappier response, more
    // jitter; smaller = smoother, more lag. At 60Hz, 0.15 reaches ~95%
    // of a step input within ~20 frames (~330ms) — fast enough to look
    // alive, slow enough to absorb single-tick gaps from network jitter.
    const ALPHA: f32 = 0.15;
    // If we haven't seen a position change in this long, decay the
    // smoothed velocity toward zero so the animation eventually settles
    // to idle when the player stops moving.
    const STALE_AFTER: f32 = 0.2;
    let now = time.elapsed_secs();

    // Per-NetworkedPlayer entity → (velocity, yaw, airborne_blend, casting_blend).
    let mut per_player: HashMap<Entity, (Vec3, f32, f32, f32)> = HashMap::new();
    for (entity, pos, mut state) in &mut anim_state_q {
        let current = pos.to_vec3();
        if !state.initialized {
            // Seed without inferring a phantom velocity on the spawn frame.
            state.last_pos = current;
            state.last_pos_time = now;
            state.smoothed_velocity = Vec3::ZERO;
            state.airborne_blend = if pos.airborne { 1.0 } else { 0.0 };
            state.casting_blend = if pos.casting { 1.0 } else { 0.0 };
            state.initialized = true;
        } else if current != state.last_pos {
            // Compute velocity from the gap since the *last position
            // change*, not the local frame delta — that way frames that
            // fall between network ticks don't report velocity=0.
            let elapsed = (now - state.last_pos_time).max(1e-3);
            let sample = (current - state.last_pos) / elapsed;
            state.smoothed_velocity =
                state.smoothed_velocity * (1.0 - ALPHA) + sample * ALPHA;
            state.last_pos = current;
            state.last_pos_time = now;
        } else if now - state.last_pos_time > STALE_AFTER {
            // No update for a while — decay toward zero.
            state.smoothed_velocity = state.smoothed_velocity * (1.0 - ALPHA);
        }
        state.airborne_blend = step_airborne_blend(state.airborne_blend, !pos.airborne);
        state.casting_blend = step_casting_blend(state.casting_blend, pos.casting);
        per_player.insert(
            entity,
            (
                state.smoothed_velocity,
                pos.yaw,
                state.airborne_blend,
                state.casting_blend,
            ),
        );
    }

    for (anim_entity, mut player) in &mut anim_players {
        // Walk up to find the NetworkedPlayer ancestor (if any) so this
        // system stays out of the local Player's hierarchy.
        let mut e = anim_entity;
        loop {
            if let Some(&(velocity, yaw, airborne_blend, casting_blend)) =
                per_player.get(&e)
            {
                apply_locomotion_blend(
                    &mut player,
                    &wizard,
                    velocity,
                    yaw,
                    airborne_blend,
                    casting_blend,
                );
                break;
            }
            let Ok(parent) = parents.get(e) else { break };
            e = parent.parent();
        }
    }
}

/// When a `TestCube` arrives via replication, attach a visible mesh so we
/// can confirm the wire is delivering components. Filters by `Replicated`
/// so server-side spawns don't accidentally trip on themselves.
fn on_test_cube_replicated(
    trigger: On<Add, TestCube>,
    replicated: Query<(), With<Replicated>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if replicated.get(trigger.entity).is_err() {
        return;
    }
    info!("TestCube arrived from server; spawning local mesh.");
    commands.entity(trigger.entity).insert((
        Name::new("TestCube"),
        Mesh3d(meshes.add(Cuboid::new(0.5, 0.5, 0.5))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.5, 0.0),
            emissive: LinearRgba::rgb(2.0, 1.0, 0.0),
            ..default()
        })),
        Transform::from_xyz(0.0, 2.0, 0.0),
    ));
}
