//! Persistent nested seats.
//!
//! A nested seat is a second Hyprland running as a client of the current
//! session, with its own output, focus, and window list. Work done there cannot
//! touch the user's desktop and the user cannot touch it, which is what makes
//! hermetic testing and unattended automation possible.
//!
//! The seat outlives the process that created it. An earlier design tore it down
//! on drop, which meant every window an invocation opened died with it and no
//! later command could observe anything.
//!
//! Three things the compositor dictates, learned the hard way:
//!
//!   * `HYPRLAND_INSTANCE_SIGNATURE` is not an input. Hyprland always mints its
//!     own, so the instance has to be discovered after launch — by asking the
//!     compositor which instance belongs to our process — never dictated.
//!   * `WAYLAND_DISPLAY` must stay set. Hyprland selects the nested Wayland
//!     backend from its presence; unsetting it forces a DRM backend that cannot
//!     open the already-owned hardware and aborts.
//!   * `WLR_BACKENDS` does nothing here. Backend selection moved to aquamarine.

use koto_core::CoreError;
use std::{
    ffi::OsString,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

const STATE: &str = "koto/seat.json";

#[derive(Debug, Clone)]
pub struct SeatState {
    pub signature: String,
    pub display: String,
    pub pid: u32,
}

fn runtime_dir() -> Result<PathBuf, CoreError> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| CoreError::Backend("nested seat needs XDG_RUNTIME_DIR".into()))
}

fn state_path() -> Result<PathBuf, CoreError> {
    Ok(runtime_dir()?.join(STATE))
}

fn hypr_dir() -> Result<PathBuf, CoreError> {
    Ok(runtime_dir()?.join("hypr"))
}

fn socket_of(signature: &str) -> Result<PathBuf, CoreError> {
    Ok(hypr_dir()?.join(signature).join(".socket.sock"))
}

fn alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

/// Reads the recorded seat, if one is still running.
pub fn current() -> Option<SeatState> {
    let raw = fs::read_to_string(state_path().ok()?).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let state = SeatState {
        signature: value.get("signature")?.as_str()?.to_owned(),
        display: value.get("display")?.as_str()?.to_owned(),
        pid: value.get("pid")?.as_u64()? as u32,
    };
    // A stale file outlives a crashed compositor; treat it as absent so the
    // next attach starts a fresh seat instead of pointing at nothing.
    if alive(state.pid) && socket_of(&state.signature).ok()?.exists() {
        Some(state)
    } else {
        let _ = fs::remove_file(state_path().ok()?);
        None
    }
}

fn write_state(state: &SeatState) -> Result<(), CoreError> {
    let path = state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| CoreError::Backend(error.to_string()))?;
    }
    let body = serde_json::json!({
        "signature": state.signature,
        "display": state.display,
        "pid": state.pid,
    });
    fs::write(&path, body.to_string()).map_err(|error| CoreError::Backend(error.to_string()))
}

/// Minimal config: one virtual output, nothing that would autostart the user's
/// session inside the seat.
fn write_config() -> Result<PathBuf, CoreError> {
    let path = runtime_dir()?.join("koto/seat.conf");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| CoreError::Backend(error.to_string()))?;
    }
    fs::write(
        &path,
        "monitor = , 1920x1080@60, 0x0, 1\n\
         animations { enabled = false }\n\
         decoration { blur { enabled = false } }\n\
         misc {\n\
         \x20 disable_hyprland_logo = true\n\
         \x20 disable_splash_rendering = true\n\
         \x20 force_default_wallpaper = 0\n\
         }\n",
    )
    .map_err(|error| CoreError::Backend(error.to_string()))?;
    Ok(path)
}

/// Finds the running instance owned by `pid`, returning its signature and socket.
///
/// Discovery is by process, not by watching the runtime directory: dead
/// compositors leave their directories behind, so a before/after diff can select
/// a corpse. The compositor's own instance list is authoritative and carries the
/// Wayland socket name in the same record.
fn instance_of(pid: u32) -> Option<(String, String)> {
    let output = std::process::Command::new("hyprctl")
        .args(["-j", "instances"])
        .output()
        .ok()?;
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    parsed.as_array()?.iter().find_map(|entry| {
        (entry.get("pid")?.as_u64()? as u32 == pid).then(|| {
            Some((
                entry.get("instance")?.as_str()?.to_owned(),
                entry.get("wl_socket")?.as_str()?.to_owned(),
            ))
        })?
    })
}

/// Starts a nested compositor and records it, or returns the running one.
pub fn attach_or_start() -> Result<SeatState, CoreError> {
    if let Some(state) = current() {
        return Ok(state);
    }
    let config = write_config()?;
    let mut child = std::process::Command::new("Hyprland")
        .arg("--config")
        .arg(&config)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|error| CoreError::Backend(format!("launch nested Hyprland: {error}")))?;
    let pid = child.id();

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| CoreError::Backend(error.to_string()))?
        {
            return Err(CoreError::Backend(format!(
                "nested Hyprland exited early: {status}"
            )));
        }
        if let Some((signature, display)) = instance_of(pid) {
            if socket_of(&signature)?.exists() {
                let state = SeatState {
                    signature,
                    display,
                    pid,
                };
                write_state(&state)?;
                return Ok(state);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    Err(CoreError::Backend(
        "nested Hyprland did not register an instance within 20s".into(),
    ))
}

/// Points this process at a seat. Child processes inherit the environment, so
/// anything spawned afterwards lands inside the seat rather than on the desktop.
pub fn enter(state: &SeatState) {
    unsafe {
        std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", &state.signature);
        std::env::set_var("WAYLAND_DISPLAY", &state.display);
        // Marks the seat for the process backend, which must launch directly
        // rather than through the session manager to keep this environment.
        std::env::set_var("KOTO_SEAT", &state.signature);
    }
}

/// Tears the seat down. Returns false when there was nothing to stop.
pub fn stop() -> Result<bool, CoreError> {
    let Some(state) = current() else {
        let _ = fs::remove_file(state_path()?);
        return Ok(false);
    };
    unsafe {
        libc::kill(state.pid as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && alive(state.pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
    if alive(state.pid) {
        unsafe {
            libc::kill(state.pid as i32, libc::SIGKILL);
        }
    }
    let _ = fs::remove_file(state_path()?);
    Ok(true)
}

/// Restores the ambient environment when a command finishes, so a seat entered
/// for one invocation does not leak into anything else this process does.
pub struct Restore {
    signature: Option<OsString>,
    display: Option<OsString>,
    marker: Option<OsString>,
}
impl Restore {
    pub fn capture() -> Self {
        Self {
            signature: std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE"),
            display: std::env::var_os("WAYLAND_DISPLAY"),
            marker: std::env::var_os("KOTO_SEAT"),
        }
    }
}
impl Drop for Restore {
    fn drop(&mut self) {
        unsafe {
            match &self.signature {
                Some(value) => std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", value),
                None => std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE"),
            }
            match &self.display {
                Some(value) => std::env::set_var("WAYLAND_DISPLAY", value),
                None => std::env::remove_var("WAYLAND_DISPLAY"),
            }
            match &self.marker {
                Some(value) => std::env::set_var("KOTO_SEAT", value),
                None => std::env::remove_var("KOTO_SEAT"),
            }
        }
    }
}
