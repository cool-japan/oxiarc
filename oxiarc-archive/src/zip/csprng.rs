//! Operating-system CSPRNG access used by the ZIP encryption paths.
//!
//! Every random byte that ends up in an archive — WinZip-AES salts and the 11
//! random bytes of a traditional ZipCrypto encryption header — is drawn from
//! the platform's cryptographically secure random number generator:
//!
//! | Target family        | Source                                             |
//! |----------------------|----------------------------------------------------|
//! | Unix-like            | `/dev/urandom` (kernel CSPRNG, non-blocking)       |
//! | Windows              | `BCryptGenRandom` with `BCRYPT_USE_SYSTEM_PREFERRED_RNG` |
//! | anything else        | *no source* — a typed error is returned             |
//!
//! There is deliberately **no** software fallback. An earlier revision expanded
//! a clock/PID/ASLR-derived seed with SHA-1 whenever the OS source was
//! unavailable; that produced salts with far less entropy than PBKDF2-SHA1 key
//! derivation requires (a salt collision under AES-CTR is direct keystream
//! reuse). Failing loudly with [`OxiArcError::Io`] is the only sound behaviour:
//! a caller that cannot get real randomness must not get a weak archive that
//! *looks* encrypted.
//!
//! The bindings are pure Rust declarations against the OS-provided library — no
//! C toolchain and no external crate is involved.

use oxiarc_core::error::{OxiArcError, Result};
use std::io;

/// Fill `buf` with cryptographically secure random bytes from the OS.
///
/// # Errors
///
/// Returns [`OxiArcError::Io`] when the platform CSPRNG cannot be reached:
/// `/dev/urandom` missing or unreadable (chroot, seccomp, fd exhaustion),
/// `BCryptGenRandom` reporting a failure status, or a target with no supported
/// entropy source at all. The buffer contents are unspecified on error and must
/// not be used.
pub(crate) fn fill_random(buf: &mut [u8]) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    imp::fill_random(buf)
}

/// Allocate `len` cryptographically secure random bytes.
///
/// # Errors
///
/// See [`fill_random`].
pub(crate) fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; len];
    fill_random(&mut out)?;
    Ok(out)
}

/// Build the error returned when the platform offers no usable CSPRNG.
///
/// Only compiled where it can actually be reached (targets without an entropy
/// source) plus under `cfg(test)`, so no dead code ships on Unix or Windows.
#[cfg(any(not(any(unix, windows)), test))]
fn unsupported_source_error() -> OxiArcError {
    OxiArcError::Io(io::Error::new(
        io::ErrorKind::Unsupported,
        "no operating-system CSPRNG is available on this target; \
         refusing to generate encryption material from a weak source",
    ))
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::fs::File;
    use std::io::Read;

    /// Read from the kernel CSPRNG exposed as `/dev/urandom`.
    pub(super) fn fill_random(buf: &mut [u8]) -> Result<()> {
        let mut file = File::open("/dev/urandom").map_err(|err| {
            OxiArcError::Io(io::Error::new(
                err.kind(),
                format!("failed to open /dev/urandom: {err}"),
            ))
        })?;
        file.read_exact(buf).map_err(|err| {
            OxiArcError::Io(io::Error::new(
                err.kind(),
                format!(
                    "failed to read {} bytes from /dev/urandom: {err}",
                    buf.len()
                ),
            ))
        })?;
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;

    /// `BCRYPT_USE_SYSTEM_PREFERRED_RNG` — use the system-preferred RNG so no
    /// algorithm provider handle has to be opened first.
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

    #[link(name = "bcrypt")]
    unsafe extern "system" {
        #[link_name = "BCryptGenRandom"]
        fn bcrypt_gen_random(
            algorithm: *mut c_void,
            buffer: *mut u8,
            count: u32,
            flags: u32,
        ) -> i32;
    }

    /// Fill `buf` via `BCryptGenRandom`, the documented Windows CSPRNG entry point.
    pub(super) fn fill_random(buf: &mut [u8]) -> Result<()> {
        // BCryptGenRandom takes a ULONG length, so very large buffers are split.
        for chunk in buf.chunks_mut(u32::MAX as usize) {
            let len = u32::try_from(chunk.len()).map_err(|_| {
                OxiArcError::Io(io::Error::other("CSPRNG request length overflowed u32"))
            })?;
            // SAFETY: `chunk` is a valid, uniquely borrowed slice of `len` bytes;
            // BCryptGenRandom only writes `len` bytes into it and does not retain
            // the pointer. A null algorithm handle is required (and only valid)
            // together with BCRYPT_USE_SYSTEM_PREFERRED_RNG.
            let status = unsafe {
                bcrypt_gen_random(
                    core::ptr::null_mut(),
                    chunk.as_mut_ptr(),
                    len,
                    BCRYPT_USE_SYSTEM_PREFERRED_RNG,
                )
            };
            if status != 0 {
                return Err(OxiArcError::Io(io::Error::other(format!(
                    "BCryptGenRandom failed with NTSTATUS {status:#010x}"
                ))));
            }
        }
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::*;

    /// No OS entropy source is known for this target: fail instead of guessing.
    pub(super) fn fill_random(_buf: &mut [u8]) -> Result<()> {
        Err(unsupported_source_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_is_ok() {
        let mut empty: [u8; 0] = [];
        assert!(fill_random(&mut empty).is_ok());
    }

    #[test]
    fn unsupported_platform_yields_typed_error() {
        // The `#[cfg(not(any(unix, windows)))]` arm returns this error instead of
        // silently manufacturing low-entropy bytes. Assert the shape here so the
        // contract is covered on every host.
        let err = unsupported_source_error();
        match err {
            OxiArcError::Io(io_err) => {
                assert_eq!(io_err.kind(), io::ErrorKind::Unsupported);
            }
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn os_csprng_produces_distinct_high_entropy_output() {
        let a = random_bytes(32).expect("OS CSPRNG must be available on this target");
        let b = random_bytes(32).expect("OS CSPRNG must be available on this target");
        assert_eq!(a.len(), 32);
        assert_eq!(b.len(), 32);
        assert_ne!(a, b, "two 32-byte CSPRNG draws must not be identical");
        assert!(
            a.iter().any(|&byte| byte != a[0]),
            "CSPRNG output must not be a constant byte pattern"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn os_csprng_fills_odd_lengths() {
        for len in [1usize, 7, 11, 16, 33, 4096] {
            let bytes = random_bytes(len).expect("OS CSPRNG must be available on this target");
            assert_eq!(bytes.len(), len);
        }
    }
}
