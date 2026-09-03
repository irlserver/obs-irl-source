//! Builder tests that need no running OBS: what the frame builders put where
//! in the raw `obs_source_frame` / `obs_source_audio`, plus the small pure
//! helpers around them.

use obs::{AudioFormat, AudioFrame, SpeakerLayout, VideoFormat, VideoFrame};

#[test]
fn video_frame_places_planes_and_timestamp() {
    let y = vec![0u8; 1920 * 1080];
    let u = vec![0u8; 960 * 540];
    let v = vec![0u8; 960 * 540];

    let frame = VideoFrame::new(1920, 1080, VideoFormat::I420)
        .plane(0, &y, 1920)
        .plane(1, &u, 960)
        .plane(2, &v, 960)
        .timestamp(1_234_567_890);

    let sys = frame.as_sys();
    assert_eq!(sys.width, 1920);
    assert_eq!(sys.height, 1080);
    assert_eq!(sys.format, obs::sys::video_format::VIDEO_FORMAT_I420);
    assert_eq!(sys.timestamp, 1_234_567_890);

    assert_eq!(sys.data[0].cast_const(), y.as_ptr());
    assert_eq!(sys.data[1].cast_const(), u.as_ptr());
    assert_eq!(sys.data[2].cast_const(), v.as_ptr());
    assert_eq!(sys.linesize[0], 1920);
    assert_eq!(sys.linesize[1], 960);
    assert_eq!(sys.linesize[2], 960);

    // Untouched planes stay NULL with a zero stride, which is how libobs tells
    // a two-plane NV12 frame from a three-plane I420 one.
    for i in 3..obs::sys::MAX_AV_PLANES {
        assert!(sys.data[i].is_null());
        assert_eq!(sys.linesize[i], 0);
    }

    // Nothing has set colorimetry yet, so the whole colour block is zero.
    assert!(!sys.full_range);
    assert_eq!(sys.color_matrix, [0.0f32; 16]);
    assert!(!sys.flip);
}

#[test]
fn video_frame_colorimetry_sets_full_range_flag() {
    let plane = vec![0u8; 64];

    let partial = VideoFrame::new(8, 8, VideoFormat::Nv12)
        .plane(0, &plane, 8)
        .colorimetry(obs::ColorSpace::Bt709, obs::ColorRange::Partial);
    assert!(!partial.as_sys().full_range);

    let full = VideoFrame::new(8, 8, VideoFormat::Nv12)
        .plane(0, &plane, 8)
        .colorimetry(obs::ColorSpace::Bt709, obs::ColorRange::Full);
    assert!(full.as_sys().full_range);

    // BT.709 is a combination libobs knows, so the matrix must have been
    // written (it is all zeroes before the call).
    assert_ne!(full.as_sys().color_matrix, [0.0f32; 16]);
}

#[test]
fn audio_frame_is_single_plane_interleaved() {
    let samples = vec![0u8; 1024 * 2 * 4];
    let audio = AudioFrame::interleaved(
        &samples,
        1024,
        SpeakerLayout::Stereo,
        48_000,
        AudioFormat::Float,
        42,
    );

    let sys = audio.as_sys();
    assert_eq!(sys.data[0], samples.as_ptr());
    for i in 1..obs::sys::MAX_AV_PLANES {
        assert!(sys.data[i].is_null());
    }
    assert_eq!(sys.frames, 1024);
    assert_eq!(sys.samples_per_sec, 48_000);
    assert_eq!(sys.timestamp, 42);
    assert_eq!(sys.speakers, obs::sys::speaker_layout::SPEAKERS_STEREO);
    assert_eq!(sys.format, obs::sys::audio_format::AUDIO_FORMAT_FLOAT);
}

#[test]
fn speaker_layout_matches_the_c_cast() {
    // The C plugin casts the channel count straight to `enum speaker_layout`,
    // which is only meaningful for the values libobs defines.
    assert_eq!(SpeakerLayout::from_channels(1), SpeakerLayout::Mono);
    assert_eq!(SpeakerLayout::from_channels(2), SpeakerLayout::Stereo);
    assert_eq!(
        SpeakerLayout::from_channels(3),
        SpeakerLayout::Stereo2Point1
    );
    assert_eq!(SpeakerLayout::from_channels(4), SpeakerLayout::Quad4Point0);
    assert_eq!(SpeakerLayout::from_channels(5), SpeakerLayout::Quad4Point1);
    assert_eq!(
        SpeakerLayout::from_channels(6),
        SpeakerLayout::Surround5Point1
    );
    assert_eq!(
        SpeakerLayout::from_channels(8),
        SpeakerLayout::Surround7Point1
    );

    // 7 has no libobs layout, and neither does 0 or anything above 8.
    for channels in [0, 7, 9, 16, 255] {
        assert_eq!(
            SpeakerLayout::from_channels(channels),
            SpeakerLayout::Unknown,
            "{channels} channels"
        );
    }

    // Every mapped value must land on the numeric layout libobs defines, since
    // that is what the C cast produced.
    for channels in [1u32, 2, 3, 4, 5, 6, 8] {
        assert_eq!(
            SpeakerLayout::from_channels(channels).to_sys() as u32,
            channels
        );
    }
}

#[test]
fn panic_payloads_render_as_text() {
    let str_payload: Box<dyn std::any::Any + Send> = Box::new("boom");
    assert_eq!(obs::panic::payload_message(str_payload.as_ref()), "boom");

    let string_payload: Box<dyn std::any::Any + Send> = Box::new(String::from("boom 2"));
    assert_eq!(
        obs::panic::payload_message(string_payload.as_ref()),
        "boom 2"
    );

    let other: Box<dyn std::any::Any + Send> = Box::new(7u32);
    assert_eq!(
        obs::panic::payload_message(other.as_ref()),
        "<non-string panic payload>"
    );
}
