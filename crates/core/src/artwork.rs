//! Cover art for iPods and iPhone OS devices: taken from the song files,
//! rendered in each size the device asks for, and described by an ArtworkDB.
//!
//! Each format's images go one after another into `F<format>_1.ithmb`, each
//! starting on the format's offset alignment (4096 on the iPod touch). Pixels
//! are 16-bit little-endian RGB555 with the top bit set. Formats with
//! "PixelOrder 1" (the Cover Flow sizes) are stored twiddled, in Morton
//! order with the y bit first, as the PowerVR GPU reads textures, and keep a
//! border ("inset") around the picture.
//!
//! The ArtworkDB mirrors the iTunesDB's record style:
//!
//! ```text
//! mhfd                         (header 132)
//!   mhsd 1  mhli               image list
//!     mhii                     one per image: ID, song dbid, original size
//!       mhod 2  mhni           one per format: offset, size, padding, w, h
//!         mhod 3               ithmb file name, ":F3005_1.ithmb"
//!   mhsd 2  mhla               albums (empty)
//!   mhsd 3  mhlf               files
//!     mhif                     one per format: image size
//! ```
//!
//! Tracks point at their image from the iTunesDB (mhit: has-artwork flag,
//! image ID, original size).

use image::imageops::{self, FilterType};
use image::{DynamicImage, Rgba, RgbaImage};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// One image size a device wants.
#[derive(Debug, Clone, PartialEq)]
pub struct Format {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// Border around the picture, in pixels.
    pub inset: u32,
    /// Morton order instead of rows.
    pub twiddled: bool,
    /// Fills the space a non-square cover leaves, RGBA.
    pub background: [u8; 4],
    pub row_alignment: u32,
    pub offset_alignment: u32,
}

impl Format {
    fn stride(&self) -> u32 {
        (self.width * 2).next_multiple_of(self.row_alignment.max(1))
    }

    /// Bytes of one image.
    pub fn image_size(&self) -> u32 {
        if self.twiddled {
            self.width * self.height * 2
        } else {
            self.stride() * self.height
        }
    }

    /// Bytes from one image's start to the next.
    fn slot(&self) -> u32 {
        self.image_size()
            .next_multiple_of(self.offset_alignment.max(1))
    }

    pub fn file_name(&self) -> String {
        format!("F{}_1.ithmb", self.id)
    }

    /// Renders `cover` to this format's pixels.
    pub fn render(&self, cover: &DynamicImage) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let (inner_w, inner_h) = (w - 2 * self.inset, h - 2 * self.inset);
        let fitted = cover
            .resize(inner_w, inner_h, FilterType::Triangle)
            .to_rgba8();
        let mut canvas = RgbaImage::from_pixel(w, h, Rgba(self.background));
        let x = self.inset + (inner_w - fitted.width()) / 2;
        let y = self.inset + (inner_h - fitted.height()) / 2;
        imageops::overlay(&mut canvas, &fitted, i64::from(x), i64::from(y));

        let mut out = vec![0u8; self.image_size() as usize];
        for (x, y, &Rgba([r, g, b, _])) in canvas.enumerate_pixels() {
            let at = if self.twiddled {
                morton(x, y) as usize * 2
            } else {
                (y * self.stride() + x * 2) as usize
            };
            out[at..at + 2].copy_from_slice(&rgb555(r, g, b).to_le_bytes());
        }
        out
    }
}

fn rgb555(r: u8, g: u8, b: u8) -> u16 {
    0x8000 | (u16::from(r >> 3) << 10) | (u16::from(g >> 3) << 5) | u16::from(b >> 3)
}

/// Morton index with y in the even bits and x in the odd ones.
fn morton(x: u32, y: u32) -> u32 {
    let spread = |mut v: u32| {
        v &= 0xFFFF;
        v = (v | (v << 8)) & 0x00FF_00FF;
        v = (v | (v << 4)) & 0x0F0F_0F0F;
        v = (v | (v << 2)) & 0x3333_3333;
        (v | (v << 1)) & 0x5555_5555
    };
    spread(y) | (spread(x) << 1)
}

