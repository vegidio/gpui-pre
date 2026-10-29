use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui_util::ResultExt;
use windows::{
    Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_EX_TOOLWINDOW, WS_POPUP},
    core::w,
};

use crate::*;
use gpui::*;

/// Renders scenes offscreen with GPUI's Direct3D 11 renderer, for tests that check pixels, such as through
/// `HeadlessAppContext::with_platform` and `Window::render_to_image`.
///
/// Frames are drawn into the renderer's own target and read back, never presented. The swap chain needs a window,
/// so it owns one that is never shown.
pub struct DirectXHeadlessRenderer {
    renderer: DirectXRenderer,
    // Dropped after the renderer, whose swap chain refers to it.
    _window: HiddenWindow,
}

struct HiddenWindow(HWND);

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        // SAFETY: the window was created by `DirectXHeadlessRenderer::new` and is destroyed once, here.
        unsafe { DestroyWindow(self.0) }.log_err();
    }
}

impl DirectXHeadlessRenderer {
    /// Creates the renderer on the adapter GPUI picks for windows.
    pub fn new() -> Result<Self> {
        let devices = DirectXDevices::new().context("Creating DirectX devices")?;
        // SAFETY: the system's `STATIC` class needs no registration, and the window has no parent or creation data.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                None,
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .context("Creating the headless renderer's window")?;
        let window = HiddenWindow(hwnd);
        let renderer = DirectXRenderer::new(hwnd, &devices, true)?;
        Ok(Self {
            renderer,
            _window: window,
        })
    }
}

impl PlatformHeadlessRenderer for DirectXHeadlessRenderer {
    fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> Result<image::RgbaImage> {
        self.renderer.resize(size)?;
        self.renderer
            .render_to_image(scene, WindowBackgroundAppearance::Opaque)
    }

