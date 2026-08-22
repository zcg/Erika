//! 临时诊断测试：Erika 软件解码器解码本地下载的 fMP4 视频前 200 包——
//! 隔离 HTTP 与硬件解码，定位 `avcodec_send_packet: Invalid data`
//!（系统 ffmpeg 软件解码同一数据正常）。
//!
//! 前置：`%TEMP%\cur_video_1MB.bin`（curl 完整 206 下载，已验证可解）。

use erika::ffmpeg::{Decoder, DecoderBackend, DecoderConfig, Demuxer};

#[test]
fn decode_local_fmp4_with_software_decoder() {
    eprintln!("[diag] stage=open_path");
    let path = std::env::temp_dir().join("cur_video_1MB.bin");
    let mut demuxer = Demuxer::open_path(&path).expect("open local fmp4");
    eprintln!("[diag] stage=codec_params");
    let params = demuxer
        .owned_codec_parameters(0)
        .expect("codec parameters for stream 0");
    eprintln!("[diag] stage=decoder_open");
    let mut decoder = Decoder::open_owned_with_config(
        &params,
        DecoderConfig {
            backend: DecoderBackend::Software,
            ..Default::default()
        },
    )
    .expect("open software decoder");
    eprintln!("[diag] stage=decode_loop");

    let mut sent = 0usize;
    let mut frames = 0usize;
    let mut send_errors = Vec::new();
    loop {
        if sent >= 200 {
            break;
        }
        eprintln!("[diag] read_packet #{sent}");
        match demuxer.read_packet().expect("read_packet") {
            Some(packet) => {
                eprintln!("[diag] send_packet #{sent} size={}", packet.size());
                match decoder.send_packet(&packet) {
                    Ok(()) => sent += 1,
                    Err(e) => {
                        send_errors.push(format!("packet #{sent} size={} err={e}", packet.size()));
                        // 坏包跳过继续（Erika 播放路径是 fatal；这里统计）
                        sent += 1;
                    }
                }
            }
            None => {
                eprintln!("[diag] demux EOF at sent={sent}");
                break;
            }
        }
        // drain 解码帧（加上限防死循环）
        let mut drained = 0usize;
        loop {
            match decoder.receive_frame() {
                Ok(_) => {
                    frames += 1;
                    drained += 1;
                }
                Err(_) => break, // EAGAIN / EOF
            }
            if drained > 100_000 {
                eprintln!("[diag] drain 超过 100k 帧——疑似 receive_frame 死循环");
                break;
            }
        }
        if sent % 25 == 0 {
            eprintln!("[diag] sent={sent} frames={frames}");
        }
    }
    eprintln!("[diag] stage=done sent={sent} frames={frames}");
    eprintln!(
        "结果: 发送 {sent} 包, 解码 {frames} 帧, send 错误 {} 个",
        send_errors.len()
    );
    for e in send_errors.iter().take(5) {
        eprintln!("  {e}");
    }
    assert!(
        send_errors.is_empty(),
        "软件解码出现 send_packet 错误（Erika 解码配置/流程 bug）"
    );
    assert!(frames >= 30, "应至少解码 30 帧，实际 {frames}");
}
