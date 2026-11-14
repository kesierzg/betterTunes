//! Converting tracks to AAC with the system's encoder: `afconvert` (Apple's
//! AAC encoder, part of macOS) or `ffmpeg`, preferring its best AAC encoder.
//! Converted files are cached per bit rate under their persistent ID, so
//! each track is encoded once.

use crate::sync::{Action, Plan, Remap};
use crate::{Error, Result, Track};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Formats `afconvert` can't read.
const FFMPEG_ONLY: [&str; 6] = ["ogg", "oga", "opus", "wma", "ape", "wv"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoder {
    AfConvert,
    /// ffmpeg and the AAC encoder to use with it.
    Ffmpeg(String),
}

impl Encoder {
    /// The encoders installed here, best first.
    pub fn available() -> Vec<Encoder> {
        let mut found = Vec::new();
        if cfg!(target_os = "macos") && Path::new("/usr/bin/afconvert").exists() {
            found.push(Encoder::AfConvert);
        }
        if let Ok(out) = Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .output()
        {
            let list = String::from_utf8_lossy(&out.stdout);
            let has = |name: &str| list.split_whitespace().any(|w| w == name);
            if let Some(enc) = ["libfdk_aac", "aac_at", "aac"].into_iter().find(|e| has(e)) {
                found.push(Encoder::Ffmpeg(enc.into()));
            }
        }
        found
    }

    pub fn describe(&self) -> String {
        match self {
            Encoder::AfConvert => "afconvert (Apple AAC)".into(),
            Encoder::Ffmpeg(enc) => format!("ffmpeg ({enc})"),
        }
    }

    /// The best of `encoders` that can read `source`.
    pub fn pick<'a>(encoders: &'a [Encoder], source: &Path) -> Option<&'a Encoder> {
        let ext = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let ffmpeg_only = FFMPEG_ONLY.contains(&ext.as_str());
        encoders
            .iter()
            .find(|e| !(ffmpeg_only && **e == Encoder::AfConvert))
    }

    /// Encodes `source` to AAC at `kbps` into `dest` (an .m4a file). Writes to
    /// a temporary file first, so `dest` is never left half-written.
    pub fn convert(
        &self,
        source: &Path,
        dest: &Path,
        kbps: u32,
        sample_rate: Option<u32>,
    ) -> Result<()> {
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = dest.with_extension("part.m4a");
        // AAC goes up to 48 kHz; resample higher rates to their base rate.
        let resample =
            sample_rate
                .filter(|&r| r > 48_000)
                .map(|r| if r % 44_100 == 0 { 44_100 } else { 48_000 });
        let mut cmd;
        match self {
            Encoder::AfConvert => {
                let format = match resample {
                    Some(r) => format!("aac@{r}"),
                    None => "aac".into(),
                };
                cmd = Command::new("/usr/bin/afconvert");
                // -s 2: constrained VBR, as iTunes' own converter.
                cmd.args(["-f", "m4af", "-d", &format, "-s", "2", "-q", "127", "-b"])
                    .arg((kbps * 1000).to_string())
                    .arg(source)
                    .arg(&tmp);
            }
            Encoder::Ffmpeg(enc) => {
                cmd = Command::new("ffmpeg");
                cmd.args(["-nostdin", "-hide_banner", "-v", "error", "-y", "-i"])
                    .arg(source)
                    .args(["-map", "0:a:0", "-c:a", enc, "-b:a", &format!("{kbps}k")]);
                if let Some(r) = resample {
                    cmd.args(["-ar", &r.to_string()]);
                }
                cmd.args(["-f", "ipod"]).arg(&tmp);
            }
        }
        let out = cmd.output()?;
        if !out.status.success() {
            let _ = std::fs::remove_file(&tmp);
            let msg = String::from_utf8_lossy(&out.stderr);
            let msg = msg.trim();
            return Err(Error::format(format!(
                "{} failed on {}: {}",
                self.describe(),
                source.display(),
                if msg.is_empty() { "no message" } else { msg }
            )));
        }
        std::fs::rename(&tmp, dest)?;
        Ok(())
    }
}

/// One track to convert.
#[derive(Debug, Clone)]
pub struct Job<'a> {
    pub track: &'a Track,
    pub source: PathBuf,
    pub dest: PathBuf,
    pub kbps: u32,
}

/// The conversions a plan still needs, and how many are already cached.
pub fn jobs<'a>(plan: &Plan<'a>, cache: &Path, remap: &Remap) -> (Vec<Job<'a>>, usize) {
    let mut todo = Vec::new();
    let mut cached = 0;
    for item in plan.converted() {
        let (Action::Convert { kbps }, Some(location)) = (item.action, &item.track.location) else {
            continue;
        };
        let source = remap.apply(location);
        let dest = cached_path(cache, item.track, kbps);
        if is_fresh(&dest, &source) {
            cached += 1;
        } else {
            todo.push(Job {
                track: item.track,
                source,
                dest,
                kbps,
            });
        }
    }
    (todo, cached)
}

/// Runs `jobs` on `threads` threads, calling `done` after each with the
/// number finished so far. When `done` returns false, no new jobs start.
pub fn run(
    jobs: &[Job],
    encoders: &[Encoder],
    threads: usize,
    done: impl Fn(usize, &Job, std::result::Result<(), String>) -> bool + Sync,
) {
    let next = AtomicUsize::new(0);
    let finished = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, jobs.len().max(1)) {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        break;
                    };
                    let result = match Encoder::pick(encoders, &job.source) {
                        Some(enc) => enc
                            .convert(&job.source, &job.dest, job.kbps, job.track.sample_rate)
                            .map_err(|e| e.to_string()),
                        None => Err(format!(
                            "no encoder here reads {} (install ffmpeg)",
                            job.source.display()
                        )),
                    };
                    if !done(finished.fetch_add(1, Ordering::Relaxed) + 1, job, result) {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            });
        }
    });
}

/// Threads to convert with by default: one per core.
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get())
}

/// Where the converted copy of `track` at `kbps` lives.
pub fn cached_path(cache: &Path, track: &Track, kbps: u32) -> PathBuf {
    cache
        .join(format!("aac-{kbps}"))
        .join(format!("{:016X}.m4a", track.persistent_id))
}

/// Whether a cached copy exists and is no older than its source.
pub fn is_fresh(cached: &Path, source: &Path) -> bool {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (mtime(cached), mtime(source)) {
        (Some(c), Some(s)) => c >= s,
        (Some(_), None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_an_encoder_that_reads_the_source() {
        let both = [Encoder::AfConvert, Encoder::Ffmpeg("aac".into())];
        assert_eq!(Encoder::pick(&both, Path::new("a.flac")), Some(&both[0]));
        assert_eq!(Encoder::pick(&both, Path::new("a.OGG")), Some(&both[1]));
        assert_eq!(Encoder::pick(&both[..1], Path::new("a.opus")), None);
        let t = Track {
            persistent_id: 0xAB,
            ..Track::default()
        };
        assert_eq!(
            cached_path(Path::new("/c"), &t, 256),
            Path::new("/c/aac-256/00000000000000AB.m4a")
        );
    }
}
