//! Pieces shared by iTunes' `.itl` and Music.app's `.musicdb` formats, which
//! descend from the same design: a body encrypted with the same key and
//! zlib-compressed, holding tagged records whose children carry strings.

use crate::bytes::utf16le;
use crate::{Error, Result, Track};
use aes::cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};
use aes::Aes128;
use flate2::read::ZlibDecoder;
use std::io::Read;

pub(crate) const KEY: &[u8; 16] = b"BHUILuilfghuila3";

/// Name of the hidden playlist that holds the whole library.
pub(crate) const MASTER_PLAYLIST_NAME: &str = "####!####";

/// Decrypts the first `crypt_size` bytes of a library body (rounded down to
/// the AES block size; 0 means all of it) and inflates the result if it is
/// zlib-compressed.
pub(crate) fn decode_body(mut body: Vec<u8>, crypt_size: usize) -> Result<Vec<u8>> {
    let crypt_size = if crypt_size == 0 {
        body.len()
    } else {
        crypt_size
    };
    let encrypted = crypt_size.min(body.len()) / 16 * 16;
    let cipher = Aes128::new(GenericArray::from_slice(KEY));
    for block in body[..encrypted].as_chunks_mut::<16>().0 {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    // Zlib streams start with 0x78; very old libraries are not compressed.
    if body.first() != Some(&0x78) {
        return Ok(body);
    }
    let mut out = Vec::new();
    ZlibDecoder::new(&body[..])
        .read_to_end(&mut out)
        .map_err(|e| Error::format(format!("decompression failed: {e}")))?;
    Ok(out)
}

/// Splits consecutive tagged records: 4-byte tag, u32 header length @4,
/// u32 total length (header and children) @8.
pub(crate) fn records(mut b: &[u8]) -> Result<Vec<&[u8]>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let len = crate::bytes::u32_at(b, 8).unwrap_or(0) as usize;
        if len < 12 || len > b.len() {
            return Err(Error::format("bad record length"));
        }
        out.push(&b[..len]);
        b = &b[len..];
    }
    Ok(out)
}

/// The records following `parent`'s header.
pub(crate) fn children(parent: &[u8]) -> Result<Vec<&[u8]>> {
    let header_len = crate::bytes::u32_at(parent, 4).unwrap_or(0) as usize;
    let body = parent
        .get(header_len..)
        .ok_or_else(|| Error::format("record header past end"))?;
    records(body)
}

/// The items of a list chunk tagged `list_tag`, all of which must be tagged
/// `item_tag`.
pub(crate) fn list_items<'a>(
    list: &'a [u8],
    list_tag: &[u8],
    item_tag: &[u8],
) -> Result<Vec<&'a [u8]>> {
    if list.get(..4) != Some(list_tag) {
        return Err(Error::format(format!(
            "expected {} list",
            String::from_utf8_lossy(list_tag)
        )));
    }
    let items = children(list)?;
    if items.iter().any(|r| &r[..4] != item_tag) {
        return Err(Error::format(format!(
            "unexpected record in {} list",
            String::from_utf8_lossy(list_tag)
        )));
    }
    Ok(items)
}

pub(crate) fn subtype(rec: &[u8]) -> u32 {
    crate::bytes::u32_at(rec, 12).unwrap_or(0)
}

/// Decodes string data by its encoding field.
pub(crate) fn text(encoding: u32, raw: &[u8]) -> String {
    match encoding {
        1 => utf16le(raw),
        2 => String::from_utf8_lossy(raw).into_owned(),
        // 3 (and the rare 0) is a single-byte Western encoding. Bytes
        // 0xA0-0xFF were verified as Latin-1; 0x80-0x9F are assumed to be
        // Windows-1252, which iTunes inherited from its Windows side.
        _ => raw.iter().map(|&b| cp1252(b)).collect(),
    }
}

fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[usize::from(b - 0x80)],
        _ => char::from(b),
    }
}

