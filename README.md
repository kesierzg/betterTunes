# betterTunes

THIS SOFTWARE IS ENTIRELY VIBECODED!!!

v1 supports syncing current iTunes library to iPod touch 1G on iPhoneOS 1.1.2
that's all from me let claude speak for itself

An open-source replacement for iTunes, for macOS and Linux: it reads your
iTunes library and syncs it to iPods and iPhones, including old devices that
current macOS and iTunes no longer handle (such as the first iPod touch on its
original firmware).

**Status: early, but it syncs.** The first iPod touch syncs fully: songs,
playlists and cover art. Other devices are on the roadmap. Your iTunes
library stays the master copy: betterTunes reads it, and changes made on a
device go back into iTunes through AppleScript.

## What works

| | |
|---|---|
| **Library** | Reads `iTunes Library.itl` (iTunes 12.8, likely 10+), Music.app's library and iTunes XML exports. A library on another Mac can be used over a network mount. |
| **iPod touch 1G** (iPhone OS 1.x) | Full sync: songs, playlists, cover art. Works on current macOS, which no longer recognises it, by talking to it over USB directly. |
| **iPhones** (iOS 5+) | Reading only: device info, files, and playlist edits made on the phone, which `bt pull` merges into iTunes (no more "Name 1" copies left behind). Syncing to them is on the roadmap. |
| **Classic iPods** | Detected; syncing is on the roadmap. |
| **Conversion** | Lossless and high bit rate songs to AAC (128–320 kbps), like iTunes' "Convert higher bit rate songs", using `afconvert` on macOS or `ffmpeg`. Converted files are cached. |

## Build

Requires Rust (stable). Everything is bundled (OpenSSL with the old TLS the
first iPod touch needs, SQLite, USB access), so there are no system libraries
to install.

```sh
cargo build --release
./target/release/bt --help
```

For an Intel Mac back to macOS 10.13 High Sierra (Apple Silicon needs macOS 11):

```sh
rustup target add x86_64-apple-darwin
MACOSX_DEPLOYMENT_TARGET=10.13 cargo build --release --target x86_64-apple-darwin
```

## Getting started

1. **Pick the library.** betterTunes uses this Mac's library by default. To use
   another Mac's, mount its disk and point at its iTunes folder once; song paths
   are mapped to the mount automatically:

   ```sh
   bt library /Volumes/macOS/Users/me/Music/iTunes
   ```

2. **Connect the device** and check it's seen: `bt devices`. A first iPod touch
   needs pairing once: `bt pair`.

3. **Run `bt`** for the full-screen view, choose what goes on the device, and
   press Enter to sync.

| Key | |
|---|---|
| space | choose or unchoose a playlist, artist, album or genre |
| enter | sync the device |
| ← → / tab | switch between Playlists, Artists, Albums, Genres |
| `/` | search |
| `c` / `C` | conversion: off, or AAC 128 to 320 kbps |
| `P` | bring changes made on the device back into iTunes |
| `p` | convert now, without syncing |
| `d` | switch device |
| `q` | quit (choices are saved) |

The bar at the bottom shows how much the selection takes and whether it fits.
A row marked `[~]` has songs coming in through another choice.

## Commands

Everything the full-screen view does is also a command:

```sh
bt devices                    # connected iPhones/iPod touches and mounted iPods
bt pair                       # pair a device only betterTunes can reach
bt library [PATH | --reset]   # show or choose the library
bt info                       # library summary
bt playlists [--sort length]  # playlists with IDs, song counts and lengths
bt playlists NAME             # songs of a playlist (name, part of it, or ID)

bt sync select                # choose what goes on the device (asks)
bt sync select -p "Road trip" --artist "Some Artist" --genre Trance   # or directly
bt sync convert 256           # or off, 128, 160, 192, 224, 320
bt sync plan [--tracks]       # what would go on, and whether it fits
bt sync run                   # sync
bt pull [--script FILE]       # bring device changes back into iTunes

bt device backup              # copy a device's music and database here
bt device erase-music         # remove synced music (asks about a backup first)
bt clones [--script FILE]     # find "Name 1" playlist copies in the library
```

