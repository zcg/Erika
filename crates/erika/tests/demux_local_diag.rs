//! 临时诊断测试：Erika `Demuxer` 读本地下载的 B 站 fMP4 视频，检查
//! 前 200 个 packet 的流/大小/DTS 序列是否合理——定位
//! `avcodec_send_packet: Invalid data`（系统 ffmpeg 解码同一数据正常，
//! 嫌疑在 Erika demux 输出端）。
//!
//! 前置：`%TEMP%\cur_video_1MB.bin`（curl 完整 206 下载的 0-1MB，
//! 系统 ffmpeg 可解码前 317 帧）。

use erika::ffmpeg::Demuxer;

#[test]
fn demux_local_fmp4_packet_sequence() {
    let path = std::env::temp_dir().join("cur_video_1MB.bin");
    let demuxer = Demuxer::open_path(&path).expect("open local fmp4");
    let probe = demuxer.probe();
    eprintln!(
        "probe: duration={:?} tracks={} video={}",
        probe.duration,
        probe.tracks.len(),
        probe.video.len()
    );

    let mut demuxer = demuxer;
    let mut packets = 0usize;
    let mut zero_size = 0usize;
    let mut weird = Vec::new();
    let mut last_dts: Option<f64> = None;
    let mut dts_regress = 0usize;
    while packets < 200 {
        let Some(packet) = demuxer.read_packet().expect("read_packet") else {
            eprintln!("EOF after {packets} packets");
            break;
        };
        let size = packet.size();
        if size == 0 {
            zero_size += 1;
        }
        let dts = packet.dts().map(|t| t.seconds());
        if let (Some(last), Some(cur)) = (last_dts, dts) {
            if cur < last - 0.01 {
                dts_regress += 1;
                if dts_regress <= 3 {
                    weird.push(format!("dts regress {last:.3} -> {cur:.3}"));
                }
            }
        }
        last_dts = dts.or(last_dts);
        if size > 1_000_000 {
            weird.push(format!("huge packet size={size}"));
        }
        if packets < 8 {
            let data = packet.data();
            let head: Vec<String> = data.iter().take(8).map(|b| format!("{b:02X}")).collect();
            eprintln!(
                "packet #{packets}: stream={} size={size} dts={:?} head=[{}]",
                packet.stream_index(),
                dts,
                head.join(" ")
            );
        }
        packets += 1;
    }
    eprintln!(
        "结果: 共读 {packets} 包, size=0 的包 {zero_size}, DTS 回退 {dts_regress} 次, 异常: {:?}",
        weird
    );
    assert!(packets >= 20, "应至少读到 20 个 packet");
    assert!(zero_size == 0, "存在 size=0 的 packet（数据损坏嫌疑）");
    assert!(weird.is_empty(), "packet 序列异常: {weird:?}");
}
