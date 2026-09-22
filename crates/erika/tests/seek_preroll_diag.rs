//! 诊断测试（非产品路径）：拆开「续播 seek 的预滚」为什么慢。
//!
//! 要回答的问题（真机实测 `video_seek_preroll elapsedMs=5103` +
//! `totalDroppedFrames=120`）：
//!
//! 1. 那 120 帧是哪来的？——`AVSEEK_FLAG_BACKWARD` 只能落在目标**之前**的
//!    关键帧上，解码器必须从该关键帧顺序解到目标，途中的帧只能丢弃。
//!    所以「丢多少帧」= 「seek 落点关键帧到目标的距离」，由 GOP 长度决定。
//! 2. 那 5.1 秒花在哪？——是解码这 120 帧的 CPU 时间，还是等这 120 帧对应的
//!    字节从链路到达的等待时间？
//!
//! 本测试用**同一份真机样本**跑两组：本地全速（等待≈0）与限速 320KB/s
//! （真机 2026-09-15 实测单流速率，见 `open_stages_diag.rs`）。若后者比前者
//! 多出的时间恰好等于「seek 后新增读取字节 ÷ 链路速率」，则预滚耗时与 open
//! 的耗时**同源**（都是"字节数 ÷ 单流速率"），而不是解码慢。
//!
//! 样本：`%TEMP%\erika-open-stage-video.bin`（真机预载桶去掉 32 字节头）。
//! 样本缺失时跳过（不失败）。
//!
//! 运行：
//! ```text
//! cargo test --manifest-path D:\Projects\Erika\crates\erika\Cargo.toml \
//!     --target-dir D:\Projects\bili-rs\target --test seek_preroll_diag -- --nocapture
//! ```

use erika::core::{MediaRequest, MediaSourceHint};
use erika::ffmpeg::Demuxer;
use erika::playback::{PlaybackSessionConfig, TimedVideoFrame, VideoPlaybackEngine};
use erika::source::{ByteRange, LocalFileSource, MediaSource, Result as SourceResult};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 真机实测的单流链路速率：宿主日志 `[net] V: speed=316~360KB/s`。
const RATE_BPS: f64 = 320.0 * 1024.0;

/// 限速包装：把本地文件源包成"字节按 `bytes_per_sec` 到达"的源，并对外暴露
/// 累计被消费的字节数（= 真机 HTTP 路径下的下载量）。
struct ThrottledSource {
    inner: LocalFileSource,
    uri: String,
    bytes_per_sec: Option<f64>,
    started: Instant,
    /// 与测试共享：本输入自构造起被读走的字节总数。
    consumed: Arc<AtomicU64>,
}

