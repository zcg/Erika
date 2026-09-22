//! 诊断测试（非产品路径）：测量一次 DASH `open` 在各阶段花了多少时间、
//! 以及从源消费了多少字节。
//!
//! 要回答的问题：「点播放 → 真正开始播放」之间内核停在 `Opening` 的那几秒，
//! 是网络慢、是 demux 在读数据、还是解码器初始化——宿主侧只能看到
//! `duration` 仍为 0，区分不了。
//!
//! 样本来自宿主真实的磁盘预载桶（`%LOCALAPPDATA%\bili-rs\preload\*.part`
//! 的前 32 字节是 `ERIKAPRE` 头 + 起始偏移 + 长度，其后即 B 站原始 m4s
//! 字节）。把桶文件去掉 32 字节头写到下面两个路径即可运行；样本缺失时本
//! 测试跳过（不失败），以便在干净检出上无副作用。
//!
//! 运行：
//! ```text
//! cargo test --manifest-path D:\Projects\Erika\crates\erika\Cargo.toml \
//!     --target-dir D:\Projects\bili-rs\target --test open_stages_diag -- --nocapture
//! ```

use erika::ffmpeg::Demuxer;
use erika::source::{ByteRange, LocalFileSource, MediaSource, Result as SourceResult};
use std::time::{Duration, Instant};

/// 限速包装：把本地文件源包成"字节按 `bytes_per_sec` 到达"的源。
///
/// 用途：在无网络的环境里复现「open 必须读完 N 字节才能判定流信息 ⇒ 耗时 =
/// N ÷ 链路速率」这一条因果，而不是靠推断——本地读同样是那 N 字节，只是没有
/// 等待。把它限到真机实测的链路速率，得到的 open 耗时应当回到真机量级。
struct ThrottledSource {
    inner: LocalFileSource,
    uri: String,
    bytes_per_sec: f64,
    served: u64,
    started: Instant,
}

impl ThrottledSource {
    fn new(path: &std::path::Path, bytes_per_sec: f64) -> Self {
        Self {
            inner: LocalFileSource::open(path).expect("open sample"),
            uri: format!("throttled://{}", path.display()),
            bytes_per_sec,
            served: 0,
            started: Instant::now(),
        }
    }
}

impl MediaSource for ThrottledSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> SourceResult<Option<u64>> {
        self.inner.len()
    }

    fn read_range(&mut self, range: ByteRange) -> SourceResult<Vec<u8>> {
        let bytes = self.inner.read_range(range)?;
        // 计时基准取**本源的第一次读**，而不是构造时刻：DASH 路径下两条输入
        // 是顺序 open 的，靠后的那条在构造后要等前一條读完才开始被读，用构造
        // 时刻当基准会让它的等待被"等待别人"抵消掉，等于没限速。
        if self.served == 0 {
            self.started = Instant::now();
        }
        self.served += bytes.len() as u64;
        let due = Duration::from_secs_f64(self.served as f64 / self.bytes_per_sec);
        let elapsed = self.started.elapsed();
        if due > elapsed {
            std::thread::sleep(due - elapsed);
        }
        Ok(bytes)
    }
}

