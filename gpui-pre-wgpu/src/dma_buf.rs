//! Linux: importing the dma-bufs painted with `Window::paint_surface` on GPUI's own device, with no copy.
//!
//! What the device can import is found once per device ([`probe`]), published through
//! [`gpui::set_dma_buf_import`], and each buffer is then imported along one of three paths ([`import`]):
//!
//! - **Vulkan with `VK_EXT_image_drm_format_modifier`**: wgpu-hal's `texture_from_dmabuf_fd`, for any modifier the
//!   driver lists.
//! - **Vulkan without it**, as Mesa's lavapipe: LINEAR buffers only, bound to a `VK_IMAGE_TILING_LINEAR` image.
//! - **OpenGL** (wgpu's GLES backend on EGL): an EGLImage on wgpu's own display, bound to a GL texture.
//!
//! A LINEAR buffer is imported as wide as its rows (`stride / 4` pixels), so the image's row pitch is the buffer's
//! stride on every path: a linear image cannot be given an explicit pitch without the modifier extension, and some
//! drivers ignore the pitch given to EGL. The draw then samples only the buffer's own width.

use crate::surface_cache::SurfaceCache;
use gpui::{DmaBufImport, DmaBufLayout, DmaBufTexture, PaintSurface};
use std::ffi::{CStr, c_char, c_void};
use std::num::NonZeroU32;
use std::os::fd::{AsRawFd, IntoRawFd};

/// `DRM_FORMAT_ABGR8888`: R, G, B, A bytes in memory order, which is `Rgba8Unorm`.
pub(crate) const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;
/// `DRM_FORMAT_MOD_LINEAR`.
pub(crate) const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// How a device imports dma-bufs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImportPath {
    /// Vulkan with `VULKAN_EXTERNAL_MEMORY_DMA_BUF`: wgpu-hal's `texture_from_dmabuf_fd`.
    VulkanModifiers,
    /// Vulkan with `VK_EXT_external_memory_dma_buf` but no modifiers: LINEAR buffers into linear images.
    VulkanLinear,
    /// EGL with `EGL_EXT_image_dma_buf_import`, and whether it has `EGL_EXT_image_dma_buf_import_modifiers`.
    Gl { modifiers: bool },
}

/// Finds how `device` imports dma-bufs, publishes it through [`gpui::set_dma_buf_import`] and logs it. `None`, also
/// published, when it cannot.
pub(crate) fn probe(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Option<ImportPath> {
    let name = adapter.get_info().name;
    let probed = match adapter.get_info().backend {
        wgpu::Backend::Vulkan => probe_vulkan(device),
        wgpu::Backend::Gl => probe_gl(device),
        _ => None,
    };
    match &probed {
        Some((path, import)) => log::info!(
            "dma-buf surfaces on {name}: {path:?}, render node {:?}, modifiers {:x?}",
            import.render_node,
            import.modifiers
        ),
        None => {
            log::info!("dma-buf surfaces on {name}: unavailable, the driver cannot import dma-bufs")
        }
    }
    let (path, import) = probed.unzip();
    gpui::set_dma_buf_import(import);
    path
}

fn probe_vulkan(device: &wgpu::Device) -> Option<(ImportPath, DmaBufImport)> {
    use ash::vk;

    // SAFETY: only queries; the device is not destroyed while the guard is held.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }?;
    let instance = hal.shared_instance().raw_instance();
    let physical = hal.raw_physical_device();
    let enabled = hal.enabled_device_extensions();
    // SAFETY: a valid physical device of this instance.
    let supported = unsafe { instance.enumerate_device_extension_properties(physical) }.ok()?;
    let supports = |name: &CStr| {
        supported
            .iter()
            .any(|extension| extension.extension_name_as_c_str() == Ok(name))
    };

    let render_node = supports(ash::ext::physical_device_drm::NAME)
        .then(|| {
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            // SAFETY: the extension is supported, so the chained structure is understood.
            unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
            (drm.has_render == vk::TRUE).then(|| (drm.render_major as u32, drm.render_minor as u32))
        })
        .flatten();

    if device
        .features()
        .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
    {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: the modifier extension is enabled, so the chained list is understood; first call counts.
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                vk::Format::R8G8B8A8_UNORM,
                &mut properties,
            )
        };
        let mut entries = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut entries);
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: as above; the second call fills `entries`, whose length it was given.
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                vk::Format::R8G8B8A8_UNORM,
                &mut properties,
            )
        };
        let count = list.drm_format_modifier_count as usize;
        let modifiers = entries[..count.min(entries.len())]
            .iter()
            .filter(|entry| {
                entry.drm_format_modifier_plane_count == 1
                    && entry
                        .drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
            })
            .map(|entry| entry.drm_format_modifier);
        return Some((
            ImportPath::VulkanModifiers,
            DmaBufImport {
                render_node,
                modifiers: linear_last(modifiers),
            },
        ));
    }

    let external = |name: &CStr| enabled.contains(&name);
    if external(ash::ext::external_memory_dma_buf::NAME)
        && external(ash::khr::external_memory_fd::NAME)
    {
        return Some((
            ImportPath::VulkanLinear,
            DmaBufImport {
                render_node,
                modifiers: vec![DRM_FORMAT_MOD_LINEAR],
            },
        ));
    }
    None
}