    fn render_scene(&mut self, scene: &Scene, size: Size<DevicePixels>) -> Result<()> {
        self.renderer.resize(size)?;
        self.renderer
            .render(scene, WindowBackgroundAppearance::Opaque)
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.renderer.sprite_atlas()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // GPUI's glob brings in its own `test` macro; these are plain tests.
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::prelude::v1::test;
    use windows::{
        Win32::{
            Foundation::{GENERIC_ALL, HMODULE},
            Graphics::{
                Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
                Direct3D11::*,
                Dxgi::{Common::*, IDXGIResource1},
            },
            System::Threading::CreateEventW,
        },
        core::{Interface, PCWSTR},
    };

    /// The window and the surface's bounds in it, in device pixels.
    const WINDOW: i32 = 96;
    const SURFACE: Bounds<ScaledPixels> = Bounds {
        origin: Point {
            x: ScaledPixels(16.),
            y: ScaledPixels(16.),
        },
        size: Size {
            width: ScaledPixels(64.),
            height: ScaledPixels(64.),
        },
    };
    /// The texture's quadrants: top left, top right, bottom left, bottom right, as RGB.
    const QUADRANTS: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
    /// What GPUI clears an opaque window to.
    const BACKGROUND: [u8; 3] = [255, 255, 255];

    /// A 32x32 BGRA texture of [`QUADRANTS`], made on a device of its own on GPUI's adapter, as a video producer
    /// would, and finished on the GPU. Returns the device too: the texture lives as long as either is held.
    fn quadrant_texture() -> (ID3D11Device, OwnedHandle) {
        let adapter = DirectXDevices::new().unwrap().adapter;
        let (mut device, mut context) = (None, None);
        // SAFETY: the out-pointers are valid; the adapter is used with the driver type it requires.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .unwrap();
        }
        let (device, context) = (device.unwrap(), context.unwrap());

        const SIZE: usize = 32;
        let mut pixels = vec![0u8; SIZE * SIZE * 4];
        for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            let (x, y) = (index % SIZE, index / SIZE);
            let [r, g, b] = QUADRANTS[usize::from(y >= SIZE / 2) * 2 + usize::from(x >= SIZE / 2)];
            pixel.copy_from_slice(&[b, g, r, 255]);
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: SIZE as u32,
            Height: SIZE as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                as u32,
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: (SIZE * 4) as u32,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        let mut query = None;
        // SAFETY: the description and initial data are complete and outlive the calls; the out-pointers are valid,
        // and the texture was created shareable through NT handles.
        let handle = unsafe {
            device
                .CreateTexture2D(&desc, Some(&data), Some(&mut texture))
                .unwrap();
            let resource: IDXGIResource1 = texture.unwrap().cast().unwrap();
            let handle = resource
                .CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())
                .unwrap();

            // Finished before it is painted, as `SharedTexture` requires.
            let desc = D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            };
            device.CreateQuery(&desc, Some(&mut query)).unwrap();
            let query = query.unwrap();
            context.End(&query);
            context.Flush();
            let mut done = windows::core::BOOL(0);
            while context
                .GetData(
                    &query,
                    Some((&raw mut done).cast()),
                    size_of_val(&done) as u32,
                    0,
                )
                .is_err()
                || !done.as_bool()
            {
                std::thread::yield_now();
            }
            OwnedHandle::from_raw_handle(handle.0)
        };
        (device, handle)
    }

    /// The whole window as its content mask.
    fn window_mask() -> ContentMask<ScaledPixels> {
        ContentMask {
            bounds: Bounds {
                origin: Point::default(),
                size: size(ScaledPixels(WINDOW as f32), ScaledPixels(WINDOW as f32)),
            },
        }
    }

    fn surface(texture: SharedTexture, content_mask: ContentMask<ScaledPixels>) -> PaintSurface {
        PaintSurface {
            order: 0,
            bounds: SURFACE,
            content_mask,
            shared_texture: texture,
        }
    }

    fn render(
        renderer: &mut DirectXHeadlessRenderer,
        primitives: Vec<Primitive>,
    ) -> image::RgbaImage {
        let mut scene = Scene::default();
        for primitive in primitives {
            scene.insert_primitive(primitive);
        }
        scene.finish();
        renderer
            .render_scene_to_image(&scene, size(DevicePixels(WINDOW), DevicePixels(WINDOW)))
            .unwrap()
    }

    fn rgb(image: &image::RgbaImage, x: u32, y: u32) -> [u8; 3] {
        let [r, g, b, _] = image.get_pixel(x, y).0;
        [r, g, b]
    }

    /// The centres of the surface's quadrants, in window pixels: top left, top right, bottom left, bottom right.
    const CENTRES: [(u32, u32); 4] = [(32, 32), (64, 32), (32, 64), (64, 64)];

    #[test]
    fn draws_the_texture_in_its_bounds() {
        let mut renderer = DirectXHeadlessRenderer::new().unwrap();
        let (_device, handle) = quadrant_texture();
        let image = render(
            &mut renderer,
            vec![surface(SharedTexture::new(handle), window_mask()).into()],
        );

        for (&(x, y), &colour) in CENTRES.iter().zip(&QUADRANTS) {
            assert_eq!(rgb(&image, x, y), colour, "quadrant at ({x}, {y})");
        }
        // Stretched over exactly its bounds, upright: the corners are their quadrants' colours.
        assert_eq!(rgb(&image, 16, 16), QUADRANTS[0]);
        assert_eq!(rgb(&image, 79, 79), QUADRANTS[3]);
        for (x, y) in [(15, 48), (80, 48), (48, 15), (48, 80), (0, 0), (95, 95)] {
            assert_eq!(
                rgb(&image, x, y),
                BACKGROUND,
                "outside the bounds at ({x}, {y})"
            );
        }
    }

    #[test]
    fn clipped_by_the_content_mask() {
        let mut renderer = DirectXHeadlessRenderer::new().unwrap();
        let (_device, handle) = quadrant_texture();
        // Everything left of x = 40, inside the left quadrants, so the edge does not fall where filtering blends them.
        let mask = ContentMask {
            bounds: Bounds {
                origin: point(ScaledPixels(0.), ScaledPixels(0.)),
                size: size(ScaledPixels(40.), ScaledPixels(WINDOW as f32)),
            },
        };
        let image = render(
            &mut renderer,
            vec![surface(SharedTexture::new(handle), mask).into()],
        );

        assert_eq!(rgb(&image, 32, 32), QUADRANTS[0]);
        assert_eq!(rgb(&image, 39, 64), QUADRANTS[2]);
        for (x, y) in [(40, 64), (64, 32), (64, 64)] {
            assert_eq!(rgb(&image, x, y), BACKGROUND, "clipped at ({x}, {y})");
        }
    }

    #[test]
    fn unopenable_surface_is_skipped_and_the_rest_draws() {
        let mut renderer = DirectXHeadlessRenderer::new().unwrap();
        // A handle, but not of a texture.
        // SAFETY: no security attributes or name; the returned handle is new, and owned from here on.
        let event = unsafe {
            OwnedHandle::from_raw_handle(CreateEventW(None, true, false, None).unwrap().0)
        };
        let magenta = Rgba {
            r: 1.,
            g: 0.,
            b: 1.,
            a: 1.,
        };
        // Painted after the surface, over its lower half.
        let quad = Quad {
            order: 0,
            border_style: BorderStyle::default(),
            bounds: Bounds {
                origin: point(ScaledPixels(16.), ScaledPixels(48.)),
                size: size(ScaledPixels(64.), ScaledPixels(32.)),
            },
            content_mask: window_mask(),
            background: magenta.into(),
            border_color: Hsla::default(),
            corner_radii: Corners::default(),
            border_widths: Edges::default(),
        };
        let texture = SharedTexture::new(event);

        for _ in 0..3 {
            let image = render(
                &mut renderer,
                vec![surface(texture.clone(), window_mask()).into(), quad.into()],
            );
            assert_eq!(rgb(&image, 32, 32), BACKGROUND, "the surface is not drawn");
            assert_eq!(rgb(&image, 32, 64), [255, 0, 255], "the quad after it is");
            assert!(
                renderer.renderer.surface_warned,
                "the failure was warned about"
            );
        }

        // A texture that opens re-arms the warning.
        let (_device, handle) = quadrant_texture();
        render(
            &mut renderer,
            vec![surface(SharedTexture::new(handle), window_mask()).into()],
        );
        assert!(!renderer.renderer.surface_warned);
    }

    #[test]
    fn repeated_paints_through_new_duplicates() {
        let mut renderer = DirectXHeadlessRenderer::new().unwrap();
        let (_device, handle) = quadrant_texture();
        use std::os::windows::io::AsHandle;

        for frame in 0..10 {
            let texture = SharedTexture::duplicate(handle.as_handle()).unwrap();
            let image = render(&mut renderer, vec![surface(texture, window_mask()).into()]);
            for (&(x, y), &colour) in CENTRES.iter().zip(&QUADRANTS) {
                assert_eq!(
                    rgb(&image, x, y),
                    colour,
                    "frame {frame}, quadrant at ({x}, {y})"
                );
            }
        }
    }
}