`-l PATH` uses a library for one command; `-d DEVICE` picks a device by UDID
(or its start) or name when several are connected.

## How it works

### Choosing what to sync

Each device has its own profile: playlists, artists, albums and genres to put
on it (or the whole library), and the conversion setting. Playlists are
remembered by their iTunes ID, so renaming one doesn't lose it. Formats iPods
can't play (FLAC, Ogg) are always converted. Profiles live in
`~/Library/Application Support/betterTunes` (Linux: `~/.local/share/bettertunes`),
converted songs in `~/Library/Caches/betterTunes` (Linux: `~/.cache/bettertunes`).

### Syncing the first iPod touch

betterTunes copies the songs into the device's `iTunes_Control/Music`,
renders each cover in the five sizes the device asks for, and writes a new
iTunesDB in the iTunes 7.4 layout, signed the way iPhone OS 1.x requires
("hash58"). Songs already on the device are kept, so later syncs only copy
what changed.

### Bringing changes back

`bt pull` turns what changed on a device into an AppleScript for iTunes. Where
iTunes is installed it runs it (after asking); elsewhere it saves it for the
Mac that has iTunes. The script sets values rather than adding to them, so
running it twice is harmless.

- **iPhones:** a phone can't edit a playlist that came from iTunes; it makes a
  new one with the same name, which iTunes imports as "Name 1". betterTunes adds
  those songs to the original playlist and deletes the copies. Songs are only
  ever added, never removed, and playlists the phone got from iTunes are left
  as iTunes has them.
- **iPod touch 1G:** play counts, skips, ratings and last played dates (in
  progress: iPhone OS 1.x keeps them in a format still being worked out).

### Devices current macOS ignores

macOS no longer recognises iPhone OS 1.x devices. betterTunes then talks to them
over USB itself (shown as "USB (direct)"): it switches the device to its usbmux
configuration and speaks the protocol, then pairs with the device and opens an
encrypted session using the old SSL 3/TLS 1.0 the firmware knows.

## Roadmap

1. Classic iPods (3G "2003", 5G video): write the iTunesDB directly, adopting
   what's already on them so nothing has to be copied again
2. Jailbroken iPhones (iOS 6, 8, 10): an on-device helper that adds songs and
   edits playlists through iOS's own music library code; this also ends the
   "Name 1" playlist copies for good
3. Play counts from the iPod touch 1G
4. Running on the High Sierra Mac and on Linux (builds, untested so far)
5. `bt device`: what's on a device compared with the library
6. Faster USB transfers; skipping unchanged cover art; covers from iTunes'
   artwork cache
7. A desktop app on top of `core`

## Project layout

- `crates/core`: library readers (`itl.rs`, `musicdb.rs` and `chunks.rs`
  document the reverse-engineered formats), selection and planning (`sync.rs`),
  the iPod database and its signature (`itunesdb.rs`, `hash58.rs`), cover art
  (`artwork.rs`), conversion, device profiles, and `pull.rs`
- `crates/device`: usbmuxd client, usbmux over USB for devices the system
  ignores (`usb.rs`), lockdown sessions and pairing, file access (AFC),
  notifications, classic iPod detection
- `crates/cli`: the `bt` command, the full-screen view (`tui.rs`) and syncing
  (`push.rs`)

## Testing

```sh
cargo test
```

Some tests need real data and are skipped by default; for example, checking the
`.itl` reader against iTunes' XML export of the same library:

```sh
BT_ITL=".../iTunes Library.itl" BT_XML=".../iTunes Library.xml" \
    cargo test -p bettertunes-core --test itl_vs_xml -- --ignored
```

## Credits

The iPod database layout, its hash58 signature and the sync handshake follow
[libgpod](https://github.com/gtkpod/libgpod).

## Licence

betterTunes is free software under the [GNU General Public License v3.0 or
later](LICENSE): you may use, study, change and share it, including selling
copies, as long as what you distribute, changed or not, stays under the same
licence with its source code available.

`crates/core/src/hash58.rs` contains tables and an algorithm from libgpod
under its BSD licence; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
