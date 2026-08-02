//! Stable C ABI for parsing and headless execution.

use koto_core::{Backend, CoreError, Observation, ObserveMode, TraceEntry, Vm, Wait};
use std::{
    collections::BTreeSet,
    ffi::{CStr, CString, c_char, c_int},
    time::Duration,
};

struct Runtime {
    hypr: koto_hypr::HyprBackend,
    tmux: koto_tmux::Tmux,
    web: Option<koto_web::WebEngine>,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            hypr: koto_hypr::HyprBackend::default(),
            tmux: koto_tmux::Tmux::default(),
            web: None,
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
            if let Some(web) = self.web.as_mut() {
                let source = if web.is_bw() { "betterwright" } else { "cdp" };
                if let Ok(Some(text)) = web.action("read", &[], Duration::from_secs(5)) {
                    return Ok(Observation {
                        source: source.into(),
                        fidelity: "exact, structured".into(),
                        text: Some(text),
                        image: None,
                    });
                }
            }
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
    fn web(
        &mut self,
        action: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Option<String>, CoreError> {
        if action == "attach" {
            let spec = koto_web::parse_attach(args)?;
            if !matches!(spec, koto_web::AttachSpec::Bw { .. }) {
                return Err(CoreError::Unsupported(
                    "web attach in library context supports only the betterwright engine (web attach bw)".into(),
                ));
            }
            self.web = Some(koto_web::attach(spec)?);
            return Ok(None);
        }
        self.web
            .as_mut()
            .ok_or_else(|| CoreError::Backend("web attach has not established a CDP pipe".into()))?
            .action(action, args, timeout)
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
    let capabilities = unsafe { caps(capabilities) };
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
            if !output.is_null() && output_len > 0 {
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
/// Executes basm and returns a JSON envelope describing the run. The buffer is
/// owned by the caller and must be released with `koto_free`; never NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn koto_run_basm_json(
    source: *const c_char,
    capabilities: *const c_char,
) -> *mut c_char {
    let started = std::time::Instant::now();
    if source.is_null() {
        return envelope_error(
            8,
            "source pointer is null",
            &[],
            started.elapsed().as_millis(),
        );
    }
    let Ok(source) = (unsafe { CStr::from_ptr(source) }).to_str() else {
        return envelope_error(
            8,
            "source is not valid UTF-8",
            &[],
            started.elapsed().as_millis(),
        );
    };
    let capabilities = unsafe { caps(capabilities) };
    let program = match koto_core::parse_script(source) {
        Ok(program) => program,
        Err(error) => {
            return envelope_error(8, &error.to_string(), &[], started.elapsed().as_millis());
        }
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
        Ok(execution) => into_raw(serde_json::json!({
            "status": "ok",
            "exit": execution.registers.status.clamp(0, 255),
            "error": serde_json::Value::Null,
            "out": execution.registers.out,
            "elapsed_ms": execution.elapsed_ms,
            "observation": execution.observation.as_ref().map(observation_json),
            "trace": trace_json(&execution.trace),
        })),
        Err(error) => {
            let trace = std::mem::take(&mut vm.last_trace);
            envelope_error(
                code(&error),
                &error.to_string(),
                &trace,
                started.elapsed().as_millis(),
            )
        }
    }
}

/// Releases a buffer returned by `koto_run_basm_json`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn koto_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

unsafe fn caps(capabilities: *const c_char) -> BTreeSet<String> {
    if capabilities.is_null() {
        return BTreeSet::new();
    }
    unsafe { CStr::from_ptr(capabilities) }
        .to_str()
        .unwrap_or("")
        .split(',')
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}
fn observation_json(observation: &Observation) -> serde_json::Value {
    serde_json::json!({
        "source": observation.source,
        "fidelity": observation.fidelity,
        "text": observation.text,
        "image_path": observation.image,
    })
}
fn trace_json(trace: &[TraceEntry]) -> serde_json::Value {
    serde_json::to_value(trace).unwrap_or_else(|_| serde_json::Value::Array(Vec::new()))
}
fn envelope_error(
    exit: c_int,
    message: &str,
    trace: &[TraceEntry],
    elapsed_ms: u128,
) -> *mut c_char {
    into_raw(serde_json::json!({
        "status": "error",
        "exit": exit,
        "error": message,
        "out": "",
        "elapsed_ms": elapsed_ms,
        "observation": serde_json::Value::Null,
        "trace": trace_json(trace),
    }))
}
fn into_raw(value: serde_json::Value) -> *mut c_char {
    let text = value.to_string().replace('\0', "\u{fffd}");
    CString::new(text).unwrap_or_default().into_raw()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn run_json(source: &str, capabilities: &str) -> serde_json::Value {
        let source = CString::new(source).unwrap();
        let capabilities = CString::new(capabilities).unwrap();
        let ptr = unsafe { koto_run_basm_json(source.as_ptr(), capabilities.as_ptr()) };
        assert!(!ptr.is_null());
        let text = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned();
        unsafe { koto_free(ptr) };
        serde_json::from_str(&text).unwrap()
    }
    #[test]
    fn trivial_program_yields_an_ok_envelope() {
        let value = run_json("nop", "");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["exit"], 0);
        assert!(value["error"].is_null());
        assert_eq!(value["out"], "");
        assert!(value["elapsed_ms"].is_number());
        assert_eq!(value["trace"][0]["op"], "nop");
    }
    #[test]
    fn parse_failure_yields_an_error_envelope() {
        let value = run_json("@@@ not basm @@@", "");
        assert_eq!(value["status"], "error");
        assert_eq!(value["exit"], 8);
        assert!(value["error"].as_str().is_some_and(|msg| !msg.is_empty()));
        assert_eq!(value["trace"].as_array().unwrap().len(), 0);
    }
    #[test]
    fn denied_capability_yields_exit_five() {
        let value = run_json("require web\nnop", "");
        assert_eq!(value["status"], "error");
        assert_eq!(value["exit"], 5);
        assert!(value["observation"].is_null());
    }
    #[test]
    fn zero_length_output_buffer_is_never_written() {
        let source = CString::new("nop").unwrap();
        let capabilities = CString::new("").unwrap();
        let mut buffer = [0x41u8; 4];
        let exit = unsafe {
            koto_run_basm(
                source.as_ptr(),
                capabilities.as_ptr(),
                buffer.as_mut_ptr().cast(),
                0,
            )
        };
        assert_eq!(exit, 0);
        assert_eq!(buffer, [0x41u8; 4]);
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
