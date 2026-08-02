use anyhow::Result;

/// Routes writes through the DaemonKuzu file-drop protocol instead of
/// opening a direct embedded Kuzu connection. See
/// docs/superpowers/specs/2026-08-01-daemonkuzu-daemon-wiring-design.md.
///
/// This is a placeholder: every method panics. Task 12/13 of the
/// implementation plan replace this with the real three-tier wrapper
/// (read passthrough / covered-write routing / loud error for everything
/// else).
pub struct DaemonKuzuBackend;

impl DaemonKuzuBackend {
    pub fn open(_root: &std::path::Path) -> Result<Self> {
        Ok(Self)
    }
}
