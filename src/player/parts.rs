//! Slot-based mesh selection for the unified `character.glb`.
//!
//! `assets/character.glb` is a single rig + every class outfit, face
//! variant, and hair variant the asset pack ships. To present a
//! custom-looking character we keep all those meshes loaded and
//! toggle `Visibility` per mesh based on a [`PartSelection`].
//!
//! Slots:
//!   - **Top / Bottom / Headwear** — each is a list of variants where
//!     every variant is a *set* of mesh node-names. So "Knight Helmet"
//!     picks both `F_Knight_ArmetHelmet` and `F_Knight_ArmetHelmet_Visor`.
//!     This keeps the wire small (one u8 per slot) while preserving
//!     authored-together intent.
//!   - Weapons (`F_*_Staff`, `F_*_Sword`, `F_*_Bow`, etc.) and capes
//!     (`F_*_Cape`, `F_*_Shawl`, `F_*_Scarf`) are kept loaded but
//!     hidden — gameplay drives held items via spell / inventory
//!     state, and capes were removed from the customizer after the
//!     cape physics attempt didn't pan out.
//!   - **Hair** — optional. `None` for bald, otherwise one of the
//!     working `F_hair_*` variants.
//!   - **Eyes / Eyebrows / Mouth** — pick from `F_eyes0..4` etc. Only
//!     the `*0` variant is transform-correct in the current asset
//!     pack export — extending the tables here is enough to add more
//!     once the source FBX fixes them.
//!
//! Body skin (`F_TopBody`, `F_BottomBody`) is hidden always — every
//! class top/bottom fully covers it and keeping it visible
//! double-renders limbs.

use bevy::camera::visibility::Visibility;
use bevy::mesh::skinning::SkinnedMesh;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::player::LocalWizardBody;

pub struct PartsPlugin;

impl Plugin for PartsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PartSelection>().add_systems(
            Update,
            (
                apply_local_part_visibility,
                refresh_local_part_visibility_on_change,
            ),
        );
    }
}

/// Per-slot index into the variant tables below. Wire-friendly:
/// fixed-size, all `u8`s, so it serdes cleanly inside
/// `PlayerCustomization` without growing the message meaningfully.
#[derive(Resource, Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartSelection {
    pub top: u8,
    pub bottom: u8,
    pub headwear: u8,
    pub hair: u8,
    pub eyes: u8,
    pub eyebrows: u8,
    pub mouth: u8,
}

impl Default for PartSelection {
    fn default() -> Self {
        Self {
            top: 0,      // Witch Top
            bottom: 0,   // Witch Bottom
            headwear: 1, // Witch Hat
            hair: 1,     // Hair 1
            eyes: 0,
            eyebrows: 0,
            mouth: 0,
        }
    }
}

/// One variant in a slot table: a display label plus the set of mesh
/// node-names that compose it. Empty mesh list = "None".
type MeshList = &'static [&'static str];

pub const TOP_VARIANTS: &[(&str, MeshList)] = &[
    ("Witch", &["F_Witch_Top"]),
    ("Archer", &["F_Archer_Top"]),
    ("Fighter", &["F_Fighter_Top"]),
    ("Hunter", &["F_Hunter_Top"]),
    // Knight pauldrons are authored as a separate node but visually
    // belong to the top — bundle them so we can't pick the Knight top
    // without the matching shoulders.
    ("Knight", &["F_Knight_Top", "F_Knight_Pauldrons"]),
    ("Mage", &["F_Mage_Top"]),
    ("Rogue", &["F_Rogue_Top"]),
    ("Sorcerer", &["F_Sorcerer_Top"]),
    ("Swordsman", &["F_Swordsman_Top"]),
];

pub const BOTTOM_VARIANTS: &[(&str, MeshList)] = &[
    ("Witch", &["F_Witch_Bottom"]),
    ("Archer", &["F_Archer_Bottom"]),
    ("Fighter", &["F_Fighter_Bottom"]),
    ("Hunter", &["F_Hunter_Bottom"]),
    ("Knight", &["F_Knight_Bottom", "F_Knight_Skirt"]),
    ("Mage", &["F_Mage_Bottom"]),
    ("Rogue", &["F_Rogue_Bottom"]),
    // Sorcerer's shoes were authored as a separate node but stay
    // with the bottom slot in every pre-baked combination.
    ("Sorcerer", &["F_Sorcerer_Bottom", "F_Sorcerer_Shoes"]),
    ("Swordsman", &["F_Swordsman_Bottom"]),
];

