/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Actual movie playback for `MPMoviePlayerController`.
//!
//! Supports MPEG-4 files with H.264 video (decoded with OpenH264, so Baseline
//! profile only) and an optional AAC audio track (decoded with Symphonia).
//! This is what iPhone OS 2 and 3 era apps typically use.
//!
//! Playback is modal: like a full-screen movie on the original iPhone OS, the
//! movie takes over the screen until it finishes or the user skips it (by
//! tapping/clicking, or pressing Escape, Space or Return). The app does not
//! run in the meantime.

use crate::audio::openal as al;
use crate::audio::symphonia_formats::decode_symphonia_to_pcm;
use crate::fs::GuestPath;
use crate::gles::gles11_raw as gles11; // constants only
use crate::gles::present::present_frame;
use crate::matrix::Matrix;
use crate::window::Event;
use crate::Environment;
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use std::io::Cursor;
use std::time::{Duration, Instant};
use symphonia::core::codecs::video::well_known::extra_data::VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG;
use symphonia::core::codecs::video::well_known::CODEC_ID_H264;
use symphonia::core::formats::FormatReader;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::packet::Packet;
use symphonia::core::units::TimeBase;

/// How long after a movie starts before user input can skip it.
const SKIP_GRACE_PERIOD: Duration = Duration::from_millis(1000);

/// `AL_SEC_OFFSET` from the OpenAL 1.1 specification.
const AL_SEC_OFFSET: al::al_types::ALenum = 0x1024;

/// H.264 video track being demuxed (with Symphonia), ready to decode.
struct VideoTrack {
    reader: Box<dyn FormatReader>,
    track_id: u32,
    time_base: TimeBase,
    /// Size of the NAL unit length prefixes in samples, usually 4.
    nal_length_size: usize,
    /// SPS and PPS in Annex B format, to prime the decoder.
    parameter_sets: Vec<u8>,
    /// Packet read ahead of time that isn't due to be shown yet.
    pending: Option<Packet>,
    finished: bool,
}

/// Parses an `AVCDecoderConfigurationRecord` (`avcC`), returning the NAL unit
/// length size and the SPS and PPS in Annex B format.
fn parse_avcc(avcc: &[u8]) -> Option<(usize, Vec<u8>)> {
    let nal_length_size = (*avcc.get(4)? & 0x3) as usize + 1;
    let mut parameter_sets = Vec::new();
    let mut pos = 5;
    // SPS count is in the low 5 bits, PPS count is a full byte.
    for mask in [0x1f, 0xff] {
        let count = *avcc.get(pos)? & mask;
        pos += 1;
        for _ in 0..count {
            let len = u16::from_be_bytes([*avcc.get(pos)?, *avcc.get(pos + 1)?]) as usize;
            pos += 2;
            parameter_sets.extend_from_slice(&[0, 0, 0, 1]);
            parameter_sets.extend_from_slice(avcc.get(pos..pos + len)?);
            pos += len;
        }
    }
    Some((nal_length_size, parameter_sets))
}

fn open_video_track(bytes: Vec<u8>) -> Result<VideoTrack, String> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
    let reader = symphonia::default::get_probe()
        .probe(
            &Default::default(),
            mss,
            Default::default(),
            Default::default(),
        )
        .map_err(|e| format!("couldn't parse movie file: {e}"))?;
    let track = reader
        .tracks()
        .iter()
        .find(|t| {
            t.codec_params
                .as_ref()
                .and_then(|p| p.video())
                .is_some_and(|v| v.codec == CODEC_ID_H264)
        })
        .ok_or("no H.264 video track")?;
    let video_params = track.codec_params.as_ref().unwrap().video().unwrap();
    let avcc = video_params
        .extra_data
        .iter()
        .find(|e| e.id == VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG)
        .ok_or("no H.264 decoder configuration")?;
    let (nal_length_size, parameter_sets) =
        parse_avcc(&avcc.data).ok_or("invalid H.264 decoder configuration")?;
    let time_base = track.time_base.ok_or("video track has no time base")?;
    let track_id = track.id;
    Ok(VideoTrack {
        reader,
        track_id,
        time_base,
        nal_length_size,
        parameter_sets,
        pending: None,
        finished: false,
    })
}

impl VideoTrack {
    /// Returns the next video packet and its presentation time in seconds if
    /// it is due by `now` (in seconds).
    fn next_due(&mut self, now: f64) -> Option<(Packet, f64)> {
        if self.pending.is_none() && !self.finished {
            loop {
                match self.reader.next_packet() {
                    Ok(Some(packet)) if packet.track_id == self.track_id => {
                        self.pending = Some(packet);
                        break;
                    }
                    Ok(Some(_)) => continue, // other tracks, e.g. audio
                    Ok(None) | Err(_) => {
                        self.finished = true;
                        break;
                    }
                }
            }
        }
        let pts = self
            .time_base
            .calc_time_saturating(self.pending.as_ref()?.pts)
            .as_secs_f64();
        if pts > now {
            return None;
        }
        Some((self.pending.take().unwrap(), pts))
    }

