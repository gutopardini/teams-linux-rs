struct Params { scale: vec2<f32>, mirror: f32, padding: f32 };
@group(0) @binding(0) var frame: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;
@group(0) @binding(2) var<uniform> params: Params;
struct Vertex { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> Vertex {
    let positions = array<vec2<f32>,6>(vec2(-1.0,-1.0),vec2(1.0,-1.0),vec2(-1.0,1.0),vec2(-1.0,1.0),vec2(1.0,-1.0),vec2(1.0,1.0));
    let p = positions[index];
    var output: Vertex;
    output.position = vec4(p * params.scale,0.0,1.0);
    let u = (p.x + 1.0) * 0.5;
    output.uv = vec2(select(u,1.0-u,params.mirror > 0.5),(1.0-p.y)*0.5);
    return output;
}
@fragment fn fs_main(input: Vertex) -> @location(0) vec4<f32> {
    return textureSample(frame,frame_sampler,input.uv);
}
