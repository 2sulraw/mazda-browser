//! Media decoding and playback for `<video>`/`<audio>`.
//!
//! The engine owns the element, the fetch and the paint; this crate is the
//! thin platform edge. On Windows it decodes through Media Foundation (H.264,
//! AAC, and whatever codecs the OS ships) and plays audio through WASAPI in
//! shared mode. Other platforms currently report [`MediaError::Unsupported`]
//! until the Linux backend (VA-API/PipeWire) lands.
//!
//! DRM is deliberately unsupported: an encrypted MP4 is rejected as
//! [`MediaError::Drm`] before any decoder is created, so the page gets a
//! clean error instead of a half-working player.

use std::path::Path;
use std::time::Instant;

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(not(windows))]
#[path = "stub.rs"]
mod platform;

pub mod mp4;

/// What a media file contains, read from its container headers.
#[derive(Clone, Copy, Debug, Default)]
pub struct MediaInfo {
    /// encoded pixel size
    pub width: u32,
    pub height: u32,
    /// seconds; 0.0 when the container did not say
    pub duration: f64,
    pub has_video: bool,
    pub has_audio: bool,
}

/// One decoded video frame, `0xAARRGGBB`, row-major.
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u32>,
}

#[derive(Debug)]
pub enum MediaError {
    /// no backend for this platform, or the container/codec is not supported
    Unsupported(String),
    /// encrypted content (DRM) — always rejected
    Drm,
    Io(String),
    Decode(String),
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaError::Unsupported(m) => write!(f, "unsupported: {m}"),
            MediaError::Drm => write!(f, "DRM-protected media is not supported"),
            MediaError::Io(m) => write!(f, "io: {m}"),
            MediaError::Decode(m) => write!(f, "decode: {m}"),
        }
    }
}

impl std::error::Error for MediaError {}

/// A playing (or paused) media file.
///
/// Video decoding happens on the caller's thread; audio runs on its own
/// thread feeding the OS mixer. Dropping the player stops both.
pub struct Player {
    info: MediaInfo,
    video: Option<platform::VideoReader>,
    audio: Option<platform::AudioHandle>,
    playing: bool,
    /// media time when the current play run started
    base: f64,
    started: Instant,
    /// first frame, decoded at open so the element shows something even
    /// before playback begins
    first: Option<Frame>,
    /// decoded but not yet due (presentation timestamp in 100 ns units)
    queued: Option<(i64, Frame)>,
    eof: bool,
    looping: bool,
    /// mapped `muted` attribute: the audio thread stays paused while set
    muted: bool,
}

impl Player {
    /// Open `path`, read the container metadata, and decode the first video
    /// frame. Audio starts paused.
    pub fn open(path: &Path) -> Result<Self, MediaError> {
        let bytes = std::fs::read(path).map_err(|e| MediaError::Io(e.to_string()))?;
        let meta = mp4::parse(&bytes)
            .ok_or_else(|| MediaError::Unsupported("not an MP4/ISO-BMFF file".to_string()))?;
        drop(bytes);
        if meta.drm {
            return Err(MediaError::Drm);
        }
        if !meta.has_video && !meta.has_audio {
            return Err(MediaError::Unsupported(
                "no audio or video track".to_string(),
            ));
        }

        let info = MediaInfo {
            width: meta.width,
            height: meta.height,
            duration: meta.duration,
            has_video: meta.has_video,
            has_audio: meta.has_audio,
        };
        let mut video = None;
        let mut first = None;
        if info.has_video {
            let mut r = platform::VideoReader::open(path, info.width, info.height)?;
            first = r.read_next()?.map(|(_, f)| f);
            video = Some(r);
        }
        let audio = if info.has_audio {
            // A missing audio endpoint must not break video playback.
            platform::AudioHandle::start(path, info.duration).ok()
        } else {
            None
        };
        Ok(Self {
            info,
            video,
            audio,
            playing: false,
            base: 0.0,
            started: Instant::now(),
            first,
            queued: None,
            eof: false,
            looping: false,
            muted: false,
        })
    }

    pub fn info(&self) -> MediaInfo {
        self.info
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
        if let Some(a) = &self.audio {
            a.set_looping(looping);
        }
    }

    /// Muted playback keeps decoding video and skips the audio path entirely.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if muted {
            if let Some(a) = &self.audio {
                a.pause();
            }
        } else if self.playing {
            if let Some(a) = &self.audio {
                a.play();
            }
        }
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    pub fn play(&mut self) {
        if self.playing {
            return;
        }
        if self.eof {
            self.seek(0.0);
        }
        self.playing = true;
        self.started = Instant::now();
        if !self.muted {
            if let Some(a) = &self.audio {
                a.play();
            }
        }
    }

    pub fn pause(&mut self) {
        if !self.playing {
            return;
        }
        self.base = self.position();
        self.playing = false;
        if let Some(a) = &self.audio {
            a.pause();
        }
    }

    pub fn toggle(&mut self) {
        if self.playing {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn position(&self) -> f64 {
        if self.playing {
            self.base + self.started.elapsed().as_secs_f64()
        } else {
            self.base
        }
    }

    pub fn seek(&mut self, secs: f64) {
        let t = secs.max(0.0).min(if self.info.duration > 0.0 {
            self.info.duration
        } else {
            secs.max(0.0)
        });
        self.base = t;
        self.started = Instant::now();
        self.eof = false;
        self.queued = None;
        if let Some(v) = &mut self.video {
            v.seek(t);
        }
        if let Some(a) = &self.audio {
            a.seek(t);
        }
    }

    /// Advance decoding and return a frame whose time has arrived.
    ///
    /// Calls that are not yet due return `None`, so the renderer can poll at
    /// its refresh rate without decoding ahead of the clock.
    pub fn next_frame(&mut self) -> Option<Frame> {
        if let Some(f) = self.first.take() {
            return Some(f);
        }
        if !self.playing {
            return None;
        }
        let pos = self.position() * 10_000_000.0;
        if let Some((ts, frame)) = self.queued.take() {
            if ts as f64 <= pos {
                return Some(frame);
            }
            self.queued = Some((ts, frame));
            return None;
        }
        let looping = self.looping;
        for _ in 0..4 {
            let res = {
                let v = self.video.as_mut()?;
                v.read_next()
            };
            match res {
                Ok(Some((ts, frame))) => {
                    if ts as f64 <= pos {
                        return Some(frame);
                    }
                    self.queued = Some((ts, frame));
                    return None;
                }
                Ok(None) => {
                    // end of stream: loop or stop
                    self.eof = true;
                    if looping {
                        self.seek(0.0);
                        continue;
                    }
                    self.pause();
                    return None;
                }
                Err(e) => {
                    eprintln!("[media] decode: {e}");
                    self.pause();
                    return None;
                }
            }
        }
        None
    }
}