// EGL and GL enums and entry points that `khronos-egl` and `glow` do not have.
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: khronos_egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: khronos_egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: khronos_egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: khronos_egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: khronos_egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: khronos_egl::Attrib = 0x3444;
const EGL_DEVICE_EXT: i32 = 0x322C;
const EGL_DRM_RENDER_NODE_FILE_EXT: i32 = 0x3377;
type QueryDmaBufModifiers =
    unsafe extern "system" fn(*mut c_void, i32, i32, *mut u64, *mut u32, *mut i32) -> u32;
type QueryDisplayAttrib = unsafe extern "system" fn(*mut c_void, i32, *mut isize) -> u32;
type QueryDeviceString = unsafe extern "system" fn(*mut c_void, i32) -> *const c_char;
type ImageTargetTexture2D = unsafe extern "system" fn(u32, *mut c_void);

fn has_extension(extensions: &str, name: &str) -> bool {
    extensions
        .split_whitespace()
        .any(|extension| extension == name)
}

fn probe_gl(device: &wgpu::Device) -> Option<(ImportPath, DmaBufImport)> {
    // SAFETY: only queries; the device is not destroyed while the guard is held.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Gles>() }?;
    let context = hal.context();
    let egl = context.egl_instance()?;
    let display = *context.raw_display()?;
    let display_extensions = egl
        .query_string(Some(display), khronos_egl::EXTENSIONS)
        .ok()?
        .to_string_lossy()
        .into_owned();
    if !has_extension(&display_extensions, "EGL_EXT_image_dma_buf_import") {
        return None;
    }
    let modifiers = has_extension(
        &display_extensions,
        "EGL_EXT_image_dma_buf_import_modifiers",
    );

    let mut listed = Vec::new();
    if modifiers && let Some(query) = egl.get_proc_address("eglQueryDmaBufModifiersEXT") {
        // SAFETY: the extension is present, so this is its entry point.
        let query: QueryDmaBufModifiers = unsafe { std::mem::transmute(query) };
        let mut count = 0;
        // SAFETY: a valid display; the first call counts, the second fills arrays of that length.
        unsafe {
            if query(
                display.as_ptr(),
                DRM_FORMAT_ABGR8888 as i32,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut count,
            ) != 0
            {
                let mut values = vec![0u64; count.max(0) as usize];
                let mut external_only = vec![0u32; values.len()];
                if query(
                    display.as_ptr(),
                    DRM_FORMAT_ABGR8888 as i32,
                    count,
                    values.as_mut_ptr(),
                    external_only.as_mut_ptr(),
                    &mut count,
                ) != 0
                {
                    // External-only layouts can only be sampled through `samplerExternalOES`.
                    listed.extend(
                        values
                            .iter()
                            .zip(&external_only)
                            .take(count.max(0) as usize)
                            .filter(|(_, external)| **external == 0)
                            .map(|(value, _)| *value),
                    );
                }
            }
        }
    }

    Some((
        ImportPath::Gl { modifiers },
        DmaBufImport {
            render_node: gl_render_node(egl, display),
            modifiers: linear_last(listed),
        },
    ))
}

