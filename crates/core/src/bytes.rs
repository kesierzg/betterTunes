//! Bounds-checked little-endian readers. Library files are untrusted input,
//! so every read returns `None` instead of panicking when out of range.

fn array<const N: usize>(b: &[u8], off: usize) -> Option<[u8; N]> {
    b.get(off..off.checked_add(N)?)?.try_into().ok()
}

pub(crate) fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    array(b, off).map(u16::from_le_bytes)
}

pub(crate) fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    array(b, off).map(u32::from_le_bytes)
}

pub(crate) fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    array(b, off).map(u64::from_le_bytes)
}

pub(crate) fn f32_at(b: &[u8], off: usize) -> Option<f32> {
    array(b, off).map(f32::from_le_bytes)
}

/// Treats zero (unset in iTunes records) as absent.
pub(crate) fn nonzero<T: Default + PartialEq>(v: Option<T>) -> Option<T> {
    v.filter(|v| *v != T::default())
}

pub(crate) fn utf16le(raw: &[u8]) -> String {
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect();
    String::from_utf16_lossy(&units)
}
