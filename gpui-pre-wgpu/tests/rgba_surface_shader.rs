//! The RGBA surface shader with a texture made by wgpu, on every adapter wgpu finds: a LINEAR dma-buf is imported as
//! wide as its rows, so the shader must sample only the buffer's own width. Steer the adapters with `VK_DRIVER_FILES`
//! (`/dev/null` hides every Vulkan driver).
#![cfg(target_os = "linux")]

mod common;

use common::{HEIGHT, TEXTURE_WIDTH};

/// One test, so one thread uses each adapter: a GL context stays bound to the thread that made it current.
#[test]
fn draws_the_buffer_width_without_the_padding_clipped_to_the_content_mask() {
    common::on_every_adapter(|name, _, device, queue| {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: TEXTURE_WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            texture.as_image_copy(),
            &common::picture(TEXTURE_WIDTH),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(TEXTURE_WIDTH * 4),
                rows_per_image: None,
            },
            texture.size(),
        );
        common::check(
            name,
            device,
            queue,
            &texture.create_view(&Default::default()),
            TEXTURE_WIDTH,
        );
    });
}
