//! Radial menu: hold F to open, sweep the cursor toward a slot, release to
//! select. Slots come from the **active weapon's spells** — switching the
//! active weapon (Tab) repopulates the wheel. Slot labels come from the
//! [`SpellCatalog`]/[`SpellRegistry`], so the menu has no spell-specific
//! knowledge. Selection emits a [`SwitchSpell`] event; the shared
//! dispatcher in `spells::apply_switch_spell` flips the player's
//! `*Active` marker components.

use bevy::{
    color::palettes::css,
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};
use bevy_enhanced_input::prelude::*;

use crate::input::{InputMode, OpenRadial};
use crate::player::{LocalPlayer, Player};
use crate::spells::catalog::SpellCatalog;
use crate::spells::{ActiveSpell, SpellId, SpellRegistry, SwitchSpell};
use crate::weapons::{ActiveWeaponSlot, EquippedWeapons, WeaponCatalog};

/// Maximum visual sectors. Weapons with fewer spells leave the rest of
/// the ring empty; weapons with more get truncated (no weapon has more
/// than three today).
const MAX_SEGMENTS: usize = 8;
const RING_RADIUS: f32 = 140.0;
const SEGMENT_SIZE: f32 = 88.0;

#[derive(Component)]
pub struct RadialMenuRoot;

#[derive(Component)]
pub struct RadialSegment {
    pub index: usize,
}

#[derive(Component)]
pub struct RadialCenterLabel;

#[derive(Resource, Default)]
pub struct RadialCursor {
    pub offset: Vec2,
    pub selected: Option<usize>,
}

/// Pulls the active weapon's spells out of the catalog. Used by every
/// system in this file so they all see the same wheel population.
fn active_spells(
    equipped: &EquippedWeapons,
    slot: &ActiveWeaponSlot,
    weapons: &WeaponCatalog,
) -> Vec<SpellId> {
    let Some(weapon_id) = equipped.0.get(slot.0 as usize).and_then(|s| s.as_ref()) else {
        return Vec::new();
    };
    let Some(def) = weapons.get(weapon_id) else {
        return Vec::new();
    };
    def.spells.iter().take(MAX_SEGMENTS).cloned().collect()
}

pub fn on_open_radial(
    _: On<Start<OpenRadial>>,
    mut commands: Commands,
    mut mode: ResMut<InputMode>,
    mut window: Single<&mut Window, With<PrimaryWindow>>,
    mut cursor_opts: Single<&mut CursorOptions, With<PrimaryWindow>>,
    mut radial_cursor: ResMut<RadialCursor>,
    registry: Res<SpellRegistry>,
    catalog: Res<SpellCatalog>,
    weapons: Res<WeaponCatalog>,
    player: Single<
        (&EquippedWeapons, &ActiveWeaponSlot, &ActiveSpell),
        (With<Player>, With<LocalPlayer>),
    >,
    existing: Query<Entity, With<RadialMenuRoot>>,
) {
    if !existing.is_empty() {
        return;
    }
    let (equipped, slot, active_spell) = *player;
    let spells = active_spells(equipped, slot, &weapons);
    let segment_count = spells.len().max(1);

    *mode = InputMode::RadialMenu;
    cursor_opts.grab_mode = CursorGrabMode::Confined;
    cursor_opts.visible = false;
    let center = Vec2::new(window.width(), window.height()) * 0.5;
    window.set_cursor_position(Some(center));
    radial_cursor.offset = Vec2::ZERO;
    radial_cursor.selected = None;

    let anchor = commands
        .spawn(Node {
            width: Val::Px(0.0),
            height: Val::Px(0.0),
            ..default()
        })
        .id();

    for index in 0..segment_count {
        let angle = segment_angle(index, segment_count);
        let dx = angle.cos() * RING_RADIUS;
        let dy = -angle.sin() * RING_RADIUS;
        let label = spells
            .get(index)
            .map(|id| lookup_label(&catalog, &registry, id))
            .unwrap_or_else(|| "—".to_string());

        commands.entity(anchor).with_child((
            RadialSegment { index },
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(dx - SEGMENT_SIZE * 0.5),
                top: Val::Px(dy - SEGMENT_SIZE * 0.5),
                width: Val::Px(SEGMENT_SIZE),
                height: Val::Px(SEGMENT_SIZE),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.1, 0.1, 0.15, 0.85)),
            children![(
                Text::new(label),
                TextFont {
                    font_size: 16.0,
                    ..default()
                },
                TextColor(Color::WHITE),
            )],
        ));
    }

    let weapon_label = equipped
        .0
        .get(slot.0 as usize)
        .and_then(|s| s.as_ref())
        .and_then(|id| weapons.get(id))
        .map(|d| d.label.clone())
        .unwrap_or_else(|| "—".to_string());

    let center_label =
        format!("{} · {}", weapon_label, lookup_label(&catalog, &registry, &active_spell.0));

    commands.entity(anchor).with_child((
        RadialCenterLabel,
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(-110.0),
            top: Val::Px(-24.0),
            width: Val::Px(220.0),
            height: Val::Px(48.0),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.6)),
        children![(
            Text::new(center_label),
            TextFont {
                font_size: 18.0,
                ..default()
            },
            TextColor(Color::WHITE),
        )],
    ));

    commands
        .spawn((
            RadialMenuRoot,
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
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.25)),
        ))
        .add_child(anchor);
}

