//! Radial menu: hold F to open, sweep the cursor toward a slot, release to
//! select. Slot labels come from the [`SpellRegistry`] so the menu has no
//! spell-specific knowledge. Selection emits a [`SwitchSpell`] event; the
//! shared dispatcher in `spells::apply_switch_spell` flips the player's
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
use crate::spells::{ActiveSpell, EquippedSpells, SpellId, SpellRegistry, SwitchSpell};

const SEGMENT_COUNT: usize = 8;
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

pub fn on_open_radial(
    _: On<Start<OpenRadial>>,
    mut commands: Commands,
    mut mode: ResMut<InputMode>,
    mut window: Single<&mut Window, With<PrimaryWindow>>,
    mut cursor_opts: Single<&mut CursorOptions, With<PrimaryWindow>>,
    mut radial_cursor: ResMut<RadialCursor>,
    registry: Res<SpellRegistry>,
    catalog: Res<SpellCatalog>,
    player: Single<(&EquippedSpells, &ActiveSpell), (With<Player>, With<LocalPlayer>)>,
    existing: Query<Entity, With<RadialMenuRoot>>,
) {
    if !existing.is_empty() {
        return;
    }
    let (equipped, active_spell) = *player;

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

    for index in 0..SEGMENT_COUNT {
        let angle = segment_angle(index);
        let dx = angle.cos() * RING_RADIUS;
        let dy = -angle.sin() * RING_RADIUS;
        let label = equipped
            .0
            .get(index)
            .and_then(|slot| slot.as_ref())
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

    commands.entity(anchor).with_child((
        RadialCenterLabel,
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(-70.0),
            top: Val::Px(-24.0),
            width: Val::Px(140.0),
            height: Val::Px(48.0),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.6)),
        children![(
            Text::new(lookup_label(&catalog, &registry, &active_spell.0)),
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
) {
    if *mode != InputMode::RadialMenu {
        return;
    }

    let Some(cursor_pos) = window.cursor_position() else {
        return;
    };
    let center = Vec2::new(window.width(), window.height()) * 0.5;
    radial_cursor.offset = cursor_pos - center;

    let selected = pick_segment(radial_cursor.offset);
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
    player: Single<(Entity, &EquippedSpells), (With<Player>, With<LocalPlayer>)>,
    mut switch_writer: MessageWriter<SwitchSpell>,
    root: Query<Entity, With<RadialMenuRoot>>,
) {
    if *mode != InputMode::RadialMenu {
        return;
    }
    if !keys.just_released(KeyCode::KeyF) {
        return;
    }

    let (player_entity, equipped) = *player;
    if let Some(index) = radial_cursor.selected {
        if let Some(spell) = equipped.0.get(index).and_then(|s| s.as_ref()).cloned() {
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

fn segment_angle(index: usize) -> f32 {
    let step = std::f32::consts::TAU / SEGMENT_COUNT as f32;
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

fn pick_segment(offset: Vec2) -> Option<usize> {
    if offset.length_squared() < 25.0 {
        return None;
    }
    let angle = (-offset.y).atan2(offset.x);
    let step = std::f32::consts::TAU / SEGMENT_COUNT as f32;
    let normalized =
        (angle - std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU) % std::f32::consts::TAU;
    Some(((normalized / step).round() as usize) % SEGMENT_COUNT)
}