/// The cover art embedded in a song file (MP4/M4A, MP3 or FLAC), as the
/// encoded image (usually JPEG or PNG).
pub fn extract(path: &Path) -> Option<Vec<u8>> {
    let mut f = File::open(path).ok()?;
    let mut magic = [0u8; 12];
    f.read_exact(&mut magic).ok()?;
    f.seek(SeekFrom::Start(0)).ok()?;
    if &magic[..3] == b"ID3" {
        id3_picture(&mut f)
    } else if &magic[..4] == b"fLaC" {
        flac_picture(&mut f)
    } else if &magic[4..8] == b"ftyp" || &magic[4..8] == b"moov" {
        mp4_cover(&mut f)
    } else {
        None
    }
}

/// Follows moov/udta/meta/ilst/covr/data.
fn mp4_cover(f: &mut File) -> Option<Vec<u8>> {
    let end = f.seek(SeekFrom::End(0)).ok()?;
    let mut range = (0, end);
    for name in [b"moov", b"udta", b"meta", b"ilst", b"covr", b"data"] {
        range = find_atom(f, range, name)?;
        if name == b"meta" {
            range.0 += 4; // version and flags
        }
    }
    // The data atom: type and locale, then the image.
    let (start, end) = (range.0 + 8, range.1);
    let len = usize::try_from(end.checked_sub(start)?).ok()?;
    if len > 32 << 20 {
        return None;
    }
    let mut data = vec![0; len];
    f.seek(SeekFrom::Start(start)).ok()?;
    f.read_exact(&mut data).ok()?;
    Some(data)
}

/// The body of the first child atom called `name` within `(start, end)`.
fn find_atom(f: &mut File, (start, end): (u64, u64), name: &[u8; 4]) -> Option<(u64, u64)> {
    let mut at = start;
    while at + 8 <= end {
        f.seek(SeekFrom::Start(at)).ok()?;
        let mut header = [0u8; 16];
        f.read_exact(&mut header[..8]).ok()?;
        let mut size = u64::from(u32::from_be_bytes(header[..4].try_into().unwrap()));
        let mut body = at + 8;
        if size == 1 {
            f.read_exact(&mut header[8..]).ok()?;
            size = u64::from_be_bytes(header[8..].try_into().unwrap());
            body += 8;
        } else if size == 0 {
            size = end - at;
        }
        if size < body - at || at + size > end {
            return None;
        }
        if &header[4..8] == name {
            return Some((body, at + size));
        }
        at += size;
    }
    None
}

/// The front cover from an ID3v2 tag (v2.2 PIC, v2.3/2.4 APIC frames), or
/// the first picture if none is marked as the front cover.
fn id3_picture(f: &mut File) -> Option<Vec<u8>> {
    let mut header = [0u8; 10];
    f.read_exact(&mut header).ok()?;
    let version = header[3];
    let size = syncsafe(&header[6..10]) as usize;
    if size > 64 << 20 || header[5] & 0x80 != 0 {
        return None; // too big, or unsynchronised (rare; not handled)
    }
    let mut tag = vec![0u8; size];
    f.read_exact(&mut tag).ok()?;
    let mut at = 0;
    if header[5] & 0x40 != 0 && version >= 3 {
        // Extended header.
        let len = u32::from_be_bytes(tag.get(..4)?.try_into().ok()?) as usize;
        at = if version == 4 {
            syncsafe(&tag[..4]) as usize
        } else {
            len + 4
        };
    }
    let mut first = None;
    while at + 10 <= tag.len() {
        let (id, len, body) = if version == 2 {
            let len = u32::from_be_bytes([0, tag[at + 3], tag[at + 4], tag[at + 5]]) as usize;
            (&tag[at..at + 3], len, at + 6)
        } else {
            let raw = &tag[at + 4..at + 8];
            let len = if version == 4 {
                syncsafe(raw)
            } else {
                u32::from_be_bytes(raw.try_into().unwrap())
            };
            (&tag[at..at + 4], len as usize, at + 10)
        };
        if id.iter().all(|&b| b == 0) || body + len > tag.len() {
            break;
        }
        if id == b"APIC" || id == b"PIC" {
            if let Some((kind, data)) = parse_apic(&tag[body..body + len], version == 2) {
                if kind == 3 {
                    return Some(data);
                }
                first.get_or_insert(data);
            }
        }
        at = body + len;
    }
    first
}

