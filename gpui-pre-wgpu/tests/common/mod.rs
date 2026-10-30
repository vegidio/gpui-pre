//! What the surface tests share: a picture with padding, drawn with the RGBA surface shader (`vs_rgba_surface`,
//! `fs_rgba_surface`) and checked, on every adapter wgpu finds.
#![allow(dead_code)]

use std::num::NonZeroU64;
use std::sync::LazyLock;

/// The shader source the renderer compiles on native backends.
const SHADERS: &str = concat!(
    include_str!("../../src/shaders.wgsl"),
    include_str!("../../src/shaders_storage.wgsl")
);

/// The buffer's own size, and the padded width of its rows.
pub const WIDTH: u32 = 48;
pub const HEIGHT: u32 = 32;
pub const TEXTURE_WIDTH: u32 = 64;
/// The target, twice the buffer's size, so the texture is magnified with linear filtering.
pub const TARGET: (u32, u32) = (96, 64);

pub const RED: [u8; 4] = [255, 0, 0, 255];
pub const GREEN: [u8; 4] = [0, 255, 0, 255];
pub const BLUE: [u8; 4] = [0, 0, 255, 255];
pub const WHITE: [u8; 4] = [255, 255, 255, 255];
/// The padding columns, which must never show.
pub const MAGENTA: [u8; 4] = [255, 0, 255, 255];

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    viewport_size: [f32; 2],
    premultiplied_alpha: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    bounds: [f32; 4],
    content_mask: [f32; 4],
    texture_scale: [f32; 2],
    texture_max: [f32; 2],
}

/// Kept for the whole run: dropping a GL instance unloads libEGL, and Mesa's thread-local destructors then crash the
/// test thread as it exits.
pub static INSTANCE: LazyLock<wgpu::Instance> = LazyLock::new(|| {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
        flags: wgpu::InstanceFlags::default(),
        backend_options: wgpu::BackendOptions::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        display: None,
    })
});

pub fn adapters() -> Vec<wgpu::Adapter> {
    gpui::block_on(INSTANCE.enumerate_adapters(wgpu::Backends::all()))
}

/// The picture, rows `row_width` pixels wide: quadrants red, green, blue and white in the first `WIDTH` columns,
/// magenta padding after them.
pub fn picture(row_width: u32) -> Vec<u8> {
    (0..row_width * HEIGHT)
        .flat_map(|index| {
            let (x, y) = (index % row_width, index / row_width);
            if x >= WIDTH {
                MAGENTA
            } else {
                [[RED, GREEN], [BLUE, WHITE]][usize::from(y >= HEIGHT / 2)]
                    [usize::from(x >= WIDTH / 2)]
            }
        })
        .collect()
}

