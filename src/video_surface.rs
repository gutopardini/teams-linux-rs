//! Persistent wgpu video texture. A new decoded frame updates one texture;
//! unchanged frames, resize and local mirroring never decode/resize on CPU.
use crate::AppMessage;
use iced::widget::{image, shader};
use iced::{Element, Rectangle, mouse, wgpu};
use std::collections::HashMap;

pub fn view(stream: u64, frame: image::Handle, mirror: bool) -> Element<'static, AppMessage> {
    shader::Shader::new(Surface {
        stream,
        frame,
        mirror,
    })
    .width(iced::Fill)
    .height(iced::Fill)
    .into()
}
struct Surface {
    stream: u64,
    frame: image::Handle,
    mirror: bool,
}
impl shader::Program<AppMessage> for Surface {
    type State = ();
    type Primitive = VideoPrimitive;
    fn draw(&self, _: &(), _: mouse::Cursor, _: Rectangle) -> VideoPrimitive {
        VideoPrimitive {
            stream: self.stream,
            frame: self.frame.clone(),
            mirror: self.mirror,
        }
    }
}
pub struct VideoPrimitive {
    stream: u64,
    frame: image::Handle,
    mirror: bool,
}
impl std::fmt::Debug for VideoPrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoPrimitive")
            .field("stream", &self.stream)
            .field("frame_id", &self.frame.id())
            .finish_non_exhaustive()
    }
}
struct GpuFrame {
    texture: wgpu::Texture,
    bind: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    dimensions: (u32, u32),
    last_frame: Option<iced::advanced::image::Id>,
    parameters: [f32; 4],
    used: bool,
}
pub struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    frames: HashMap<u64, GpuFrame>,
}
impl Drop for VideoPipeline {
    fn drop(&mut self) {
        if !self.frames.is_empty() {
            crate::live_trace::record("video_texture_released", 0, self.frames.len(), 0);
        }
    }
}
impl shader::Pipeline for VideoPipeline {
    fn new(device: &wgpu::Device, _: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("teams-video-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: std::num::NonZeroU64::new(16),
                    },
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("teams-video-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../assets/shaders/video.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("teams-video-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("teams-video-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("teams-video-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            pipeline,
            layout,
            sampler,
            frames: HashMap::new(),
        }
    }
    fn trim(&mut self) {
        let before = self.frames.len();
        self.frames.retain(|_, frame| {
            let used = frame.used;
            frame.used = false;
            used
        });
        if self.frames.len() < before {
            crate::live_trace::record("video_texture_released", 0, before - self.frames.len(), 0);
        }
    }
}
impl shader::Primitive for VideoPrimitive {
    type Pipeline = VideoPipeline;
    fn prepare(
        &self,
        pipeline: &mut VideoPipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        _: &shader::Viewport,
    ) {
        let image::Handle::Rgba {
            width,
            height,
            pixels,
            ..
        } = &self.frame
        else {
            return;
        };
        if !valid_frame(*width, *height, pixels.len())
            || bounds.width <= 0.0
            || bounds.height <= 0.0
        {
            return;
        }
        if pipeline
            .frames
            .get(&self.stream)
            .is_some_and(|frame| frame.dimensions != (*width, *height))
        {
            pipeline.frames.remove(&self.stream);
        }
        if !pipeline.frames.contains_key(&self.stream) {
            // Four visible surfaces bound GPU allocation. Multi-party subscription
            // selection is a separate feature; never grow a texture map forever.
            if pipeline.frames.len() >= 4 {
                if let Some(id) = pipeline
                    .frames
                    .iter()
                    .find(|(_, frame)| !frame.used)
                    .map(|(id, _)| *id)
                {
                    pipeline.frames.remove(&id);
                } else {
                    return;
                }
            }
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("teams-video-frame"),
                size: wgpu::Extent3d {
                    width: *width,
                    height: *height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let uniform = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("teams-video-fit"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("teams-video-bind"),
                layout: &pipeline.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&pipeline.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: uniform.as_entire_binding(),
                    },
                ],
            });
            pipeline.frames.insert(
                self.stream,
                GpuFrame {
                    texture,
                    bind,
                    uniform,
                    dimensions: (*width, *height),
                    last_frame: None,
                    parameters: [0.0; 4],
                    used: true,
                },
            );
            crate::live_trace::record(
                "video_texture_allocated",
                0,
                *width as usize,
                *height as usize,
            );
        }
        let frame = pipeline.frames.get_mut(&self.stream).unwrap();
        frame.used = true;
        if frame.last_frame != Some(self.frame.id()) {
            let started = std::time::Instant::now();
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &frame.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                pixels.as_ref(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(*width * 4),
                    rows_per_image: Some(*height),
                },
                wgpu::Extent3d {
                    width: *width,
                    height: *height,
                    depth_or_array_layers: 1,
                },
            );
            frame.last_frame = Some(self.frame.id());
            crate::live_trace::record(
                "video_gpu_upload",
                started.elapsed().as_micros() as u64,
                pixels.len(),
                0,
            );
        }
        let (x, y) = fit_scale(*width, *height, bounds.width, bounds.height);
        let parameters = [x, y, if self.mirror { 1.0 } else { 0.0 }, 0.0];
        if frame.parameters != parameters {
            let bytes: Vec<_> = parameters
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect();
            queue.write_buffer(&frame.uniform, 0, &bytes);
            frame.parameters = parameters;
        }
    }
    fn draw(&self, pipeline: &VideoPipeline, pass: &mut wgpu::RenderPass<'_>) -> bool {
        if let Some(frame) = pipeline.frames.get(&self.stream) {
            pass.set_pipeline(&pipeline.pipeline);
            pass.set_bind_group(0, &frame.bind, &[]);
            pass.draw(0..6, 0..1);
        }
        true
    }
}
fn valid_frame(width: u32, height: u32, bytes: usize) -> bool {
    width > 0
        && height > 0
        && width <= 1920
        && height <= 1080
        && bytes == width as usize * height as usize * 4
}
fn fit_scale(width: u32, height: u32, target_width: f32, target_height: f32) -> (f32, f32) {
    if width == 0 || height == 0 || target_width <= 0.0 || target_height <= 0.0 {
        return (0.0, 0.0);
    }
    let source = width as f32 / height as f32;
    let target = target_width / target_height;
    if source > target {
        (1.0, target / source)
    } else {
        (source / target, 1.0)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires a local GPU adapter; run explicitly for video validation"]
    fn gpu_frames_update_one_texture_mirror_correctly_and_release_on_trim() {
        use shader::{Pipeline as _, Primitive as _};
        let instance = wgpu::Instance::default();
        let adapter = iced::futures::executor::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            },
        ))
        .expect("GPU adapter");
        let info = adapter.get_info();
        eprintln!(
            "video GPU adapter={} backend={:?} type={:?}",
            info.name, info.backend, info.device_type
        );
        let (device, queue) = iced::futures::executor::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor::default()),
        )
        .unwrap();
        let mut pipeline = VideoPipeline::new(&device, &queue, wgpu::TextureFormat::Rgba8UnormSrgb);
        let size = wgpu::Extent3d {
            width: 64,
            height: 32,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("video-test-target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let render = |frame: &VideoPrimitive, pipeline: &mut VideoPipeline| {
            frame.prepare(
                pipeline,
                &device,
                &queue,
                &Rectangle::with_size(iced::Size::new(64.0, 32.0)),
                &shader::Viewport::with_physical_size(iced::Size::new(64, 32), 1.0),
            );
            let target_view = target.create_view(&Default::default());
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("video-test-readback"),
                size: 64 * 32 * 4,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = device.create_command_encoder(&Default::default());
            {
                let attachments = [Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })];
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &attachments,
                    ..Default::default()
                });
                assert!(frame.draw(pipeline, &mut pass));
            }
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &target,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256),
                        rows_per_image: Some(32),
                    },
                },
                size,
            );
            queue.submit([encoder.finish()]);
            let (sender, receiver) = std::sync::mpsc::channel();
            readback
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    let _ = sender.send(result);
                });
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: Some(std::time::Duration::from_secs(5)),
                })
                .unwrap();
            receiver
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let result = readback.slice(..).get_mapped_range().to_vec();
            readback.unmap();
            result
        };
        let first = VideoPrimitive {
            stream: 1,
            mirror: true,
            frame: image::Handle::from_rgba(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255]),
        };
        let pixels = render(&first, &mut pipeline);
        let pixel =
            |pixels: &[u8], x: usize| pixels[(16 * 64 + x) * 4..(16 * 64 + x) * 4 + 4].to_vec();
        assert_eq!(pixel(&pixels, 4), vec![0, 0, 255, 255]);
        assert_eq!(pixel(&pixels, 60), vec![255, 0, 0, 255]);
        let texture = pipeline.frames[&1].texture.clone();
        let second = VideoPrimitive {
            stream: 1,
            mirror: true,
            frame: image::Handle::from_rgba(2, 1, vec![0, 255, 0, 255, 0, 255, 0, 255]),
        };
        let pixels = render(&second, &mut pipeline);
        assert_eq!(pixel(&pixels, 32), vec![0, 255, 0, 255]);
        assert_eq!(texture, pipeline.frames[&1].texture);
        drop(texture);
        pipeline.trim();
        pipeline.trim();
        assert!(pipeline.frames.is_empty());
    }

    #[test]
    fn fit_preserves_camera_aspect_for_wide_and_portrait_windows() {
        assert_eq!(fit_scale(640, 480, 800.0, 300.0), (0.5, 1.0));
        assert_eq!(fit_scale(640, 480, 400.0, 600.0), (1.0, 0.5));
        assert_eq!(fit_scale(640, 480, 640.0, 480.0), (1.0, 1.0));
        assert_eq!(fit_scale(0, 480, 640.0, 480.0), (0.0, 0.0));
    }
    #[test]
    fn frame_limits_reject_truncated_and_oversized_allocations() {
        assert!(valid_frame(640, 480, 640 * 480 * 4));
        assert!(!valid_frame(640, 480, 640 * 480 * 4 - 1));
        assert!(!valid_frame(4000, 4000, 4000 * 4000 * 4));
    }
}
