//! Protocol: identical on every peer. Declares which components are
//! replicated.
//!
//! Today: a `TestCube` smoke marker (Stage N) and `NetworkedPlayer` +
//! `NetworkOwner` for the Stage-O player-presence demo. Stage P+ adds
//! position/velocity/inputs and merges `NetworkedPlayer` with the local
//! `Player` marker.

use core::time::Duration;

use avian3d::prelude::{LinearVelocity, Position, Rotation};
use bevy::prelude::*;
use lightyear::interpolation::registry::InterpolationRegistrationExt;
use lightyear::prediction::registry::PredictionRegistrationExt;
use lightyear::prelude::{
    AppChannelExt, AppComponentExt, AppMessageExt, ChannelMode, ChannelSettings,
    ComponentReplicationConfig, NetworkDirection,
};
use serde::{Deserialize, Serialize};

use crate::spells::portal::PortalSlot;

pub struct ProtocolPlugin;

impl Plugin for ProtocolPlugin {
    fn build(&self, app: &mut App) {
        // Stage Q.3 BLOCKED: lightyear_inputs_bei 0.26.4 pins
        // `bevy_enhanced_input = "0.22"`, while wisp is on 0.25. Two
        // incompatible bei versions in the dependency graph means our
        // `InputAction`s don't satisfy the trait bound from the older
        // crate. Resolution options documented in CLAUDE.md → "Stage
        // Q blocker" — the current path is a hand-rolled
        // `PlayerInputMessage` (extend `PlayerPositionMessage`) so
        // server can run authoritative simulation. We give up
        // lightyear-native rollback in exchange.

        app.register_component::<TestCube>();
        app.register_component::<NetworkedPlayer>();
        app.register_component::<NetworkOwner>();
        // Per-player cosmetic state: 6 tint colors + archetype index.
        // Server is authoritative; updates flow as
        // `CustomizeMessage` (client → server) then propagate back
        // through component replication.
        app.register_component::<PlayerCustomization>();
        // NetworkedPosition gets frame-level interpolation: lightyear
        // buffers each received tick into a `ConfirmedHistory` and lerps
        // toward the latest sample over the local frame, hiding the
        // 60Hz server tick boundary. Replaces our ad-hoc velocity
        // smoothing for the position component itself (the eased
        // animation blends stay — they drive clip weights).
        app.register_component::<NetworkedPosition>()
            .add_interpolation_with(lerp_networked_position);
        app.register_component::<NetworkedProp>();
        app.register_component::<NetworkedLantern>();
        app.register_component::<NetworkedPortal>();
        app.register_component::<NetworkedId>();
        // NetworkedHealth replicates current + max hp on any entity with
        // a Hurtbox. Server is authoritative — clients only read this
        // for HUD / overhead bars. No interpolation: hp is discrete and
        // damage events feel best when they snap rather than slide.
        app.register_component::<NetworkedHealth>();

        // Stage Q: register avian's authoritative physics components for
        // prediction + linear interpolation. We keep them DISABLED by
        // default via `with_replication_config(disable: true)` so they
        // only replicate on entities that explicitly opt in — that lets
        // us migrate one entity class at a time without disturbing the
        // existing `NetworkedPosition` path. The `add_should_rollback`
        // thresholds match the reference example
        // `lightyear/examples/avian_3d_character/src/protocol.rs` (tag
        // 0.26.4): 0.01m / 0.01rad / 0.01 m·s⁻¹.
        let disable = ComponentReplicationConfig {
            disable: true,
            ..default()
        };
        // Position + Rotation: prediction + interpolation. Skip the
        // `add_linear_correction_fn` (visual smoothing of rollback
        // errors) for now — it requires `Ease` on the delta type which
        // avian's components don't impl out of the box in 0.5. The
        // rollback itself still works; corrections just snap rather
        // than ease over a few frames. Re-add when a custom correction
        // fn (or an Ease impl on Position/Rotation) lands.
        app.register_component::<Position>()
            .with_replication_config(disable.clone())
            .add_prediction()
            .add_should_rollback(position_should_rollback)
            .add_linear_interpolation();
        app.register_component::<Rotation>()
            .with_replication_config(disable.clone())
            .add_prediction()
            .add_should_rollback(rotation_should_rollback)
            .add_linear_interpolation();
        app.register_component::<LinearVelocity>()
            .with_replication_config(disable)
            .add_prediction()
            .add_should_rollback(linear_velocity_should_rollback);

        // Bidirectional unreliable channel for high-frequency state
        // messages: per-frame position updates client→server and beam
        // broadcasts server→client. Unreliable + every frame; we don't
        // care if a packet drops because the next overwrite covers it.
        app.add_channel::<PlayerInputChannel>(ChannelSettings {
            mode: ChannelMode::UnorderedUnreliable,
            send_frequency: Duration::default(),
            priority: 1.0,
        })
        .add_direction(NetworkDirection::ClientToServer)
        .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<PlayerInputMessage>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<BeamImpulseMessage>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<ThrowLanternMessage>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<PickupLanternMessage>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<PlacePortalMessage>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<SpawnBodyMessage>()
            .add_direction(NetworkDirection::ClientToServer);
        app.register_message::<BeamCastBroadcast>()
            .add_direction(NetworkDirection::ServerToClient);
        app.register_message::<CustomizeMessage>()
            .add_direction(NetworkDirection::ClientToServer);
    }
}

