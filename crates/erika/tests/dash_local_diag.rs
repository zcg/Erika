//! 临时诊断测试：用 Erika 自己的 ffmpeg（8.1.2）+ 自定义 AVIO 打开
//! 本地下载的 B 站 DASH 双流文件——隔离 HTTP 层，定位
//! `avformat_open_input(custom_io): Invalid data` 是数据问题还是
//! HTTP/AVIO 路径问题。文件由调查会话从播放 URL 下载
//! （video_head_10MB.bin / audio_head_10MB.bin，10MB 截断，moov/moof 完整）。

use erika::ffmpeg::Demuxer;
use erika::source::LocalFileSource;

#[test]
fn opens_bilibili_dash_streams_from_local_files() {
    let temp = std::env::temp_dir();
    let video_path = temp.join("video_head_10MB.bin");
    let audio_path = temp.join("audio_head_10MB.bin");
    if !video_path.exists() || !audio_path.exists() {
        eprintln!("skip: 本地样本文件不存在（先运行调查脚本下载）");
        return;
    }

    let video = LocalFileSource::open(&video_path).expect("open video file");
    let audio = LocalFileSource::open(&audio_path).expect("open audio file");

    let demuxer = Demuxer::open_video_audio(Box::new(video), Box::new(audio))
        .expect("open_video_audio 应成功（数据经 ffprobe 8.1.2 验证有效）");

    let probe = demuxer.probe();
    eprintln!(
        "open_video_audio OK: duration={:?} tracks={} video={} audio={}",
        probe.duration,
        probe.tracks.len(),
        probe.video.len(),
        probe.audio.len(),
    );
    assert!(!probe.video.is_empty(), "应有视频轨");
    assert!(!probe.audio.is_empty(), "应有音频轨");
}
