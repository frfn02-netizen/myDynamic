use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{SocketAddr, UnixListener};

/// Keeps an abstract Unix socket bound for the lifetime of the process.
///
/// Linux abstract socket names disappear automatically when the process exits,
/// so crashed instances do not leave stale locks behind. The name is scoped to
/// the UID, session runtime directory, and Wayland display to avoid colliding
/// with other users, other graphical sessions, or unrelated applications.
pub struct SingleInstance {
    _listener: UnixListener,
}

impl SingleInstance {
    /// Returns `Ok(None)` when another instance already owns the lock.
    pub fn acquire() -> io::Result<Option<Self>> {
        Self::acquire_with_name(&instance_name())
    }

    fn acquire_with_name(name: &str) -> io::Result<Option<Self>> {
        let address = SocketAddr::from_abstract_name(name.as_bytes())?;
        match UnixListener::bind_addr(&address) {
            Ok(listener) => Ok(Some(Self {
                _listener: listener,
            })),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn instance_name() -> String {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    let session = std::env::var("XDG_SESSION_ID").unwrap_or_default();
    instance_name_for(&runtime, &display, &session, current_uid())
}

fn current_uid() -> u32 {
    std::fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .unwrap_or(u32::MAX)
}

fn instance_name_for(runtime: &str, display: &str, session: &str, uid: u32) -> String {
    let identity = format!("{uid}\u{1f}{runtime}\u{1f}{display}\u{1f}{session}");
    format!("glass-island-{:016x}", fnv1a(identity.as_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_instance_is_rejected_and_lock_is_released() {
        let name = format!(
            "glass-island-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );

        let first = SingleInstance::acquire_with_name(&name)
            .expect("first acquire")
            .expect("first lock");
        assert!(
            SingleInstance::acquire_with_name(&name)
                .expect("second acquire")
                .is_none()
        );

        drop(first);
        assert!(
            SingleInstance::acquire_with_name(&name)
                .expect("third acquire")
                .is_some()
        );
    }

    #[test]
    fn instance_identity_is_scoped_to_uid_session_and_display() {
        let same = instance_name_for("/run/user/1000", "wayland-0", "8", 1000);
        assert_eq!(
            same,
            instance_name_for("/run/user/1000", "wayland-0", "8", 1000)
        );
        assert_ne!(
            same,
            instance_name_for("/run/user/1001", "wayland-0", "8", 1001)
        );
        assert_ne!(
            same,
            instance_name_for("/run/user/1000", "wayland-1", "8", 1000)
        );
        assert_ne!(
            same,
            instance_name_for("/run/user/1000", "wayland-0", "9", 1000)
        );
        assert!(same.starts_with("glass-island-"));
    }
}
