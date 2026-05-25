//! Player module: controller, visuals, and a `spawn_player` factory used by
//! both the single-player world setup and (later) the multiplayer server.

pub mod controller;
pub mod parts;
pub mod recolor;
pub mod visuals;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy_enhanced_input::prelude::ContextActivity;

use crate::input::{player_actions, radial_menu_actions, PlayerContext, RadialMenuContext};
use crate::physics::GameLayer;
use crate::spells::convex_lens::LensPower;
use crate::spells::iris::IrisBurst;
use crate::spells::portal::{PortalLockout, PrevPos};
use crate::spells::{ActiveSpell, CastState, PrevActionSnapshot, SpellId, SpellRegistry};
use crate::weapons::{ActiveWeaponSlot, EquippedWeapons};

pub use controller::{Facing, Player, PlayerCamera};

pub struct PlayerPlugin;

impl Plugin for PlayerPlugin {
    fn build(&self, app: &mut App) {
        // Stage Q.5b: server is authoritative for player movement, so
        // we removed `apply_movement`, `apply_jump`, and
        // `apply_ground_brake` from the system set — those used to
        // apply forces to the local body. `apply_look` (mouse) still
        // runs to update `Facing` (yaw drives the input message, pitch
        // drives camera). `apply_rotation` keeps the body yaw locally
        // snappy by writing avian's `Rotation` from `Facing.yaw`. The
        // body's *position* is copied from the server-replicated
        // `NetworkedPosition` for the locally-owned `NetworkedPlayer`
        // by `controller::sync_local_player_from_server`.
        app.add_plugins(recolor::RecolorPlugin)
            .add_plugins(parts::PartsPlugin)
            .add_observer(controller::apply_look)
            .init_resource::<visuals::CharacterRegistry>()
            .init_resource::<visuals::CurrentCharacter>()
            .add_systems(Startup, visuals::load_wizard)
            .add_systems(
                Update,
                (
                    controller::apply_rotation,
                    controller::cursor_grab,
                    controller::apply_teleport_snaps.before(controller::apply_rotation),
                    controller::sync_local_player_from_server,
                    controller::track_local_velocity.after(controller::sync_local_player_from_server),
                    visuals::apply_character_change,
                    visuals::build_graph_when_loaded,
                    visuals::attach_animation_graph,
                    visuals::disable_skinned_mesh_culling,
                    visuals::propagate_self_body_render_layer,
                    visuals::swap_local_body_scene,
                    visuals::drive_animation,
                    visuals::cycle_character_on_input,
                    visuals::update_character_label,
                ),
            )
            .add_systems(FixedUpdate, controller::ground_check)
            .add_systems(
                bevy::app::PostUpdate,
                controller::apply_aim_pitch_to_local_spine
                    .after(bevy::app::AnimationSystems)
                    .before(bevy::transform::TransformSystems::Propagate),
            );
    }
}

/// Marker on every entity belonging to the local player's rig: the player
/// root, the camera child, the wand-tip child, and the body scene root. Used
/// by per-spell systems to find the "my-player" rig entity they care about
/// (e.g. `Single<&GlobalTransform, (With<PlayerCamera>, With<LocalPlayer>)>`
/// picks out the local camera even with many remote players in the world).
///
/// In single-player it's tagged on the only player rig; in multiplayer the
/// client tags it onto the replicated rig whose NetworkOwner matches its own
/// client id.
#[derive(Component)]
pub struct LocalPlayer;

/// Marker for the local player's wizard body root entity (carries the
/// `SceneRoot`). Used by `visuals::propagate_self_body_render_layer` to
/// scope its descendant walk — without it, every mesh on the local rig
/// would land on the self-body render layer.
#[derive(Component)]
pub struct LocalWizardBody;

/// Marker for the local player's **viewmodel** body root — a duplicate
/// of the character scene attached to the camera and rendered only on
/// [`VIEWMODEL_LAYER`]. Standard FPS pattern: world body stays hidden
/// in first-person, this mesh provides the "your own hands" view.
///
/// Loads `assets/character.glb` as a placeholder. To get true "hands
/// cut off" visuals, author a hands-only GLB (e.g. trimmed to the wrist
/// + finger bones) and swap the asset path in `spawn_player`.
#[derive(Component)]
pub struct LocalViewmodel;

/// Render layer that the local player's body lives on. Main camera (layer
/// 0) does NOT see it — keeps you out of your own mesh in first-person.
/// Portal cameras include this layer, so you can place a portal pair and
/// see your own character through it. The 3rd-person customizer view
/// also enables it.
pub const SELF_BODY_LAYER: usize = 1;

