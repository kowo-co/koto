//! Stable C ABI for parsing and headless execution.

use koto_core::{Backend, CoreError, Observation, ObserveMode, Vm, Wait};
use std::{
    collections::BTreeSet,
    ffi::{CStr, c_char, c_int},
    time::Duration,
};

struct Runtime {
    hypr: koto_hypr::HyprBackend,
    tmux: koto_tmux::Tmux,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            hypr: koto_hypr::HyprBackend::default(),
            tmux: koto_tmux::Tmux::default(),
        }
    }
}
impl Backend for Runtime {
    fn key(&mut self, keys: &[String]) -> Result<(), CoreError> {
        self.hypr.key(keys)
    }
    fn key_state(&mut self, keys: &[String], pressed: bool) -> Result<(), CoreError> {
        self.hypr.key_state(keys, pressed)
    }
    fn pointer(&mut self, action: &str, args: &[String]) -> Result<(), CoreError> {
        self.hypr.pointer(action, args)
    }
    fn text(&mut self, text: &str, paste: bool) -> Result<(), CoreError> {
        self.hypr.text(text, paste)
    }
    fn wait(&mut self, wait: &Wait, timeout: Duration) -> Result<(), CoreError> {
        self.hypr.wait(wait, timeout)
    }
    fn focus(&mut self, selector: &str) -> Result<(), CoreError> {
        self.hypr.focus(selector)
    }
    fn observe(&mut self, mode: ObserveMode) -> Result<Observation, CoreError> {
        if mode != ObserveMode::Image {
            if let Ok(text) = self.tmux.read_active(None) {
                if !text.is_empty() {
                    return Ok(Observation {
                        source: "tmux".into(),
                        fidelity: "exact".into(),
                        text: Some(text),
                        image: None,
                    });
                }
            }
        }
        self.hypr.observe(mode)
    }
    fn list(&mut self, subject: &str) -> Result<String, CoreError> {
        self.hypr.list(subject)
    }
    fn pane(
        &mut self,
        action: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Option<String>, CoreError> {
        match action {
            "new" => Ok(Some(self.tmux.new_pane(args.first().map(String::as_str))?)),
            "send" => {
                self.tmux.send(&args.join(" "), false)?;
                Ok(None)
            }
            "run" => {
                self.tmux.send(&args.join(" "), true)?;
                Ok(None)
            }
            "read" => Ok(Some(
                self.tmux.read(args.first().and_then(|v| v.parse().ok()))?,
            )),
            "wait" => {
                self.tmux.wait(
                    args.first()
                        .ok_or_else(|| CoreError::Parse("pane wait needs a pattern".into()))?,
                    timeout,
                )?;
                Ok(None)
            }
            "kill" => {
                self.tmux.kill(args.first().map(String::as_str))?;
                Ok(None)
            }
            _ => Err(CoreError::Parse(format!("unknown pane action `{action}`"))),
        }
    }
    fn spawn(&mut self, command: &[String]) -> Result<String, CoreError> {
        self.hypr.spawn(command)
    }
    fn kill(&mut self, selector: &str) -> Result<(), CoreError> {
        self.hypr.kill(selector)
    }
    fn window(&mut self, action: &str, args: &[String]) -> Result<(), CoreError> {
        self.hypr.window(action, args)
    }
    fn checkpoint(&mut self, name: &str, rollback: bool) -> Result<(), CoreError> {
        self.hypr.checkpoint(name, rollback)
    }
    fn metadata(&mut self, field: &str) -> Result<String, CoreError> {
        self.hypr.metadata(field)
    }
    fn selector_count(&mut self, selector: &str) -> Result<usize, CoreError> {
        Ok(self.hypr.resolve_all(selector)?.len())
    }
    fn focused_window(&mut self) -> Result<koto_core::WindowRecord, CoreError> {
        self.hypr.focused_window()
    }
}

/// ABI version, incremented only for incompatible C header changes.
#[unsafe(no_mangle)]
pub extern "C" fn koto_abi_version() -> c_int {
    1
}

/// Validate a UTF-8, NUL-terminated basm program. Returns 0 on success and 8 for a basm parse error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn koto_parse_basm(source: *const c_char) -> c_int {
    if source.is_null() {
        return 8;
    }
    let Ok(source) = unsafe { CStr::from_ptr(source) }.to_str() else {
        return 8;
    };
    if koto_core::parse_script(source).is_ok() {
        0
    } else {
        8
    }
}

/// Executes basm and writes `$out` to `output` when supplied. `capabilities` is
/// a comma-separated list; an empty string grants no capabilities. The ABI has
/// no ambient default grants.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn koto_run_basm(
    source: *const c_char,
    capabilities: *const c_char,
    output: *mut c_char,
    output_len: usize,
) -> c_int {
    if source.is_null() {
        return 8;
    }
    let Ok(source) = unsafe { CStr::from_ptr(source) }.to_str() else {
        return 8;
    };
    let capabilities = if capabilities.is_null() {
        BTreeSet::new()
    } else {
        unsafe { CStr::from_ptr(capabilities) }
            .to_str()
            .unwrap_or("")
            .split(',')
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let program = match koto_core::parse_script(source) {
        Ok(program) => program,
        Err(_) => return 8,
    };
    let mut backend = Runtime::default();
    let mut vm = Vm {
        backend: &mut backend,
        capabilities,
        op_budget: koto_core::DEFAULT_OP_BUDGET,
        time_budget: koto_core::DEFAULT_TIME_BUDGET,
        default_timeout: koto_core::DEFAULT_TIMEOUT,
        registers: Default::default(),
        last_trace: Vec::new(),
        cancel_file: None,
    };
    match vm.run(&program) {
        Ok(execution) => {
            if !output.is_null() {
                let bytes = execution.registers.out.as_bytes();
                let count = bytes.len().min(output_len.saturating_sub(1));
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), output.cast(), count);
                    *output.add(count) = 0;
                }
            }
            execution.registers.status.clamp(0, 255)
        }
        Err(error) => code(&error),
    }
}
fn code(error: &CoreError) -> c_int {
    match error {
        CoreError::Assertion(_) => 1,
        CoreError::Timeout(_) => 2,
        CoreError::SelectorNotFound(_) => 3,
        CoreError::Budget(_) => 4,
        CoreError::Capability(_) => 5,
        CoreError::SelectorAmbiguous(_) => 6,
        CoreError::ObservationUnavailable(_) => 7,
        CoreError::Parse(_) => 8,
        CoreError::Backend(_) | CoreError::Unsupported(_) | CoreError::Aborted => 9,
    }
}
