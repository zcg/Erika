//! 临时诊断测试：Erika `open_video_audio`（本地双流文件）+ Software 解码
//! 双流前 200 包——定位 `avcodec_send_packet: Invalid data`（HTTP 播放
//! 路径）是否来自双流合并 demux（本地单流解码已证明无坏包）。
//!
//! 前置：`%TEMP%\cur_video_1MB.bin` + `%TEMP%\cur_audio_1MB.bin`
//!（curl 完整 206 下载，均经 ffprobe 验证）。

use erika::ffmpeg::{Decoder, DecoderBackend, DecoderConfig, DecoderOutputFrame, Demuxer};
use erika::source::LocalFileSource;

#[test]
#[ignore = "deps on %TEMP%/cur_video_1MB.bin + %TEMP%/cur_audio_1MB.bin (manually downloaded 206 captures); run: cargo test -- --ignored"]
fn decode_local_dash_streams_software() {
    let temp = std::env::temp_dir();
    let video = LocalFileSource::open(temp.join("cur_video_1MB.bin")).expect("video file");
    let audio = LocalFileSource::open(temp.join("cur_audio_1MB.bin")).expect("audio file");
    let mut demuxer =
        Demuxer::open_video_audio(Box::new(video), Box::new(audio)).expect("open dual inputs");
    let probe = demuxer.probe();
    eprintln!(
        "[diag] probe tracks={} video={} audio={}",
        probe.tracks.len(),
        probe.video.len(),
        probe.audio.len()
    );

    let video_params = demuxer.owned_codec_parameters(0).expect("video params");
    let audio_params = demuxer.owned_codec_parameters(1).expect("audio params");
    let mut video_dec = Decoder::open_owned_with_config(
        &video_params,
        DecoderConfig {
            backend: DecoderBackend::Software,
            ..Default::default()
        },
    )
    .expect("video decoder");
    let mut audio_dec = Decoder::open_owned_with_config(
        &audio_params,
        DecoderConfig {
            backend: DecoderBackend::Software,
            ..Default::default()
        },
    )
    .expect("audio decoder");

    let mut sent = 0usize;
    let mut v_frames = 0usize;
    let mut a_frames = 0usize;
    let mut send_errors: Vec<String> = Vec::new();
    while sent < 200 {
        let Some(packet) = demuxer.read_packet().expect("read_packet") else {
            eprintln!("[diag] EOF at {sent}");
            break;
        };
        let is_video = packet.stream_index() == 0;
        let r = if is_video {
            video_dec.send_packet(&packet)
        } else {
            audio_dec.send_packet(&packet)
        };
        match r {
            Ok(()) => sent += 1,
            Err(e) => {
                send_errors.push(format!(
                    "{} packet #{sent} stream={} size={} err={e}",
                    if is_video { "V" } else { "A" },
                    packet.stream_index(),
                    packet.size()
                ));
                sent += 1;
            }
        }
        // drain 两个解码器（Frame 计数，NeedMoreInput/EndOfStream 退出）
        loop {
            match video_dec.receive_frame().expect("v receive") {
                DecoderOutputFrame::Frame(_) => v_frames += 1,
                _ => break,
            }
        }
        loop {
            match audio_dec.receive_frame().expect("a receive") {
                DecoderOutputFrame::Frame(_) => a_frames += 1,
                _ => break,
            }
        }
    }
    eprintln!(
        "[diag] done sent={sent} v_frames={v_frames} a_frames={a_frames} errors={}",
        send_errors.len()
    );
    for e in send_errors.iter().take(8) {
        eprintln!("  {e}");
    }
    assert!(
        send_errors.is_empty(),
        "双流软件解码出现 send_packet 错误（双流合并 demux bug）"
    );
}