/// Render layer for the first-person **viewmodel** — a separate mesh
/// (e.g. a hands-only model) attached to the camera and rendered ONLY
/// by the 1st-person camera. Portal / customizer cameras exclude this
/// layer so the viewmodel doesn't appear in the third-person view.
///
/// The main body mesh stays on [`SELF_BODY_LAYER`] (hidden from 1st-
/// person); together with this layer that gives the classic FPS split:
/// world body hidden, viewmodel hands visible.
pub const VIEWMODEL_LAYER: usize = 2;

/// Authorship of the player entity. `Local` for single-player; phase 2 will
/// add a `Network(ClientId)` variant.
#[derive(Component, Clone, Copy, Debug)]
pub enum PlayerOwner {
    Local,
}

/// Marker for the wand-tip child entity. Its world transform is where beam
/// spells originate; its rotation is where they aim. Spawned as a child of
/// the player camera by [`spawn_player`].
#[derive(Component)]
pub struct LensAnchor;

/// Caches the player's rig child entities so per-frame systems don't have
/// to walk the hierarchy. Currently only the wand tip needs O(1) lookup;
/// add more fields here as they're needed.
#[derive(Component)]
pub struct PlayerRig {
    pub lens_anchor: Entity,
}

/// Cosmetic / animation descriptor that drives the wizard scene swap. For
/// now there's a single asset path; later this can hold material tints,
/// alternate scenes, or a custom AnimationGraphDescriptor for outfits.
#[derive(Component, Clone)]
pub struct CharacterVisuals {
    pub scene_asset: &'static str,
    /// Local translation of the body scene under the player root. Adjusts
    /// where the feet sit relative to the capsule.
    pub body_offset: Vec3,
    /// Local yaw on the body root. The default wizard glTF has +Z forward
    /// but Bevy / our controller uses -Z forward, so the default is π.
    pub body_yaw: f32,
}

impl Default for CharacterVisuals {
    fn default() -> Self {
        // The unified character: every class outfit + face/hair
        // variant in one glb. Visibility is toggled at runtime by
        // `player::parts::PartSelection` so the customizer can swap
        // any individual mesh without reloading.
        Self {
            scene_asset: "character.glb",
            body_offset: Vec3::new(0.0, -1.0, 0.0),
            body_yaw: std::f32::consts::PI,
        }
    }
}

/// Spawn parameters for [`spawn_player`].
pub struct PlayerSpawn {
    pub position: Vec3,
    pub yaw: f32,
    pub loadout: EquippedWeapons,
    pub active_slot: ActiveWeaponSlot,
    pub initial_spell: SpellId,
    pub visuals: CharacterVisuals,
    pub owner: PlayerOwner,
    /// Whether to mark this player as the client's local point of view.
    /// Single-player always sets this true; multiplayer client sets it true
    /// only for the player it owns.
    pub is_local: bool,
}

impl Default for PlayerSpawn {
    fn default() -> Self {
        Self {
            position: Vec3::new(0.0, 1.5, 5.0),
            yaw: 0.0,
            loadout: EquippedWeapons::starter(),
            active_slot: ActiveWeaponSlot::default(),
            initial_spell: SpellId::new("convex_lens"),
            visuals: CharacterVisuals::default(),
            owner: PlayerOwner::Local,
            is_local: true,
        }
    }
}

