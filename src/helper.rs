use std::io;
use std::io::ErrorKind;
use std::mem;

use bincode::{DefaultOptions, Options};
use nix::sys::stat::mode_t;

use crate::FileType;

pub trait Apply: Sized {
    fn apply<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut Self),
    {
        f(&mut self);
        self
    }
}

impl<T> Apply for T {}

#[inline]
pub fn get_first_null_position(data: impl AsRef<[u8]>) -> Option<usize> {
    data.as_ref().iter().position(|char| *char == 0)
}

// Some platforms like Linux x86_64 have mode_t = u32, and lint warns of a trivial_numeric_casts.
// But others like macOS x86_64 have mode_t = u16, requiring a typecast. So, just silence lint.
#[allow(trivial_numeric_casts)]
/// returns the mode for a given file kind and permission
pub fn mode_from_kind_and_perm(kind: FileType, perm: u16) -> u32 {
    mode_t::from(kind) | perm as u32
}

/// returns the permission for a given file kind and mode
pub fn perm_from_mode_and_kind(kind: FileType, mode: u32) -> u16 {
    (mode ^ mode_t::from(kind)) as u16
}

#[inline]
pub fn get_padding_size(dir_entry_size: usize) -> usize {
    let entry_size = (dir_entry_size + mem::size_of::<u64>() - 1) & !(mem::size_of::<u64>() - 1); // 64bit align

    entry_size - dir_entry_size
}

#[inline]
pub fn io_error_from_nix_error(err: nix::Error) -> io::Error {
    match err {
        nix::Error::Sys(errno) => io::Error::from_raw_os_error(errno as i32),
        nix::Error::UnsupportedOperation => io::Error::new(ErrorKind::Other, err),
        nix::Error::InvalidPath | nix::Error::InvalidUtf8 => {
            io::Error::from(ErrorKind::InvalidInput)
        }
    }
}

pub fn get_bincode_config() -> impl Options {
    DefaultOptions::new()
        .with_little_endian()
        .allow_trailing_bytes()
        .with_fixint_encoding()
}
