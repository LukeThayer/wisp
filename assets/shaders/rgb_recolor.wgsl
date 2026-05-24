// RGB-mask recolor extension for StandardMaterial.
//
// The asset pack ships every character with a single texture
// (`genericRGB_medievalTexture`) where the red, green, and blue
// channels of each texel are interpreted as **masks** for three
// independently-tintable regions. The asset creator's reference
// Blender shader graph (`RGB_Recoloring_Blender.jpg`) composes the
// final albedo as a weighted sum:
//
//     albedo = sample.r * tint_r
//            + sample.g * tint_g
//            + sample.b * tint_b
//
// This file is an *extension* in Bevy's `ExtendedMaterial` sense:
// we let the upstream PBR pipeline build a full `PbrInput` (so
// shadows, lighting, fog, tone-mapping all behave normally) and
// only override `pbr_input.material.base_color` before the lighting
// pass runs. The base `StandardMaterial` keeps the mask texture
// out of its own `baseColorTexture` slot (we strip it during the
// swap-in step in Rust) — the mask lives here, in the extension.

#import bevy_pbr::{
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, apply_pbr_lighting, main_pass_post_lighting_processing},
    forward_io::{VertexOutput, FragmentOutput},
}

@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<uniform> tint_r: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var<uniform> tint_g: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(102) var<uniform> tint_b: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(103) var mask_tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(104) var mask_sampler: sampler;

@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    // Sample the RGB-mask texture: rgb encodes the per-region colors,
    // alpha encodes per-pixel opacity for decal meshes (eyebrows,
    // mouth, eyes — whose quads only want the brow / lip / iris
    // shape rendered, not the surrounding rectangle).
    let m = textureSample(mask_tex, mask_sampler, in.uv);
    let albedo = m.r * tint_r.rgb + m.g * tint_g.rgb + m.b * tint_b.rgb;

    // Let StandardMaterial assemble the PbrInput, then override base
    // color. Pass the texture's alpha through; `alpha_discard` looks
    // at the base material's `alpha_mode` and `alpha_cutoff` and
    // discards low-alpha pixels when the mode is `Mask`. Body meshes
    // keep `alpha_mode = Opaque` and ignore alpha; face-feature
    // meshes get `Mask` set by the recolor swap and get their
    // surrounding transparent pixels clipped.
    var pbr_input = pbr_input_from_standard_material(in, is_front);
    pbr_input.material.base_color = vec4<f32>(albedo, m.a);
    pbr_input.material.base_color = alpha_discard(pbr_input.material, pbr_input.material.base_color);

    var out: FragmentOutput;
    out.color = apply_pbr_lighting(pbr_input);
    out.color = main_pass_post_lighting_processing(pbr_input, out.color);
    return out;
}