/// Unit-struct channel marker for player input messages. `Channel` is
/// blanket-impl'd by lightyear for any `Send + Sync + 'static`.
pub struct PlayerInputChannel;

/// Per-tick input from each client to the server. Server runs the
/// movement controller on its authoritative `NetworkedPlayer` entity
/// using these inputs, then ships the resulting pose back via
/// `NetworkedPosition`. Stage Q option (b) — lightyear-native bei input
/// replication is blocked on a version mismatch, so we hand-roll the
/// wire here. `casting` is sent because the cast engine still runs
/// locally (no server-side cast simulation yet); server only needs the
/// flag for remote animation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlayerInputMessage {
    /// WASD axis vector in the player's local frame ([forward, right]).
    pub movement: [f32; 2],
    /// Body yaw (camera-controlled) in radians.
    pub yaw: f32,
    /// Camera aim pitch in radians, positive = looking up. Cosmetic
    /// only — drives every observer's spine-lean rendering of this
    /// player.
    pub pitch: f32,
    pub jump: bool,
    pub casting: bool,
}

/// Marker + shape descriptor for replicated world props (cubes, spheres,
/// etc.). The server runs avian physics on these; clients render mesh +
/// static collider so the local player can navigate around them. Spell
/// interaction with replicated props lands once client→server spell casts
/// are wired.
/// Client → server impulse application. The server raycasts from
/// `(origin, direction)` up to `range`, finds the first `NetworkedProp`
/// it hits, and applies a linear impulse of `direction * magnitude` to it.
/// Used by lens-family spells to push replicated props authoritatively.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BeamImpulseMessage {
    pub origin: [f32; 3],
    pub direction: [f32; 3],
    pub range: f32,
    pub magnitude: f32,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetworkedProp {
    pub shape: PropShape,
    /// Tint seed — clients use it to pick a deterministic color. Match
    /// what the previous single-player `spawn_arena` did.
    pub tint_seed: f32,
    /// Mass for the locally-simulated dynamic body. Same value on every
    /// client so spell-impulse feel matches.
    pub mass: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum PropShape {
    Cube { size: f32 },
    Sphere { radius: f32 },
}

impl Default for PropShape {
    fn default() -> Self {
        PropShape::Cube { size: 0.5 }
    }
}

/// Marker for replicated lanterns. Server spawns one per throw and owns
/// its dynamics; clients receive and attach mesh + light + the local
/// `Lantern` marker so the lens-power computation finds them.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkedLantern;

/// Client → server: "I want to throw a lantern at this position with this
/// velocity." Server validates the cap and spawns the lantern.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ThrowLanternMessage {
    pub origin: [f32; 3],
    pub velocity: [f32; 3],
}

/// Client → server: "Despawn any lantern within reach." Server walks all
/// lanterns and despawns those within `pickup_radius`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PickupLanternMessage {
    pub player_position: [f32; 3],
}

/// Client → server: "Spawn a dynamic body for me with these physics
/// params at `origin` with `velocity`." Generic wire for every
/// `PayloadDef::SpawnBody` cast. The server materializes a `NetworkedProp`
/// and replicates it back so every client renders the same body. Avoids
/// one bespoke message + handler per projectile-style spell — adding a
/// new such spell is RON-only.
///
/// Visual params (colour, emission, mesh details) are NOT carried here;
/// each client looks them up in its local `BodyCatalog` keyed off the
/// arena's existing prop visualization path. For more bespoke visuals,
/// extend the `NetworkedProp.tint_seed` channel or add a template id
/// field.
///
/// `parent_cast` is the originating spell/cast id. If present, the server
/// looks up that cast in its `SpellCatalog`, reads any `on_event` body
/// triggers, and attaches a `BodyTriggers` component to the spawned
/// entity. Carries `captured_charge` + `chain_depth` so child-cast
/// dispatch can propagate them through the chain.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpawnBodyMessage {
    pub origin: [f32; 3],
    pub velocity: [f32; 3],
    pub shape: PropShape,
    pub mass: f32,
    pub friction: f32,
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub restitution: f32,
    pub tint_seed: f32,
    pub parent_cast: Option<ParentCastInfo>,
}