/// Build a fully-formed player entity. The hierarchy is:
///
/// ```text
/// Player ─┬─ PlayerCamera ── LensAnchor
///         └─ WizardBody (scene)
/// ```
///
/// The camera and lens-anchor child entity IDs are cached on `PlayerRig` for
/// O(1) lookup by beam systems.
pub fn spawn_player(
    commands: &mut Commands,
    asset_server: &AssetServer,
    registry: &SpellRegistry,
    spawn: PlayerSpawn,
) -> Entity {
    // LensAnchor is an invisible transform-only entity — the beam
    // origin / aim for spells that read `PlayerRig.lens_anchor`. The
    // gold wand cuboid that used to live here was removed in favor of
    // a viewmodel hands mesh on the camera (see below).
    let lens_anchor = commands
        .spawn((
            LensAnchor,
            Name::new("LensAnchor"),
            Transform::from_xyz(0.25, -0.3, -0.5),
            Visibility::default(),
        ))
        .id();

    // Viewmodel "hands": attaches a separate skinned mesh to the
    // camera so the player sees their own hands in 1st-person without
    // exposing the inside of the world body. Tagged `LocalViewmodel`
    // so `visuals::propagate_self_body_render_layer` stamps every
    // child mesh onto `VIEWMODEL_LAYER` — the layer the 1st-person
    // camera renders and the portal / customizer cameras exclude.
    //
    // **Asset note**: this is hidden by default because the engine
    // doesn't ship a hands-only GLB yet. Author one (trim the wizard
    // model down to the wrist + finger bones, save as
    // `assets/hands_viewmodel.glb`), swap the asset path below, and
    // flip the Visibility to `Inherited`. The render-layer plumbing
    // around this entity is already correct.
    let viewmodel = commands
        .spawn((
            Name::new("LocalViewmodel"),
            LocalViewmodel,
            SceneRoot(asset_server.load(GltfAssetLabel::Scene(0).from_asset(spawn.visuals.scene_asset))),
            Transform::from_xyz(0.0, -1.4, 0.0)
                .with_rotation(Quat::from_rotation_y(spawn.visuals.body_yaw)),
            Visibility::Hidden,
        ))
        .id();

    let camera = commands
        .spawn((
            PlayerCamera,
            Camera3d::default(),
            Projection::Perspective(PerspectiveProjection {
                fov: 90.0_f32.to_radians(),
                ..default()
            }),
            Transform::from_xyz(0.0, 0.7, 0.0),
            // 1st-person default: world (layer 0) + viewmodel layer.
            // The world body stays on `SELF_BODY_LAYER` (hidden here
            // so we don't render the inside of our own torso); portal
            // and customizer cameras enable that layer.
            bevy::camera::visibility::RenderLayers::from_layers(&[0, VIEWMODEL_LAYER]),
        ))
        .add_children(&[lens_anchor, viewmodel])
        .id();

    // The local rig's wizard body is the visible third-person mesh of
    // the player. We use `SELF_BODY_LAYER` render-layer filtering so the
    // main first-person camera doesn't render it (avoids the
    // see-inside-mesh problem) but portal cameras do — placing a portal
    // pair lets you check your character from outside.
    let body = commands
        .spawn((
            Name::new("WizardBody"),
            LocalWizardBody,
            SceneRoot(asset_server.load(GltfAssetLabel::Scene(0).from_asset(spawn.visuals.scene_asset))),
            Transform::from_translation(spawn.visuals.body_offset)
                .with_rotation(Quat::from_rotation_y(spawn.visuals.body_yaw)),
            Visibility::default(),
        ))
        .id();

    let player_entity = commands
        .spawn((
            Name::new("Player"),
            Player,
            Facing {
                yaw: spawn.yaw,
                pitch: 0.0,
                grounded: false,
            },
            (
                spawn.loadout,
                spawn.active_slot,
                ActiveSpell(spawn.initial_spell.clone()),
                LensPower::default(),
                IrisBurst::default(),
                PrevPos::default(),
                PortalLockout::default(),
                CastState::default(),
                PrevActionSnapshot::default(),
                visuals::LocalAnimBlend::default(),
            ),
            PlayerRig { lens_anchor },
            spawn.visuals,
            spawn.owner,
            Transform::from_translation(spawn.position)
                .with_rotation(Quat::from_axis_angle(Vec3::Y, spawn.yaw)),
            Visibility::default(),
            (
                // Stage Q.5b: Kinematic — server owns the authoritative
                // position, this body's `Position` is overwritten each
                // frame by `sync_local_player_from_server`. Mass /
                // damping / friction kept for code that reads them but
                // they're effectively dead (kinematic bodies ignore
                // forces). Collider kept so the local capsule still
                // raycasts for ground check + portal queries.
                RigidBody::Kinematic,
                Collider::capsule(0.4, 1.2),
                LockedAxes::ROTATION_LOCKED,
                Mass(80.0),
                LinearDamping(0.5),
                Friction::new(0.0),
                Restitution::new(0.0),
                CollisionLayers::new(GameLayer::Player, LayerMask::ALL),
                RayCaster::new(Vec3::new(0.0, -0.9, 0.0), Dir3::NEG_Y)
                    .with_max_distance(0.25)
                    .with_max_hits(1),
            ),
            PlayerContext,
            ContextActivity::<PlayerContext>::ACTIVE,
            player_actions(),
        ))
        .id();

    commands.entity(player_entity).insert((
        RadialMenuContext,
        ContextActivity::<RadialMenuContext>::INACTIVE,
        radial_menu_actions(),
    ));
    commands.entity(player_entity).add_children(&[camera, body]);

    if spawn.is_local {
        commands.entity(player_entity).insert(LocalPlayer);
        commands.entity(camera).insert(LocalPlayer);
        commands.entity(lens_anchor).insert(LocalPlayer);
        commands.entity(body).insert(LocalPlayer);
    }

    // Insert the initial spell's marker directly. The SwitchSpell message
    // path is reserved for player-driven switches (radial menu, networked
    // SwitchSpell messages).
    if let Some(entry) = registry.get(&spawn.initial_spell) {
        (entry.activate)(commands, player_entity);
    }

    player_entity
}
