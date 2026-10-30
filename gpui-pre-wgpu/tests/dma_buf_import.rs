//! Importing a dma-buf shaped like the ones `player-rs` paints (ABGR8888, LINEAR, rows padded past the picture's
//! width) on every adapter wgpu finds, then drawing it with the RGBA surface shader and reading the pixels back. The
//! buffer is a `vgem` dumb buffer; the test returns early, with a message, without `vgem` (`sudo modprobe vgem`).
//! Steer the adapters with `VK_DRIVER_FILES` (`/dev/null` hides every Vulkan driver). On WSL2, run it with
//! `LIBGL_ALWAYS_SOFTWARE=1`: GL is otherwise Mesa's `d3d12`, which advertises the import but cannot do it.
#![cfg(target_os = "linux")]

mod common;
#[allow(dead_code)]
#[path = "../src/dma_buf.rs"]
mod dma_buf;
#[path = "../src/surface_cache.rs"]
mod surface_cache;

use common::{HEIGHT, TEXTURE_WIDTH, WIDTH};
use gpui::{DmaBufLayout, DmaBufTexture};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// `struct drm_mode_create_dumb`.
#[repr(C)]
#[derive(Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

/// `struct drm_prime_handle`.
#[repr(C)]
#[derive(Default)]
struct PrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

/// `DRM_IOCTL_MODE_CREATE_DUMB`, `DRM_IOCTL_PRIME_HANDLE_TO_FD` and `DMA_BUF_IOCTL_SYNC`.
const CREATE_DUMB: libc::c_ulong = 0xC020_64B2;
const PRIME_HANDLE_TO_FD: libc::c_ulong = 0xC00C_642D;
const DMA_BUF_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_WRITE: u64 = 2;
const DMA_BUF_SYNC_END: u64 = 4;

/// `vgem`'s card node, if the module is loaded.
fn vgem() -> Option<File> {
    let cards = std::fs::read_dir("/sys/class/drm").ok()?;
    cards.flatten().find_map(|card| {
        let name = card.file_name().into_string().ok()?;
        // `devices/platform/vgem/drm/card0`, or `devices/faux/vgem/drm/card0` on newer kernels.
        let device = std::fs::read_link(card.path()).ok()?;
        (name.starts_with("card")
            && !name.contains('-')
            && device.iter().any(|part| part == "vgem"))
        .then(|| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("/dev/dri/{name}"))
                .ok()
        })?
    })
}

/// The picture in a `vgem` buffer `TEXTURE_WIDTH` pixels wide, described as `WIDTH` pixels wide.
fn padded_buffer(card: &File) -> DmaBufTexture {
    let mut dumb = CreateDumb {
        height: HEIGHT,
        width: TEXTURE_WIDTH,
        bpp: 32,
        ..Default::default()
    };
    // SAFETY: the ioctl's own argument structure.
    assert_eq!(
        unsafe { libc::ioctl(card.as_raw_fd(), CREATE_DUMB, &mut dumb) },
        0,
        "DRM_IOCTL_MODE_CREATE_DUMB"
    );
    let mut prime = PrimeHandle {
        handle: dumb.handle,
        flags: (libc::O_CLOEXEC | libc::O_RDWR) as u32,
        fd: -1,
    };
    // SAFETY: as above.
    assert_eq!(
        unsafe { libc::ioctl(card.as_raw_fd(), PRIME_HANDLE_TO_FD, &mut prime) },
        0,
        "DRM_IOCTL_PRIME_HANDLE_TO_FD"
    );
    // SAFETY: the ioctl returned a new descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(prime.fd) };
    assert!(
        dumb.pitch.is_multiple_of(4) && dumb.pitch / 4 > WIDTH,
        "pitch {}",
        dumb.pitch
    );

    let pixels = common::picture(dumb.pitch / 4);
    let size = dumb.size as usize;
    // SAFETY: maps the whole buffer, writes inside it between the dma-buf's CPU-access brackets, and unmaps it.
    unsafe {
        let map = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        );
        assert_ne!(map, libc::MAP_FAILED, "mmap");
        let mut sync = DMA_BUF_SYNC_WRITE;
        libc::ioctl(fd.as_raw_fd(), DMA_BUF_SYNC, &mut sync);
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), map.cast::<u8>(), pixels.len().min(size));
        sync = DMA_BUF_SYNC_WRITE | DMA_BUF_SYNC_END;
        libc::ioctl(fd.as_raw_fd(), DMA_BUF_SYNC, &mut sync);
        libc::munmap(map, size);
    }
    DmaBufTexture::new(
        fd,
        DmaBufLayout {
            width: WIDTH,
            height: HEIGHT,
            fourcc: dma_buf::DRM_FORMAT_ABGR8888,
            modifier: dma_buf::DRM_FORMAT_MOD_LINEAR,
            stride: dumb.pitch,
            offset: 0,
        },
    )
}

/// One test, so one thread uses each adapter: a GL context stays bound to the thread that made it current.
#[test]
fn imports_a_padded_linear_buffer_and_draws_its_width() {
    let Some(card) = vgem() else {
        eprintln!("skipped: no vgem card node (sudo modprobe vgem)");
        return;
    };
    let buffer = padded_buffer(&card);
    common::on_every_adapter(|name, adapter, device, queue| {
        let Some(path) = dma_buf::probe(adapter, device) else {
            assert_eq!(
                gpui::dma_buf_import(),
                None,
                "{name}: published without a path"
            );
            eprintln!("skipped on {name}: it cannot import dma-bufs");
            return;
        };
        let published = gpui::dma_buf_import().expect("published with a path");
        assert!(
            published
                .modifiers
                .contains(&dma_buf::DRM_FORMAT_MOD_LINEAR),
            "{name}: {published:?}"
        );
        eprintln!("importing on {name} along {path:?}, published {published:x?}");

        let imported = dma_buf::import(device, path, &buffer)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let texture_width = dma_buf::texture_width(&buffer.layout());
        assert_eq!(
            imported.texture.width(),
            texture_width,
            "{name}: imported as wide as its rows"
        );
        common::check(
            name,
            device,
            queue,
            &imported.texture.create_view(&Default::default()),
            texture_width,
        );
    });
}
