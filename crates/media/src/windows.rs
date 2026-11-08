//! Windows backend: Media Foundation for decode, WASAPI for audio.
//!
//! Video is decoded by the source reader into RGB32 on the caller's thread.
//! Audio runs on its own thread: a second source reader outputs PCM in the
//! endpoint's mix format, which is written straight into a shared-mode WASAPI
//! render client. Both talk to the OS only; no C code and no third-party
//! decoder lives in the browser.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::{GUID, HSTRING};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::MediaFoundation::{
    IMFMediaBuffer, IMFSample, IMFSourceReader, MFAudioFormat_Float, MFAudioFormat_PCM,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Audio,
    MFMediaType_Video, MFShutdown, MFStartup, MFVideoFormat_RGB32, MFSTARTUP_FULL,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::VT_I8;

use crate::{Frame, MediaError};

fn err(what: &str, e: windows::core::Error) -> MediaError {
    MediaError::Decode(format!("{what}: {e}"))
}

fn com_and_mf_init() -> Result<(), MediaError> {
    unsafe {
        // S_FALSE (already initialized) is fine; failure is not fatal either:
        // MF can still work in an existing apartment.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| err("mf startup", e))?;
    }
    Ok(())
}

fn source_reader(path: &Path) -> Result<IMFSourceReader, MediaError> {
    let url = HSTRING::from(path.to_string_lossy().as_ref());
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 2).map_err(|e| err("mf attributes", e))?;
        let attrs = attrs.ok_or_else(|| MediaError::Decode("no attributes".into()))?;
        // let the pipeline convert the decoder's native NV12 into RGB32
        let _ = attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1);
        MFCreateSourceReaderFromURL(&url, &attrs).map_err(|e| err("open media", e))
    }
}

// ---------------------------------------------------------------------------
// Video

pub struct VideoReader {
    reader: IMFSourceReader,
    stream: u32,
    w: u32,
    h: u32,
    stride: i32,
    eof: bool,
}

impl VideoReader {
    pub fn open(path: &Path, info_w: u32, info_h: u32) -> Result<Self, MediaError> {
        com_and_mf_init()?;
        let reader = source_reader(path)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        unsafe {
            reader
                .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
                .map_err(|e| err("stream select", e))?;
            reader
                .SetStreamSelection(stream, true)
                .map_err(|e| err("video stream select", e))?;

            let ty = MFCreateMediaType().map_err(|e| err("media type", e))?;
            ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| err("video major", e))?;
            ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(|e| err("video subtype", e))?;
            reader
                .SetCurrentMediaType(stream, None, &ty)
                .map_err(|e| err("video output type", e))?;

            let actual = reader
                .GetCurrentMediaType(stream)
                .map_err(|e| err("video current type", e))?;
            let size = actual
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .unwrap_or(((info_w as u64) << 32) | info_h as u64);
            let w = (size >> 32) as u32;
            let h = (size & 0xffff_ffff) as u32;
            let stride = actual
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|s| s as i32)
                .unwrap_or((w * 4) as i32);
            if w == 0 || h == 0 {
                return Err(MediaError::Decode("video frame size 0".into()));
            }
            Ok(Self {
                reader,
                stream,
                w,
                h,
                stride,
                eof: false,
            })
        }
    }

    /// Decode the next available frame. `Ok(None)` = end of stream.
    pub fn read_next(&mut self) -> Result<Option<(i64, Frame)>, MediaError> {
        if self.eof {
            return Ok(None);
        }
        unsafe {
            for _ in 0..8 {
                let mut flags = 0u32;
                let mut ts = 0i64;
                let mut sample: Option<IMFSample> = None;
                self.reader
                    .ReadSample(
                        self.stream,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut ts),
                        Some(&mut sample),
                    )
                    .map_err(|e| err("read video", e))?;
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    self.eof = true;
                    return Ok(None);
                }
                let Some(sample) = sample else {
                    continue; // stream gap
                };
                let frame = sample_frame(&sample, self.w, self.h, self.stride)?;
                return Ok(Some((ts, frame)));
            }
            Ok(None)
        }
    }

    pub fn seek(&mut self, secs: f64) {
        unsafe {
            let _ = self.reader.Flush(self.stream);
            let mut pv: PROPVARIANT = std::mem::zeroed();
            let inner = &mut *pv.Anonymous.Anonymous;
            inner.vt = VT_I8;
            inner.Anonymous.hVal = (secs * 10_000_000.0) as i64;
            let _ = self.reader.SetCurrentPosition(&GUID::from_u128(0), &pv);
        }
        self.eof = false;
    }
}

