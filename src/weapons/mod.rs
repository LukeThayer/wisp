//! Weapons: magical tools that bind spells together. The player equips up
//! to two weapons at a time; the radial menu shows only the active
//! weapon's spells. Each weapon is a `.weapon.ron` asset under
//! `assets/weapons/` listing the spell ids it grants.
//!
//! Data flow mirrors the spell catalog:
//! 1. `WeaponsPlugin` registers `WeaponDef` as a typed RON asset and kicks
//!    off `AssetServer::load_folder("weapons")`.
//! 2. When the folder finishes (or hot-reloads) we rebuild `WeaponCatalog`.
//! 3. Other systems query `WeaponCatalog::get(WeaponId)` to enumerate a
//!    weapon's spell list at runtime.

use std::collections::HashMap;

use bevy::asset::LoadedFolder;
use bevy::prelude::*;
use bevy_common_assets::ron::RonAssetPlugin;
use serde::{Deserialize, Serialize};

use crate::input::SwapWeapon;
use crate::net::protocol::EquipWeaponsMessage;
use crate::net::protocol::PlayerInputChannel;
use crate::player::{LocalPlayer, Player};
use crate::spells::{SpellId, SwitchSpell};
use bevy_enhanced_input::prelude::*;
use lightyear::prelude::MessageSender;

pub struct WeaponsPlugin;

impl Plugin for WeaponsPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(RonAssetPlugin::<WeaponDef>::new(&["weapon.ron"]))
            .init_resource::<WeaponCatalog>()
            .init_resource::<WeaponFolder>()
            .add_systems(Startup, kick_off_folder)
            .add_systems(
                Update,
                (
                    rebuild_on_folder_event,
                    send_equip_on_local_change,
                ),
            )
            .add_observer(on_swap_weapon);
    }
}

/// Whenever the local player's `EquippedWeapons` or `ActiveWeaponSlot`
/// changes (inventory click, Tab to swap weapon), ship an
/// `EquipWeaponsMessage` to the server so the authoritative
/// `NetworkedPlayer` components stay in sync and replication
/// propagates to every other observer. Pure mirror — no local-side
/// effects, the inventory + Tab handlers already updated the local
/// rig before this runs.
fn send_equip_on_local_change(
    player: Option<
        Single<
            (&EquippedWeapons, &ActiveWeaponSlot),
            (
                With<Player>,
                With<LocalPlayer>,
                Or<(Changed<EquippedWeapons>, Changed<ActiveWeaponSlot>)>,
            ),
        >,
    >,
    sender: Option<Single<&mut MessageSender<EquipWeaponsMessage>>>,
) {
    let Some(player) = player else { return };
    let Some(mut sender) = sender else { return };
    let (equipped, slot) = *player;
    let slots = [
        equipped.0[0].as_ref().map(|w| w.0.clone()),
        equipped.0[1].as_ref().map(|w| w.0.clone()),
    ];
    let _ = sender.send::<PlayerInputChannel>(EquipWeaponsMessage {
        slots,
        active: slot.0,
    });
}

/// Tab handler: cycle `ActiveWeaponSlot` between equipped slots and snap
/// the player's `ActiveSpell` to the new weapon's first spell. Skips
/// empty slots so cycling through a partial loadout still works.
fn on_swap_weapon(
    _: On<Start<SwapWeapon>>,
    catalog: Res<WeaponCatalog>,
    mut player: Single<
        (Entity, &EquippedWeapons, &mut ActiveWeaponSlot),
        (With<Player>, With<LocalPlayer>),
    >,
    mut switch: MessageWriter<SwitchSpell>,
) {
    let (entity, equipped, slot) = &mut *player;
    let n = equipped.0.len() as u8;
    if n == 0 {
        return;
    }
    let start = slot.0;
    for step in 1..=n {
        let candidate = (start + step) % n;
        if let Some(weapon_id) = equipped
            .0
            .get(candidate as usize)
            .and_then(|s| s.as_ref())
        {
            if let Some(spell) =
                catalog.get(weapon_id).and_then(|d| d.spells.first()).cloned()
            {
                slot.0 = candidate;
                switch.write(SwitchSpell {
                    player: *entity,
                    spell,
                });
                return;
            }
        }
    }
}

/// Identifier for a weapon. String newtype so weapons can be added purely
/// as data — a new `.weapon.ron` file is a new id, no Rust change required.
#[derive(
    Deserialize, Serialize, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(transparent)]
