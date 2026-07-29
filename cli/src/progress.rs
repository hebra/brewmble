use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const ENV_OVERRIDE: &str = "BREWMBLE_NATIVE_PROGRESS";

type Writer = Arc<dyn Fn(&[u8]) + Send + Sync>;

pub struct NativeProgress {
    supported: bool,
    active: AtomicBool,
    writer: Writer,
}

impl NativeProgress {
    pub fn new() -> Self {
        Self::with_writer(detect_native_progress(), Arc::new(default_writer))
    }

    fn with_writer(supported: bool, writer: Writer) -> Self {
        Self {
            supported,
            active: AtomicBool::new(false),
            writer,
        }
    }

    pub fn is_supported(&self) -> bool {
        self.supported
    }

    pub fn set_indeterminate(&self) {
        self.emit(3, 0);
    }

    pub fn set_progress(&self, pos: u64, len: Option<u64>) {
        let Some(len) = len else {
            self.set_indeterminate();
            return;
        };
        if len == 0 {
            self.set_indeterminate();
            return;
        }
        let pct = ((pos as f64 / len as f64) * 100.0).clamp(0.0, 100.0) as u8;
        self.emit(1, pct);
    }

    pub fn finish_success(&self) {
        self.emit(1, 100);
    }

    pub fn finish_error(&self) {
        self.emit(2, 100);
    }

    pub fn clear(&self) {
        self.emit(0, 0);
    }

    fn emit(&self, state: u8, progress: u8) {
        if !self.supported {
            return;
        }
        let seq = format!("\x1b]9;4;{};{}\x07", state, progress);
        (self.writer)(seq.as_bytes());
        self.active.store(state != 0, Ordering::Relaxed);
    }
}

impl Drop for NativeProgress {
    fn drop(&mut self) {
        if self.supported && self.active.load(Ordering::Relaxed) {
            let clear = b"\x1b]9;4;0;0\x07";
            (self.writer)(clear);
        }
    }
}

fn default_writer(data: &[u8]) {
    let _ = std::io::stderr().write_all(data);
}

pub fn detect_native_progress() -> bool {
    match std::env::var(ENV_OVERRIDE).as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_writer() -> (Arc<Mutex<Vec<u8>>>, Writer) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let buf_clone = Arc::clone(&buf);
        let writer = Arc::new(move |data: &[u8]| {
            buf_clone.lock().unwrap().extend_from_slice(data);
        });
        (buf, writer)
    }

    #[test]
    fn detect_respects_disable_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var(ENV_OVERRIDE).ok();
        std::env::set_var(ENV_OVERRIDE, "0");
        assert!(!detect_native_progress());
        match previous {
            Some(v) => std::env::set_var(ENV_OVERRIDE, v),
            None => std::env::remove_var(ENV_OVERRIDE),
        }
    }

    #[test]
    fn detect_defaults_to_enabled() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var(ENV_OVERRIDE).ok();
        std::env::remove_var(ENV_OVERRIDE);
        assert!(detect_native_progress());
        match previous {
            Some(v) => std::env::set_var(ENV_OVERRIDE, v),
            None => std::env::remove_var(ENV_OVERRIDE),
        }
    }

    #[test]
    fn unsupported_emits_nothing() {
        let (buf, writer) = test_writer();
        let progress = NativeProgress::with_writer(false, writer);
        progress.set_progress(50, Some(100));
        progress.finish_success();
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn supported_indeterminate() {
        let (buf, writer) = test_writer();
        let progress = NativeProgress::with_writer(true, writer);
        progress.set_indeterminate();
        assert_eq!(&*buf.lock().unwrap(), b"\x1b]9;4;3;0\x07");
    }

    #[test]
    fn supported_progress() {
        let (buf, writer) = test_writer();
        let progress = NativeProgress::with_writer(true, writer);
        progress.set_progress(50, Some(100));
        assert_eq!(&*buf.lock().unwrap(), b"\x1b]9;4;1;50\x07");
    }

    #[test]
    fn supported_progress_clamps_to_100() {
        let (buf, writer) = test_writer();
        let progress = NativeProgress::with_writer(true, writer);
        progress.set_progress(150, Some(100));
        assert_eq!(&*buf.lock().unwrap(), b"\x1b]9;4;1;100\x07");
    }

    #[test]
    fn supported_zero_length_is_indeterminate() {
        let (buf, writer) = test_writer();
        let progress = NativeProgress::with_writer(true, writer);
        progress.set_progress(0, Some(0));
        assert_eq!(&*buf.lock().unwrap(), b"\x1b]9;4;3;0\x07");
    }

    #[test]
    fn supported_none_length_is_indeterminate() {
        let (buf, writer) = test_writer();
        let progress = NativeProgress::with_writer(true, writer);
        progress.set_progress(0, None);
        assert_eq!(&*buf.lock().unwrap(), b"\x1b]9;4;3;0\x07");
    }

    #[test]
    fn supported_finish_success() {
        let (buf, writer) = test_writer();
        let progress = NativeProgress::with_writer(true, writer);
        progress.finish_success();
        assert_eq!(&*buf.lock().unwrap(), b"\x1b]9;4;1;100\x07");
    }

    #[test]
    fn supported_finish_error() {
        let (buf, writer) = test_writer();
        let progress = NativeProgress::with_writer(true, writer);
        progress.finish_error();
        assert_eq!(&*buf.lock().unwrap(), b"\x1b]9;4;2;100\x07");
    }

    #[test]
    fn supported_clear() {
        let (buf, writer) = test_writer();
        let progress = NativeProgress::with_writer(true, writer);
        progress.set_progress(50, Some(100));
        progress.clear();
        assert_eq!(&*buf.lock().unwrap(), b"\x1b]9;4;1;50\x07\x1b]9;4;0;0\x07");
    }

    #[test]
    fn drop_clears_active_progress() {
        let (buf, writer) = test_writer();
        {
            let progress = NativeProgress::with_writer(true, writer);
            progress.set_indeterminate();
        }
        assert_eq!(&*buf.lock().unwrap(), b"\x1b]9;4;3;0\x07\x1b]9;4;0;0\x07");
    }

    #[test]
    fn drop_does_not_clear_after_explicit_clear() {
        let (buf, writer) = test_writer();
        {
            let progress = NativeProgress::with_writer(true, writer);
            progress.finish_success();
            progress.clear();
        }
        assert_eq!(&*buf.lock().unwrap(), b"\x1b]9;4;1;100\x07\x1b]9;4;0;0\x07");
    }
}