/// 闭环验证：同一份数据、同样的解析工作量，只把"字节到达速率"降到真机实测
/// 的链路速率，open 墙钟耗时应当涨到真机量级。
///
/// 这一条把「那几秒是等字节」从推断变成实测：本地全速 open 只要几十毫秒，
/// 限到 320KB/s 后同样的代码、同样的数据要数秒——差异只可能来自等待字节。
///
/// 同时它是**并行 open 的回归测试**：DASH 两条输入并行后，墙钟由较慢的那条
/// 决定（≈ 单条 0.85MB ÷ 320KB/s）；若哪天被改回串行，墙钟会翻倍、这里的
/// 上界断言立刻失败。
#[test]
fn dash_open_wall_time_scales_with_link_rate() {
    let (Some(video), Some(audio)) = (
        sample("erika-open-stage-video.bin"),
        sample("erika-open-stage-audio.bin"),
    ) else {
        eprintln!("skip: 样本缺失（%TEMP%\\erika-open-stage-{{video,audio}}.bin）");
        return;
    };
    // 真机 2026-09-15 15:00 那次 open 的实测链路速率（`[net] V: speed=316~360KB/s`）。
    const RATE_BPS: f64 = 320.0 * 1024.0;

    let v = ThrottledSource::new(&video, RATE_BPS);
    let a = ThrottledSource::new(&audio, RATE_BPS);
    let started = Instant::now();
    let demuxer = Demuxer::open_video_audio(Box::new(v), Box::new(a)).expect("open_video_audio");
    let wall_s = started.elapsed().as_secs_f64();

    let total = demuxer
        .open_stages()
        .iter()
        .find(|stage| stage.stage == "dash_open_total")
        .expect("dash_open_total");
    let video_bytes = total.video_bytes;
    let audio_bytes = total.audio_bytes;
    // 并行下墙钟 ≈ 较慢那条输入的读取时长；串行则 ≈ 两条之和。
    let parallel_expected_s = video_bytes.max(audio_bytes) as f64 / RATE_BPS;
    let serial_expected_s = (video_bytes + audio_bytes) as f64 / RATE_BPS;
    eprintln!(
        "限速 {:.0}KB/s：open 墙钟={wall_s:.2}s 读入 video={video_bytes}B audio={audio_bytes}B | 并行期望≈{parallel_expected_s:.2}s 串行期望≈{serial_expected_s:.2}s",
        RATE_BPS / 1024.0
    );

    // 容差 35%：读取是 64KiB 块状的，且两条输入起点略有先后。
    let deviation = (wall_s - parallel_expected_s).abs() / parallel_expected_s;
    assert!(
        deviation < 0.35,
        "open 墙钟与「较慢输入读取时长」不符：{wall_s:.2}s vs 期望 {parallel_expected_s:.2}s（偏差 {:.0}%）",
        deviation * 100.0
    );
    assert!(
        wall_s < serial_expected_s * 0.7,
        "两条输入的 open 又变成串行了：墙钟 {wall_s:.2}s 接近串行量级 {serial_expected_s:.2}s"
    );
}

/// 量化"两条输入的 open 串行 vs 并行"的墙钟差。
///
/// 生产路径 `Demuxer::open_video_audio` 里 video / audio 的
/// `open+find_stream_info` 是**顺序**执行的（各自持独立 FormatContext 与独立
/// HTTP 连接，互不依赖）。在单连接受限的链路上（真机实测单流 ~320KB/s，而两条
/// 流同时下载时合计可达 ~1.1MB/s），串行等于把靠后那条流的读取时间整段白等。
///
/// 本测试用同一对限速源分别跑「串行两次单流 open」与「并行两次单流 open」，
/// 给出可复算的差值——这是"能并行就不要串行"这一修复方向的实测依据，不是推断。
#[test]
fn parallel_input_open_beats_serial_on_a_rate_limited_link() {
    let (Some(video), Some(audio)) = (
        sample("erika-open-stage-video.bin"),
        sample("erika-open-stage-audio.bin"),
    ) else {
        eprintln!("skip: 样本缺失（%TEMP%\\erika-open-stage-{{video,audio}}.bin）");
        return;
    };
    const RATE_BPS: f64 = 320.0 * 1024.0;

    let serial_started = Instant::now();
    let _ = Demuxer::open_source(Box::new(ThrottledSource::new(&video, RATE_BPS)))
        .expect("serial video");
    let _ = Demuxer::open_source(Box::new(ThrottledSource::new(&audio, RATE_BPS)))
        .expect("serial audio");
    let serial_s = serial_started.elapsed().as_secs_f64();

    let parallel_started = Instant::now();
    let video_thread = std::thread::spawn({
        let path = video.clone();
        move || Demuxer::open_source(Box::new(ThrottledSource::new(&path, RATE_BPS)))
    });
    let audio_thread = std::thread::spawn({
        let path = audio.clone();
        move || Demuxer::open_source(Box::new(ThrottledSource::new(&path, RATE_BPS)))
    });
    let _ = video_thread
        .join()
        .expect("join video")
        .expect("parallel video");
    let _ = audio_thread
        .join()
        .expect("join audio")
        .expect("parallel audio");
    let parallel_s = parallel_started.elapsed().as_secs_f64();

    eprintln!("串行 open 两个输入={serial_s:.2}s | 并行={parallel_s:.2}s");
    assert!(
        parallel_s < serial_s,
        "并行未带来收益：串行 {serial_s:.2}s vs 并行 {parallel_s:.2}s"
    );
}

