//! What the track being heard is encoded as: codec, sample rate, bit depth, bitrate. The player
//! bar and the mini player show it as a quality readout.
//!
//! Everything comes from observed properties, never `get_property`, for the reason `event_loop`
//! gives: asking mpv synchronously from the event thread can stall it exactly when mpv is busiest.

use libmpv2::events::{EventContext, PropertyData};
use libmpv2::Format;

/// The quality readout for one file. A field mpv can't tell is `None`, and the UI leaves it out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioFormat {
    /// ffmpeg's decoder name as mpv reports it: `opus`, `aac`, `flac`, `pcm_s24le`, `mp3float`.
    pub codec: Option<String>,
    /// What the decoder outputs, which for every codec this app meets is the source's own rate.
    pub sample_rate: Option<u32>,
    /// Lossless only. A perceptual codec has no bit depth to speak of: its decoder picks a sample
    /// format (opus and aac decode to float) that says nothing about the source.
    pub bit_depth: Option<u8>,
    pub bitrate_kbps: Option<u32>,
    pub lossless: bool,
}

/// Observed after the four `spawn_deck_events` asks for, so the ids don't collide.
///
/// Not `current-tracks/audio/demux-bitrate`, the container's declared bitrate: webm (YouTube's
/// opus) carries none, and since an unavailable property never reaches us (see [`Tracker`]), an
/// m4a's declared rate would stay on screen through every opus track after it.
const PROPS: [(&str, Format); 4] = [
    ("audio-codec-name", Format::String),
    ("audio-params/samplerate", Format::Int64),
    ("audio-params/format", Format::String),
    // Measured from packet sizes, about once a second.
    ("audio-bitrate", Format::Double),
];

pub(crate) fn observe(ev: &EventContext) -> Result<(), libmpv2::Error> {
    for (i, (name, format)) in PROPS.iter().enumerate() {
        ev.observe_property(name, *format, 100 + i as u64)?;
    }
    Ok(())
}

pub(crate) fn is_format_property(name: &str) -> bool {
    PROPS.iter().any(|(n, _)| *n == name)
}

/// One deck's view of its current file, built up from property changes.
///
/// Nothing is cleared between files. libmpv2 swallows the "property unavailable" change mpv sends
/// as a file ends (`wait_event` returns `None` for it), so a cleared field would only come back if
/// the next file's value *differs*; two opus tracks in a row would leave the second one blank.
/// Kept, a field is still right for the next file unless a change says otherwise. The measured
/// bitrate is the exception: it is an average over one file, so [`Tracker::new_file`] restarts it.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    codec: Option<String>,
    rate: Option<u32>,
    sample_fmt: Option<String>,
    sum_bps: f64,
    samples: u32,
    measured_kbps: Option<u32>,
    sent: Option<AudioFormat>,
}

impl Tracker {
    /// Take one property change. `Some` when the readout changed and is worth sending.
    pub(crate) fn apply(&mut self, name: &str, change: &PropertyData) -> Option<AudioFormat> {
        match (name, change) {
            ("audio-codec-name", PropertyData::Str(s)) => self.codec = Some((*s).to_owned()),
            ("audio-params/samplerate", PropertyData::Int64(r)) => {
                self.rate = u32::try_from(*r).ok().filter(|r| *r > 0)
            }
            ("audio-params/format", PropertyData::Str(s)) => {
                self.sample_fmt = Some((*s).to_owned())
            }
            ("audio-bitrate", PropertyData::Double(b)) if *b > 0.0 => self.sample_bitrate(*b),
            _ => return None,
        }
        let now = self.current();
        (self.sent.as_ref() != Some(&now) && now.codec.is_some()).then(|| {
            self.sent = Some(now.clone());
            now
        })
    }

    /// A new file started on this deck: its bitrate is its own, not an average with the last one.
    pub(crate) fn new_file(&mut self) {
        self.sum_bps = 0.0;
        self.samples = 0;
        self.measured_kbps = None;
    }

    /// Fold one measurement into the file's running average. A VBR stream swings by tens of kbps
    /// second to second, so the shown value only moves once the average has drifted noticeably:
    /// it settles within a few seconds instead of flickering for the whole track.
    fn sample_bitrate(&mut self, bps: f64) {
        self.sum_bps += bps;
        self.samples += 1;
        let Some(avg) = kbps(self.sum_bps / self.samples as f64) else { return };
        let moved = match self.measured_kbps {
            None => true,
            Some(shown) => avg.abs_diff(shown) >= (shown / 25).max(3),
        };
        if moved {
            self.measured_kbps = Some(avg);
        }
    }