pub const HEADWEAR_VARIANTS: &[(&str, MeshList)] = &[
    ("None", &[]),
    ("Witch Hat", &["F_Witch_Headwear"]),
    ("Mage Hood", &["F_Mage_Headwear"]),
    ("Archer Bycocket", &["F_Archer_BycocketHat"]),
    ("Hunter Beret", &["F_Hunter_Beret"]),
    ("Knight Helmet", &["F_Knight_ArmetHelmet", "F_Knight_ArmetHelmet_Visor"]),
    ("Swordsman Kettle", &["F_Swordsman_KettleHat"]),
    ("Sorcerer Circlet", &["F_Sorcerer_Circlet"]),
    ("Fighter Coif", &["F_Fighter_LeatherCoif"]),
    ("Rogue Mask", &["F_Rogue_FaceMask", "F_Rogue_FaceMask_NeckWrap"]),
];

/// Every cape / shawl / scarf node-name that ships in `character.glb`.
/// Hidden categorically — no customizer slot exposes them. Lives
/// here so `is_visible` can hide them without having to know about
/// every class. (If the cape physics attempt is revisited or capes
/// become a customization slot again, remove these and add a
/// `CAPE_VARIANTS` table.)
const ALL_CAPES: &[&str] = &[
    "F_Witch_Cape",
    "F_Witch_Choker",
    "F_Archer_Cape",
    "F_Mage_Cape",
    "F_Mage_Scarf",
    "F_Sorcerer_Shawl",
];

/// Every weapon node-name that ships in `character.glb`. Listed here
/// so `is_visible` can hide them all categorically — no customizer
/// slot exposes a weapon choice. If gameplay (spells / inventory)
/// later wants to *show* a specific weapon, it'll override visibility
/// on the matching mesh entity directly.
const ALL_WEAPONS: &[&str] = &[
    "F_Witch_Staff",
    "F_Mage_Staff",
    "F_Sorcerer_Staff",
    "F_Archer_Bow",
    "F_Archer_Arrow",
    "F_Archer_ArrowQuiver",
    "F_Hunter_Bow",
    "F_Hunter_Arrow",
    "F_Hunter_ArrowQuiver",
    "F_Fighter_Sword",
    "F_Fighter_SwordScabbard",
    "F_Knight_Sword",
    "F_Knight_SwordScabbard",
    "F_Knight_Dagger",
    "F_Knight_DaggerScabbard",
    "F_Swordsman_Sword",
    "F_Swordsman_SwordScabbard",
    "F_Rogue_Dagger",
    "F_Rogue_DaggerScabbard",
];

pub const HAIR_VARIANTS: &[(&str, Option<&str>)] = &[
    ("None", None),
    ("Hair 1", Some("F_hair_1")),
    ("Hair 1b", Some("F_hair_1b")),
    ("Hair 14b Bun", Some("F_hair_14b_Bun")),
];

pub const EYES_VARIANTS: &[(&str, &str)] = &[("Eyes 0", "F_eyes0")];
pub const EYEBROWS_VARIANTS: &[(&str, &str)] = &[("Eyebrows 0", "F_eyebrows0")];
pub const MOUTH_VARIANTS: &[(&str, &str)] = &[("Mouth 0", "F_mouth0")];

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Slot {
    Top,
    Bottom,
    Headwear,
    Hair,
    Eyes,
    Eyebrows,
    Mouth,
}

/// Iteration order used by the customizer UI.
pub const SLOTS: &[Slot] = &[
    Slot::Top,
    Slot::Bottom,
    Slot::Headwear,
    Slot::Hair,
    Slot::Eyes,
    Slot::Eyebrows,
    Slot::Mouth,
];