/// Draws the picture in `view`, `texture_width` texels wide, as the renderer draws a buffer `WIDTH` pixels wide over the whole target, clipped to `content_mask`, and reads the target back as RGBA rows.
pub fn draw(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &wgpu::TextureView,
    texture_width: u32,
    content_mask: [f32; 4],
) -> Vec<u8> {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(SHADERS.into()),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("rgba_surfaces"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_rgba_surface"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_rgba_surface"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8Unorm,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    });

    // The renderer's atlas sampler: linear, clamped to the edge.
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    let (width, height) = (TARGET.0 as f32, TARGET.1 as f32);
    let uniform = |label, bytes: &[u8]| {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&buffer, 0, bytes);
        buffer
    };
    let globals = uniform(
        "globals",
        bytemuck::bytes_of(&Globals {
            viewport_size: [width, height],
            premultiplied_alpha: 0,
            pad: 0,
        }),
    );
    let params = uniform(
        "params",
        bytemuck::bytes_of(&Params {
            bounds: [0.0, 0.0, width, height],
            content_mask,
            texture_scale: [WIDTH as f32 / texture_width as f32, 1.0],
            texture_max: [
                (WIDTH as f32 - 0.5) / texture_width as f32,
                (HEIGHT as f32 - 0.5) / HEIGHT as f32,
            ],
        }),
    );
    let bind_group = |index: u32, entries: &[wgpu::BindGroupEntry<'_>]| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(index),
            entries,
        })
    };
    let globals = bind_group(
        0,
        &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer(&globals),
        }],
    );
    let params = bind_group(
        1,
        &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer(&params),
        }],
    );
    let textures = bind_group(
        2,
        &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    );

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: TARGET.0,
            height: TARGET.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let bytes_per_row = (TARGET.0 * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(bytes_per_row * TARGET.1),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let target_view = target.create_view(&Default::default());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &globals, &[]);
        pass.set_bind_group(1, &params, &[]);
        pass.set_bind_group(2, &textures, &[]);
        pass.draw(0..4, 0..1);
    }
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: None,
            },
        },
        target.size(),
    );
    queue.submit([encoder.finish()]);
    readback
        .slice(..)
        .map_async(wgpu::MapMode::Read, |result| result.unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let mapped = readback.slice(..).get_mapped_range().unwrap();
    (0..TARGET.1)
        .flat_map(|y| {
            let start = (y * bytes_per_row) as usize;
            mapped[start..start + (TARGET.0 * 4) as usize].to_vec()
        })
        .collect()
}

pub fn buffer(buffer: &wgpu::Buffer) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::Buffer(wgpu::BufferBinding {
        buffer,
        offset: 0,
        size: NonZeroU64::new(buffer.size()),
    })
}

pub fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let at = ((y * TARGET.0 + x) * 4) as usize;
    pixels[at..at + 4].try_into().unwrap()
}

/// Runs `check` on a device of each adapter, naming the adapter in its failures.
pub fn on_every_adapter(check: impl Fn(&str, &wgpu::Adapter, &wgpu::Device, &wgpu::Queue)) {
    let adapters = adapters();
    assert!(!adapters.is_empty(), "no adapter");
    for adapter in adapters {
        let info = adapter.get_info();
        let name = format!("{} ({:?})", info.name, info.backend);
        let (device, queue) = gpui::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            // As GPUI's renderer asks for it.
            required_features: adapter.features() & wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF,
            ..Default::default()
        }))
        .unwrap();
        eprintln!("checking on {name}");
        check(&name, &adapter, &device, &queue);
    }
}

/// Checks a drawing of the picture: its quadrants, the padding never shown, and clipping to the content mask.
pub fn check(
    name: &str,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &wgpu::TextureView,
    texture_width: u32,
) {
    let pixels = draw(
        device,
        queue,
        view,
        texture_width,
        [0.0, 0.0, TARGET.0 as f32, TARGET.1 as f32],
    );
    for ((x, y), colour) in [
        ((24, 16), RED),
        ((72, 16), GREEN),
        ((24, 48), BLUE),
        ((72, 48), WHITE),
    ] {
        assert_eq!(pixel(&pixels, x, y), colour, "{name}: ({x}, {y})");
    }
    // The last column shows the buffer's last column, green over white (blended where they meet), never the magenta
    // padding next to it, which would take green away.
    for y in 0..TARGET.1 {
        let [red, green, blue, alpha] = pixel(&pixels, TARGET.0 - 1, y);
        assert_eq!((green, alpha), (255, 255), "{name}: right edge at row {y}");
        assert_eq!(red, blue, "{name}: right edge at row {y}");
    }

    let pixels = draw(
        device,
        queue,
        view,
        texture_width,
        [0.0, 0.0, TARGET.0 as f32 / 2.0, TARGET.1 as f32],
    );
    assert_eq!(pixel(&pixels, 24, 16), RED, "{name}: inside the mask");
    assert_eq!(
        pixel(&pixels, 72, 16),
        [0, 0, 0, 255],
        "{name}: outside the mask"
    );
}