    fn current(&self) -> AudioFormat {
        let lossless = self.codec.as_deref().is_some_and(is_lossless);
        AudioFormat {
            codec: self.codec.clone(),
            sample_rate: self.rate,
            bit_depth: if lossless {
                self.codec
                    .as_deref()
                    .and_then(pcm_bits)
                    .or_else(|| self.sample_fmt.as_deref().and_then(decoded_bits))
            } else {
                None
            },
            bitrate_kbps: self.measured_kbps,
            lossless,
        }
    }
}

fn kbps(bps: f64) -> Option<u32> {
    let k = (bps / 1000.0).round();
    (k.is_finite() && k >= 1.0).then_some(k as u32)
}

fn is_lossless(codec: &str) -> bool {
    codec.starts_with("pcm_")
        || matches!(
            codec,
            "flac"
                | "alac"
                | "ape"
                | "wavpack"
                | "tta"
                | "tak"
                | "truehd"
                | "mlp"
                | "shorten"
                | "wmalossless"
        )
}

/// A PCM decoder's name says its width outright: `pcm_s24le` is 24-bit, `pcm_f32le` 32.
fn pcm_bits(codec: &str) -> Option<u8> {
    let rest = codec.strip_prefix("pcm_")?.get(1..)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The decoder's sample format, for a lossless codec that isn't PCM. ffmpeg has no 24-bit sample
/// format, so a 24-bit FLAC or ALAC decodes to `s32`: read as 24, since a true 32-bit integer
/// master is vanishingly rare and 24 is what such a file almost always is. mpv doesn't expose
/// `bits_per_raw_sample`, which would say it exactly.
fn decoded_bits(sample_fmt: &str) -> Option<u8> {
    match sample_fmt.trim_end_matches('p') {
        "u8" => Some(8),
        "s16" => Some(16),
        "s32" => Some(24),
        "float" => Some(32),
        "s64" | "double" => Some(64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(t: &mut Tracker, changes: &[(&str, PropertyData)]) -> Option<AudioFormat> {
        let mut last = None;
        for (name, change) in changes {
            if let Some(f) = t.apply(name, change) {
                last = Some(f);
            }
        }
        last
    }

    #[test]
    fn opus_has_no_bit_depth_and_uses_measured_bitrate() {
        let mut t = Tracker::default();
        let f = feed(
            &mut t,
            &[
                ("audio-codec-name", PropertyData::Str("opus")),
                ("audio-params/samplerate", PropertyData::Int64(48000)),
                ("audio-params/format", PropertyData::Str("floatp")),
                ("audio-bitrate", PropertyData::Double(158_400.0)),
            ],
        )
        .unwrap();
        assert_eq!(f.codec.as_deref(), Some("opus"));
        assert_eq!(f.sample_rate, Some(48000));
        assert_eq!(f.bit_depth, None);
        assert_eq!(f.bitrate_kbps, Some(158));
        assert!(!f.lossless);
    }

    #[test]
    fn flac_24_bit_reads_from_s32() {
        let mut t = Tracker::default();
        let f = feed(
            &mut t,
            &[
                ("audio-codec-name", PropertyData::Str("flac")),
                ("audio-params/samplerate", PropertyData::Int64(96000)),
                ("audio-params/format", PropertyData::Str("s32")),
            ],
        )
        .unwrap();
        assert!(f.lossless);
        assert_eq!(f.bit_depth, Some(24));
    }

    #[test]
    fn pcm_width_comes_from_the_codec_name() {
        let mut t = Tracker::default();
        let f = feed(
            &mut t,
            &[
                ("audio-codec-name", PropertyData::Str("pcm_s16le")),
                ("audio-params/format", PropertyData::Str("s16")),
            ],
        )
        .unwrap();
        assert_eq!(f.bit_depth, Some(16));
        assert_eq!(pcm_bits("pcm_f32le"), Some(32));
        assert_eq!(pcm_bits("pcm_s24be"), Some(24));
    }

    #[test]
    fn measured_bitrate_ignores_small_swings_and_restarts_per_file() {
        let mut t = Tracker::default();
        t.apply("audio-codec-name", &PropertyData::Str("opus"));
        t.apply("audio-bitrate", &PropertyData::Double(160_000.0));
        // 160 → avg 161: under the threshold, nothing to send.
        assert!(t.apply("audio-bitrate", &PropertyData::Double(162_000.0)).is_none());
        t.new_file();
        let f = t.apply("audio-bitrate", &PropertyData::Double(96_000.0)).unwrap();
        assert_eq!(f.bitrate_kbps, Some(96));
    }

    #[test]
    fn nothing_is_sent_before_a_codec_is_known_or_when_unchanged() {
        let mut t = Tracker::default();
        assert!(t.apply("audio-params/samplerate", &PropertyData::Int64(44100)).is_none());
        assert!(t.apply("audio-codec-name", &PropertyData::Str("aac")).is_some());
        assert!(t.apply("audio-codec-name", &PropertyData::Str("aac")).is_none());
        assert_eq!(t.sent.unwrap().sample_rate, Some(44100));
    }
}