/// Carries the originating cast's identifiers + chain state alongside a
/// `SpawnBodyMessage` so the server can wire body lifecycle triggers and
/// propagate parent state down to child casts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ParentCastInfo {
    /// `SpellId` of the originating spell (matches a key in the server's
    /// `SpellCatalog`). Plain `String` on the wire so the protocol stays
    /// decoupled from the spell-data module.
    pub spell_id: String,
    /// `CastId` within that spell.
    pub cast_id: String,
    /// Charge captured at the moment the parent cast committed. Propagated
    /// to child casts as a scale factor (bigger explosion at higher charge).
    pub captured_charge: Option<f32>,
    /// Parent's chain depth. The server-side trigger increments this
    /// before dispatching the child so the cap (`MAX_CHILD_CAST_CHAIN_DEPTH`)
    /// works.
    pub chain_depth: u8,
}

/// Replicated portal: server is the source of truth for which slot is
/// filled and where. Clients reconstruct the local visual (mesh,
/// render-to-texture camera, PortalMaterial) on receive. Rotation is not
/// replicated as a quat — it's computed deterministically from `normal`
/// via the same `disc_rotation` math on every peer.
#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetworkedPortal {
    pub slot: PortalSlot,
    pub normal: [f32; 3],
}

/// Client → server: "Place a portal at this position with this surface
/// normal." Server despawns any existing portal in `slot` and spawns a
/// new replicated entity. Portals are globally shared (any client's
/// placement overwrites the slot for everyone) — keeps multiplayer
/// behavior identical to single-player.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacePortalMessage {
    pub slot: PortalSlot,
    pub position: [f32; 3],
    pub normal: [f32; 3],
}

/// Stage-N smoke marker. Server spawns one of these with `Replicate`;
/// clients receive it via the wire and attach a visible mesh locally.
#[derive(
    Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize,
)]
pub struct TestCube;

/// Server-side player-presence marker. The server spawns one entity per
/// connected client with this marker plus `NetworkOwner`; clients
/// materialize a wizard body for each one on receive.
///
/// Distinct from `crate::player::Player` so the local-spawned single-player
/// rig and the server-driven network avatars don't collide on observer
/// triggers. Stage P+ will unify them once position/input replication is
/// in place.
#[derive(
    Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize,
)]
pub struct NetworkedPlayer;

/// Identifies which connected client "owns" a replicated entity. Carries
/// the netcode `client_id` (a `u64`). Replicated alongside `NetworkedPlayer`
/// so clients can distinguish "my own avatar" from "everyone else's".
#[derive(
    Component, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize,
)]
pub struct NetworkOwner(pub u64);

/// Per-player cosmetic state replicated to every peer. Drives the
/// `RgbRecolorMaterial` tints + the visible character archetype. Edits
/// originate client-side (the customization UI) and round-trip
/// through `CustomizeMessage` → server → component replication so the
/// server stays authoritative and every observer agrees on what each
/// player looks like.
///
/// Colors are stored as `[f32; 3]` (linear-RGB) per slot so the
/// component derives `Serialize`/`Deserialize` without depending on
/// Bevy types in the wire format. 19 floats + 1 u32 per player —
/// negligible bandwidth even at a high edit rate.
#[derive(
    Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize,
)]
pub struct PlayerCustomization {
    pub body: [[f32; 3]; 3],
    pub objects: [[f32; 3]; 3],
    /// Per-slot mesh selection (class outfit, hair, face features).
    /// See `player::parts` for the variant tables.
    pub parts: crate::player::parts::PartSelection,
}

impl Default for PlayerCustomization {
    fn default() -> Self {
        // Mirrors `CharacterColors::default` in `player::recolor` and
        // `PartSelection::default` in `player::parts`.
        Self {
            body: [
                [0.85, 0.74, 0.62],
                [0.45, 0.20, 0.55],
                [0.10, 0.08, 0.12],
            ],
            objects: [
                [0.65, 0.55, 0.30],
                [0.20, 0.15, 0.25],
                [0.90, 0.88, 0.80],
            ],
            parts: crate::player::parts::PartSelection::default(),
        }
    }
}

/// Client → server: "Set my customization to this." Server stamps the
/// received value onto the matching `NetworkedPlayer`'s
/// `PlayerCustomization` component; replication then ships it to every
/// other connected client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomizeMessage {
    pub customization: PlayerCustomization,
}