/// Picture type and image data of an APIC/PIC frame.
fn parse_apic(frame: &[u8], v22: bool) -> Option<(u8, Vec<u8>)> {
    let encoding = *frame.first()?;
    let mut at = 1;
    if v22 {
        at += 3; // image format, e.g. "JPG"
    } else {
        at += frame[at..].iter().position(|&b| b == 0)? + 1; // MIME type
    }
    let kind = *frame.get(at)?;
    at += 1;
    // Description, ended by one zero byte, or two for UTF-16.
    if encoding == 1 || encoding == 2 {
        let mut i = at;
        while i + 1 < frame.len() && (frame[i], frame[i + 1]) != (0, 0) {
            i += 2;
        }
        at = i + 2;
    } else {
        at += frame[at..].iter().position(|&b| b == 0)? + 1;
    }
    Some((kind, frame.get(at..)?.to_vec()))
}

fn syncsafe(b: &[u8]) -> u32 {
    b.iter().fold(0, |v, &x| (v << 7) | u32::from(x & 0x7F))
}

/// The front cover (or first picture) from FLAC metadata.
fn flac_picture(f: &mut File) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(4)).ok()?;
    let mut first = None;
    loop {
        let mut header = [0u8; 4];
        f.read_exact(&mut header).ok()?;
        let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        if header[0] & 0x7F == 6 && len < 32 << 20 {
            let mut block = vec![0u8; len];
            f.read_exact(&mut block).ok()?;
            let u32_at = |at: usize| {
                Some(u32::from_be_bytes(block.get(at..at + 4)?.try_into().ok()?) as usize)
            };
            let kind = u32_at(0)?;
            let mime = u32_at(4)?;
            let desc = u32_at(8 + mime)?;
            let data_len_at = 12 + mime + desc + 16;
            let data_len = u32_at(data_len_at)?;
            let data = block
                .get(data_len_at + 4..data_len_at + 4 + data_len)?
                .to_vec();
            if kind == 3 {
                return Some(data);
            }
            first.get_or_insert(data);
        } else {
            f.seek(SeekFrom::Current(len as i64)).ok()?;
        }
        if header[0] & 0x80 != 0 {
            return first;
        }
    }
}

/// An image in the ArtworkDB.
pub struct Image {
    pub id: u32,
    /// dbid of the (first) track that uses it.
    pub song: u64,
    /// Size of the source image file.
    pub source_size: u32,
}