unsafe fn sample_frame(
    sample: &IMFSample,
    w: u32,
    h: u32,
    stride: i32,
) -> Result<Frame, MediaError> {
    let buffer: IMFMediaBuffer = sample
        .ConvertToContiguousBuffer()
        .map_err(|e| err("contiguous buffer", e))?;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut len = 0u32;
    buffer
        .Lock(&mut ptr, None, Some(&mut len))
        .map_err(|e| err("lock buffer", e))?;

    let stride_abs = stride.unsigned_abs() as usize;
    let rows = (len as usize / stride_abs.max(1)).min(h as usize);
    let mut px = Vec::with_capacity(w as usize * rows);
    for y in 0..rows {
        let row = if stride >= 0 {
            ptr.add(y * stride_abs)
        } else {
            ptr.add((rows - 1 - y) * stride_abs)
        };
        for x in 0..w as usize {
            let p = row.add(x * 4);
            let b = *p as u32;
            let g = *p.add(1) as u32;
            let r = *p.add(2) as u32;
            px.push(0xff00_0000 | (r << 16) | (g << 8) | b);
        }
    }
    buffer.Unlock().map_err(|e| err("unlock buffer", e))?;
    if rows != h as usize {
        return Err(MediaError::Decode(format!(
            "short RGB frame: {len} bytes for {}x{h}",
            w
        )));
    }
    Ok(Frame { w, h, px })
}

// ---------------------------------------------------------------------------
// Audio

#[derive(Clone, Copy)]
enum Cmd {
    None,
    Play,
    Pause,
    Seek(f64),
    Stop,
}