pub fn track_cursor(
    mode: Res<InputMode>,
    window: Single<&Window, With<PrimaryWindow>>,
    mut radial_cursor: ResMut<RadialCursor>,
    mut segments: Query<(&RadialSegment, &mut BackgroundColor)>,
    player: Option<
        Single<(&EquippedWeapons, &ActiveWeaponSlot), (With<Player>, With<LocalPlayer>)>,
    >,
    weapons: Res<WeaponCatalog>,
) {
    if *mode != InputMode::RadialMenu {
        return;
    }
    let Some(player) = player else { return };
    let (equipped, slot) = *player;
    let segment_count = active_spells(equipped, slot, &weapons).len().max(1);

    let Some(cursor_pos) = window.cursor_position() else {
        return;
    };
    let center = Vec2::new(window.width(), window.height()) * 0.5;
    radial_cursor.offset = cursor_pos - center;

    let selected = pick_segment(radial_cursor.offset, segment_count);
    radial_cursor.selected = selected;

    for (segment, mut bg) in &mut segments {
        bg.0 = if Some(segment.index) == selected {
            Color::from(css::ORANGE).with_alpha(0.85)
        } else {
            Color::srgba(0.1, 0.1, 0.15, 0.85)
        };
    }
}

pub fn detect_release(
    mut mode: ResMut<InputMode>,
    keys: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
    mut cursor_opts: Single<&mut CursorOptions, With<PrimaryWindow>>,
    radial_cursor: Res<RadialCursor>,
    player: Single<
        (Entity, &EquippedWeapons, &ActiveWeaponSlot),
        (With<Player>, With<LocalPlayer>),
    >,
    weapons: Res<WeaponCatalog>,
    mut switch_writer: MessageWriter<SwitchSpell>,
    root: Query<Entity, With<RadialMenuRoot>>,
) {
    if *mode != InputMode::RadialMenu {
        return;
    }
    if !keys.just_released(KeyCode::KeyF) {
        return;
    }

    let (player_entity, equipped, slot) = *player;
    let spells = active_spells(equipped, slot, &weapons);
    if let Some(index) = radial_cursor.selected {
        if let Some(spell) = spells.get(index).cloned() {
            switch_writer.write(SwitchSpell {
                player: player_entity,
                spell,
            });
        }
    }

    for entity in &root {
        commands.entity(entity).despawn();
    }

    *mode = InputMode::Player;
    cursor_opts.grab_mode = CursorGrabMode::Locked;
    cursor_opts.visible = false;
}

fn segment_angle(index: usize, segment_count: usize) -> f32 {
    let step = std::f32::consts::TAU / segment_count.max(1) as f32;
    std::f32::consts::FRAC_PI_2 + step * index as f32
}

/// Resolve a spell's display label: prefer the data-driven catalog;
/// fall back to the legacy registry for spells not yet migrated to RON.
fn lookup_label(catalog: &SpellCatalog, registry: &SpellRegistry, id: &SpellId) -> String {
    if let Some(def) = catalog.get(id) {
        return def.label.clone();
    }
    registry.label(id).to_string()
}

fn pick_segment(offset: Vec2, segment_count: usize) -> Option<usize> {
    if offset.length_squared() < 25.0 {
        return None;
    }
    let angle = (-offset.y).atan2(offset.x);
    let step = std::f32::consts::TAU / segment_count.max(1) as f32;
    let normalized =
        (angle - std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU) % std::f32::consts::TAU;
    Some(((normalized / step).round() as usize) % segment_count)
}
