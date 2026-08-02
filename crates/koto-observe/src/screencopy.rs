//! In-process `wlr-screencopy` PNG capture.

use koto_core::CoreError;
use memmap2::MmapOptions;
use std::{
    fs::{self, File, OpenOptions},
    io::BufWriter,
    os::fd::AsFd,
    path::{Path, PathBuf},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{Event as FrameEvent, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

#[derive(Default)]
struct State {
    buffer: Option<FrameBuffer>,
    ready: bool,
    failed: bool,
    y_invert: bool,
}
#[derive(Clone, Copy)]
struct FrameBuffer {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
wayland_client::delegate_noop!(State: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: FrameEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            FrameEvent::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                if let Ok(format) = format.into_result() {
                    state.buffer = Some(FrameBuffer {
                        format,
                        width,
                        height,
                        stride,
                    });
                }
            }
            FrameEvent::Flags { flags } => {
                state.y_invert = flags.into_result().is_ok_and(|flags| flags.bits() & 1 != 0)
            }
            FrameEvent::Ready { .. } => state.ready = true,
            FrameEvent::Failed => state.failed = true,
            _ => {}
        }
    }
}

/// Captures the first compositor output into `destination`, returning its path.
/// The caller chooses the output file path so observation names remain stable.
pub fn capture_png(destination: &Path) -> Result<PathBuf, CoreError> {
    let connection = Connection::connect_to_env()
        .map_err(|error| CoreError::Backend(format!("Wayland screencopy unavailable: {error}")))?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)
        .map_err(|error| CoreError::Backend(format!("Wayland registry unavailable: {error}")))?;
    let qh = queue.handle();
    let output: wl_output::WlOutput = globals
        .bind(&qh, 1..=4, ())
        .map_err(|error| CoreError::Backend(format!("Wayland output unavailable: {error}")))?;
    let manager: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .map_err(|error| CoreError::Backend(format!("wlr-screencopy unavailable: {error}")))?;
    let frame = manager.capture_output(0, &output, &qh, ());
    let mut state = State::default();
    while state.buffer.is_none() && !state.failed {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|error| CoreError::Backend(format!("screencopy buffer event: {error}")))?;
    }
    if state.failed {
        return Err(CoreError::ObservationUnavailable(
            "screencopy frame failed".into(),
        ));
    }
    let info = state.buffer.ok_or_else(|| {
        CoreError::ObservationUnavailable("screencopy sent no shared-memory buffer".into())
    })?;
    if !matches!(
        info.format,
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
    ) {
        return Err(CoreError::ObservationUnavailable(format!(
            "unsupported screencopy pixel format {:?}",
            info.format
        )));
    }
    let size = (info.stride as u64)
        .checked_mul(info.height as u64)
        .ok_or_else(|| CoreError::Backend("screencopy buffer too large".into()))?;
    let backing = runtime_file("screencopy", size)?;
    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|error| CoreError::Backend(format!("wl_shm unavailable: {error}")))?;
    let pool = shm.create_pool(backing.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        info.width as i32,
        info.height as i32,
        info.stride as i32,
        info.format,
        &qh,
        (),
    );
    frame.copy(&buffer);
    connection
        .flush()
        .map_err(|error| CoreError::Backend(format!("screencopy flush: {error}")))?;
    while !state.ready && !state.failed {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|error| CoreError::Backend(format!("screencopy result: {error}")))?;
    }
    if state.failed {
        return Err(CoreError::ObservationUnavailable(
            "screencopy copy failed".into(),
        ));
    }
    let pixels = unsafe { MmapOptions::new().len(size as usize).map(&backing) }
        .map_err(|error| CoreError::Backend(format!("map screencopy buffer: {error}")))?;
    write_png(destination, &pixels, info, state.y_invert)?;
    Ok(destination.to_owned())
}

fn runtime_file(prefix: &str, size: u64) -> Result<File, CoreError> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("koto");
    fs::create_dir_all(&base).map_err(|error| CoreError::Backend(error.to_string()))?;
    let path = base.join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| CoreError::Backend(error.to_string()))?;
    let _ = fs::remove_file(path);
    file.set_len(size)
        .map_err(|error| CoreError::Backend(error.to_string()))?;
    Ok(file)
}
fn write_png(
    destination: &Path,
    pixels: &[u8],
    info: FrameBuffer,
    y_invert: bool,
) -> Result<(), CoreError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| CoreError::Backend(error.to_string()))?;
    }
    let mut rgba = vec![0_u8; info.width as usize * info.height as usize * 4];
    for y in 0..info.height as usize {
        let source_y = if y_invert {
            info.height as usize - 1 - y
        } else {
            y
        };
        for x in 0..info.width as usize {
            let source = source_y * info.stride as usize + x * 4;
            let target = (y * info.width as usize + x) * 4;
            rgba[target] = pixels[source + 2];
            rgba[target + 1] = pixels[source + 1];
            rgba[target + 2] = pixels[source];
            rgba[target + 3] = if matches!(info.format, wl_shm::Format::Argb8888) {
                pixels[source + 3]
            } else {
                255
            };
        }
    }
    let file = File::create(destination).map_err(|error| CoreError::Backend(error.to_string()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), info.width, info.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .map_err(|error| CoreError::Backend(format!("PNG header: {error}")))?
        .write_image_data(&rgba)
        .map_err(|error| CoreError::Backend(format!("PNG data: {error}")))
}