    /// Converts a sample (length-prefixed NAL units) to Annex B format (start
    /// code-prefixed NAL units), which OpenH264 expects.
    fn to_annex_b(&self, mut sample: &[u8]) -> Vec<u8> {
        let n = self.nal_length_size;
        let mut out = Vec::with_capacity(sample.len() + 16);
        while sample.len() >= n {
            let len = sample[..n]
                .iter()
                .fold(0usize, |acc, &b| (acc << 8) | b as usize)
                .min(sample.len() - n);
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&sample[n..n + len]);
            sample = &sample[n + len..];
        }
        out
    }
}

/// Audio track playing via OpenAL, also used as the playback clock.
struct AudioPlayback {
    source: al::al_types::ALuint,
    buffer: al::al_types::ALuint,
}

fn start_audio(env: &mut Environment, bytes: Vec<u8>) -> Option<AudioPlayback> {
    let pcm = match decode_symphonia_to_pcm(Cursor::new(bytes)) {
        Ok(pcm) => pcm,
        Err(()) => {
            log!("Movie has no supported audio track, playing without sound.");
            return None;
        }
    };
    let format = match pcm.channels {
        1 => al::AL_FORMAT_MONO16,
        2 => al::AL_FORMAT_STEREO16,
        n => {
            log!("Movie audio has unsupported channel count {n}, playing without sound.");
            return None;
        }
    };
    log!(
        "Movie audio: {:.1}s, {} Hz, {} channel(s).",
        pcm.bytes.len() as f64 / (2 * pcm.channels * pcm.sample_rate) as f64,
        pcm.sample_rate,
        pcm.channels
    );
    let context = env
        .framework_state
        .audio_toolbox
        .make_al_context_current(&mut env.openal_manager);
    let mut source = 0;
    let mut buffer = 0;
    unsafe {
        context.GenSources(1, &mut source);
        context.GenBuffers(1, &mut buffer);
        context.BufferData(
            buffer,
            format,
            pcm.bytes.as_ptr() as *const _,
            pcm.bytes.len() as _,
            pcm.sample_rate as _,
        );
        context.Sourcei(source, al::AL_BUFFER, buffer as _);
        context.SourcePlay(source);
        if context.GetError() != 0 {
            log!("Warning: OpenAL error while starting movie audio.");
        }
    }
    Some(AudioPlayback { source, buffer })
}

impl AudioPlayback {
    /// Playback position in seconds, or [None] once the audio has finished.
    fn position(&self, env: &mut Environment) -> Option<f64> {
        let context = env
            .framework_state
            .audio_toolbox
            .make_al_context_current(&mut env.openal_manager);
        let mut state = 0;
        let mut offset = 0.0f32;
        unsafe {
            context.GetSourcei(self.source, al::AL_SOURCE_STATE, &mut state);
            context.GetSourcef(self.source, AL_SEC_OFFSET, &mut offset);
        }
        if state == al::AL_STOPPED {
            None
        } else {
            Some(offset as f64)
        }
    }

    fn stop(self, env: &mut Environment) {
        let context = env
            .framework_state
            .audio_toolbox
            .make_al_context_current(&mut env.openal_manager);
        unsafe {
            context.SourceStop(self.source);
            context.DeleteSources(1, &self.source);
            context.DeleteBuffers(1, &self.buffer);
        }
    }
}

/// Presents one RGBA frame (rows top-to-bottom), letterboxed to fit the app's
/// viewport.
fn present_video_frame(
    env: &mut Environment,
    texture: &mut Option<u32>,
    rgba: &[u8],
    w: u32,
    h: u32,
) {
    let window = env.window_mut();
    let (vx, vy, vw, vh) = window.viewport();
    let y_offset = window.viewport_y_offset();
    // Fit the video inside the viewport, preserving its aspect ratio.
    let scale = (vw as f32 / w as f32).min(vh as f32 / h as f32);
    let (fw, fh) = ((w as f32 * scale) as u32, (h as f32 * scale) as u32);
    let viewport = (vx + (vw - fw) / 2, vy + (vh - fh) / 2 + y_offset, fw, fh);

    // OpenGL wants rows bottom-to-top.
    let row = (w * 4) as usize;
    let mut flipped = Vec::with_capacity(rgba.len());
    for y in (0..h as usize).rev() {
        flipped.extend_from_slice(&rgba[y * row..(y + 1) * row]);
    }

    let mut gles = window.make_internal_gl_ctx_current();
    unsafe {
        gles.BindFramebufferOES(gles11::FRAMEBUFFER_OES, 0);
        let tex = *texture.get_or_insert_with(|| {
            let mut tex = 0;
            gles.GenTextures(1, &mut tex);
            tex
        });
        gles.BindTexture(gles11::TEXTURE_2D, tex);
        gles.TexParameteri(
            gles11::TEXTURE_2D,
            gles11::TEXTURE_MIN_FILTER,
            gles11::LINEAR as _,
        );
        gles.TexParameteri(
            gles11::TEXTURE_2D,
            gles11::TEXTURE_MAG_FILTER,
            gles11::LINEAR as _,
        );
        gles.PixelStorei(gles11::UNPACK_ALIGNMENT, 1);
        gles.TexImage2D(
            gles11::TEXTURE_2D,
            0,
            gles11::RGBA as _,
            w as _,
            h as _,
            0,
            gles11::RGBA,
            gles11::UNSIGNED_BYTE,
            flipped.as_ptr() as *const _,
        );
        gles.Color4f(1.0, 1.0, 1.0, 1.0);
        gles.Disable(gles11::BLEND);
        gles.MatrixMode(gles11::PROJECTION);
        gles.LoadIdentity();
        gles.MatrixMode(gles11::MODELVIEW);
        gles.LoadIdentity();
        present_frame(gles.as_mut(), viewport, Matrix::identity(), None);
    }
    std::mem::drop(gles);
    window.swap_window();
}

