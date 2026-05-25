//! Two-slot weapon panel sitting next to the health bar. Shows both
//! equipped weapons; the active one is highlighted (bright text + tint
//! bar), the stowed one is dimmed. Drives off the local Player rig's
//! [`EquippedWeapons`] + [`ActiveWeaponSlot`] and the
//! [`WeaponCatalog`] for human labels.

use bevy::prelude::*;

use crate::player::{LocalPlayer, Player};
use crate::spells::catalog::SpellCatalog;
use crate::spells::{ActiveSpell, SpellRegistry};
use crate::ui::health::{H as HEALTH_H, MARGIN as HEALTH_MARGIN, W as HEALTH_W};
use crate::ui::hud::HudRoot;
use crate::weapons::{ActiveWeaponSlot, EquippedWeapons, WeaponCatalog};

pub struct WeaponsHudPlugin;

impl Plugin for WeaponsHudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, spawn_weapons_hud)
            .add_systems(Update, update_weapons_hud);
    }
}

#[derive(Component)]
struct WeaponsHudRoot;

#[derive(Component)]
struct WeaponSlotPanel {
    slot: u8,
}

#[derive(Component)]
struct WeaponSlotLabel {
    slot: u8,
}

#[derive(Component)]
struct ActiveSpellPanel;

#[derive(Component)]
struct ActiveSpellLabel;

const SLOT_W: f32 = 140.0;
const SLOT_GAP: f32 = 8.0;
const PANEL_OFFSET: f32 = 12.0;
const SPELL_PANEL_W: f32 = 200.0;

fn spawn_weapons_hud(
    mut commands: Commands,
    hud: Option<Single<Entity, With<HudRoot>>>,
) {
    let Some(hud) = hud else { return };
    let left = HEALTH_MARGIN + HEALTH_W + PANEL_OFFSET;
    commands.entity(*hud).with_children(|root| {
        root.spawn((
            WeaponsHudRoot,
            Name::new("WeaponsHud"),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(left),
                bottom: Val::Px(HEALTH_MARGIN),
                height: Val::Px(HEALTH_H),
                flex_direction: FlexDirection::Row,
                column_gap: Val::Px(SLOT_GAP),
                align_items: AlignItems::Stretch,
                display: Display::None,
                ..default()
            },
        ))
        .with_children(|row| {
            for slot in 0u8..2 {
                row.spawn((
                    WeaponSlotPanel { slot },
                    Node {
                        width: Val::Px(SLOT_W),
                        padding: UiRect::axes(Val::Px(10.0), Val::Px(2.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.08, 0.08, 0.08, 0.85)),
                ))
                .with_children(|panel| {
                    panel.spawn((
                        WeaponSlotLabel { slot },
                        Text::new("—"),
                        TextFont { font_size: 14.0, ..default() },
                        TextColor(Color::srgba(0.7, 0.7, 0.75, 1.0)),
                    ));
                });
            }

            // Active spell label — sits to the right of the two slot
            // panels, slightly wider since spell names tend to run long.
            row.spawn((
                ActiveSpellPanel,
                Node {
                    width: Val::Px(SPELL_PANEL_W),
                    padding: UiRect::axes(Val::Px(10.0), Val::Px(2.0)),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
                BackgroundColor(Color::srgba(0.08, 0.08, 0.08, 0.85)),
            ))
            .with_children(|panel| {
                panel.spawn((
                    ActiveSpellLabel,
                    Text::new("—"),
                    TextFont { font_size: 14.0, ..default() },
                    TextColor(Color::srgba(1.0, 0.92, 0.7, 1.0)),
                ));
            });
        });
    });
}

fn update_weapons_hud(
    catalog: Res<WeaponCatalog>,
    spells: Res<SpellCatalog>,
    registry: Res<SpellRegistry>,
    player: Option<
        Single<
            (&EquippedWeapons, &ActiveWeaponSlot, &ActiveSpell),
            (With<Player>, With<LocalPlayer>),
        >,
    >,
    mut root: Query<&mut Node, With<WeaponsHudRoot>>,
    mut panels: Query<
        (&WeaponSlotPanel, &mut BackgroundColor),
        (Without<WeaponSlotLabel>, Without<ActiveSpellLabel>),
    >,
    mut labels: Query<
        (&WeaponSlotLabel, &mut Text, &mut TextColor),
        Without<ActiveSpellLabel>,
    >,
    mut spell_labels: Query<&mut Text, With<ActiveSpellLabel>>,
) {
    let Some(player) = player else {
        for mut node in &mut root {
            node.display = Display::None;
        }
        return;
    };
    let (equipped, active, active_spell) = *player;
    for mut node in &mut root {
        node.display = Display::Flex;
    }

    for (panel, mut bg) in &mut panels {
        let is_active = panel.slot == active.0;
        bg.0 = if is_active {
            // Saturated warm border-tone so the active slot reads as "in
            // hand"; matches the lantern/lens HUD palette.
            Color::srgba(0.32, 0.22, 0.08, 0.95)
        } else {
            Color::srgba(0.08, 0.08, 0.08, 0.85)
        };
    }

    for (slot_label, mut text, mut color) in &mut labels {
        let weapon_id = equipped
            .0
            .get(slot_label.slot as usize)
            .and_then(|s| s.as_ref());
        let new_text = weapon_id
            .and_then(|id| catalog.get(id))
            .map(|d| d.label.clone())
            .unwrap_or_else(|| "—".to_string());
        if text.0 != new_text {
            text.0 = new_text;
        }
        let is_active = slot_label.slot == active.0;
        color.0 = if is_active {
            Color::srgba(1.0, 1.0, 1.0, 1.0)
        } else {
            Color::srgba(0.65, 0.65, 0.7, 1.0)
        };
    }

    // Active spell label: pulls from SpellCatalog (with the legacy
    // SpellRegistry as a fallback for any not-yet-migrated names).
    let spell_text = spells
        .get(&active_spell.0)
        .map(|s| s.label.clone())
        .unwrap_or_else(|| registry.label(&active_spell.0).to_string());
    for mut text in &mut spell_labels {
        if text.0 != spell_text {
            text.0 = spell_text.clone();
        }
    }
}