/// The render node of `display`'s device, through `EGL_EXT_device_query` and `EGL_EXT_device_drm_render_node`.
fn gl_render_node(
    egl: &khronos_egl::DynamicInstance<khronos_egl::EGL1_4>,
    display: khronos_egl::Display,
) -> Option<(u32, u32)> {
    let client_extensions = egl
        .query_string(None, khronos_egl::EXTENSIONS)
        .ok()?
        .to_string_lossy()
        .into_owned();
    if !has_extension(&client_extensions, "EGL_EXT_device_query")
        && !has_extension(&client_extensions, "EGL_EXT_device_base")
    {
        return None;
    }
    // SAFETY: the extension is present, so these are its entry points.
    let (query_attrib, query_string) = unsafe {
        (
            std::mem::transmute::<extern "system" fn(), QueryDisplayAttrib>(
                egl.get_proc_address("eglQueryDisplayAttribEXT")?,
            ),
            std::mem::transmute::<extern "system" fn(), QueryDeviceString>(
                egl.get_proc_address("eglQueryDeviceStringEXT")?,
            ),
        )
    };
    let mut egl_device = 0isize;
    // SAFETY: a valid display; the strings returned are owned by EGL and valid while the display is.
    let path = unsafe {
        if query_attrib(display.as_ptr(), EGL_DEVICE_EXT, &mut egl_device) == 0 || egl_device == 0 {
            return None;
        }
        let path = query_string(egl_device as *mut c_void, EGL_DRM_RENDER_NODE_FILE_EXT);
        if path.is_null() {
            return None;
        }
        CStr::from_ptr(path).to_owned()
    };
    device_number(&path)
}

