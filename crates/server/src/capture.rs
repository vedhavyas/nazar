//! Capturing this thread's tracing output in tests.
//!
//! The subscriber is process-global and routes each event to the emitting
//! thread's buffer: tracing caches a callsite's interest process-wide, so a
//! thread-local subscriber can go silent once a sibling test has run the
//! same line without one. The capture only sees events emitted on the
//! capturing thread, so the code under test must run there - the default
//! current-thread tokio runtime keeps it there.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

thread_local! {
    static CAPTURE: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

/// The events this thread logs while the value lives.
pub struct Captured {
    buffer: Arc<Mutex<Vec<u8>>>,
    previous: Option<Arc<Mutex<Vec<u8>>>>,
}

impl Captured {
    pub fn start() -> Self {
        static INSTALL: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        INSTALL.get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(CaptureWriter)
                .with_ansi(false)
                .finish();
            let installed = tracing::subscriber::set_global_default(subscriber);
            assert!(
                installed.is_ok(),
                "the capture subscriber could not be installed: {:?}",
                installed.err()
            );
        });
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let previous = CAPTURE.with(|slot| slot.replace(Some(Arc::clone(&buffer))));
        Self { buffer, previous }
    }

    pub fn text(&self) -> String {
        let buffer = self
            .buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&buffer).into_owned()
    }
}

impl Drop for Captured {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CAPTURE.with(|slot| *slot.borrow_mut() = previous);
    }
}

struct CaptureWriter;

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURE.with(|slot| {
            if let Some(buffer) = slot.borrow().as_ref() {
                buffer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A nested capture puts the outer buffer back when it goes.
    #[test]
    fn a_nested_capture_restores_the_outer_buffer() {
        let outer = Captured::start();
        tracing::warn!("outer one");
        let inner = Captured::start();
        tracing::warn!("inner one");
        assert!(
            inner.text().contains("inner one"),
            "the inner capture sees its own line: {}",
            inner.text()
        );
        assert!(
            !inner.text().contains("outer one"),
            "and not the outer's: {}",
            inner.text()
        );
        drop(inner);
        tracing::warn!("outer two");
        assert!(
            outer.text().contains("outer two"),
            "the outer capture is live again: {}",
            outer.text()
        );
        assert!(
            !outer.text().contains("inner one"),
            "and did not take the inner's lines: {}",
            outer.text()
        );
    }
}