pub struct AudioHandle {
    stop: Arc<AtomicBool>,
    cmd: Arc<Mutex<Cmd>>,
    looping: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioHandle {
    /// Start the audio thread and wait (briefly) until the endpoint and the
    /// decoder are initialized, so a missing device is reported immediately.
    pub fn start(path: &Path, _duration: f64) -> Result<Self, MediaError> {
        let path = path.to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let cmd = Arc::new(Mutex::new(Cmd::Pause));
        let looping = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), MediaError>>();
        let (s, c, l) = (stop.clone(), cmd.clone(), looping.clone());
        let thread = std::thread::spawn(move || {
            let result = audio_loop(&path, &s, &c, &l, &ready_tx);
            if let Err(e) = result {
                eprintln!("[media] audio: {e}");
            }
        });
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                cmd,
                looping,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(MediaError::Decode("audio init timed out".into())),
        }
    }

    pub fn play(&self) {
        *self.cmd.lock().unwrap() = Cmd::Play;
    }

    pub fn pause(&self) {
        *self.cmd.lock().unwrap() = Cmd::Pause;
    }

    pub fn seek(&self, secs: f64) {
        *self.cmd.lock().unwrap() = Cmd::Seek(secs);
    }

    pub fn set_looping(&self, looping: bool) {
        self.looping.store(looping, Ordering::Relaxed);
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        *self.cmd.lock().unwrap() = Cmd::Stop;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn audio_loop(
    path: &Path,
    stop: &AtomicBool,
    cmd: &Mutex<Cmd>,
    looping: &AtomicBool,
    ready: &mpsc::Sender<Result<(), MediaError>>,
) -> Result<(), MediaError> {
    com_and_mf_init()?;
    let (client, render, buffer_frames, format) = wasapi()?;
    let reader = source_reader(path)?;
    let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
    unsafe {
        reader
            .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
            .map_err(|e| err("audio stream select", e))?;
        reader
            .SetStreamSelection(stream, true)
            .map_err(|e| err("audio stream select", e))?;

        let ty = MFCreateMediaType().map_err(|e| err("audio type", e))?;
        ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(|e| err("audio major", e))?;
        let subtype = if format.float {
            MFAudioFormat_Float
        } else {
            MFAudioFormat_PCM
        };
        ty.SetGUID(&MF_MT_SUBTYPE, &subtype)
            .map_err(|e| err("audio subtype", e))?;
        ty.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, format.channels as u32)
            .map_err(|e| err("audio channels", e))?;
        ty.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, format.rate)
            .map_err(|e| err("audio rate", e))?;
        ty.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, format.bits as u32)
            .map_err(|e| err("audio bits", e))?;
        ty.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, format.block as u32)
            .map_err(|e| err("audio block", e))?;
        reader
            .SetCurrentMediaType(stream, None, &ty)
            .map_err(|e| err("audio output type", e))?;
    }

    ready
        .send(Ok(()))
        .map_err(|_| MediaError::Decode("audio consumer gone".into()))?;

    let mut playing = false;
    let mut pcm: Vec<u8> = Vec::new();
    let mut off = 0usize;
    let mut eof = false;
    let mut started = false;

    while !stop.load(Ordering::Relaxed) {
        // control
        let c = {
            let mut g = cmd.lock().unwrap();
            let c = *g;
            *g = Cmd::None;
            c
        };
        match c {
            Cmd::Play => unsafe {
                let _ = client.Start();
                playing = true;
            },
            Cmd::Pause => unsafe {
                let _ = client.Stop();
                playing = false;
            },
            Cmd::Seek(t) => {
                seek_reader(&reader, stream, t);
                pcm.clear();
                off = 0;
                eof = false;
            }
            Cmd::Stop => break,
            Cmd::None => {}
        }
        if !playing {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        if !started {
            started = true;
        }

        let padding = unsafe { client.GetCurrentPadding() }.unwrap_or(0);
        let avail = buffer_frames.saturating_sub(padding);
        if avail == 0 {
            std::thread::sleep(Duration::from_millis(4));
            continue;
        }
        let ptr = match unsafe { render.GetBuffer(avail) } {
            Ok(p) => p,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(4));
                continue;
            }
        };
        let need = avail as usize * format.block as usize;
        let mut written = 0usize;
        while written < need {
            if off >= pcm.len() {
                match read_audio_sample(&reader, stream, &mut pcm) {
                    Ok(true) => off = 0,
                    Ok(false) => {
                        eof = true;
                        break;
                    }
                    Err(e) => {
                        eprintln!("[media] audio decode: {e}");
                        eof = true;
                        break;
                    }
                }
            }
            let n = (pcm.len() - off).min(need - written);
            unsafe {
                std::ptr::copy_nonoverlapping(pcm.as_ptr().add(off), ptr.add(written), n);
            }
            off += n;
            written += n;
        }
        if written < need {
            // underrun / end: silence keeps the endpoint running cleanly
            unsafe { std::ptr::write_bytes(ptr.add(written), 0, need - written) };
        }
        let _ = unsafe { render.ReleaseBuffer(avail, 0) };

        if eof {
            if looping.load(Ordering::Relaxed) {
                seek_reader(&reader, stream, 0.0);
                pcm.clear();
                off = 0;
                eof = false;
            } else {
                unsafe {
                    let _ = client.Stop();
                }
                playing = false;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    unsafe {
        let _ = client.Stop();
        let _ = MFShutdown();
        CoUninitialize();
    }
    Ok(())
}

struct MixFormat {
    channels: u16,
    rate: u32,
    bits: u16,
    block: u16,
    float: bool,
}

fn wasapi() -> Result<(IAudioClient, IAudioRenderClient, u32, MixFormat), MediaError> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| err("audio endpoint enumerator", e))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| err("default audio endpoint", e))?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| err("audio client", e))?;
        let mix = client.GetMixFormat().map_err(|e| err("mix format", e))?;
        let f = *mix;
        let mut float = f.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16;
        if f.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 && f.cbSize >= 22 {
            let ext = &*(mix as *const WAVEFORMATEXTENSIBLE);
            let sub: GUID = std::ptr::read_unaligned(std::ptr::addr_of!(ext.SubFormat));
            float = sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
        }
        let format = MixFormat {
            channels: f.nChannels,
            rate: f.nSamplesPerSec,
            bits: f.wBitsPerSample,
            block: f.nBlockAlign,
            float,
        };
        let hr = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            0,
            10_000_000, // 1 s buffer
            0,
            mix,
            None,
        );
        CoTaskMemFree(Some(mix as *const core::ffi::c_void));
        hr.map_err(|e| err("audio init", e))?;
        let render: IAudioRenderClient = client
            .GetService()
            .map_err(|e| err("audio render client", e))?;
        let frames = client.GetBufferSize().map_err(|e| err("audio buffer", e))?;
        let _ = DEVICE_STATE_ACTIVE; // documented default-device path
        Ok((client, render, frames, format))
    }
}

fn seek_reader(reader: &IMFSourceReader, stream: u32, secs: f64) {
    unsafe {
        let _ = reader.Flush(stream);
        let mut pv: PROPVARIANT = std::mem::zeroed();
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_I8;
        inner.Anonymous.hVal = (secs * 10_000_000.0) as i64;
        let _ = reader.SetCurrentPosition(&GUID::from_u128(0), &pv);
    }
}

fn read_audio_sample(
    reader: &IMFSourceReader,
    stream: u32,
    pcm: &mut Vec<u8>,
) -> Result<bool, MediaError> {
    unsafe {
        for _ in 0..8 {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            reader
                .ReadSample(
                    stream,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample),
                )
                .map_err(|e| err("read audio", e))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(false);
            }
            let Some(sample) = sample else {
                continue;
            };
            let buffer: IMFMediaBuffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| err("audio buffer", e))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut ptr, None, Some(&mut len))
                .map_err(|e| err("lock audio", e))?;
            pcm.clear();
            pcm.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
            buffer.Unlock().map_err(|e| err("unlock audio", e))?;
            if pcm.is_empty() {
                continue;
            }
            return Ok(true);
        }
        Ok(true)
    }
}
