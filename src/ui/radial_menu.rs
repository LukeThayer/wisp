use bevy::{
    color::palettes::css,
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};
use bevy_enhanced_input::prelude::*;

use crate::input::{InputMode, OpenRadial};
use crate::player::spells::{ActiveSpell, EquippedSpells};

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

/// Mouse position relative to the screen center, accumulated while the menu is open.
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
    equipped: Single<&EquippedSpells>,
    active_spell: Res<ActiveSpell>,
    existing: Query<Entity, With<RadialMenuRoot>>,
) {
    if !existing.is_empty() {
        return;
    }

    *mode = InputMode::RadialMenu;
    // Keep the cursor hidden but free to move so we can read its position
    // for segment selection.
    cursor_opts.grab_mode = CursorGrabMode::Confined;
    cursor_opts.visible = false;
    // Re-center the cursor when the menu opens so the player starts neutral.
    let center = Vec2::new(window.width(), window.height()) * 0.5;
    window.set_cursor_position(Some(center));
    radial_cursor.offset = Vec2::ZERO;
    radial_cursor.selected = None;

    // Root: full-screen flex container that centers a 0×0 anchor at the
    // middle of the viewport. Segments are spawned as absolutely positioned
    // children of the anchor, so their `left`/`top` are offsets from the
    // screen center.
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
        // Screen y grows downward; the ring angle is in math (y-up) space.
        let dy = -angle.sin() * RING_RADIUS;
        let spell = equipped.0[index];

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
                Text::new(spell.label()),
                TextFont {
                    font_size: 16.0,
                    ..default()
                },
                TextColor(Color::WHITE),
            )],
        ));
    }

    // Center label sits at the anchor's origin (= screen center).
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
            Text::new(active_spell.0.label()),
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
    mode: ResMut<InputMode>,
    keys: Res<ButtonInput<KeyCode>>,
    commands: Commands,
    cursor_opts: Single<&mut CursorOptions, With<PrimaryWindow>>,
    radial_cursor: Res<RadialCursor>,
    active_spell: ResMut<ActiveSpell>,
    equipped: Single<&EquippedSpells>,
    root: Query<Entity, With<RadialMenuRoot>>,
) {
    if *mode != InputMode::RadialMenu {
        return;
    }
    if !keys.just_released(KeyCode::KeyF) {
        return;
    }
    close_menu(mode, commands, cursor_opts, radial_cursor, active_spell, equipped, root);
}

fn close_menu(
    mut mode: ResMut<InputMode>,
    mut commands: Commands,
    mut cursor_opts: Single<&mut CursorOptions, With<PrimaryWindow>>,
    radial_cursor: Res<RadialCursor>,
    mut active_spell: ResMut<ActiveSpell>,
    equipped: Single<&EquippedSpells>,
    root: Query<Entity, With<RadialMenuRoot>>,
) {
    if let Some(index) = radial_cursor.selected {
        active_spell.0 = equipped.0[index];
        info!("active spell = {}", active_spell.0.label());
    }

    for entity in &root {
        commands.entity(entity).despawn();
    }

    *mode = InputMode::Player;
    cursor_opts.grab_mode = CursorGrabMode::Locked;
    cursor_opts.visible = false;
}

fn segment_angle(index: usize) -> f32 {
    // 0 = right, going counterclockwise (math convention).
    // Slot 1 sits at the top; rotate by +90° so index 0 is up.
    let step = std::f32::consts::TAU / SEGMENT_COUNT as f32;
    std::f32::consts::FRAC_PI_2 + step * index as f32
}

fn pick_segment(offset: Vec2) -> Option<usize> {
    if offset.length_squared() < 25.0 {
        return None;
    }
    // Convert screen-space (y-down) angle to math-space (y-up).
    let angle = (-offset.y).atan2(offset.x);
    let step = std::f32::consts::TAU / SEGMENT_COUNT as f32;
    let normalized = (angle - std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU) % std::f32::consts::TAU;
    Some(((normalized / step).round() as usize) % SEGMENT_COUNT)
}
