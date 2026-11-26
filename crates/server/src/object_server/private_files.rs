//! Reading, and creating when absent, the owner-only files the server keeps in
//! `object_server.credentials_dir` (ADR-462, ADR-491, ADR-505): the credential files
//! ([`super::credentials`]) and the TLS key and certificate ([`super::tls`]) share these steps.
//! The caller judges what a file holds; nothing here looks inside it.

use std::io::{self, Read};
use std::path::Path;

use shared_crypto::Zeroizing;

use crate::fs_guard::{self, FileFault, LinkFault};

/// Why a file could not be read or created.
#[derive(Debug)]
pub(super) enum Fault {
    /// The file is unsafe or an operation on it failed ([`fs_guard::open_private`], a read, the
    /// link).
    File(FileFault),
    /// The operating system has no randomness for the temporary file's name or the value; the
    /// source is the generator's error.
    Random(Box<dyn std::error::Error + Send + Sync>),
    /// The file is longer than the bound the caller gave.
    TooLong,
}

/// A file's bytes, and whether the call created the file.
pub(super) struct Loaded {
    pub bytes: Zeroizing<Vec<u8>>,
    pub created: bool,
}

/// Reads `path` if it exists: `None` when it is absent. The file is judged as
/// [`fs_guard::open_private`] says and read up to `max_len` bytes; a longer one is
/// [`Fault::TooLong`].
pub(super) fn read_existing(
    path: &Path,
    max_len: u64,
) -> Result<Option<Zeroizing<Vec<u8>>>, Fault> {
    let file = fs_guard::open_private(path).map_err(Fault::File)?;
    file.map(|file| read_bounded(file, max_len)).transpose()
}

/// Reads `path`, creating it first from `make` when it is absent ([`fs_guard::create_by_link`]
/// in `dir` with temporary names beginning `tmp_prefix`). Of several creators one wins, and every
/// one returns what the file holds, which is read back, not what its own `make` returned. A
/// failure of `make` is returned as it is and nothing is created.
pub(super) fn read_or_create<E: From<Fault>>(
    dir: &Path,
    path: &Path,
    tmp_prefix: &str,
    max_len: u64,
    make: impl FnOnce() -> Result<Zeroizing<Vec<u8>>, E>,
) -> Result<Loaded, E> {
    let mut created = false;
    let file = match fs_guard::open_private(path).map_err(Fault::File)? {
        Some(file) => file,
        None => {
            let value = make()?;
            created = match fs_guard::create_by_link(dir, path, tmp_prefix, &value) {
                Ok(created) => created,
                Err(LinkFault::Random(source)) => return Err(Fault::Random(source.into()).into()),
                Err(LinkFault::Io { op, error }) => {
                    return Err(Fault::File(FileFault::Io {
                        op,
                        kind: error.kind(),
                    })
                    .into());
                }
            };
            fs_guard::open_private(path)
                .map_err(Fault::File)?
                .ok_or(Fault::File(FileFault::Io {
                    op: "read back",
                    kind: io::ErrorKind::NotFound,
                }))?
        }
    };
    Ok(Loaded {
        bytes: read_bounded(file, max_len)?,
        created,
    })
}

fn read_bounded(file: std::fs::File, max_len: u64) -> Result<Zeroizing<Vec<u8>>, Fault> {
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(max_len + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            Fault::File(FileFault::Io {
                op: "read",
                kind: error.kind(),
            })
        })?;
    if bytes.len() as u64 > max_len {
        return Err(Fault::TooLong);
    }
    Ok(bytes)
}