/// Play the movie at `path`, blocking until it ends or is skipped.
///
/// Returns [Err] (without having shown anything) if the movie can't be played,
/// so the caller can fall back to pretending it played.
pub fn play_movie_blocking(env: &mut Environment, path: &GuestPath) -> Result<(), String> {
    if env.window.is_none() {
        return Err("headless mode".to_string());
    }
    let bytes = env
        .fs
        .read(path)
        .map_err(|()| format!("couldn't read {path:?}"))?;
    let mut video = open_video_track(bytes.clone())?;
    let mut decoder = Decoder::new().map_err(|e| format!("couldn't create decoder: {e}"))?;
    // Feeding SPS/PPS alone doesn't produce a frame, so ignore the result.
    let _ = decoder.decode(&video.parameter_sets);

    log!(
        "Playing movie {:?}. Click or press Escape, Space or Return to skip.",
        path
    );

    let audio = start_audio(env, bytes);
    let start = Instant::now();
    let mut texture = None;
    let mut last_pts = 0.0;
    let mut skipped = false;
    let mut quit = false;

    'playback: loop {
        // Handle input. Events must be polled on the main stack.
        let (skip_input, quit_input) = env.on_parent_stack_in_coroutine(|window, options| {
            window.poll_for_events(options);
            let mut skip = window.is_skip_key_pressed();
            let mut quit = false;
            while let Some(event) = window.pop_event() {
                match event {
                    Event::Quit | Event::AppWillResignActive | Event::AppWillTerminate => {
                        quit = true;
                    }
                    Event::TouchesDown(..) => skip = true,
                    _ => (),
                }
            }
            (skip, quit)
        });
        // Ignore skip input at the very start, so a click or key press that
        // was meant for something else (e.g. launching the app, or the
        // previous screen) doesn't skip the movie by accident.
        if start.elapsed() >= SKIP_GRACE_PERIOD {
            skipped |= skip_input;
        }
        quit |= quit_input;
        if skipped || quit {
            break;
        }

        // Use the audio as the clock while it's playing, so the picture stays
        // in sync with it. Otherwise (no audio, or it ended early), use the
        // time since playback started.
        let audio_pos = audio.as_ref().and_then(|audio| audio.position(env));
        let audio_done = audio_pos.is_none();
        let now = audio_pos.unwrap_or_else(|| start.elapsed().as_secs_f64());

        // Decode up to the current time, showing the latest frame.
        let mut frame: Option<(Vec<u8>, u32, u32)> = None;
        while let Some((packet, pts)) = video.next_due(now) {
            last_pts = pts;
            if let Ok(Some(yuv)) = decoder.decode(&video.to_annex_b(&packet.data)) {
                let (w, h) = yuv.dimensions();
                let mut rgba = frame.take().map(|f| f.0).unwrap_or_default();
                rgba.resize(w * h * 4, 0);
                yuv.write_rgba8(&mut rgba);
                frame = Some((rgba, w as u32, h as u32));
            }
        }
        if let Some((rgba, w, h)) = frame {
            present_video_frame(env, &mut texture, &rgba, w, h);
        }

        let video_done = video.finished && video.pending.is_none();
        if video_done && audio_done && now > last_pts + 0.1 {
            break 'playback;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    if let Some(audio) = audio {
        audio.stop(env);
    }
    if let Some(texture) = texture {
        let mut gles = env.window_mut().make_internal_gl_ctx_current();
        unsafe { gles.DeleteTextures(1, &texture) };
    }
    if skipped {
        log!("Movie skipped.");
    } else {
        log!("Movie finished.");
    }
    if quit {
        echo!("User requested quit, exiting.");
        crate::frameworks::uikit::ui_application::exit(env);
    }
    Ok(())
}