/// Stores a track string by its subtype, numbered the same in both formats.
pub(crate) fn set_track_string(t: &mut Track, subtype: u32, value: String) {
    use crate::model::file_url_to_path;
    match subtype {
        2 => t.title = Some(value),
        3 => t.album = Some(value),
        4 => t.artist = Some(value),
        5 => t.genre = Some(value),
        6 => t.kind = Some(value),
        11 => t.location = file_url_to_path(&value),
        12 => t.composer = Some(value),
        27 => t.album_artist = Some(value),
        _ => {}
    }
}

/// Sample rates are stored as f32.
pub(crate) fn sample_rate(v: Option<f32>) -> Option<u32> {
    v.filter(|r| r.is_finite() && *r > 0.0).map(|r| r as u32)
}

/// Builders for synthetic library files in tests.
#[cfg(test)]
pub(crate) mod build {
    use super::KEY;
    use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
    use aes::Aes128;
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;

    pub fn u32s(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    /// A record with `header_len` bytes of header and the given children;
    /// `fill` writes fields into the header. @12 holds the child count, as in
    /// tracks and playlists (and the subtype, for leaf records).
    pub fn record(
        tag: &[u8],
        header_len: usize,
        kids: &[Vec<u8>],
        fill: impl Fn(&mut [u8]),
    ) -> Vec<u8> {
        let mut h = vec![0; header_len];
        h[..4].copy_from_slice(tag);
        h[4..8].copy_from_slice(&u32s(header_len as u32));
        let total = header_len + kids.iter().map(Vec::len).sum::<usize>();
        h[8..12].copy_from_slice(&u32s(total as u32));
        h[12..16].copy_from_slice(&u32s(kids.len() as u32));
        fill(&mut h);
        h.extend(kids.concat());
        h
    }

    /// A leaf record of `subtype` whose data starts at offset 16.
    pub fn leaf(tag: &[u8], subtype: u32, data: &[u8]) -> Vec<u8> {
        let mut r = tag.to_vec();
        r.extend(u32s(if tag == b"boma" { 20 } else { 24 }));
        r.extend(u32s(16 + data.len() as u32));
        r.extend(u32s(subtype));
        r.extend(data);
        r
    }

    /// A string leaf whose encoding field sits at `enc_off` (byte length
    /// after it, text 12 bytes after that).
    pub fn string_leaf(
        tag: &[u8],
        subtype: u32,
        enc_off: usize,
        encoding: u32,
        s: &str,
    ) -> Vec<u8> {
        let raw: Vec<u8> = match encoding {
            1 => s.encode_utf16().flat_map(u16::to_le_bytes).collect(),
            2 => s.as_bytes().to_vec(),
            _ => s
                .chars()
                .map(|c| u8::try_from(u32::from(c)).unwrap())
                .collect(),
        };
        let mut data = vec![0; enc_off];
        data[enc_off - 16..enc_off - 12].copy_from_slice(&u32s(encoding));
        data[enc_off - 12..enc_off - 8].copy_from_slice(&u32s(raw.len() as u32));
        data.extend(raw);
        leaf(tag, subtype, &data)
    }

    /// Compresses `payload` and encrypts its first `crypt_size` bytes.
    pub fn encrypt_body(payload: &[u8], crypt_size: u32) -> Vec<u8> {
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(payload).unwrap();
        let mut body = z.finish().unwrap();
        let n = (crypt_size as usize).min(body.len()) / 16 * 16;
        let cipher = Aes128::new(GenericArray::from_slice(KEY));
        for block in body[..n].as_chunks_mut::<16>().0 {
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_text_encodings() {
        assert_eq!(text(1, &[0x41, 0x01, b'o', 0]), "Ło");
        assert_eq!(text(2, "Bóg".as_bytes()), "Bóg");
        assert_eq!(text(3, b"H\xe5stad APH\xd8TIC"), "Håstad APHØTIC");
        assert_eq!(text(3, b"don\x92t"), "don’t");
    }
}
