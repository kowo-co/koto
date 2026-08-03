//! A browser that outlives the program that opened it.
//!
//! A koto run is short and a browser is slow. Relaunching one per invocation
//! throws away the page, the scroll position, and everything the last run
//! learned, which makes step-by-step work impossible: every `end` would be a
//! reset rather than a pause. So the browser belongs to a holder process that
//! owns the CDP pipe and relays it over a unix socket. koto connects, drives
//! the page it left open, and disconnects. The session persists until it is
//! stopped, the same bargain nested seats make.
use koto_core::CoreError;
use std::{
    io::{Read, Write},
    os::unix::{
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("koto")
}
pub fn socket_path() -> PathBuf {
    runtime_dir().join("web.sock")
}

/// Connects to the running session, starting one if there is none. A socket
/// left behind by a dead holder is not a session; it is litter, and is removed.
pub fn connect_or_start(browser: Option<&str>) -> Result<UnixStream, CoreError> {
    let path = socket_path();
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }
    let _ = std::fs::remove_file(&path);
    start_holder(browser)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(stream) = UnixStream::connect(&path) {
            return Ok(stream);
        }
        if Instant::now() >= deadline {
            return Err(CoreError::Backend(format!(
                "browser session did not come up; see {}",
                runtime_dir().join("web-session.log").display()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn start_holder(browser: Option<&str>) -> Result<(), CoreError> {
    let exe = std::env::current_exe()
        .map_err(|error| CoreError::Backend(format!("locating koto: {error}")))?;
    std::fs::create_dir_all(runtime_dir())
        .map_err(|error| CoreError::Backend(format!("runtime dir: {error}")))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(runtime_dir().join("web-session.log"))
        .map_err(|error| CoreError::Backend(format!("session log: {error}")))?;
    let mut command = Command::new(exe);
    command.arg("--web-session-holder");
    if let Some(browser) = browser {
        command.arg(browser);
    }
    // The holder outlives this process, so it must not hold this process's
    // stdout: a pipe nobody closes is a caller that never returns.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            log.try_clone()
                .map_err(|error| CoreError::Backend(error.to_string()))?,
        ))
        .stderr(Stdio::from(log));
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| CoreError::Backend(format!("starting browser session: {error}")))
}

/// Runs the holder: owns the browser, relays its pipe to one client at a time.
/// Bytes are forwarded verbatim — the CDP framing is the client's business.
pub fn serve(browser: Option<&str>) -> Result<(), CoreError> {
    let path = socket_path();
    std::fs::create_dir_all(runtime_dir())
        .map_err(|error| CoreError::Backend(format!("runtime dir: {error}")))?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .map_err(|error| CoreError::Backend(format!("binding {}: {error}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let mut browser = crate::cdp::Browser::launch(browser)?;
    let result = accept_loop(&listener, &mut browser);
    let _ = std::fs::remove_file(&path);
    result
}

fn accept_loop(listener: &UnixListener, browser: &mut crate::cdp::Browser) -> Result<(), CoreError> {
    // The browser must be drained continuously, not only while a client is
    // attached: a pipe nobody reads fills up and wedges the browser, and a
    // reader that blocks between clients never lets the next one in. So one
    // thread reads forever and hands frames to whoever is connected, if
    // anyone. Frames that arrive between clients are answers to questions
    // nobody is waiting for any more, and are dropped.
    let current: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
    let mut from_browser = browser.reader()?;
    let pump_slot = Arc::clone(&current);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 16384];
        loop {
            match from_browser.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let mut slot = pump_slot.lock().unwrap_or_else(|error| error.into_inner());
                    if let Some(client) = slot.as_mut()
                        && (client.write_all(&buffer[..count]).is_err() || client.flush().is_err())
                    {
                        *slot = None;
                    }
                }
            }
        }
    });
    for client in listener.incoming() {
        let Ok(client) = client else { continue };
        if browser.exited() {
            return Ok(());
        }
        let Ok(mut client_read) = client.try_clone() else {
            continue;
        };
        let Ok(mut to_browser) = browser.writer() else {
            return Ok(());
        };
        *current.lock().unwrap_or_else(|error| error.into_inner()) = Some(client);
        let mut buffer = [0u8; 16384];
        loop {
            match client_read.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if to_browser.write_all(&buffer[..count]).is_err()
                        || to_browser.flush().is_err()
                    {
                        return Ok(());
                    }
                }
            }
        }
        *current.lock().unwrap_or_else(|error| error.into_inner()) = None;
    }
    Ok(())
}

/// Stops the session and the browser with it.
pub fn stop() -> Result<bool, CoreError> {
    let path = socket_path();
    if UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
        return Ok(false);
    }
    let pid_file = runtime_dir().join("web-session.pid");
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .ok_or_else(|| CoreError::Backend("browser session pid is unknown".into()))?;
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if UnixStream::connect(&path).is_err() {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&pid_file);
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&pid_file);
    Ok(true)
}

pub fn write_pid() {
    let _ = std::fs::create_dir_all(runtime_dir());
    let _ = std::fs::write(
        runtime_dir().join("web-session.pid"),
        std::process::id().to_string(),
    );
}

pub fn status() -> String {
    let path = socket_path();
    if UnixStream::connect(&path).is_ok() {
        format!("browser session running at {}", path.display())
    } else {
        "no browser session".into()
    }
}