impl ThrottledSource {
    fn new(path: &std::path::Path, bytes_per_sec: Option<f64>, consumed: Arc<AtomicU64>) -> Self {
        Self {
            inner: LocalFileSource::open(path).expect("open sample"),
            uri: format!("throttled://{}", path.display()),
            bytes_per_sec,
            started: Instant::now(),
            consumed,
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
        // 计时基准取本源的第一次读：seek 后的读取要与"从此刻起应到达多少字节"
        // 比较，而不是从构造时刻起算。
        if self.consumed.load(Ordering::Relaxed) == 0 {
            self.started = Instant::now();
        }
        let total = self
            .consumed
            .fetch_add(bytes.len() as u64, Ordering::Relaxed)
            .saturating_add(bytes.len() as u64);
        if let Some(bps) = self.bytes_per_sec {
            let due = Duration::from_secs_f64(total as f64 / bps);
            let elapsed = self.started.elapsed();
            if due > elapsed {
                std::thread::sleep(due - elapsed);
            }
        }
        Ok(bytes)
    }
}

#[derive(Debug)]
struct SeekOutcome {
    /// `Demuxer::seek()` 调用本身返回所需时间（真机上含 `av_seek_frame` 建索引）。
    seek_call_ms: f64,
    /// seek 返回到「首个 pts >= target 的包」之间的墙钟时间——真机打点的
    /// `video_seek_preroll elapsedMs` 口径与此一致（同为 seek 之后起算）。
    preroll_ms: f64,
    /// 预滚期间从源新读走的字节数（含 AVIO 64KiB 预读窗口，同真机口径）。
    preroll_bytes: u64,
    /// 落点：seek 后第一个视频包的 pts（应为 target 之前的关键帧）。
    first_pts_s: f64,
    /// 落点帧是不是关键帧。
    first_is_key: bool,
    /// 从落点到「首个 pts >= target 的包」之间的包数 = 解码器必须解出并丢弃的帧数。
    backfill_packets: usize,
    /// 首个 pts >= target 的包与其 pts。
    first_target_pts_s: f64,
}

fn run_seek(sample: &std::path::Path, target_s: f64, limit_bps: Option<f64>) -> SeekOutcome {
    let consumed = Arc::new(AtomicU64::new(0));
    let source = Box::new(ThrottledSource::new(
        sample,
        limit_bps,
        Arc::clone(&consumed),
    )) as Box<dyn MediaSource>;
    let mut demuxer = Demuxer::open_source(source).expect("open_source");

    let seek_started = Instant::now();
    demuxer
        .seek(Duration::from_secs_f64(target_s))
        .expect("seek");
    let seek_call_ms = seek_started.elapsed().as_secs_f64() * 1000.0;
    let bytes_at_seek = consumed.load(Ordering::Relaxed);

    let preroll_started = Instant::now();
    let mut first_pts_s = f64::NAN;
    let mut first_is_key = false;
    let mut first_target_pts_s = f64::NAN;
    let mut backfill_packets = 0usize;
    let mut have_first = false;

    while let Some(packet) = demuxer.read_packet().expect("read_packet") {
        let Some(pts) = packet.pts().map(|pts| pts.seconds()) else {
            continue;
        };
        if !have_first {
            first_pts_s = pts;
            first_is_key = packet.flags().is_key();
            have_first = true;
        }
        if pts + 1e-6 >= target_s {
            first_target_pts_s = pts;
            break;
        }
        backfill_packets += 1;
        assert!(
            preroll_started.elapsed() < Duration::from_secs(120),
            "预滚超过 120s 仍未到达目标 {target_s}s：疑似死循环或 EOF"
        );
    }

    let preroll_ms = preroll_started.elapsed().as_secs_f64() * 1000.0;
    let preroll_bytes = consumed
        .load(Ordering::Relaxed)
        .saturating_sub(bytes_at_seek);

    SeekOutcome {
        seek_call_ms,
        preroll_ms,
        preroll_bytes,
        first_pts_s,
        first_is_key,
        backfill_packets,
        first_target_pts_s,
    }
}

fn sample() -> Option<std::path::PathBuf> {
    let path = std::env::temp_dir().join("erika-open-stage-video.bin");
    path.exists().then_some(path)
}

/// 按真机 worker 的节奏（Playing 时 2ms poll）驱动 `tick`，直到出现满足
/// `accept` 的帧。返回该帧与它出现前经历了多少次 tick。
fn tick_until(
    engine: &mut VideoPlaybackEngine,
    budget: Duration,
    mut accept: impl FnMut(&TimedVideoFrame) -> bool,
) -> Option<(TimedVideoFrame, u64)> {
    let started = Instant::now();
    let mut ticks = 0u64;
    loop {
        if started.elapsed() >= budget {
            return None;
        }
        ticks += 1;
        match engine.tick() {
            Ok(Some(frame)) => {
                if accept(&frame) {
                    return Some((frame, ticks));
                }
            }
            Ok(None) => {}
            Err(error) => panic!("tick 失败：{error}"),
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// ③ 端到端：走**完整引擎路径**（demux 线程 → 解码器 → `tick` 的预滚丢帧
/// 预算），测「seek 提交 → 首个 pts >= target 的帧出帧」。
///
/// 为什么必须单独测这一段：测试 ①② 走的是 `Demuxer`，只覆盖"把回填字节读回
/// 来"；而真机 `video_seek_preroll` 的口径还包含**解码器**和 `tick` 里那条
/// "每次最多丢 `max_consecutive_drops` 帧 / `SEEK_PREROLL_DECODE_TIME_BUDGET`"
/// 的预算。如果预算是瓶颈，本地全速下这段耗时会远大于解码量级。
///
/// 样本全在本地（等待≈0）→ 这里测到的就是"解码 + 预算 + 调度"的固有开销；
/// 真机那 5.1 秒减去它，才是"等字节"。
#[test]
fn engine_seek_preroll_overhead_is_small_when_bytes_are_local() {
    let Some(video) = sample() else {
        eprintln!("skip: 样本缺失（%TEMP%\\erika-open-stage-video.bin）");
        return;
    };
    let request = MediaRequest {
        uri: video.to_string_lossy().into_owned(),
        source_hint: MediaSourceHint::LocalFile,
        ..MediaRequest::new("")
    };
    let mut engine = VideoPlaybackEngine::open(&request, PlaybackSessionConfig::default())
        .expect("VideoPlaybackEngine::open");

    engine.play().expect("play");
    let (first, first_ticks) = tick_until(&mut engine, Duration::from_secs(20), |frame| {
        frame.pts.is_some()
    })
    .expect("首帧");
    eprintln!(
        "首帧 pts={:?}（{} 次 tick）",
        first.pts.map(|pts| pts.as_secs_f64()),
        first_ticks
    );

    // 真机形态：抢跑一小段后立刻纠偏 seek。这里不等 0.45s 真播完——抢跑期间
    // 的丢弃量由 `playback_queue_cleanup` 单独打点（真机实测只有 4 帧）。
    let target = Duration::from_secs_f64(24.0);
    let seek_started = Instant::now();
    engine.seek(target).expect("seek");
    let (frame, ticks) = tick_until(&mut engine, Duration::from_secs(30), |frame| {
        frame
            .pts
            .is_some_and(|pts| pts + Duration::from_micros(1) >= target)
    })
    .expect("首个目标帧");
    let elapsed = seek_started.elapsed();

    let frame_pts = frame.pts.unwrap_or_default();
    eprintln!(
        "端到端 seek(24s)：首个目标帧 pts={:.3}s 耗时={:.0}ms（{} 次 tick）",
        frame_pts.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0,
        ticks,
    );

    assert!(
        frame_pts + Duration::from_micros(1) >= target,
        "出帧 pts {:.3}s 低于目标 {:.3}s",
        frame_pts.as_secs_f64(),
        target.as_secs_f64()
    );
    // 本地无等待，这里只剩解码 113 帧 + 预算分片 + 2ms 轮询。真机 5103ms 与之
    // 相差三个数量级，所以"预滚慢"不可能来自解码或丢帧预算。
    assert!(
        elapsed < Duration::from_millis(1500),
        "本地全速下端到端预滚仍耗时 {:.0}ms —— 瓶颈不在链路，在解码或丢帧预算",
        elapsed.as_secs_f64() * 1000.0
    );
}

/// ① 落点与帧数：seek 到 24s 落在 20s 的关键帧上，必须回填 4 秒 —— 30fps 下
/// 正好是 120 帧，与真机 `totalDroppedFrames=120` 同形。
///
/// 这条把「120 帧」从"观测到的数字"变成"GOP 决定的必然量"：样本关键帧在
/// 0/5/10/15/20/25s（`ffprobe` 实测），24s 的 backward seek 只能落在 20s。
#[test]
fn seek_preroll_backfills_from_the_previous_keyframe() {
    let Some(video) = sample() else {
        eprintln!("skip: 样本缺失（%TEMP%\\erika-open-stage-video.bin）");
        return;
    };
    let outcome = run_seek(&video, 24.0, None);
    eprintln!(
        "seek(24.0s) 落点={:.3}s key={} 回填包数={} 目标包pts={:.3}s 预滚={:.1}ms（其中 av_seek_frame={:.1}ms）读入={}B",
        outcome.first_pts_s,
        outcome.first_is_key,
        outcome.backfill_packets,
        outcome.first_target_pts_s,
        outcome.preroll_ms,
        outcome.seek_call_ms,
        outcome.preroll_bytes,
    );

    assert!(
        outcome.seek_call_ms < 500.0,
        "本地样本上 av_seek_frame 耗时 {:.0}ms —— 异常高，seek 本身不该在本地文件上阻塞",
        outcome.seek_call_ms
    );
    assert!(
        outcome.preroll_bytes >= 900_000,
        "seek 后回填 4 秒（20s→24s）应读入约 0.94MB，实际 {}B —— 落点或读取口径变了",
        outcome.preroll_bytes
    );
    assert!(
        outcome.first_is_key,
        "seek 后的第一个包必须是关键帧（落点 {:.3}s）",
        outcome.first_pts_s
    );
    assert!(
        (outcome.first_pts_s - 20.0).abs() < 0.5,
        "落点应为 20.0s 的关键帧（GOP=5s），实际 {:.3}s",
        outcome.first_pts_s
    );
    // 20.0s → 24.0s @30fps = 120 帧。包数与帧数在 h264 时一一对应。
    assert!(
        outcome.backfill_packets >= 100 && outcome.backfill_packets <= 125,
        "回填帧数应在 120 附近（30fps × 4s），实际 {}",
        outcome.backfill_packets
    );
}

/// ② 耗时归因（本测试的核心）：同一份数据、同样的 seek，只把"字节到达速率"
/// 换成本地全速，预滚墙钟应当从数秒塌缩到解码量级；两者的差值应当等于
/// 「预滚新增字节 ÷ 链路速率」。
///
/// 若这条通过，则真机那 5.1 秒**不是解码慢**，而是"读满 seek 后的目标窗口"
/// 所需的链路时间——与 open 那 0.9MB ÷ 320KB/s 同源。
#[test]
fn seek_preroll_wall_time_is_bytes_over_link_rate() {
    let Some(video) = sample() else {
        eprintln!("skip: 样本缺失（%TEMP%\\erika-open-stage-video.bin）");
        return;
    };
    let local = run_seek(&video, 24.0, None);
    let limited = run_seek(&video, 24.0, Some(RATE_BPS));

    let expected_extra_s = limited.preroll_bytes as f64 / RATE_BPS;
    let actual_extra_s = (limited.preroll_ms - local.preroll_ms) / 1000.0;
    eprintln!(
        "全速：预滚={:.1}ms 读入={}B | 限速320KB/s：预滚={:.0}ms 读入={}B | 期望增量={:.2}s 实测增量={:.2}s",
        local.preroll_ms,
        local.preroll_bytes,
        limited.preroll_ms,
        limited.preroll_bytes,
        expected_extra_s,
        actual_extra_s,
    );

    assert_eq!(
        local.backfill_packets, limited.backfill_packets,
        "限速不应改变回填帧数（同一条 seek 路径）"
    );
    assert!(
        local.preroll_ms < 1500.0,
        "本地全速下预滚仍耗时 {:.0}ms —— 说明瓶颈不在链路而在解码/调度",
        local.preroll_ms
    );
    // 容差 35%：AVIO 按 64KiB 块预读，且限速是"累计字节 ÷ 速率"的线性模型。
    let deviation = (actual_extra_s - expected_extra_s).abs() / expected_extra_s;
    assert!(
        deviation < 0.35,
        "限速增量与「预滚字节 ÷ 速率」不符：实测增量 {actual_extra_s:.2}s vs 期望 {expected_extra_s:.2}s（偏差 {:.0}%）",
        deviation * 100.0
    );
}
