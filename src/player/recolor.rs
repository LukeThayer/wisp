//! RGB-mask recolor material for asset-pack characters.
//!
//! The asset pack ships every character with a single `genericRGB`
//! texture whose R/G/B channels are interpreted as masks for three
//! independently-tintable regions. Bevy's default glTF loader installs
//! a `StandardMaterial` that just samples the texture as a literal
//! albedo — that produces the loud red+green+blue "checker" you'd see
//! in Blender's solid view. To get the actually-styled character we
//! swap in an [`ExtendedMaterial<StandardMaterial, RgbRecolorExt>`]
//! whose fragment shader feeds `r*tint_r + g*tint_g + b*tint_b` into
//! the upstream PBR pipeline. Using `ExtendedMaterial` (vs. a fully
//! bespoke `Material`) keeps shadow receiving, lighting, fog, and
//! tone-mapping behaving identically to every other PBR object in
//! the scene — only the base-color computation is overridden.
//!
//! See `assets/shaders/rgb_recolor.wgsl` for the fragment program.

use bevy::asset::AssetServer;
use bevy::gltf::Gltf;
use bevy::mesh::skinning::SkinnedMesh;
use bevy::pbr::{ExtendedMaterial, MaterialExtension, MaterialPlugin, MeshMaterial3d};
use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

use crate::player::visuals::WizardAssets;
use crate::player::LocalWizardBody;

pub type RgbRecolorMaterial = ExtendedMaterial<StandardMaterial, RgbRecolorExt>;

/// Plug-in: registers the material + the swap-in system.
pub struct RecolorPlugin;

impl Plugin for RecolorPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<RgbRecolorMaterial>::default())
            .init_resource::<CharacterColors>()
            .add_systems(Startup, preload_remote_character_gltf)
            .add_systems(
                Update,
                (
                    apply_recolor_materials,
                    apply_remote_recolor_materials,
                    refresh_tints_on_change,
                    refresh_remote_tints_on_change,
                ),
            );
    }
}

/// Pre-loaded `Gltf` handles for every entry in [`CharacterRegistry`],
/// keyed by archetype index. Used by [`apply_remote_recolor_materials`]
/// to look up `named_materials` for whichever character a given remote
/// player is currently rendered as.
#[derive(Resource)]
pub struct RemoteCharacterGltf {
    pub by_index: Vec<Handle<Gltf>>,
}

impl RemoteCharacterGltf {
    pub fn get(&self, index: usize) -> Option<&Handle<Gltf>> {
        self.by_index.get(index)
    }
}

fn preload_remote_character_gltf(
    mut commands: Commands,
    assets: Res<AssetServer>,
    registry: Res<crate::player::visuals::CharacterRegistry>,
) {
    let by_index = registry
        .characters
        .iter()
        .map(|def| assets.load::<Gltf>(def.asset))
        .collect();
    commands.insert_resource(RemoteCharacterGltf { by_index });
}

/// Tints applied to the RGB-mask channels. Two tint sets per
/// character: one for the `Body` material slot, one for `Objects`.
///
/// For now this is a local-only `Resource` driving the local
/// player's character. A follow-up will replace it with a
/// per-`NetworkedPlayer` replicated `PlayerCustomization` component.
#[derive(Resource, Clone, Debug)]
pub struct CharacterColors {
    pub body: [LinearRgba; 3],
    pub objects: [LinearRgba; 3],
}

impl Default for CharacterColors {
    fn default() -> Self {
        // Matches the witch's pre-baked palette (purple robe + black
        // accents + skin tone). Adjustable via the in-game picker
        // (next task).
        Self {
            body: [
                LinearRgba::rgb(0.85, 0.74, 0.62), // R-channel: skin
                LinearRgba::rgb(0.45, 0.20, 0.55), // G-channel: cloth-primary
                LinearRgba::rgb(0.10, 0.08, 0.12), // B-channel: cloth-secondary
            ],
            objects: [
                LinearRgba::rgb(0.65, 0.55, 0.30), // R-channel: metal-warm
                LinearRgba::rgb(0.20, 0.15, 0.25), // G-channel: wood / dark
                LinearRgba::rgb(0.90, 0.88, 0.80), // B-channel: light highlight
            ],
        }
    }
}

