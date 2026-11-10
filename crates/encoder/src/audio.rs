//! Sound through FFmpeg. Encoding: Opus (libopus) where the build has it,
//! the built-in AAC encoder otherwise. Decoding: Opus (a client's microphone).

use anyhow::{anyhow, bail, Context, Result};
use ff::format::sample::Type;
use ff::format::Sample;
use ffmpeg_next as ff;
use remothing_core::encode::{AudioDecoder, AudioEncoder};
use remothing_core::platform::{AUDIO_CHANNELS, AUDIO_RATE};

/// Transparent for music with either codec at two channels.
const BITRATE: usize = 128_000;

#[derive(Clone, Copy)]
pub(crate) enum Codec {
    Opus,
    Aac,
}

/// Best first: Opus frames are 10 ms and need no look-ahead; AAC's are
/// 21 ms and the encoder holds one back.
pub(crate) const CODECS: [Codec; 2] = [Codec::Opus, Codec::Aac];

impl Codec {
    pub(crate) fn ffmpeg_name(self) -> &'static str {
        match self {
            Codec::Opus => "libopus",
            Codec::Aac => "aac",
        }
    }

    /// Sample layout the FFmpeg encoder takes.
    fn format(self) -> Sample {
        match self {
            Codec::Opus => Sample::I16(Type::Packed),
            Codec::Aac => Sample::F32(Type::Planar),
        }
    }
}

pub(crate) struct FfmpegAudio {
    enc: ff::encoder::audio::Encoder,
    codec: Codec,
    frame: ff::frame::Audio,
    packet: ff::Packet,
    description: Vec<u8>,
    pts: i64,
}

// SAFETY: FFmpeg codec contexts may move between threads; the encoder is
// used by the audio thread only.
unsafe impl Send for FfmpegAudio {}

impl FfmpegAudio {
    pub(crate) fn open(codec: Codec) -> Result<Self> {
        let name = codec.ffmpeg_name();
        let found = ff::encoder::find_by_name(name).ok_or_else(|| anyhow!("not in this FFmpeg build"))?;
        let mut enc = ff::codec::context::Context::new_with_codec(found).encoder().audio()?;
        enc.set_rate(AUDIO_RATE as i32);
        enc.set_channel_layout(ff::ChannelLayout::STEREO);
        enc.set_format(codec.format());
        enc.set_bit_rate(BITRATE);
        enc.set_time_base(ff::Rational::new(1, AUDIO_RATE as i32));
        let mut opts = ff::Dictionary::new();
        if let Codec::Opus = codec {
            // The music mode alone (no speech look-ahead), in short frames.
            opts.set("application", "lowdelay");
            opts.set("frame_duration", "10");
        }
        let enc = enc.open_as_with(found, opts).with_context(|| format!("open {name}"))?;
        let description = match codec {
            // Opus packets describe themselves.
            Codec::Opus => Vec::new(),
            // SAFETY: an opened encoder's extradata is `extradata_size` bytes, or null.
            Codec::Aac => unsafe {
                let ctx = enc.as_ptr();
                match (*ctx).extradata.is_null() {
                    true => Vec::new(),
                    false => std::slice::from_raw_parts((*ctx).extradata, (*ctx).extradata_size as usize).to_vec(),
                }
            },
        };
        let mut frame = ff::frame::Audio::new(codec.format(), enc.frame_size() as usize, ff::ChannelLayout::STEREO);
        frame.set_rate(AUDIO_RATE);
        Ok(Self { enc, codec, frame, packet: ff::Packet::empty(), description, pts: 0 })
    }
}

impl AudioEncoder for FfmpegAudio {
    fn codec(&self) -> &'static str {
        match self.codec {
            Codec::Opus => "opus",
            Codec::Aac => "mp4a.40.2",
        }
    }

    fn description(&self) -> &[u8] {
        &self.description
    }

    fn frame_samples(&self) -> usize {
        self.frame.samples()
    }

    fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<bool> {
        let pairs = pcm.chunks_exact(2);
        match self.codec {
            Codec::Opus => {
                for (dst, src) in self.frame.plane_mut::<(i16, i16)>(0).iter_mut().zip(pairs) {
                    *dst = (src[0], src[1]);
                }
            }
            Codec::Aac => {
                for channel in 0..2 {
                    for (dst, src) in self.frame.plane_mut::<f32>(channel).iter_mut().zip(pairs.clone()) {
                        *dst = f32::from(src[channel]) / 32768.0;
                    }
                }
            }
        }
        self.frame.set_pts(Some(self.pts));
        self.pts += self.frame.samples() as i64;
        self.enc.send_frame(&self.frame)?;
        // An encoder that holds input back has nothing yet for its first frames.
        if self.enc.receive_packet(&mut self.packet).is_err() {
            return Ok(false);
        }
        out.extend_from_slice(self.packet.data().unwrap_or_default());
        Ok(true)
    }
}

pub(crate) struct FfmpegOpusDecoder {
    dec: ff::decoder::Audio,
    frame: ff::frame::Audio,
}

// SAFETY: as for the encoder; used by the microphone thread only.
unsafe impl Send for FfmpegOpusDecoder {}

impl FfmpegOpusDecoder {
    pub(crate) fn open(channels: usize) -> Result<Self> {
        let codec = ff::decoder::find(ff::codec::Id::OPUS).ok_or_else(|| anyhow!("no Opus decoder in this FFmpeg build"))?;
        let mut ctx = ff::codec::context::Context::new_with_codec(codec);
        // SAFETY: plain fields of a context that is not opened yet. Bare
        // Opus packets carry no header, so the decoder is told what to expect.
        unsafe {
            (*ctx.as_mut_ptr()).sample_rate = AUDIO_RATE as i32;
            ff::ffi::av_channel_layout_default(&raw mut (*ctx.as_mut_ptr()).ch_layout, channels as i32);
        }
        let dec = ctx.decoder().open_as(codec).context("open the Opus decoder")?.audio()?;
        Ok(Self { dec, frame: ff::frame::Audio::empty() })
    }
}

impl AudioDecoder for FfmpegOpusDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> Result<()> {
        self.dec.send_packet(&ff::Packet::borrow(packet))?;
        while self.dec.receive_frame(&mut self.frame).is_ok() {
            let samples = self.frame.samples();
            // One channel goes to both sides.
            let right = usize::from(self.frame.channels()).min(AUDIO_CHANNELS) - 1;
            match self.frame.format() {
                Sample::F32(Type::Planar) => {
                    let (left, right) = (self.frame.plane::<f32>(0), self.frame.plane::<f32>(right));
                    for (l, r) in left.iter().zip(right).take(samples) {
                        out.extend([l, r].map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
                    }
                }
                Sample::I16(Type::Packed) if right == 0 => {
                    out.extend(self.frame.plane::<i16>(0).iter().take(samples).flat_map(|&s| [s, s]));
                }
                Sample::I16(Type::Packed) => {
                    out.extend(self.frame.plane::<(i16, i16)>(0).iter().take(samples).flat_map(|&(l, r)| [l, r]));
                }
                other => bail!("the Opus decoder gave samples as {other:?}"),
            }
        }
        Ok(())
    }
}
