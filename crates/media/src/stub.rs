//! Placeholder backend for platforms without a media implementation yet.
//!
//! Everything reports [`MediaError::Unsupported`], so a page with `<video>`
//! degrades to its poster/alt content instead of breaking the renderer.

use std::path::Path;

use crate::{Frame, MediaError};

pub struct VideoReader;

impl VideoReader {
    pub fn open(_path: &Path, _w: u32, _h: u32) -> Result<Self, MediaError> {
        Err(MediaError::Unsupported(
            "media decoding is not implemented on this platform".to_string(),
        ))
    }

    pub fn read_next(&mut self) -> Result<Option<(i64, Frame)>, MediaError> {
        Ok(None)
    }

    pub fn seek(&mut self, _secs: f64) {}
}

pub struct AudioHandle;

impl AudioHandle {
    pub fn start(_path: &Path, _duration: f64) -> Result<Self, MediaError> {
        Err(MediaError::Unsupported(
            "audio output is not implemented on this platform".to_string(),
        ))
    }

    pub fn play(&self) {}

    pub fn pause(&self) {}

    pub fn seek(&self, _secs: f64) {}

    pub fn set_looping(&self, _looping: bool) {}
}