fn device_number(path: &CStr) -> Option<(u32, u32)> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a valid path and a buffer for one `stat`.
    if unsafe { libc::stat(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: filled by the successful call.
    let rdev = unsafe { stat.assume_init() }.st_rdev;
    Some((libc::major(rdev), libc::minor(rdev)))
}

/// The modifiers in order, with LINEAR present once and last: every importer takes it, and it is the slowest.
fn linear_last(modifiers: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let mut modifiers: Vec<u64> = modifiers
        .into_iter()
        .filter(|modifier| *modifier != DRM_FORMAT_MOD_LINEAR)
        .collect();
    modifiers.dedup();
    modifiers.push(DRM_FORMAT_MOD_LINEAR);
    modifiers
}

/// A dma-buf imported as a texture. It holds its own reference to the buffer's memory.
pub(crate) struct Imported {
    pub(crate) texture: wgpu::Texture,
}

/// The width a buffer is imported with: its rows' width for a LINEAR buffer (see the module's documentation),
/// otherwise its own.
pub(crate) fn texture_width(layout: &DmaBufLayout) -> u32 {
    if layout.modifier == DRM_FORMAT_MOD_LINEAR
        && layout.stride.is_multiple_of(4)
        && layout.stride / 4 >= layout.width
    {
        layout.stride / 4
    } else {
        layout.width
    }
}

/// Imports `texture` on `device` along `path`.
pub(crate) fn import(
    device: &wgpu::Device,
    path: ImportPath,
    texture: &DmaBufTexture,
) -> Result<Imported, String> {
    let layout = texture.layout();
    if layout.fourcc != DRM_FORMAT_ABGR8888 {
        return Err(format!(
            "format {:#010x} is not DRM_FORMAT_ABGR8888",
            layout.fourcc
        ));
    }
    if layout.width == 0 || layout.height == 0 {
        return Err("the buffer is empty".into());
    }
    let width = texture_width(&layout);
    let max = device.limits().max_texture_dimension_2d;
    if width > max || layout.height > max {
        return Err(format!(
            "{width}x{} is larger than the device's textures ({max})",
            layout.height
        ));
    }
    let hal_descriptor = wgpu::hal::TextureDescriptor {
        label: Some("dma_buf_surface"),
        size: wgpu::Extent3d {
            width,
            height: layout.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUses::RESOURCE,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    let descriptor = wgpu::TextureDescriptor {
        label: Some("dma_buf_surface"),
        size: hal_descriptor.size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    let texture = match path {
        ImportPath::VulkanModifiers => {
            let fd = texture
                .fd()
                .try_clone_to_owned()
                .map_err(|error| format!("dup: {error}"))?;
            // SAFETY: the device has `VULKAN_EXTERNAL_MEMORY_DMA_BUF`; the layout describes the buffer, as
            // `DmaBufTexture` requires, and `hal_descriptor` matches it.
            let raw = unsafe {
                let hal = device
                    .as_hal::<wgpu::hal::api::Vulkan>()
                    .ok_or("not a Vulkan device")?;
                hal.texture_from_dmabuf_fd(
                    fd,
                    &hal_descriptor,
                    layout.modifier,
                    u64::from(layout.stride),
                    u64::from(layout.offset),
                )
            }
            .map_err(|error| format!("texture_from_dmabuf_fd: {error}"))?;
            // SAFETY: `raw` was made from `hal_descriptor`, which `descriptor` matches; its memory holds the
            // producer's finished pixels, so it needs no initialisation.
            unsafe {
                device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
                    raw,
                    &descriptor,
                    wgpu::TextureUses::UNINITIALIZED,
                )
            }
        }
        ImportPath::VulkanLinear => {
            let raw = import_linear_vulkan(device, texture, &hal_descriptor)?;
            // SAFETY: as above.
            unsafe {
                device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
                    raw,
                    &descriptor,
                    wgpu::TextureUses::UNINITIALIZED,
                )
            }
        }
        ImportPath::Gl { modifiers } => {
            let raw = import_gl(device, texture, &hal_descriptor, modifiers)?;
            // SAFETY: as above.
            unsafe {
                device.create_texture_from_hal::<wgpu::hal::api::Gles>(
                    raw,
                    &descriptor,
                    wgpu::TextureUses::UNINITIALIZED,
                )
            }
        }
    };
    Ok(Imported { texture })
}

/// Binds a LINEAR buffer's memory to a `VK_IMAGE_TILING_LINEAR` image, for drivers without the modifier extension.
fn import_linear_vulkan(
    device: &wgpu::Device,
    texture: &DmaBufTexture,
    descriptor: &wgpu::hal::TextureDescriptor<'_>,
) -> Result<wgpu::hal::vulkan::Texture, String> {
    use ash::vk;

    let layout = texture.layout();
    if layout.modifier != DRM_FORMAT_MOD_LINEAR {
        return Err(format!(
            "modifier {:#x} needs VK_EXT_image_drm_format_modifier, which the driver lacks",
            layout.modifier
        ));
    }
    if layout.offset != 0 {
        return Err(format!(
            "offset {} is not 0, which a linear image needs",
            layout.offset
        ));
    }
    // SAFETY: the device is not destroyed while the guard is held.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }.ok_or("not a Vulkan device")?;
    let raw_device = hal.raw_device();
    let fd_functions =
        ash::khr::external_memory_fd::Device::new(hal.shared_instance().raw_instance(), raw_device);

    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D {
            width: descriptor.size.width,
            height: descriptor.size.height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external);
    // SAFETY: a valid device and create info; the image is destroyed below on every failure, or by wgpu once wrapped.
    let image = unsafe { raw_device.create_image(&info, None) }
        .map_err(|error| format!("vkCreateImage: {error}"))?;
    let destroy_image = || unsafe { raw_device.destroy_image(image, None) };

    let subresource = vk::ImageSubresource {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        array_layer: 0,
    };
    // SAFETY: a linear image of this device.
    let image_layout = unsafe { raw_device.get_image_subresource_layout(image, subresource) };
    if image_layout.row_pitch != u64::from(layout.stride) || image_layout.offset != 0 {
        destroy_image();
        return Err(format!(
            "the driver's linear rows are {} bytes apart, the buffer's {}",
            image_layout.row_pitch, layout.stride
        ));
    }

    // SAFETY: as above.
    let requirements = unsafe { raw_device.get_image_memory_requirements(image) };
    let fd = match texture.fd().try_clone_to_owned() {
        Ok(fd) => fd.into_raw_fd(),
        Err(error) => {
            destroy_image();
            return Err(format!("dup: {error}"));
        }
    };
    // From here `fd` is ours to close until `vkAllocateMemory` takes it.
    let fail = |message: String| {
        // SAFETY: `fd` is open and not taken by Vulkan.
        unsafe { libc::close(fd) };
        destroy_image();
        Err(message)
    };
    let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
    // SAFETY: a dma-buf fd and the extension enabled on this device.
    if let Err(error) = unsafe {
        fd_functions.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd,
            &mut fd_properties,
        )
    } {
        return fail(format!("vkGetMemoryFdPropertiesKHR: {error}"));
    }
    let memory_types = requirements.memory_type_bits & fd_properties.memory_type_bits;
    if memory_types == 0 {
        return fail("no memory type both takes the buffer and backs the image".into());
    }
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let mut import = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(fd);
    let allocate = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(memory_types.trailing_zeros())
        .push_next(&mut import)
        .push_next(&mut dedicated);
    // SAFETY: valid infos; on success Vulkan owns `fd`.
    let memory = match unsafe { raw_device.allocate_memory(&allocate, None) } {
        Ok(memory) => memory,
        Err(error) => return fail(format!("vkAllocateMemory: {error}")),
    };
    // SAFETY: the memory was allocated for this image.
    if let Err(error) = unsafe { raw_device.bind_image_memory(image, memory, 0) } {
        unsafe { raw_device.free_memory(memory, None) };
        destroy_image();
        return Err(format!("vkBindImageMemory: {error}"));
    }
    // SAFETY: the image matches `descriptor`; wgpu-hal destroys it and frees its dedicated memory.
    Ok(unsafe {
        hal.texture_from_raw(
            image,
            descriptor,
            None,
            wgpu::hal::vulkan::TextureMemory::Dedicated(memory),
        )
    })
}

/// Imports a buffer as an EGLImage on wgpu's own EGL display and binds it to a new GL texture.
fn import_gl(
    device: &wgpu::Device,
    texture: &DmaBufTexture,
    descriptor: &wgpu::hal::TextureDescriptor<'_>,
    modifiers: bool,
) -> Result<wgpu::hal::gles::Texture, String> {
    use glow::HasContext;

    let layout = texture.layout();
    if !modifiers && layout.modifier != DRM_FORMAT_MOD_LINEAR {
        return Err(format!(
            "modifier {:#x} needs EGL_EXT_image_dma_buf_import_modifiers, which the driver lacks",
            layout.modifier
        ));
    }
    // SAFETY: the device is not destroyed while the guard is held.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Gles>() }.ok_or("not an OpenGL device")?;
    let context = hal.context();
    let egl = context
        .egl_instance()
        .ok_or("wgpu's OpenGL context is not on EGL")?;
    let display = *context
        .raw_display()
        .ok_or("wgpu's OpenGL context has no EGL display")?;
    let egl = egl
        .upcast::<khronos_egl::EGL1_5>()
        .ok_or("EGL 1.5 is not available")?;
    let target: ImageTargetTexture2D = {
        let address = egl
            .get_proc_address("glEGLImageTargetTexture2DOES")
            .ok_or("glEGLImageTargetTexture2DOES is not available")?;
        // SAFETY: the entry point of that name.
        unsafe { std::mem::transmute(address) }
    };

    let mut attributes: Vec<khronos_egl::Attrib> = vec![
        khronos_egl::WIDTH as _,
        descriptor.size.width as _,
        khronos_egl::HEIGHT as _,
        descriptor.size.height as _,
        EGL_LINUX_DRM_FOURCC_EXT,
        layout.fourcc as _,
        EGL_DMA_BUF_PLANE0_FD_EXT,
        texture.fd().as_raw_fd() as _,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT,
        layout.offset as _,
        EGL_DMA_BUF_PLANE0_PITCH_EXT,
        layout.stride as _,
    ];
    if modifiers {
        attributes.extend([
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            (layout.modifier & 0xffff_ffff) as _,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            (layout.modifier >> 32) as _,
        ]);
    }
    attributes.push(khronos_egl::ATTRIB_NONE);

    // wgpu's context is current while the lock is held.
    let gl = context.lock();
    // SAFETY: a valid display and attribute list; EGL does not take the fd, which stays ours.
    let image = unsafe {
        egl.create_image(
            display,
            khronos_egl::Context::from_ptr(khronos_egl::NO_CONTEXT),
            EGL_LINUX_DMA_BUF_EXT,
            khronos_egl::ClientBuffer::from_ptr(std::ptr::null_mut()),
            &attributes,
        )
    }
    .map_err(|error| format!("eglCreateImage: {error}"))?;
    // SAFETY: the context is current; the texture keeps the image's storage, so the image is destroyed right away.
    let name = unsafe {
        let name = gl.create_texture();
        if let Ok(name) = name {
            gl.bind_texture(glow::TEXTURE_2D, Some(name));
            target(glow::TEXTURE_2D, image.as_ptr());
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAX_LEVEL, 0);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        let _ = egl.destroy_image(display, image);
        let name = name.map_err(|error| format!("glGenTextures: {error}"))?;
        let error = gl.get_error();
        if error != glow::NO_ERROR {
            gl.delete_texture(name);
            return Err(format!("glEGLImageTargetTexture2DOES: GL error {error:#x}"));
        }
        name
    };
    drop(gl);
    let name: NonZeroU32 = name.0;
    // SAFETY: a texture made from `descriptor`; without a drop callback wgpu-hal deletes it under its lock.
    Ok(unsafe { hal.texture_from_raw(name, descriptor, None) })
}

/// The cache key of a painted buffer: its file's device and inode, which every duplicate of its fd shares, and its
/// layout.
pub(crate) fn cache_key(texture: &DmaBufTexture) -> Result<(u64, u64, DmaBufLayout), String> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: an open fd and a buffer for one `stat`.
    if unsafe { libc::fstat(texture.fd().as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(format!("fstat: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: filled by the successful call.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_dev, stat.st_ino, texture.layout()))
}

/// The parameters of one surface draw, as `vs_rgba_surface` reads them.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RgbaSurfaceParams {
    bounds: [f32; 4],
    content_mask: [f32; 4],
    /// The part of the texture the buffer's own pixels take: `width / texture_width` across.
    texture_scale: [f32; 2],
    /// The largest texture coordinate sampled: half a texel inside the last column and row, so linear filtering
    /// never reaches a padded texture's extra columns.
    texture_max: [f32; 2],
}

pub(crate) const RGBA_SURFACE_PARAMS_SIZE: u64 = std::mem::size_of::<RgbaSurfaceParams>() as u64;

/// An imported buffer, ready to draw.
struct Entry {
    bind_group: wgpu::BindGroup,
    texture_width: u32,
    _texture: wgpu::Texture,
}

/// One surface's draw: its texture's bind group, and the dynamic offset of its parameters.
pub(crate) struct Draw {
    pub(crate) bind_group: wgpu::BindGroup,
    pub(crate) offset: u32,
}

/// The per-frame parameters of every surface drawn, in one uniform buffer.
struct Params {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    capacity: u64,
}

/// The renderer's dma-buf surfaces: how its device imports them, the imports kept across frames, and the warning
/// state. Part of the device's resources, so it is dropped with them, also on device loss.
pub(crate) struct Surfaces {
    path: Option<ImportPath>,
    adapter: String,
    cache: SurfaceCache<(u64, u64, DmaBufLayout), Entry>,
    /// Set by a failure, which is logged only while it is clear, and cleared by a successful import.
    warned: bool,
    params: Option<Params>,
}

impl Surfaces {
    pub(crate) fn new(path: Option<ImportPath>, adapter: String) -> Self {
        Self {
            path,
            adapter,
            cache: SurfaceCache::new(),
            warned: false,
            params: None,
        }
    }

    /// The bind group of the parameters the last [`prepare`](Self::prepare) wrote.
    pub(crate) fn params_bind_group(&self) -> Option<&wgpu::BindGroup> {
        self.params.as_ref().map(|params| &params.bind_group)
    }

    /// Imports each surface's buffer, or reuses its import, and writes its draw parameters. Returns one draw per
    /// surface, in order, `None` for those that cannot be drawn. Called once per frame.
    pub(crate) fn prepare<'a>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        params_layout: &wgpu::BindGroupLayout,
        texture_layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        surfaces: impl IntoIterator<Item = &'a PaintSurface>,
    ) -> Vec<Option<Draw>> {
        self.cache.begin_frame();
        let mut params = Vec::new();
        let mut draws = Vec::new();
        for surface in surfaces {
            match self.entry(device, texture_layout, sampler, &surface.dma_buf) {
                Ok((bind_group, texture_width)) => {
                    let layout = surface.dma_buf.layout();
                    let (width, height) = (layout.width as f32, layout.height as f32);
                    let bounds = |bounds: gpui::Bounds<gpui::ScaledPixels>| {
                        [
                            bounds.origin.x.0,
                            bounds.origin.y.0,
                            bounds.size.width.0,
                            bounds.size.height.0,
                        ]
                    };
                    draws.push(Some((bind_group, params.len())));
                    params.push(RgbaSurfaceParams {
                        bounds: bounds(surface.bounds),
                        content_mask: bounds(surface.content_mask.bounds),
                        texture_scale: [width / texture_width as f32, 1.0],
                        texture_max: [
                            (width - 0.5) / texture_width as f32,
                            (height - 0.5) / height,
                        ],
                    });
                }
                Err(error) => {
                    if !self.warned {
                        log::warn!("A dma-buf surface cannot be drawn and is skipped: {error}");
                        self.warned = true;
                    }
                    draws.push(None);
                }
            }
        }
        if params.is_empty() {
            return draws.into_iter().map(|_| None).collect();
        }

        let stride = RGBA_SURFACE_PARAMS_SIZE.next_multiple_of(u64::from(
            device.limits().min_uniform_buffer_offset_alignment,
        ));
        let needed = stride * params.len() as u64;
        if self
            .params
            .as_ref()
            .is_none_or(|current| current.capacity < needed)
        {
            let capacity = needed.next_power_of_two();
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rgba_surface_params"),
                size: capacity,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rgba_surface_params"),
                layout: params_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buffer,
                        offset: 0,
                        size: std::num::NonZeroU64::new(RGBA_SURFACE_PARAMS_SIZE),
                    }),
                }],
            });
            self.params = Some(Params {
                buffer,
                bind_group,
                capacity,
            });
        }
        let mut bytes = vec![0u8; needed as usize];
        for (index, param) in params.iter().enumerate() {
            let at = index * stride as usize;
            bytes[at..at + RGBA_SURFACE_PARAMS_SIZE as usize]
                .copy_from_slice(bytemuck::bytes_of(param));
        }
        if let Some(current) = &self.params {
            queue.write_buffer(&current.buffer, 0, &bytes);
        }
        draws
            .into_iter()
            .map(|draw| {
                draw.map(|(bind_group, index)| Draw {
                    bind_group,
                    offset: (index as u64 * stride) as u32,
                })
            })
            .collect()
    }

    /// The import of `texture`'s buffer, made now if it is not kept from an earlier frame, with its texture's width.
    fn entry(
        &mut self,
        device: &wgpu::Device,
        texture_layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        texture: &DmaBufTexture,
    ) -> Result<(wgpu::BindGroup, u32), String> {
        let path = self
            .path
            .ok_or_else(|| format!("GPUI's renderer on {} cannot import dma-bufs", self.adapter))?;
        let key = cache_key(texture)?;
        let mut imported = false;
        let entry = self.cache.get_or_try_insert_with(key, || {
            let Imported {
                texture: imported_texture,
            } = import(device, path, texture)?;
            imported = true;
            let view = imported_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rgba_surface_texture"),
                layout: texture_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            });
            Ok::<_, String>(Entry {
                bind_group,
                texture_width: imported_texture.width(),
                _texture: imported_texture,
            })
        })?;
        let found = (entry.bind_group.clone(), entry.texture_width);
        if imported {
            self.warned = false;
        }
        Ok(found)
    }
}