pub struct WeaponId(pub String);

impl WeaponId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

impl std::borrow::Borrow<str> for WeaponId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WeaponId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Data definition of one weapon. Matches the `.weapon.ron` files.
#[derive(Asset, TypePath, Deserialize, Clone, Debug)]
pub struct WeaponDef {
    pub id: WeaponId,
    pub label: String,
    /// Spell ids granted by holding this weapon. Order is the order the
    /// spells appear in the radial wheel.
    pub spells: Vec<SpellId>,
}

#[derive(Resource, Default)]
pub struct WeaponCatalog {
    by_id: HashMap<WeaponId, WeaponDef>,
    order: Vec<WeaponId>,
}

impl WeaponCatalog {
    pub fn get(&self, id: &WeaponId) -> Option<&WeaponDef> {
        self.by_id.get(id)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&WeaponId, &WeaponDef)> {
        self.order
            .iter()
            .filter_map(|id| self.by_id.get(id).map(|w| (id, w)))
    }
    pub fn ids(&self) -> impl Iterator<Item = &WeaponId> {
        self.order.iter()
    }
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[derive(Resource, Default)]
struct WeaponFolder(Option<Handle<LoadedFolder>>);

fn kick_off_folder(asset_server: Res<AssetServer>, mut folder: ResMut<WeaponFolder>) {
    folder.0 = Some(asset_server.load_folder("weapons"));
}

fn rebuild_on_folder_event(
    mut events: MessageReader<AssetEvent<LoadedFolder>>,
    folder: Res<WeaponFolder>,
    loaded_folders: Res<Assets<LoadedFolder>>,
    weapon_assets: Res<Assets<WeaponDef>>,
    mut catalog: ResMut<WeaponCatalog>,
) {
    let mut dirty = false;
    for ev in events.read() {
        match ev {
            AssetEvent::LoadedWithDependencies { .. }
            | AssetEvent::Added { .. }
            | AssetEvent::Modified { .. } => dirty = true,
            _ => {}
        }
    }
    if !dirty {
        return;
    }
    let Some(loaded) = folder.0.as_ref().and_then(|h| loaded_folders.get(h)) else {
        return;
    };

    let mut next_by_id: HashMap<WeaponId, WeaponDef> = HashMap::new();
    let mut next_order: Vec<WeaponId> = Vec::new();
    for handle in &loaded.handles {
        if let Ok(typed) = handle.clone().try_typed::<WeaponDef>() {
            if let Some(def) = weapon_assets.get(&typed) {
                if !next_by_id.contains_key(&def.id) {
                    next_order.push(def.id.clone());
                }
                next_by_id.insert(def.id.clone(), def.clone());
            }
        }
    }
    next_order.sort();

    let was_empty = catalog.is_empty();
    let changed = next_order != catalog.order;
    if was_empty || changed {
        info!(
            "Weapon catalog: {} weapon{}.",
            next_by_id.len(),
            if next_by_id.len() == 1 { "" } else { "s" },
        );
    }
    catalog.by_id = next_by_id;
    catalog.order = next_order;
}

// --- Player equipment components ------------------------------------------

/// The two weapons the player has equipped. `None` in a slot = empty.
/// Lives on the local Player rig and on the server's `NetworkedPlayer`
/// (replicated so other peers can see who's holding what).
#[derive(Component, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EquippedWeapons(pub [Option<WeaponId>; 2]);

impl EquippedWeapons {
    /// Default starter loadout — lens + needle&thread. Demon core stays
    /// available in the inventory for the player to swap in.
    pub fn starter() -> Self {
        Self([
            Some(WeaponId::new("lens")),
            Some(WeaponId::new("needle_and_thread")),
        ])
    }
}

/// Which of the two equipped slots is currently active. Drives the
/// radial wheel population and the cast-engine's `ActiveSpell`.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveWeaponSlot(pub u8);

impl Default for ActiveWeaponSlot {
    fn default() -> Self {
        Self(0)
    }
}

/// Helper: which spell should be active given a slot + the catalog?
/// Returns the first spell of the weapon in that slot, or `None` if the
/// slot is empty or the weapon is unknown.
pub fn first_spell_in_slot(
    equipped: &EquippedWeapons,
    slot: u8,
    catalog: &WeaponCatalog,
) -> Option<SpellId> {
    let weapon_id = equipped.0.get(slot as usize)?.as_ref()?;
    let def = catalog.get(weapon_id)?;
    def.spells.first().cloned()
}
