// Portal disc shader.
//
// The texture is rendered by a separate camera positioned at the paired
// portal's vantage. We sample it using the fragment's *screen-space* UV
// (in.position.xy / viewport size), so the disc behaves like a true window
// into the other place rather than a circular crop of the texture.
//
// A faint rim, driven by the local mesh UV's distance from the cap center,
// keeps the disc edge readable as a portal.

#import bevy_pbr::mesh_view_bindings::view
#import bevy_pbr::forward_io::VertexOutput

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> rim_color: vec4<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var portal_tex: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var portal_sampler: sampler;

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // Screen-space UV in the main render pass.
    let uv = in.position.xy / view.viewport.zw;
    let sampled = textureSample(portal_tex, portal_sampler, uv);

    // Cap UVs come in centered on (0.5, 0.5) with a unit-square mapping;
    // distance from center scales to [0, 1] at the disc rim.
    let r = length(in.uv - vec2<f32>(0.5, 0.5)) * 2.0;
    let rim = smoothstep(0.92, 1.0, r);

    return mix(sampled, rim_color, rim);
}