/// Extension over [`StandardMaterial`]: holds the three per-channel
/// tints and the RGB-mask texture. Binding indices start at 100 to
/// avoid collisions with `StandardMaterial`'s 0–15 range.
#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct RgbRecolorExt {
    #[uniform(100)]
    pub tint_r: LinearRgba,
    #[uniform(101)]
    pub tint_g: LinearRgba,
    #[uniform(102)]
    pub tint_b: LinearRgba,
    #[texture(103)]
    #[sampler(104)]
    pub mask_tex: Handle<Image>,
}

impl MaterialExtension for RgbRecolorExt {
    fn fragment_shader() -> ShaderRef {
        "shaders/rgb_recolor.wgsl".into()
    }
}

/// Tag inserted onto every mesh entity whose material we've already
/// swapped — keeps the per-frame walk a no-op once the character
/// scene is finished loading.
#[derive(Component)]
pub struct Recolored {
    /// Which slot this mesh was assigned. Stored so
    /// `refresh_tints_on_change` knows whether to write body or
    /// object tints when `CharacterColors` flips.
    slot: RecolorSlot,
    /// Owner of the tints. `Local` reads from `CharacterColors`,
    /// `Remote(player_entity)` reads from the `PlayerCustomization`
    /// on that NetworkedPlayer.
    owner: RecolorOwner,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum RecolorSlot {
    Body,
    Objects,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum RecolorOwner {
    Local,
    Remote(Entity),
}

/// Walk newly-spawned skinned meshes that are descendants of
/// `LocalWizardBody`, identify whether their `StandardMaterial`
/// matches the gltf's `genericRGBMat_Body` / `genericRGBMat_Objects`
/// asset, and swap in an `ExtendedMaterial<StandardMaterial,
/// RgbRecolorExt>` with the `CharacterColors`-sourced tints.
///
/// Not an `On<Add, ...>` observer because the gltf scene populates
/// `MeshMaterial3d<StandardMaterial>` over multiple frames as
/// dependencies resolve; a plain `Without<Recolored>` filter is
/// simpler and trivially re-entrant for character swaps.
pub fn apply_recolor_materials(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    wizard: Res<WizardAssets>,
    gltfs: Res<Assets<Gltf>>,
    std_materials: Res<Assets<StandardMaterial>>,
    mut recolor_materials: ResMut<Assets<RgbRecolorMaterial>>,
    colors: Res<CharacterColors>,
    pending: Query<
        (Entity, &MeshMaterial3d<StandardMaterial>, Option<&Name>),
        (With<SkinnedMesh>, Without<Recolored>),
    >,
    parents: Query<&ChildOf>,
    names: Query<&Name>,
    body_marker: Query<(), With<LocalWizardBody>>,
) {
    let gltf_handle = wizard.gltf();
    let Some(gltf) = gltfs.get(gltf_handle) else {
        return;
    };
    // Blender appends `.001` (etc.) to material names on re-import to
    // avoid collisions with already-resident assets, so the wizard.glb
    // and witch.glb ship the same logical slots under slightly
    // different names (e.g. `genericRGBMat_Body.001` vs
    // `genericRGBMat_Body`). Match by *prefix* so both files work.
    let body_mat = find_material_with_prefix(gltf, "genericRGBMat_Body");
    let objects_mat = find_material_with_prefix(gltf, "genericRGBMat_Objects");
    if body_mat.is_none() && objects_mat.is_none() {
        // Character glb doesn't use the RGB-recolor convention.
        return;
    }

    for (entity, mat_marker, own_name) in &pending {
        // Only swap on the local body — remote players are handled
        // separately once the replication patch lands.
        if !ancestor_has_marker(entity, &parents, &body_marker) {
            continue;
        }

        let slot = if Some(&mat_marker.0) == body_mat.as_ref() {
            RecolorSlot::Body
        } else if Some(&mat_marker.0) == objects_mat.as_ref() {
            RecolorSlot::Objects
        } else {
            // Some auxiliary material we don't know how to recolor
            // (eyes/eyebrows quad, etc.). Leave it alone.
            continue;
        };

        // Snapshot the source StandardMaterial: its PBR knobs
        // (metallic, roughness, occlusion, …) carry over to the
        // extended material's base. We drop the base color texture
        // because the mask now lives on the extension — the lighting
        // pass uses `base.base_color` as the unlit fallback, so we
        // neutralise it so it doesn't multiply against the tinted
        // albedo our shader produces.
        let Some(src_std) = std_materials.get(&mat_marker.0).cloned() else {
            continue;
        };
        let Some(mask_tex) = src_std.base_color_texture.clone() else {
            commands
                .entity(entity)
                .insert(Recolored { slot, owner: RecolorOwner::Local });
            continue;
        };
        let mut base = src_std;
        base.base_color_texture = None;
        base.base_color = Color::WHITE;

        // Face features (eyes / eyebrows / mouth):
        //   - depth_bias forces them on top of the head curve
        //     (eyes > eyebrows ordering keeps the eyebrow "brim"
        //     hidden behind the eye material where they overlap).
        //   - alpha_mode = Mask makes the recolor shader's
        //     `alpha_discard` cut the surrounding rectangle of each
        //     decal quad (the eyebrow texture has the brow shape
        //     painted with alpha=1 and the rest at alpha=0). Without
        //     this, the body's skin-toned RGB shows through the
        //     whole quad — the user described it as eyebrows that
        //     "sit off the face and are colored skin tone".
        let resolved = resolve_node_name(entity, own_name, &parents, &names);
        if let Some(bias) = face_feature_depth_bias(&resolved) {
            base.depth_bias = bias;
            base.alpha_mode = AlphaMode::Mask(0.5);
        }

        let tints = colors.tints_for(slot);
        let new_mat = recolor_materials.add(RgbRecolorMaterial {
            base,
            extension: RgbRecolorExt {
                tint_r: tints[0],
                tint_g: tints[1],
                tint_b: tints[2],
                mask_tex,
            },
        });
        commands
            .entity(entity)
            .insert(MeshMaterial3d(new_mat))
            .insert(Recolored { slot, owner: RecolorOwner::Local })
            .remove::<MeshMaterial3d<StandardMaterial>>();

        // Touch the shader so any first-build lag happens early.
        let _ = asset_server.load::<bevy::shader::Shader>("shaders/rgb_recolor.wgsl");
    }
}

/// Mirror of `apply_recolor_materials` for remote players: walks mesh
/// entities under `RemoteWizardBody`, finds the `NetworkedPlayer`
/// ancestor, and swaps materials using that player's
/// `PlayerCustomization` tints.
pub fn apply_remote_recolor_materials(
    mut commands: Commands,
    gltfs: Res<Assets<Gltf>>,
    remote_gltf: Option<Res<RemoteCharacterGltf>>,
    std_materials: Res<Assets<StandardMaterial>>,
    mut recolor_materials: ResMut<Assets<RgbRecolorMaterial>>,
    pending: Query<
        (Entity, &MeshMaterial3d<StandardMaterial>, Option<&Name>),
        (With<SkinnedMesh>, Without<Recolored>),
    >,
    parents: Query<&ChildOf>,
    names: Query<&Name>,
    remote_body_marker: Query<(), With<crate::net::replication::RemoteWizardBody>>,
    customizations: Query<
        &crate::net::protocol::PlayerCustomization,
        With<crate::net::protocol::NetworkedPlayer>,
    >,
) {
    let Some(remote_gltf) = remote_gltf else { return };

    for (entity, mat_marker, own_name) in &pending {
        // Only handle remote bodies — the local recolor system above
        // owns LocalWizardBody descendants.
        if !ancestor_has_marker(entity, &parents, &remote_body_marker) {
            continue;
        }
        // Walk up further to the NetworkedPlayer entity (the one
        // carrying PlayerCustomization). The hierarchy is
        // NetworkedPlayer → RemoteWizardBody → ... → SkinnedMesh.
        let Some((player_entity, customization)) =
            find_ancestor_customization(entity, &parents, &customizations)
        else {
            continue;
        };

        // Every remote body uses the unified `character.glb`
        // (slot 0 in the preloaded handle list). Class/hair/face
        // variation is per-mesh visibility, not per-glb.
        let _ = customization; // tints come from the customization, but we don't need the archetype here.
        let Some(gltf_handle) = remote_gltf.get(0) else {
            continue;
        };
        let Some(gltf) = gltfs.get(gltf_handle) else {
            continue;
        };
        let body_mat = find_material_with_prefix(gltf, "genericRGBMat_Body");
        let objects_mat = find_material_with_prefix(gltf, "genericRGBMat_Objects");

        let slot = if Some(&mat_marker.0) == body_mat.as_ref() {
            RecolorSlot::Body
        } else if Some(&mat_marker.0) == objects_mat.as_ref() {
            RecolorSlot::Objects
        } else {
            continue;
        };

        let Some(src_std) = std_materials.get(&mat_marker.0).cloned() else {
            continue;
        };
        let Some(mask_tex) = src_std.base_color_texture.clone() else {
            commands.entity(entity).insert(Recolored {
                slot,
                owner: RecolorOwner::Remote(player_entity),
            });
            continue;
        };
        let mut base = src_std;
        base.base_color_texture = None;
        base.base_color = Color::WHITE;
        // Same per-feature depth bias + alpha-mask as the local
        // recolor path — keeps face features visible AND properly
        // clipped to their shape for every observer.
        let resolved = resolve_node_name(entity, own_name, &parents, &names);
        if let Some(bias) = face_feature_depth_bias(&resolved) {
            base.depth_bias = bias;
            base.alpha_mode = AlphaMode::Mask(0.5);
        }

        let tints = tints_from_customization(customization, slot);
        let new_mat = recolor_materials.add(RgbRecolorMaterial {
            base,
            extension: RgbRecolorExt {
                tint_r: tints[0],
                tint_g: tints[1],
                tint_b: tints[2],
                mask_tex,
            },
        });
        commands
            .entity(entity)
            .insert(MeshMaterial3d(new_mat))
            .insert(Recolored {
                slot,
                owner: RecolorOwner::Remote(player_entity),
            })
            .remove::<MeshMaterial3d<StandardMaterial>>();
    }
}

/// When a remote player's `PlayerCustomization` changes (e.g. they
/// edited their colors and the server replicated the update),
/// rewrite the tints on the recolor materials we own for them.
pub fn refresh_remote_tints_on_change(
    customizations: Query<
        (Entity, &crate::net::protocol::PlayerCustomization),
        (
            With<crate::net::protocol::NetworkedPlayer>,
            Changed<crate::net::protocol::PlayerCustomization>,
        ),
    >,
    mut materials: ResMut<Assets<RgbRecolorMaterial>>,
    recolored: Query<(&MeshMaterial3d<RgbRecolorMaterial>, &Recolored)>,
) {
    if customizations.is_empty() {
        return;
    }
    for (mat_handle, recolored) in &recolored {
        let RecolorOwner::Remote(player) = recolored.owner else {
            continue;
        };
        let Ok((_, customization)) = customizations.get(player) else {
            continue;
        };
        let tints = tints_from_customization(customization, recolored.slot);
        if let Some(mat) = materials.get_mut(&mat_handle.0) {
            mat.extension.tint_r = tints[0];
            mat.extension.tint_g = tints[1];
            mat.extension.tint_b = tints[2];
        }
    }
}

fn tints_from_customization(
    c: &crate::net::protocol::PlayerCustomization,
    slot: RecolorSlot,
) -> [LinearRgba; 3] {
    let src = match slot {
        RecolorSlot::Body => &c.body,
        RecolorSlot::Objects => &c.objects,
    };
    [
        LinearRgba::rgb(src[0][0], src[0][1], src[0][2]),
        LinearRgba::rgb(src[1][0], src[1][1], src[1][2]),
        LinearRgba::rgb(src[2][0], src[2][1], src[2][2]),
    ]
}

fn find_ancestor_customization<'a>(
    entity: Entity,
    parents: &Query<&ChildOf>,
    customizations: &'a Query<
        &crate::net::protocol::PlayerCustomization,
        With<crate::net::protocol::NetworkedPlayer>,
    >,
) -> Option<(Entity, &'a crate::net::protocol::PlayerCustomization)> {
    let mut cur = entity;
    loop {
        if let Ok(c) = customizations.get(cur) {
            return Some((cur, c));
        }
        match parents.get(cur) {
            Ok(p) => cur = p.0,
            Err(_) => return None,
        }
    }
}

/// When `CharacterColors` changes, rewrite every existing
/// `RgbRecolorMaterial` to the new tints. Mutating in place rather
/// than replacing handles means meshes already pointing at a
/// material pick the change up for free.
pub fn refresh_tints_on_change(
    colors: Res<CharacterColors>,
    mut materials: ResMut<Assets<RgbRecolorMaterial>>,
    recolored: Query<(&MeshMaterial3d<RgbRecolorMaterial>, &Recolored)>,
) {
    if !colors.is_changed() {
        return;
    }
    for (mat_handle, recolored) in &recolored {
        // Remote-owned materials are driven by `refresh_remote_tints_on_change`
        // — they read from each NetworkedPlayer's replicated
        // PlayerCustomization, not the local UI's CharacterColors.
        if !matches!(recolored.owner, RecolorOwner::Local) {
            continue;
        }
        let tints = colors.tints_for(recolored.slot);
        if let Some(mat) = materials.get_mut(&mat_handle.0) {
            mat.extension.tint_r = tints[0];
            mat.extension.tint_g = tints[1];
            mat.extension.tint_b = tints[2];
        }
    }
}

impl CharacterColors {
    fn tints_for(&self, slot: RecolorSlot) -> &[LinearRgba; 3] {
        match slot {
            RecolorSlot::Body => &self.body,
            RecolorSlot::Objects => &self.objects,
        }
    }
}

/// Depth-buffer offset per face-feature class. The asset pack
/// authors these meshes a few millimetres inside the head surface;
/// a flat depth test bisects them along the head's curve, which the
/// user described as "eyes cut off halfway horizontally". Positive
/// values push the material toward the camera in depth-buffer space.
///
/// Eyes get the strongest bias because the eyebrows mesh has an
/// authored "brim" that arcs over the eye region — that brim is
/// intentionally hidden by the head in the asset. Without a higher
/// bias on the eyes the brim re-emerges and looks like the eyebrows
/// "come over the eyes like a baseball cap." Giving eyes more bias
/// than the eyebrows keeps the brim occluded by the eye material
/// where they overlap.
const EYES_DEPTH_BIAS: f32 = 100.0;
const EYEBROWS_DEPTH_BIAS: f32 = 50.0;
const MOUTH_DEPTH_BIAS: f32 = 50.0;

fn face_feature_depth_bias(mesh_name: &str) -> Option<f32> {
    if mesh_name.starts_with("F_eyes") {
        Some(EYES_DEPTH_BIAS)
    } else if mesh_name.starts_with("F_eyebrows") {
        Some(EYEBROWS_DEPTH_BIAS)
    } else if mesh_name.starts_with("F_mouth") {
        Some(MOUTH_DEPTH_BIAS)
    } else {
        None
    }
}

/// Resolve the meaningful gltf node-name for a mesh entity: prefer
/// its own `Name` (when it isn't a Bevy-auto `Mesh.*` label) and
/// otherwise walk parents until a non-`Mesh.*` Name turns up.
fn resolve_node_name(
    entity: Entity,
    own_name: Option<&Name>,
    parents: &Query<&ChildOf>,
    names: &Query<&Name>,
) -> String {
    let direct = own_name
        .map(|n| n.as_str().to_string())
        .filter(|s| !s.starts_with("Mesh"));
    if let Some(d) = direct {
        return d;
    }
    let mut cur = entity;
    loop {
        match parents.get(cur) {
            Ok(p) => {
                cur = p.0;
                if let Ok(n) = names.get(cur) {
                    let s = n.as_str();
                    if !s.starts_with("Mesh") {
                        return s.to_string();
                    }
                }
            }
            Err(_) => return String::new(),
        }
    }
}

fn find_material_with_prefix(gltf: &Gltf, prefix: &str) -> Option<Handle<StandardMaterial>> {
    gltf.named_materials
        .iter()
        .find(|(name, _)| name.starts_with(prefix))
        .map(|(_, h)| h.clone())
}

fn ancestor_has_marker<M: Component>(
    entity: Entity,
    parents: &Query<&ChildOf>,
    marker: &Query<(), With<M>>,
) -> bool {
    let mut cur = entity;
    loop {
        if marker.contains(cur) {
            return true;
        }
        match parents.get(cur) {
            Ok(p) => cur = p.0,
            Err(_) => return false,
        }
    }
}
