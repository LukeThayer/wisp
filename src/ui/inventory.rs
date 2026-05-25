//! Inventory modal: press `i` to open, click weapon rows to equip into
//! slot 1 / slot 2, press `i` again or Esc to close. Two-slot loadout
//! drives what shows up in the radial wheel; switching the active slot
//! is a separate concern (see [`crate::weapons::on_swap_weapon`]).

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use bevy_enhanced_input::prelude::*;

use crate::input::{InputMode, OpenInventory};
use crate::player::{LocalPlayer, Player};
use crate::spells::catalog::SpellCatalog;
use crate::spells::{ActiveSpell, SpellRegistry, SwitchSpell};
use crate::weapons::{
    first_spell_in_slot, ActiveWeaponSlot, EquippedWeapons, WeaponCatalog, WeaponId,
};

pub struct InventoryPlugin;

impl Plugin for InventoryPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(on_open_inventory).add_systems(
            Update,
            (
                handle_equip_clicks,
                handle_close_keys,
                refresh_slot_labels,
            ),
        );
    }
}

#[derive(Component)]
struct InventoryRoot;

#[derive(Component)]
struct InventoryEquipButton {
    weapon: WeaponId,
    slot: u8,
}

#[derive(Component)]
struct InventorySlotLabel {
    slot: u8,
}

fn on_open_inventory(
    _: On<Start<OpenInventory>>,
    mut commands: Commands,
    mut mode: ResMut<InputMode>,
    mut cursor_opts: Single<&mut CursorOptions, With<PrimaryWindow>>,
    existing: Query<Entity, With<InventoryRoot>>,
    catalog: Res<WeaponCatalog>,
    spells: Res<SpellCatalog>,
    registry: Res<SpellRegistry>,
    player: Single<(&EquippedWeapons,), (With<Player>, With<LocalPlayer>)>,
) {
    // Toggle: open if closed, close if already open.
    if let Some(root) = existing.iter().next() {
        commands.entity(root).despawn();
        *mode = InputMode::Player;
        cursor_opts.grab_mode = CursorGrabMode::Locked;
        cursor_opts.visible = false;
        return;
    }
    let (equipped,) = *player;

    *mode = InputMode::Inventory;
    cursor_opts.grab_mode = CursorGrabMode::None;
    cursor_opts.visible = true;

    commands
        .spawn((
            InventoryRoot,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
        ))
        .with_children(|root| {
            // Centered panel.
            root.spawn((
                Node {
                    width: Val::Px(560.0),
                    padding: UiRect::all(Val::Px(24.0)),
                    flex_direction: FlexDirection::Column,
                    row_gap: Val::Px(16.0),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.08, 0.08, 0.12, 0.96)),
            ))
            .with_children(|panel| {
                panel.spawn((
                    Text::new("Equipment"),
                    TextFont { font_size: 22.0, ..default() },
                    TextColor(Color::WHITE),
                ));

                // Slot summary row.
                panel
                    .spawn(Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: Val::Px(12.0),
                        ..default()
                    })
                    .with_children(|row| {
                        for slot in 0u8..2 {
                            let label_text = slot_label_text(slot, equipped, &catalog);
                            row.spawn((
                                Node {
                                    flex_basis: Val::Percent(50.0),
                                    padding: UiRect::all(Val::Px(10.0)),
                                    flex_direction: FlexDirection::Column,
                                    row_gap: Val::Px(4.0),
                                    ..default()
                                },
                                BackgroundColor(Color::srgba(0.16, 0.16, 0.2, 1.0)),
                            ))
                            .with_children(|cell| {
                                cell.spawn((
                                    Text::new(format!("Slot {}", slot + 1)),
                                    TextFont { font_size: 13.0, ..default() },
                                    TextColor(Color::srgba(0.7, 0.7, 0.75, 1.0)),
                                ));
                                cell.spawn((
                                    InventorySlotLabel { slot },
                                    Text::new(label_text),
                                    TextFont { font_size: 17.0, ..default() },
                                    TextColor(Color::WHITE),
                                ));
                            });
                        }
                    });

                panel.spawn((
                    Node {
                        height: Val::Px(1.0),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.4, 0.4, 0.45, 0.4)),
                ));

                panel.spawn((
                    Text::new("Available weapons"),
                    TextFont { font_size: 14.0, ..default() },
                    TextColor(Color::srgba(0.75, 0.75, 0.8, 1.0)),
                ));

                // One row per weapon: label + spell list + [Slot 1] [Slot 2] buttons.
                for (weapon_id, def) in catalog.iter() {
                    panel
                        .spawn((
                            Node {
                                flex_direction: FlexDirection::Row,
                                align_items: AlignItems::Center,
                                column_gap: Val::Px(12.0),
                                padding: UiRect::all(Val::Px(8.0)),
                                ..default()
                            },
                            BackgroundColor(Color::srgba(0.12, 0.12, 0.16, 1.0)),
                        ))
                        .with_children(|row| {
                            // Name + spell summary.
                            row.spawn(Node {
                                flex_basis: Val::Percent(50.0),
                                flex_direction: FlexDirection::Column,
                                row_gap: Val::Px(2.0),
                                ..default()
                            })
                            .with_children(|name_col| {
                                name_col.spawn((
                                    Text::new(def.label.clone()),
                                    TextFont { font_size: 17.0, ..default() },
                                    TextColor(Color::WHITE),
                                ));
                                let spell_summary = def
                                    .spells
                                    .iter()
                                    .map(|id| {
                                        spells
                                            .get(id)
                                            .map(|s| s.label.clone())
                                            .unwrap_or_else(|| registry.label(id).to_string())
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" · ");
                                name_col.spawn((
                                    Text::new(spell_summary),
                                    TextFont { font_size: 12.0, ..default() },
                                    TextColor(Color::srgba(0.7, 0.7, 0.75, 1.0)),
                                ));
                            });

                            for slot in 0u8..2 {
                                row.spawn((
                                    Button,
                                    InventoryEquipButton {
                                        weapon: weapon_id.clone(),
                                        slot,
                                    },
                                    Node {
                                        padding: UiRect::axes(Val::Px(12.0), Val::Px(6.0)),
                                        ..default()
                                    },
                                    BackgroundColor(Color::srgba(0.2, 0.2, 0.28, 1.0)),
                                    children![(
                                        Text::new(format!("→ Slot {}", slot + 1)),
                                        TextFont { font_size: 13.0, ..default() },
                                        TextColor(Color::WHITE),
                                    )],
                                ));
                            }
                        });
                }

                panel.spawn((
                    Text::new("I / Esc to close"),
                    TextFont { font_size: 12.0, ..default() },
                    TextColor(Color::srgba(0.55, 0.55, 0.6, 1.0)),
                ));
            });
        });
}

