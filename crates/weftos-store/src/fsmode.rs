//! The execute attribute and private-file modes, behind one `cfg(unix)` boundary.
//!
//! Unix carries the execute attribute as the 0111 mode bits. Windows has no such bit: a file is
//! "executable" by its extension, so there is nothing to read or set. On Windows these helpers
//! report no execute bit and set nothing, and the callers that compare against a recorded value
//! use [`observed_exec`] so a file whose bit cannot be observed is never mistaken for a chmod.
//! Nothing here changes what Linux and macOS do.

use std::fs::Metadata;
use std::io;
use std::path::Path;

/// Whether the file has any execute bit. Always false where the platform has no such bit.
pub(crate) fn has_exec_bit(md: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        false
    }
}

/// The execute attribute of `md` for comparison against a `recorded` one. Where the platform
/// cannot show it, the recorded value stands, so an unobservable attribute is not read as a local
/// change (a Windows edit of a file a Unix peer marked executable must not strip the bit).
pub(crate) fn observed_exec(md: &Metadata, recorded: bool) -> bool {
    if cfg!(unix) { has_exec_bit(md) } else { recorded }
}

/// Make the file 0755. A no-op where there is no execute bit.
pub(crate) fn make_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Apply the execute attribute in place: 0755 when `exec`, else the file's own mode with every
/// execute bit cleared. A no-op where there is no execute bit.
pub(crate) fn apply_exec(path: &Path, md: &Metadata, exec: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = md.permissions().mode();
        let new_mode = if exec { 0o755 } else { mode & !0o111 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(new_mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, md, exec);
        Ok(())
    }
}

/// Whether this platform can store the execute attribute on a file at all.
pub(crate) const EXEC_BIT_SUPPORTED: bool = cfg!(unix);

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn exec_bits_are_read_set_and_cleared_exactly_as_before() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        std::fs::write(&p, b"x").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!has_exec_bit(&std::fs::metadata(&p).unwrap()));
        make_executable(&p).unwrap();
        assert_eq!(mode(&p), 0o755, "make_executable is 0755");
        assert!(has_exec_bit(&std::fs::metadata(&p).unwrap()));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o775)).unwrap();
        apply_exec(&p, &std::fs::metadata(&p).unwrap(), false).unwrap();
        assert_eq!(mode(&p), 0o664, "clearing keeps the other bits");
        apply_exec(&p, &std::fs::metadata(&p).unwrap(), true).unwrap();
        assert_eq!(mode(&p), 0o755);
        assert!(observed_exec(&std::fs::metadata(&p).unwrap(), false), "on unix the file's bit wins over the record");
    }
}