/// Builds the ArtworkDB and the ithmb files' contents. `rendered[i][f]` is
/// image `i` in format `f`.
pub fn build(
    formats: &[Format],
    images: &[Image],
    rendered: &[Vec<Vec<u8>>],
) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    let mut files: Vec<(String, Vec<u8>)> = formats
        .iter()
        .map(|f| (f.file_name(), Vec::new()))
        .collect();
    let mut b = Vec::new();
    let put = |b: &mut Vec<u8>, v: &[u8]| b.extend_from_slice(v);
    let le32 = |v: u32| v.to_le_bytes();
    let close = |b: &mut Vec<u8>, start: usize| {
        let len = (b.len() - start) as u32;
        b[start + 8..start + 12].copy_from_slice(&len.to_le_bytes());
    };

    // mhfd
    put(&mut b, b"mhfd");
    put(&mut b, &le32(132));
    put(&mut b, &le32(0));
    put(&mut b, &le32(0));
    put(&mut b, &le32(2));
    put(&mut b, &le32(3));
    put(&mut b, &le32(0));
    let next_id = images.iter().map(|i| i.id + 1).max().unwrap_or(100);
    put(&mut b, &le32(next_id));
    b.resize(132, 0);
    b[0x30] = 1;

    // Image list.
    let mhsd = |b: &mut Vec<u8>, kind: u16| {
        let start = b.len();
        b.extend_from_slice(b"mhsd");
        b.extend_from_slice(&96u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&kind.to_le_bytes());
        b.resize(start + 96, 0);
        start
    };
    let list = |b: &mut Vec<u8>, tag: &[u8; 4], count: u32| {
        let start = b.len();
        b.extend_from_slice(tag);
        b.extend_from_slice(&92u32.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        b.resize(start + 92, 0);
    };
    let s = mhsd(&mut b, 1);
    list(&mut b, b"mhli", images.len() as u32);
    for (image, pixels) in images.iter().zip(rendered) {
        let start = b.len();
        put(&mut b, b"mhii");
        put(&mut b, &le32(152));
        put(&mut b, &le32(0));
        put(&mut b, &le32(formats.len() as u32));
        put(&mut b, &le32(image.id));
        put(&mut b, &image.song.to_le_bytes());
        b.resize(start + 0x30, 0);
        put(&mut b, &le32(image.source_size));
        b.resize(start + 152, 0);
        for (f, (format, data)) in formats.iter().zip(pixels).enumerate() {
            let ithmb = &mut files[f].1;
            let offset = ithmb.len() as u32;
            ithmb.extend_from_slice(data);
            ithmb.resize((offset + format.slot()) as usize, 0);

            let container = b.len();
            put(&mut b, b"mhod");
            put(&mut b, &le32(24));
            put(&mut b, &le32(0));
            put(&mut b, &2u16.to_le_bytes());
            b.resize(container + 24, 0);
            let mhni = b.len();
            put(&mut b, b"mhni");
            put(&mut b, &le32(76));
            put(&mut b, &le32(0));
            put(&mut b, &le32(1));
            put(&mut b, &le32(format.id));
            put(&mut b, &le32(offset));
            put(&mut b, &le32(format.image_size()));
            let pad = format.inset as u16;
            for v in [pad, pad, format.height as u16, format.width as u16] {
                put(&mut b, &v.to_le_bytes());
            }
            put(&mut b, &le32(0));
            put(&mut b, &le32(format.image_size()));
            b.resize(mhni + 76, 0);
            // File name, UTF-16LE.
            let name: Vec<u8> = format!(":{}", format.file_name())
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect();
            let m = b.len();
            put(&mut b, b"mhod");
            put(&mut b, &le32(24));
            put(&mut b, &le32(0));
            put(&mut b, &3u16.to_le_bytes());
            b.resize(m + 24, 0);
            put(&mut b, &le32(name.len() as u32));
            put(&mut b, &[2, 0, 0, 0]); // UTF-16LE
            put(&mut b, &le32(0));
            put(&mut b, &name);
            close(&mut b, m);
            close(&mut b, mhni);
            close(&mut b, container);
        }
        close(&mut b, start);
    }
    close(&mut b, s);

    let s = mhsd(&mut b, 2);
    list(&mut b, b"mhla", 0);
    close(&mut b, s);

    let s = mhsd(&mut b, 3);
    list(&mut b, b"mhlf", formats.len() as u32);
    for format in formats {
        let start = b.len();
        put(&mut b, b"mhif");
        put(&mut b, &le32(124));
        put(&mut b, &le32(124));
        put(&mut b, &le32(0));
        put(&mut b, &le32(format.id));
        put(&mut b, &le32(format.image_size()));
        b.resize(start + 124, 0);
    }
    close(&mut b, s);
    close(&mut b, 0);
    (b, files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(id: u32, size: u32, inset: u32, twiddled: bool) -> Format {
        Format {
            id,
            width: size,
            height: size,
            inset,
            twiddled,
            background: [0, 0, 0, 0],
            row_alignment: 16,
            offset_alignment: 4096,
        }
    }

    #[test]
    fn lays_out_pixels_like_the_device() {
        assert_eq!(morton(0, 1), 1);
        assert_eq!(morton(1, 0), 2);
        assert_eq!(morton(3, 3), 15);
        assert_eq!(rgb555(255, 0, 8), 0x8000 | (31 << 10) | 1);
        // 55 px rows are padded to 112 bytes.
        let small = format(3006, 55, 0, false);
        assert_eq!((small.image_size(), small.slot()), (6160, 8192));
        let red = DynamicImage::ImageRgba8(RgbaImage::from_pixel(10, 20, Rgba([255, 0, 0, 255])));
        let px = small.render(&red);
        // A tall cover is centred; the sides keep the background.
        let at = |x: u32, y: u32| {
            u16::from_le_bytes([
                px[(y * 112 + x * 2) as usize],
                px[(y * 112 + x * 2) as usize + 1],
            ])
        };
        assert_eq!(at(27, 27), rgb555(255, 0, 0));
        assert_eq!(at(0, 27), 0x8000);
        // Twiddled formats keep a border.
        let cover = format(3003, 64, 1, true);
        let px = cover.render(&red);
        let at = |x: u32, y: u32| {
            u16::from_le_bytes(px[morton(x, y) as usize * 2..][..2].try_into().unwrap())
        };
        assert_eq!(at(0, 0), 0x8000);
        assert_eq!(at(32, 32), rgb555(255, 0, 0));
    }

    #[test]
    fn builds_a_database_and_files() {
        let formats = [format(3005, 320, 0, false), format(3006, 55, 0, false)];
        let images = [
            Image {
                id: 100,
                song: 0xAB,
                source_size: 1000,
            },
            Image {
                id: 101,
                song: 0xCD,
                source_size: 2000,
            },
        ];
        let rendered: Vec<Vec<Vec<u8>>> = images
            .iter()
            .map(|_| {
                formats
                    .iter()
                    .map(|f| vec![0u8; f.image_size() as usize])
                    .collect()
            })
            .collect();
        let (db, files) = build(&formats, &images, &rendered);
        let u32_at = |at: usize| u32::from_le_bytes(db[at..at + 4].try_into().unwrap());
        assert_eq!(u32_at(8) as usize, db.len());
        assert_eq!(u32_at(0x1c), 102); // next ID
        assert_eq!(files[0], ("F3005_1.ithmb".into(), vec![0; 2 * 204_800]));
        assert_eq!(files[1].1.len(), 2 * 8192);
        // The second image's small version sits at the next 4096 boundary.
        let mhni: Vec<usize> = db
            .windows(4)
            .enumerate()
            .filter(|(_, w)| w == b"mhni")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(u32_at(mhni[3] + 20), 8192);
        let name = db.windows(28).any(|w| {
            w == ":F3006_1.ithmb"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
                .as_slice()
        });
        assert!(name);
    }

    #[test]
    fn finds_cover_art_in_mp4_and_id3() {
        let dir = std::env::temp_dir().join(format!("bt-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = b"\xFF\xD8fake jpeg";
        let atom = |name: &[u8], body: &[u8]| {
            let mut a = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            a.extend(name);
            a.extend(body);
            a
        };
        let data = atom(
            b"data",
            &[[0, 0, 0, 13, 0, 0, 0, 0].as_slice(), image].concat(),
        );
        let meta = atom(
            b"meta",
            &[[0; 4].as_slice(), &atom(b"ilst", &atom(b"covr", &data))].concat(),
        );
        let mp4 = [atom(b"ftyp", b"M4A "), atom(b"moov", &atom(b"udta", &meta))].concat();
        std::fs::write(dir.join("a.m4a"), mp4).unwrap();
        assert_eq!(extract(&dir.join("a.m4a")).as_deref(), Some(&image[..]));

        let mut frame = vec![0u8];
        frame.extend(b"image/jpeg\0");
        frame.push(3);
        frame.extend(b"cover\0");
        frame.extend(image);
        let mut tag = b"APIC".to_vec();
        tag.extend((frame.len() as u32).to_be_bytes());
        tag.extend([0, 0]);
        tag.extend(&frame);
        let mut mp3 = b"ID3\x03\x00\x00".to_vec();
        let n = tag.len() as u32;
        mp3.extend([
            (n >> 21) as u8 & 0x7F,
            (n >> 14) as u8 & 0x7F,
            (n >> 7) as u8 & 0x7F,
            n as u8 & 0x7F,
        ]);
        mp3.extend(tag);
        std::fs::write(dir.join("a.mp3"), mp3).unwrap();
        assert_eq!(extract(&dir.join("a.mp3")).as_deref(), Some(&image[..]));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