impl Slot {
    pub fn display(self) -> &'static str {
        match self {
            Slot::Top => "Top",
            Slot::Bottom => "Bottom",
            Slot::Headwear => "Headwear",
            Slot::Hair => "Hair",
            Slot::Eyes => "Eyes",
            Slot::Eyebrows => "Eyebrows",
            Slot::Mouth => "Mouth",
        }
    }

    fn outfit_table(self) -> Option<&'static [(&'static str, MeshList)]> {
        match self {
            Slot::Top => Some(TOP_VARIANTS),
            Slot::Bottom => Some(BOTTOM_VARIANTS),
            Slot::Headwear => Some(HEADWEAR_VARIANTS),
            _ => None,
        }
    }
}

impl PartSelection {
    pub fn index_for(&self, slot: Slot) -> u8 {
        match slot {
            Slot::Top => self.top,
            Slot::Bottom => self.bottom,
            Slot::Headwear => self.headwear,
            Slot::Hair => self.hair,
            Slot::Eyes => self.eyes,
            Slot::Eyebrows => self.eyebrows,
            Slot::Mouth => self.mouth,
        }
    }

    fn index_mut(&mut self, slot: Slot) -> &mut u8 {
        match slot {
            Slot::Top => &mut self.top,
            Slot::Bottom => &mut self.bottom,
            Slot::Headwear => &mut self.headwear,
            Slot::Hair => &mut self.hair,
            Slot::Eyes => &mut self.eyes,
            Slot::Eyebrows => &mut self.eyebrows,
            Slot::Mouth => &mut self.mouth,
        }
    }

    pub fn variant_count(slot: Slot) -> usize {
        match slot {
            Slot::Top => TOP_VARIANTS.len(),
            Slot::Bottom => BOTTOM_VARIANTS.len(),
            Slot::Headwear => HEADWEAR_VARIANTS.len(),
            Slot::Hair => HAIR_VARIANTS.len(),
            Slot::Eyes => EYES_VARIANTS.len(),
            Slot::Eyebrows => EYEBROWS_VARIANTS.len(),
            Slot::Mouth => MOUTH_VARIANTS.len(),
        }
    }

    /// Wrapping advance/back through the slot's variant table.
    pub fn advance(&mut self, slot: Slot, dir: i32) {
        let len = Self::variant_count(slot);
        if len == 0 {
            return;
        }
        let current = self.index_mut(slot);
        let next = (*current as i32 + dir).rem_euclid(len as i32);
        *current = next as u8;
    }

    pub fn label(&self, slot: Slot) -> &'static str {
        let idx = self.index_for(slot) as usize;
        match slot {
            Slot::Top => TOP_VARIANTS.get(idx).map(|(l, _)| *l).unwrap_or("?"),
            Slot::Bottom => BOTTOM_VARIANTS.get(idx).map(|(l, _)| *l).unwrap_or("?"),
            Slot::Headwear => HEADWEAR_VARIANTS.get(idx).map(|(l, _)| *l).unwrap_or("?"),
            Slot::Hair => HAIR_VARIANTS.get(idx).map(|(l, _)| *l).unwrap_or("?"),
            Slot::Eyes => EYES_VARIANTS.get(idx).map(|(l, _)| *l).unwrap_or("?"),
            Slot::Eyebrows => EYEBROWS_VARIANTS.get(idx).map(|(l, _)| *l).unwrap_or("?"),
            Slot::Mouth => MOUTH_VARIANTS.get(idx).map(|(l, _)| *l).unwrap_or("?"),
        }
    }

    /// Returns whether a given mesh node-name should be visible under
    /// this selection. The decision walks the slot tables and uses
    /// per-slot ownership: a mesh that's listed in any outfit slot's
    /// table is shown iff it's in the **selected** variant of that
    /// slot. Meshes not listed anywhere fall through to
    /// face / hair / always-on rules.
    pub fn is_visible(&self, mesh_name: &str) -> bool {
        // Body skin always hidden.
        if mesh_name == "F_BottomBody" || mesh_name == "F_TopBody" {
            return false;
        }
        // Weapons + capes always hidden — no customizer slots expose
        // them. Weapons are reserved for gameplay; capes were
        // dropped after the cape-physics experiment didn't land.
        if ALL_WEAPONS.contains(&mesh_name) || ALL_CAPES.contains(&mesh_name) {
            return false;
        }
        // Face features and hair are decided by their own tables.
        if let Some(rest) = mesh_name.strip_prefix("F_eyes") {
            return is_indexed_match(rest, self.eyes, EYES_VARIANTS.len());
        }
        if let Some(rest) = mesh_name.strip_prefix("F_eyebrows") {
            return is_indexed_match(rest, self.eyebrows, EYEBROWS_VARIANTS.len());
        }
        if let Some(rest) = mesh_name.strip_prefix("F_mouth") {
            return is_indexed_match(rest, self.mouth, MOUTH_VARIANTS.len());
        }
        if mesh_name.starts_with("F_hair") {
            let target = HAIR_VARIANTS.get(self.hair as usize).and_then(|(_, n)| *n);
            return target.is_some_and(|t| t == mesh_name);
        }
        // Outfit slots: if mesh appears in any slot's table, show iff
        // in that slot's selected variant.
        for &slot in SLOTS {
            let Some(table) = slot.outfit_table() else { continue };
            if let Some(visible) = decide_outfit_visibility(
                mesh_name,
                table,
                self.index_for(slot),
            ) {
                return visible;
            }
        }
        // Unrecognized mesh — default to visible.
        true
    }
}

