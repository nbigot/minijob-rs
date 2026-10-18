//! TLS setup: rustls with the `ring` provider (no C toolchain needed for cross builds).

use std::sync::Once;

/// Installs `ring` as the process-wide rustls provider (idempotent).
pub fn install_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