fn slot_label_text(slot: u8, equipped: &EquippedWeapons, catalog: &WeaponCatalog) -> String {
    equipped
        .0
        .get(slot as usize)
        .and_then(|s| s.as_ref())
        .and_then(|id| catalog.get(id))
        .map(|d| d.label.clone())
        .unwrap_or_else(|| "— empty —".to_string())
}

fn handle_equip_clicks(
    interactions: Query<
        (&Interaction, &InventoryEquipButton),
        (Changed<Interaction>, With<Button>),
    >,
    mut player: Single<
        (Entity, &mut EquippedWeapons, &ActiveWeaponSlot, &ActiveSpell),
        (With<Player>, With<LocalPlayer>),
    >,
    catalog: Res<WeaponCatalog>,
    mut switch_writer: MessageWriter<SwitchSpell>,
) {
    for (interaction, btn) in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let (entity, equipped, active_slot, active_spell) = &mut *player;
        if (btn.slot as usize) >= equipped.0.len() {
            continue;
        }
        equipped.0[btn.slot as usize] = Some(btn.weapon.clone());

        // If the slot we just changed is the active one, snap ActiveSpell
        // to the new weapon's first cast so the cast engine reflects the
        // swap immediately. If the previously-active spell is still in
        // the new weapon's roster, leave it alone (no surprise switch).
        if active_slot.0 == btn.slot {
            let still_valid = catalog
                .get(&btn.weapon)
                .map(|d| d.spells.iter().any(|s| s == &active_spell.0))
                .unwrap_or(false);
            if !still_valid {
                if let Some(spell) = first_spell_in_slot(equipped, btn.slot, &catalog) {
                    switch_writer.write(SwitchSpell {
                        player: *entity,
                        spell,
                    });
                }
            }
        }
    }
}

/// Keep the slot summary labels at the top of the modal in sync with the
/// `EquippedWeapons` component as the player clicks Equip buttons. Skips
/// when the modal isn't open (no `InventoryRoot`).
fn refresh_slot_labels(
    open: Query<(), With<InventoryRoot>>,
    catalog: Res<WeaponCatalog>,
    player: Option<Single<&EquippedWeapons, (With<Player>, With<LocalPlayer>)>>,
    mut labels: Query<(&InventorySlotLabel, &mut Text)>,
) {
    if open.iter().next().is_none() {
        return;
    }
    let Some(player) = player else { return };
    let equipped = *player;
    for (slot_label, mut text) in &mut labels {
        let new_text = slot_label_text(slot_label.slot, equipped, &catalog);
        if text.0 != new_text {
            text.0 = new_text;
        }
    }
}

/// Close the modal on Esc. (Re-press of `i` is handled by the toggle
/// path in [`on_open_inventory`] via the bei observer.)
fn handle_close_keys(
    mut commands: Commands,
    mut mode: ResMut<InputMode>,
    keys: Res<ButtonInput<KeyCode>>,
    mut cursor_opts: Single<&mut CursorOptions, With<PrimaryWindow>>,
    root: Query<Entity, With<InventoryRoot>>,
) {
    if *mode != InputMode::Inventory {
        return;
    }
    if !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    for entity in &root {
        commands.entity(entity).despawn();
    }
    *mode = InputMode::Player;
    cursor_opts.grab_mode = CursorGrabMode::Locked;
    cursor_opts.visible = false;
}