/// Server → all-clients broadcast of one player's per-tick beam cast.
/// Replicating this as a Message instead of a per-player component because
/// lightyear's component-update replication wasn't reliably propagating
/// in our setup — Message broadcasts are simpler and demonstrably work.
/// The owner is identified by `client_id`; clients look up the
/// corresponding `NetworkedPlayer` by `NetworkOwner == client_id`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BeamCastBroadcast {
    pub client_id: u64,
    pub active: bool,
    pub origin: [f32; 3],
    pub direction: [f32; 3],
    pub length: f32,
}

/// Server-assigned stable id shared across every peer. Server increments a
/// counter at each replicated-entity spawn (player, prop, lantern, portal)
/// and clients receive the same id via replication. The local Bevy
/// `Entity` differs per peer; `NetworkedId` is the cross-peer key the
/// `wisp-net-test` harness uses to correlate trace events between
/// processes.
#[derive(
    Component, Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize,
)]
pub struct NetworkedId(pub u64);

/// Replicated health snapshot. Mirrors `Hurtbox.hp` / `Hurtbox.max_hp`
/// on the server (Hurtbox itself is server-only); clients read this for
/// HUDs and overhead bars. Discrete enough that interpolation would
/// blur damage feedback, so no `add_interpolation_with`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkedHealth {
    pub hp: f32,
    pub max_hp: f32,
}

/// Serializable world pose. Bevy's `Transform` doesn't implement serde by
/// default, so the protocol carries this wrapper and a client-side system
/// copies it onto the local `Transform` for rendering. Player bodies only
/// rotate around Y, so a single yaw scalar is enough — full Quat
/// replication can be added when other replicated entity classes need it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkedPosition {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// Body rotation around the world Y axis, in radians.
    pub yaw: f32,
    /// Camera aim pitch in radians, positive = looking up. Drives the
    /// upper-body spine lean (`chest_joint` rotation) so observers
    /// see the player tilting their torso with the aim direction.
    pub pitch: f32,
    /// True while the broadcasting client is airborne (post-jump,
    /// falling, etc.). Drives the remote falling animation clip.
    pub airborne: bool,
    /// True while the broadcasting client has a cast charging.
    pub casting: bool,
}

/// Rollback thresholds for the avian physics components, matched to the
/// `avian_3d_character` reference example. Below these epsilons we let
/// the prediction state stand without rolling back — avoids visual jitter
/// from float-noise-sized server corrections.
fn position_should_rollback(this: &Position, that: &Position) -> bool {
    (this.0 - that.0).length() >= 0.01
}

fn rotation_should_rollback(this: &Rotation, that: &Rotation) -> bool {
    this.angle_between(*that) >= 0.01
}

fn linear_velocity_should_rollback(
    this: &LinearVelocity,
    that: &LinearVelocity,
) -> bool {
    (this.0 - that.0).length() >= 0.01
}

/// Linear interpolation between two NetworkedPosition snapshots. Yaw
/// uses shortest-arc wrap-around (delta clamped into [-π, π]) so a
/// player turning from +179° to -179° interpolates +2°, not -358°.
fn lerp_networked_position(
    start: NetworkedPosition,
    end: NetworkedPosition,
    t: f32,
) -> NetworkedPosition {
    use core::f32::consts::PI;
    let t = t.clamp(0.0, 1.0);
    let mut dyaw = end.yaw - start.yaw;
    if dyaw > PI {
        dyaw -= 2.0 * PI;
    } else if dyaw < -PI {
        dyaw += 2.0 * PI;
    }
    NetworkedPosition {
        x: start.x + (end.x - start.x) * t,
        y: start.y + (end.y - start.y) * t,
        z: start.z + (end.z - start.z) * t,
        yaw: start.yaw + dyaw * t,
        // Pitch is clamped at the source to [-π/2, π/2] (PITCH_LIMIT
        // in the controller), so plain lerp doesn't need wrap-around.
        pitch: start.pitch + (end.pitch - start.pitch) * t,
        // Discrete states snap at t >= 0.5 — partial interpolation of
        // booleans isn't meaningful and we'd rather show the new pose
        // promptly than blend through an intermediate.
        airborne: if t >= 0.5 { end.airborne } else { start.airborne },
        casting: if t >= 0.5 { end.casting } else { start.casting },
    }
}

impl NetworkedPosition {
    pub fn from_vec3(v: Vec3) -> Self {
        Self {
            x: v.x,
            y: v.y,
            z: v.z,
            yaw: 0.0,
            pitch: 0.0,
            airborne: false,
            casting: false,
        }
    }
    pub fn to_vec3(self) -> Vec3 {
        Vec3::new(self.x, self.y, self.z)
    }
}