fn sample(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::temp_dir().join(name);
    path.exists().then_some(path)
}

#[test]
fn dash_open_stage_timings() {
    let (Some(video), Some(audio)) = (
        sample("erika-open-stage-video.bin"),
        sample("erika-open-stage-audio.bin"),
    ) else {
        eprintln!("skip: 样本缺失（%TEMP%\\erika-open-stage-{{video,audio}}.bin）");
        return;
    };

    let video_size = std::fs::metadata(&video).map(|m| m.len()).unwrap_or(0);
    let audio_size = std::fs::metadata(&audio).map(|m| m.len()).unwrap_or(0);
    eprintln!("样本 video={video_size}B audio={audio_size}B");

    let v = LocalFileSource::open(&video).expect("open video sample");
    let a = LocalFileSource::open(&audio).expect("open audio sample");

    let started = Instant::now();
    let demuxer = Demuxer::open_video_audio(Box::new(v), Box::new(a)).expect("open_video_audio");
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    eprintln!("open_video_audio 返回：wall={wall_ms:.1}ms（本地文件，无网络）");

    let mut prev = (0u64, 0u64);
    for stage in demuxer.open_stages() {
        let dv = stage.video_bytes.saturating_sub(prev.0);
        let da = stage.audio_bytes.saturating_sub(prev.1);
        eprintln!(
            "  stage={:<22} elapsed={:>9.1}ms  videoRead={:>9}B(+{:>9})  audioRead={:>9}B(+{:>9})  streams={:?}",
            stage.stage,
            stage.elapsed_ms,
            stage.video_bytes,
            dv,
            stage.audio_bytes,
            da,
            stage.streams,
        );
        prev = (stage.video_bytes, stage.audio_bytes);
    }

    let probe = demuxer.probe();
    eprintln!(
        "probe: duration={:?} tracks={} video={} audio={}",
        probe.duration,
        probe.tracks.len(),
        probe.video.len(),
        probe.audio.len(),
    );

    let stages = demuxer.open_stages();
    assert!(
        !stages.is_empty(),
        "DASH 路径应产出 open 阶段读数（打点回归）"
    );
    for expected in [
        "avformat_open_video",
        "find_stream_info_video",
        "avformat_open_audio",
        "find_stream_info_audio",
        "dash_open_total",
    ] {
        assert!(
            stages.iter().any(|stage| stage.stage == expected),
            "缺少阶段 {expected}（阶段集合={:?}）",
            stages.iter().map(|s| s.stage).collect::<Vec<_>>()
        );
    }
    let total = stages
        .iter()
        .find(|stage| stage.stage == "dash_open_total")
        .expect("dash_open_total");
    // 字节计数必须真的接在 AVIO 读路径上：为 0 说明打点与真实读取脱节
    // （这两条断言在把 `CustomAvio::read_packet` 的累加去掉时会失败）。
    assert!(
        total.video_bytes > 0,
        "诊断计数未接入：video 读取字节为 {}",
        total.video_bytes
    );
    assert!(
        total.audio_bytes > 0,
        "诊断计数未接入：audio 读取字节为 {}",
        total.audio_bytes
    );
    // 每个输入都必须读到 moov+moof 才停：远小于 64KiB 说明读数没累积，
    // 大到接近整个样本（8MB）说明"读到文件尾"而不是"读够流信息"。
    for (name, bytes) in [("video", total.video_bytes), ("audio", total.audio_bytes)] {
        assert!(
            (64 * 1024..video_size).contains(&bytes),
            "{name} 读取字节 {bytes} 不在合理区间（样本 {video_size}B）"
        );
    }
}