fn decide_outfit_visibility(
    mesh_name: &str,
    table: &[(&str, MeshList)],
    selected: u8,
) -> Option<bool> {
    let mut found_in_table = false;
    for (i, (_, meshes)) in table.iter().enumerate() {
        if meshes.contains(&mesh_name) {
            found_in_table = true;
            if i == selected as usize {
                return Some(true);
            }
        }
    }
    if found_in_table { Some(false) } else { None }
}

fn is_indexed_match(rest: &str, selected: u8, len: usize) -> bool {
    if let Ok(n) = rest.parse::<u8>() {
        return (n as usize) < len && n == selected;
    }
    false
}

/// Marker placed on every mesh entity once we've decided its
/// visibility for the current selection. The cached `mesh_name` lets
/// `refresh_local_part_visibility_on_change` re-evaluate without
/// walking back up the hierarchy to read the `Name` component.
#[derive(Component)]
pub struct PartMesh {
    pub name: String,
}

fn apply_local_part_visibility(
    mut commands: Commands,
    selection: Res<PartSelection>,
    pending: Query<
        (Entity, Option<&Name>),
        (With<SkinnedMesh>, Without<PartMesh>),
    >,
    parents: Query<&ChildOf>,
    names: Query<&Name>,
    body_marker: Query<(), With<LocalWizardBody>>,
    mut visibility: Query<&mut Visibility>,
) {
    for (entity, name) in &pending {
        if !ancestor_has_body_marker(entity, &parents, &body_marker) {
            continue;
        }
        let own = name
            .map(|n| n.as_str().to_string())
            .filter(|s| !s.starts_with("Mesh"));
        let resolved = own
            .or_else(|| {
                let mut cur = entity;
                loop {
                    match parents.get(cur) {
                        Ok(p) => {
                            cur = p.0;
                            if let Ok(n) = names.get(cur) {
                                let s = n.as_str();
                                if !s.starts_with("Mesh") {
                                    break Some(s.to_string());
                                }
                            }
                        }
                        Err(_) => break None,
                    }
                }
            })
            .unwrap_or_default();

        let vis = if selection.is_visible(&resolved) {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if let Ok(mut v) = visibility.get_mut(entity) {
            *v = vis;
        }
        commands.entity(entity).insert(PartMesh { name: resolved });
    }
}

fn refresh_local_part_visibility_on_change(
    selection: Res<PartSelection>,
    mut meshes: Query<(&PartMesh, &mut Visibility)>,
) {
    if !selection.is_changed() {
        return;
    }
    for (part, mut vis) in &mut meshes {
        *vis = if selection.is_visible(&part.name) {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

fn ancestor_has_body_marker(
    entity: Entity,
    parents: &Query<&ChildOf>,
    marker: &Query<(), With<LocalWizardBody>>,
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
