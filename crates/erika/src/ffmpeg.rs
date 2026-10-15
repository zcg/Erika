use std::collections::BTreeSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::marker::PhantomData;
use std::mem;
use std::path::Path;
use std::ptr;
use std::slice;
use std::sync::Arc;
#[cfg(target_os = "android")]
use std::sync::Mutex;
use std::time::Duration;

use crate::core::{ColorPrimaries, FrameRate, TrackInfo, TrackKind, TransferFunction, VideoParams};
use crate::renderer::pipeline::{
    Chromaticity, ColorRange, ContentLightMetadata, DoviComponentCurve, DoviFramePq,
    DoviSourceMetadata, HdrMetadata, MasteringDisplayMetadata, MatrixCoefficients, RgbMatrix,
};
use crate::renderer::pipeline::{DOVI_MAX_MMR_ORDER, DOVI_MAX_PIECES};
use crate::source::{ByteRange, MediaSource};
use crate::subtitle::{
    AssTrackResources, DecodedSubtitleFrame, SubtitleBitmapPlane, SubtitleFontAttachment,
    SubtitleTextFormat, SubtitleTextSegment, SubtitleTrackConfig, SubtitleTrackSource,
};
use erika_ffmpeg_sys as sys;
use libc::{EAGAIN, EINVAL, EIO, ESPIPE, SEEK_CUR, SEEK_END, SEEK_SET};
use thiserror::Error;

#[cfg(target_os = "android")]
use crate::android::mediacodec::{
    AndroidHardwareBufferImage, AndroidMediaCodecError, AndroidMediaCodecFrameSource,
};
#[cfg(target_env = "ohos")]
use crate::ohos::avcodec::{
    DecodedNv12FrameView, DecodedSurfaceFrame, OhosDecoderOutput, OhosNativeBufferImage,
    OhosVideoCodec, OhosVideoDecoder,
};

const AVERROR_EOF: i32 = -541_478_725;
const MAX_SUBTITLE_FONT_ATTACHMENTS: usize = 256;
const MAX_SUBTITLE_FONT_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_SUBTITLE_FONT_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_ASS_CODEC_PRIVATE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum FfmpegError {
    #[error("path contains interior nul byte")]
    InteriorNul,
    #[error("ffmpeg error in {operation}: {message} ({code})")]
    Api {
        operation: &'static str,
        code: i32,
        message: String,
    },
    #[error("ffmpeg returned a null pointer from {0}")]
    NullPointer(&'static str),
    #[error("unknown stream index: {0}")]
    UnknownStream(i32),
    #[error("packet stream {packet_stream} does not match decoder stream {decoder_stream}")]
    StreamMismatch {
        decoder_stream: i32,
        packet_stream: i32,
    },
    #[error("expected audio frame")]
    ExpectedAudioFrame,
    #[error("expected subtitle stream")]
    ExpectedSubtitleStream,
    #[error("unsupported D3D11VA software pixel format: {0}")]
    UnsupportedD3d11vaSwFormat(i32),
    #[cfg(target_os = "android")]
    #[error("Android MediaCodec surface error: {0}")]
    AndroidMediaCodec(String),
    #[cfg(target_os = "android")]
    #[error("Android MediaCodec surface backpressure: {0}")]
    AndroidMediaCodecBackpressure(String),
    #[cfg(target_env = "ohos")]
    #[error("HarmonyOS AVCodec error: {0}")]
    OhosAvCodec(String),
    #[error(
        "invalid subtitle bitmap: width={width} height={height} stride={stride} colors={colors}"
    )]
    InvalidSubtitleBitmap {
        width: i32,
        height: i32,
        stride: i32,
        colors: i32,
    },
    #[error("source error: {0}")]
    Source(String),
}

impl FfmpegError {
    pub fn is_again(&self) -> bool {
        matches!(self, Self::Api { code, .. } if *code == av_error(EAGAIN))
    }

    pub fn is_invalid_argument(&self) -> bool {
        matches!(self, Self::Api { code, .. } if *code == av_error(EINVAL))
    }

    /// `avcodec_send_packet` 返回的 `AVERROR_INVALIDDATA`（`FFERRTAG('I','N','D','A')`
    /// = -1094995529）。表示当前**数据包损坏/不完整**（慢网下下载截断、seek 后
    /// 非关键帧数据等），**不是致命错误**——标准做法是丢弃该包、等待下一个关键帧
    /// 重同步后继续，而不是终止整个播放会话。
    ///
    /// 注意：不等于 `EINVAL`（-22）。音频 seek 容错原本只匹配 `is_invalid_argument`
    /// （EINVAL），导致真实坏包（INVALIDDATA）永远走 fatal → 慢网/seek 下反复
    /// `playback_fatal` 崩溃循环（日志实测 156 次连续 video_tick Invalid data）。
    pub fn is_invalid_data(&self) -> bool {
        matches!(
            self,
            Self::Api { code, .. } if *code == av_error(-1_095_495_529)
        )
    }

    #[cfg(target_os = "android")]
    pub(crate) fn is_android_mediacodec_backpressure(&self) -> bool {
        matches!(self, Self::AndroidMediaCodecBackpressure(_))
    }
}

pub type Result<T> = std::result::Result<T, FfmpegError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeBase {
    pub num: i32,
    pub den: i32,
}

impl TimeBase {
    pub fn seconds_from_timestamp(self, timestamp: i64) -> f64 {
        timestamp as f64 * self.num as f64 / self.den as f64
    }

    fn to_av_rational(self) -> sys::AVRational {
        sys::AVRational {
            num: self.num,
            den: self.den,
        }
    }

    fn from_av(rational: sys::AVRational) -> Self {
        if rational.den == 0 {
            return Self { num: 0, den: 1 };
        }
        Self {
            num: rational.num,
            den: rational.den,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketTimestamp {
    pub raw: i64,
    pub time_base: TimeBase,
}

impl PacketTimestamp {
    pub fn seconds(self) -> f64 {
        self.time_base.seconds_from_timestamp(self.raw)
    }

    pub fn as_duration(self) -> Option<Duration> {
        let seconds = self.seconds();
        if seconds.is_sign_negative() || !seconds.is_finite() {
            return None;
        }
        Some(Duration::from_secs_f64(seconds))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketFlags {
    bits: i32,
}

impl PacketFlags {
    pub fn bits(self) -> i32 {
        self.bits
    }

    pub fn is_key(self) -> bool {
        self.bits & sys::AV_PKT_FLAG_KEY as i32 != 0
    }

    pub fn is_corrupt(self) -> bool {
        self.bits & sys::AV_PKT_FLAG_CORRUPT as i32 != 0
    }

    pub fn is_discard(self) -> bool {
        self.bits & sys::AV_PKT_FLAG_DISCARD as i32 != 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaProbe {
    pub uri: String,
    pub duration: Option<Duration>,
    pub tracks: Vec<TrackInfo>,
    pub video: Vec<VideoProbe>,
    pub audio: Vec<AudioProbe>,
    pub subtitles: Vec<SubtitleTrackConfig>,
    pub subtitle_fonts: Arc<[SubtitleFontAttachment]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoProbe {
    pub track_id: i64,
    pub params: VideoParams,
    pub codec: Option<String>,
    pub pixel_format: Option<String>,
    pub profile: Option<String>,
    pub level: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioProbe {
    pub track_id: i64,
    pub codec: Option<String>,
    pub sample_rate: u32,
    pub channels: u32,
    pub sample_format: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmSampleFormat {
    F32Interleaved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u32,
    pub sample_format: PcmSampleFormat,
}

impl PcmFormat {
    pub fn f32_interleaved(sample_rate: u32, channels: u32) -> Self {
        Self {
            sample_rate,
            channels,
            sample_format: PcmSampleFormat::F32Interleaved,
        }
    }
}

impl Default for PcmFormat {
    fn default() -> Self {
        Self::f32_interleaved(48_000, 2)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PcmAudioFrame {
    pub format: PcmFormat,
    pub pts: Option<Duration>,
    pub frames: usize,
    pub samples: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamSelection {
    All,
    Only(BTreeSet<i32>),
}

impl StreamSelection {
    pub fn all() -> Self {
        Self::All
    }

    pub fn only<I>(stream_indices: I) -> Self
    where
        I: IntoIterator<Item = i32>,
    {
        Self::Only(stream_indices.into_iter().collect())
    }

    fn accepts(&self, stream_index: i32) -> bool {
        match self {
            Self::All => true,
            Self::Only(streams) => streams.contains(&stream_index),
        }
    }
}

/// 单个解复用输入：一个独立容器上下文 + 其时间轴与全局流偏移。
///
/// DASH 双流（video m4s + audio m4s）由两个输入组成；每个输入内部
/// stream index 均从 0 起，通过 `stream_offset` 平移到统一全局 index。
struct DemuxInput {
    context: FormatContext,
    stream_time_bases: Vec<Option<TimeBase>>,
    timeline_origin_micros: i64,
    stream_offset: i32,
}

impl DemuxInput {
    /// 从本输入读一包：时间基取自本输入的流表，时间轴用本输入原点归一化。
    fn read_packet(&mut self) -> Result<Option<Packet>> {
        let mut packet = Packet::alloc()?;
        let code = unsafe { sys::av_read_frame(self.context.as_mut_ptr(), packet.as_mut_ptr()) };
        if code == AVERROR_EOF {
            return Ok(None);
        }
        if let Err(error) = check(code, "av_read_frame") {
            // A read that failed because the media source could not deliver the
            // bytes keeps its real reason on the AVIO (HTTP status, timeout, ...).
            // Carry it into the terminal error: without it the error reads only
            // "Input/output error (-5)" and the cause lives on stderr alone, which
            // is why a report such as issue #1 needs a reproduction instead of a
            // log line.
            return Err(
                match self
                    .context
                    .avio
                    .as_ref()
                    .and_then(|avio| avio.last_error.as_deref())
                {
                    Some(source_error) => {
                        FfmpegError::Source(format!("{error}; custom AVIO: {source_error}"))
                    }
                    None => error,
                },
            );
        }
        let stream_index = packet.stream_index();
        packet.time_base = self
            .stream_time_bases
            .get(stream_index as usize)
            .copied()
            .flatten();
        packet.normalize_timeline(self.timeline_origin_micros);
        Ok(Some(packet))
    }

    /// 全局 stream index → 本输入内局部 index（不属于本输入则 None）。
    ///
    /// 双流合并后必须同时校验上界：video 输入只拥有 [offset, offset+nb_streams)，
    /// 否则 `find_map` 会让 video 输入吞掉所有 index ≥ offset 的查询（audio 全局
    /// index 1 被当成 video 的局部 index 1 → 越界报 unknown stream index）。
    fn local_index(&self, global: i32) -> Option<i32> {
        if global < self.stream_offset {
            return None;
        }
        let local = global - self.stream_offset;
        let stream_count = unsafe { (*self.context.as_ptr()).nb_streams as i32 };
        (local < stream_count).then_some(local)
    }
}

/// 一次 `open` 内某个阶段的耗时与字节读数（诊断用）。
///
/// 存在的理由：「点播放 → 真正开始播放」这段里，宿主只能看到内核停在
/// `Opening` 且 `duration` 仍为 0，偏偏网络在正常下载——是「网络慢」还是
/// 「内核在解析」在宿主侧无法区分。`avformat_open_input` 与
/// `avformat_find_stream_info` 都会按 `probesize` / `analyzeduration`
/// 读入可观的流数据才能判定流信息，本结构把这部分切分出来，让"这几秒花在
/// 哪"成为可读数字，而不是靠推断。
#[derive(Debug, Clone, PartialEq)]
pub struct OpenStageTiming {
    /// 阶段名（稳定标识，日志直接展示）。
    pub stage: &'static str,
    /// 该阶段耗时（毫秒）。
    pub elapsed_ms: f64,
    /// 阶段结束时 video 输入累计从源消费的字节（HTTP 路径下即真实下载量）。
    pub video_bytes: u64,
    /// 阶段结束时 audio 输入累计从源消费的字节。
    pub audio_bytes: u64,
    /// 该阶段观测到的流数（仅 `find_stream_info_*` 阶段有值）。
    pub streams: Option<usize>,
}

/// 把一条 open 阶段读数发到两个通道：无条件 stderr（宿主重定向到
/// `%TEMP%\bili-rs-stderr.log`，用户默认可见），以及内核 trace 文件
/// （`BILI_RS_KERNEL_TRACE=1` 时）。open 是低频事件，每行几个字节的 I/O
/// 对起播没有可测影响。
fn emit_open_stage(stage: &OpenStageTiming) {
    let line = serde_json::json!({
        "event": "erika_open_stage",
        "stage": stage.stage,
        "elapsedMs": (stage.elapsed_ms * 10.0).round() / 10.0,
        "videoReadBytes": stage.video_bytes,
        "audioReadBytes": stage.audio_bytes,
    })
    .to_string();
    crate::trace::diagnostic(line.clone());
    crate::trace::log(format!("[erika-open-trace] {line}"));
}

/// 把一条 seek 阶段读数发到与 [`emit_open_stage`] 相同的两个通道。
///
/// 存在的理由：真机「续播 seek 的预滚」实测 `elapsedMs=5103`，宿主侧过去只能
/// 看到那一行（耗时 + 丢帧数），分不清这几秒是 `av_seek_frame` 在 HTTP 源上
/// 建索引、是等目标窗口的字节到达、还是解码本身。`Demuxer::seek` 跑在 demux
/// 线程里，而 `AsyncDemuxer::seek` 只投递命令就返回（真正执行推迟到下一次
/// `Start`，见 `handle_demux_command`），所以这一段是整条预滚链上唯一没有读数
/// 的环节。读字节数用 AVIO 的累计计数差分，口径与 `OpenStageTiming` 一致
/// （HTTP 路径下即真实下载量）。
fn emit_seek_stage(
    stage: &'static str,
    target: Duration,
    elapsed: Duration,
    input: Option<usize>,
    bytes_before: &[Option<u64>],
    bytes_after: &[Option<u64>],
) {
    let delta_at = |index: usize| -> u64 {
        bytes_after
            .get(index)
            .copied()
            .flatten()
            .zip(bytes_before.get(index).copied().flatten())
            .map(|(after, before)| after.saturating_sub(before))
            .unwrap_or(0)
    };
    let (video_bytes, audio_bytes) = match input {
        Some(0) => (delta_at(0), 0),
        Some(1) => (0, delta_at(1)),
        Some(_) => (0, 0),
        None => (delta_at(0), delta_at(1)),
    };
    let mut fields = serde_json::Map::new();
    fields.insert("event".to_string(), serde_json::json!("erika_seek_stage"));
    fields.insert("stage".to_string(), serde_json::json!(stage));
    if let Some(index) = input {
        fields.insert("inputIndex".to_string(), serde_json::json!(index));
    }
    // 墙钟毫秒：本行由 demux 线程打印，`video_seek_preroll` 由播放线程打印，
    // stderr 是多线程共享的。带上墙钟才能把「demux 线程的 seek 用了多久」与
    // 「播放线程的预滚用了多久」在时间轴上对齐，也才能与宿主 diag 日志的
    // `[<epochMs>]` 前缀对上。
    fields.insert(
        "wallMs".to_string(),
        serde_json::json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis().min(u64::MAX as u128) as u64)
                .unwrap_or(0)
        ),
    );
    fields.insert(
        "targetSeconds".to_string(),
        serde_json::json!((target.as_secs_f64() * 1000.0).round() / 1000.0),
    );
    fields.insert(
        "elapsedMs".to_string(),
        serde_json::json!((elapsed.as_secs_f64() * 1000.0 * 10.0).round() / 10.0),
    );
    fields.insert("videoReadBytes".to_string(), serde_json::json!(video_bytes));
    fields.insert("audioReadBytes".to_string(), serde_json::json!(audio_bytes));
    let line = serde_json::Value::Object(fields).to_string();
    crate::trace::diagnostic(line.clone());
    crate::trace::log(format!("[erika-seek-trace] {line}"));
}

pub struct Demuxer {
    inputs: Vec<DemuxInput>,
    /// 每个输入缓存的下一包（PTS 交错读取）
    pending: Vec<Option<Packet>>,
    /// 每个输入是否已 EOF
    eof: Vec<bool>,
    probe: MediaProbe,
    stream_time_bases: Vec<Option<TimeBase>>,
    selection: StreamSelection,
    /// 本次 open 的分阶段读数（见 [`OpenStageTiming`]）；单输入路径为空。
    open_stages: Vec<OpenStageTiming>,
}

unsafe impl Send for Demuxer {}

impl Demuxer {
    pub fn open_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_uri(&path.as_ref().to_string_lossy())
    }

    pub fn open_uri(uri: &str) -> Result<Self> {
        let mut context = open_format_context(uri)?;
        find_stream_info(&mut context)?;
        Self::from_single_input(uri, context)
    }

    pub fn open_source(source: Box<dyn MediaSource>) -> Result<Self> {
        let uri = source.uri().to_string();
        let mut context = open_source_format_context(source)?;
        find_stream_info(&mut context)?;
        Self::from_single_input(&uri, context)
    }

    /// 单个输入（本地文件 / MP4 直链 / 直播等既有路径）。
    fn from_single_input(uri: &str, context: FormatContext) -> Result<Self> {
        let timeline_origin_micros = format_timeline_origin_micros(context.as_ptr());
        trace_timeline_origin(timeline_origin_micros);
        let (probe, stream_time_bases) = inspect_format_context(uri, context.as_ptr());
        Ok(Self {
            inputs: vec![DemuxInput {
                context,
                stream_time_bases: stream_time_bases.clone(),
                timeline_origin_micros,
                stream_offset: 0,
            }],
            pending: vec![None],
            eof: vec![false],
            probe,
            stream_time_bases,
            selection: StreamSelection::all(),
            open_stages: Vec::new(),
        })
    }

    /// DASH 双流：独立打开 video/audio 两个 m4s 输入并合并为统一时间轴。
    ///
    /// video 流占据全局 stream 0..video_streams，audio 流平移到其后的全局
    /// index。两个输入时间轴各自从 0 起、互相对齐（fMP4 起点一致），
    /// PTS 交错的 read_packet 按最小 PTS 输出；音频主时钟与视频渲染时间均
    /// 基于统一后的包时间戳，无需额外时钟改动。
    pub fn open_video_audio(
        video_source: Box<dyn MediaSource>,
        audio_source: Box<dyn MediaSource>,
    ) -> Result<Self> {
        let video_uri = video_source.uri().to_string();
        let audio_uri = audio_source.uri().to_string();
        let dash_open_started = std::time::Instant::now();
        let mut stages: Vec<OpenStageTiming> = Vec::new();
        crate::trace::log(format!(
            "[erika-open-trace] stage=dash_open_start uri={video_uri} | {audio_uri}"
        ));
        // —— 两条输入**并行** open ——
        //
        // video / audio 各持独立的 `FormatContext`、独立 AVIO 与独立 HTTP 连接，
        // 彼此没有共享状态，因此可以并行。串行的代价在真机上是实打实的：单条流
        // 在 B 站 CDN 上实测约 320KB/s（宿主日志 `[net] V: speed=316~360KB/s`），
        // 而两条流同时下载时合计可达 ~1.1MB/s——瓶颈在**单连接速率**而不是链路
        // 带宽，于是串行等于把靠后那条流的整段读取时间白等。
        //
        // 限速对照实验（`tests/open_stages_diag.rs`，同为 320KB/s）：
        //   串行 open 两个输入 = 5.47s，并行 = 2.73s。
        //
        // 失败语义与串行版一致：任一输入失败即整体失败。失败路径才 join 音频
        // 线程（让它把连接干净收掉，不在返回后继续下载）；成功路径两线程各自
        // 跑满，谁也不等谁。
        let audio_worker = std::thread::spawn(move || -> Result<OpenedInput> {
            let open_started = std::time::Instant::now();
            let mut context = open_source_format_context(audio_source)?;
            let open_ms = open_started.elapsed().as_secs_f64() * 1000.0;
            let bytes_after_open = context.avio_diag().map(|d| d.0).unwrap_or(0);
            let probe_started = std::time::Instant::now();
            find_stream_info(&mut context)?;
            let probe_ms = probe_started.elapsed().as_secs_f64() * 1000.0;
            let stream_count = unsafe { (*context.as_ptr()).nb_streams as usize };
            Ok(OpenedInput {
                context,
                open_ms,
                bytes_after_open,
                probe_ms,
                stream_count,
            })
        });

        let video_input_started = std::time::Instant::now();
        let mut video_context = match open_source_format_context(video_source) {
            Ok(context) => context,
            Err(error) => {
                let _ = audio_worker.join();
                return Err(error);
            }
        };
        let video_open_ms = video_input_started.elapsed().as_secs_f64() * 1000.0;
        let video_bytes_after_open = video_context.avio_diag().map(|d| d.0).unwrap_or(0);
        let video_probe_started = std::time::Instant::now();
        if let Err(error) = find_stream_info(&mut video_context) {
            let _ = audio_worker.join();
            return Err(error);
        }
        let video_probe_ms = video_probe_started.elapsed().as_secs_f64() * 1000.0;
        let video_streams = unsafe { (*video_context.as_ptr()).nb_streams as usize };
        let video_bytes_now = video_context.avio_diag().map(|d| d.0).unwrap_or(0);

        let audio = match audio_worker.join() {
            Ok(Ok(audio)) => audio,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(FfmpegError::Source(
                    "audio open worker panicked during DASH open".to_string(),
                ));
            }
        };
        let audio_context = audio.context;
        let audio_bytes_now = audio_context.avio_diag().map(|d| d.0).unwrap_or(0);

        // 阶段读数按 video → audio 的固定顺序发布（与串行版一致，便于日志对比）。
        // 注意 `elapsed_ms` 在并行下是**各自阶段自身**的耗时，两条流的数值会
        // 重叠——`dash_open_total` 才是整体墙钟（见下方）。
        stages.push(OpenStageTiming {
            stage: "avformat_open_video",
            elapsed_ms: video_open_ms,
            video_bytes: video_bytes_after_open,
            audio_bytes: 0,
            streams: None,
        });
        stages.push(OpenStageTiming {
            stage: "find_stream_info_video",
            elapsed_ms: video_probe_ms,
            video_bytes: video_bytes_now,
            audio_bytes: 0,
            streams: Some(video_streams),
        });
        stages.push(OpenStageTiming {
            stage: "avformat_open_audio",
            elapsed_ms: audio.open_ms,
            video_bytes: video_bytes_now,
            audio_bytes: audio.bytes_after_open,
            streams: None,
        });
        stages.push(OpenStageTiming {
            stage: "find_stream_info_audio",
            elapsed_ms: audio.probe_ms,
            video_bytes: video_bytes_now,
            audio_bytes: audio_bytes_now,
            streams: Some(audio.stream_count),
        });
        for stage in &stages {
            emit_open_stage(stage);
        }

        let video_origin = format_timeline_origin_micros(video_context.as_ptr());
        trace_timeline_origin(video_origin);
        let audio_origin = format_timeline_origin_micros(audio_context.as_ptr());
        trace_timeline_origin(audio_origin);

        let (video_probe, video_time_bases) =
            inspect_format_context(&video_uri, video_context.as_ptr());
        let (audio_probe, audio_time_bases) =
            inspect_format_context(&audio_uri, audio_context.as_ptr());

        // 两个输入即将被移入 `inputs`，其 AVIO 读数（累计消费字节）在此收口
        // ——这是「本次 DASH open 到底从源读了多少字节」的最终值。
        let final_reads = (
            video_context.avio_diag().map(|d| d.0).unwrap_or(0),
            audio_context.avio_diag().map(|d| d.0).unwrap_or(0),
        );

        let video_stream_count = video_time_bases.len();
        let audio_offset = video_stream_count as i32;

        let inputs = vec![
            DemuxInput {
                context: video_context,
                stream_time_bases: video_time_bases.clone(),
                timeline_origin_micros: video_origin,
                stream_offset: 0,
            },
            DemuxInput {
                context: audio_context,
                stream_time_bases: audio_time_bases.clone(),
                timeline_origin_micros: audio_origin,
                stream_offset: audio_offset,
            },
        ];

        // 合并 probe：audio 输入的 track id / source 平移到全局 index
        let mut probe = video_probe.clone();
        probe.uri = format!("{video_uri} | {audio_uri}");
        probe.duration = probe.duration.max(audio_probe.duration);
        for mut track in audio_probe.tracks {
            track.id += audio_offset as i64;
            probe.tracks.push(track);
        }
        for mut video in audio_probe.video {
            video.track_id += audio_offset as i64;
            probe.video.push(video);
        }
        for mut audio in audio_probe.audio {
            audio.track_id += audio_offset as i64;
            probe.audio.push(audio);
        }
        for mut subtitle in audio_probe.subtitles {
            subtitle.id += audio_offset as i64;
            if let SubtitleTrackSource::Embedded { stream_index } = &mut subtitle.source {
                *stream_index += audio_offset as i64;
            }
            probe.subtitles.push(subtitle);
        }
        let mut subtitle_fonts = probe.subtitle_fonts.to_vec();
        subtitle_fonts.extend(audio_probe.subtitle_fonts.iter().cloned());
        probe.subtitle_fonts = Arc::from(subtitle_fonts);

        let mut stream_time_bases = video_time_bases;
        stream_time_bases.extend(audio_time_bases);

        // 收口：本次 DASH demux 的分阶段读数（含总耗时与两条流的累计消费字节）。
        // `video_bytes` / `audio_bytes` 是**累计值**，看相邻两行的增量即该阶段
        // 新增读取量——这正是「网络在下载但 dur 还是 0」那段的可归因来源。
        let total_stage = OpenStageTiming {
            stage: "dash_open_total",
            elapsed_ms: dash_open_started.elapsed().as_secs_f64() * 1000.0,
            video_bytes: final_reads.0,
            audio_bytes: final_reads.1,
            streams: None,
        };
        emit_open_stage(&total_stage);
        stages.push(total_stage);

        Ok(Self {
            inputs,
            pending: vec![None, None],
            eof: vec![false, false],
            probe,
            stream_time_bases,
            selection: StreamSelection::all(),
            open_stages: stages,
        })
    }

    /// 本次 open 的分阶段读数（见 [`OpenStageTiming`]）；单输入路径为空。
    pub fn open_stages(&self) -> &[OpenStageTiming] {
        &self.open_stages
    }

    /// 每个输入自创建起从源消费的字节总量（顺序同 `Demuxer` 的输入顺序：
    /// DASH 双流下 0=video、1=audio）。本地路径没有自定义 AVIO → 该输入为
    /// `None`；HTTP 路径下这是**真实下载量**（含 seek 后重读的区段与 AVIO 的
    /// 64KiB 预读窗口）。播放层用相邻两次读数的差分回答"这次 seek 之后到底从
    /// 网络读了多少字节"——它是把预滚耗时归因到"等字节"而不是"解码"的依据。
    pub fn input_bytes_read(&self) -> Vec<Option<u64>> {
        self.inputs
            .iter()
            .map(|input| input.context.avio_diag().map(|(bytes, _)| bytes))
            .collect()
    }

    pub fn probe(&self) -> &MediaProbe {
        &self.probe
    }

    /// 释放各输入底层源的预读缓冲（宿主内存压力 / 后台挂起由此进入，对齐上游
    /// `Demuxer::release_source_buffer`）。
    ///
    /// 本地形态：`Demuxer` 是多输入（DASH 双流），因此逐个输入透传；`&mut self`
    /// 保证此刻没有在途读者。
    ///
    /// 暂无调用者：本地 `AsyncDemuxer` 把 `Demuxer` move 进 demux 线程，Drop 时
    /// 以「Stop + 有限超时 join」让整条所有权链析构（`FormatContext::drop` →
    /// `avio.take()` → `CustomAvio` → `Box<dyn MediaSource>`），源缓冲已随之
    /// 释放；上游需要显式调用，是因为它的外部字幕 demuxer 在 Drop 时仍被共享
    /// 存活。此处保留该方法作为形态对齐与将来的内存压力入口，**不接线**：本地
    /// `DemuxCommand` 是单向通道（无 reply），在 Drop 路径加阻塞命令与本地
    /// 「Drop 不阻塞 UI」的设计冲突。
    #[allow(dead_code)]
    pub(crate) fn release_source_buffer(&mut self) {
        for input in &mut self.inputs {
            if let Some(avio) = &mut input.context.avio {
                avio.source.release_buffer();
            }
        }
    }

    pub fn selection(&self) -> &StreamSelection {
        &self.selection
    }

    pub fn set_stream_selection(&mut self, selection: StreamSelection) -> Result<()> {
        if let StreamSelection::Only(streams) = &selection {
            for stream_index in streams {
                if !self.has_stream(*stream_index) {
                    return Err(FfmpegError::UnknownStream(*stream_index));
                }
            }
        }
        self.selection = selection;
        Ok(())
    }

    pub fn stream_time_base(&self, stream_index: i32) -> Option<TimeBase> {
        if stream_index < 0 {
            return None;
        }
        self.stream_time_bases
            .get(stream_index as usize)
            .copied()
            .flatten()
    }

    pub fn codec_parameters(&self, stream_index: i32) -> Result<CodecParameters<'_>> {
        if stream_index < 0 {
            return Err(FfmpegError::UnknownStream(stream_index));
        }
        let (input, local) = self
            .locate_input(stream_index)
            .ok_or(FfmpegError::UnknownStream(stream_index))?;
        let raw = input.context.as_ptr();
        let stream_count = unsafe { (*raw).nb_streams as usize };
        let Some(stream_slot) = (local as usize)
            .checked_sub(0)
            .filter(|index| *index < stream_count)
        else {
            return Err(FfmpegError::UnknownStream(stream_index));
        };
        let stream = unsafe { *(*raw).streams.add(stream_slot) };
        if stream.is_null() {
            return Err(FfmpegError::UnknownStream(stream_index));
        }
        let codecpar = unsafe { (*stream).codecpar };
        if codecpar.is_null() {
            return Err(FfmpegError::NullPointer("AVStream.codecpar"));
        }
        Ok(CodecParameters {
            ptr: codecpar,
            stream_index,
            time_base: TimeBase::from_av(unsafe { (*stream).time_base }),
            _owner: PhantomData,
        })
    }

    /// 全局 stream index → 所属输入 + 局部 index。
    fn locate_input(&self, stream_index: i32) -> Option<(&DemuxInput, i32)> {
        self.inputs
            .iter()
            .find_map(|input| input.local_index(stream_index).map(|local| (input, local)))
    }

    pub fn owned_codec_parameters(&self, stream_index: i32) -> Result<OwnedCodecParameters> {
        let parameters = self.codec_parameters(stream_index)?;
        OwnedCodecParameters::copy_from(parameters)
    }

    pub fn open_decoder(&self, stream_index: i32) -> Result<Decoder> {
        Decoder::open(self.codec_parameters(stream_index)?)
    }

    pub fn open_subtitle_decoder(&self, stream_index: i32) -> Result<SubtitleDecoder> {
        let parameters = self.codec_parameters(stream_index)?;
        SubtitleDecoder::open_raw_with_fonts(
            parameters.ptr,
            parameters.stream_index,
            parameters.time_base,
            self.probe.subtitle_fonts.clone(),
        )
    }

    pub fn read_packet(&mut self) -> Result<Option<Packet>> {
        // 补齐每个输入的待处理包
        for index in 0..self.inputs.len() {
            if !self.eof[index] && self.pending[index].is_none() {
                match self.inputs[index].read_packet()? {
                    Some(packet) => self.pending[index] = Some(packet),
                    None => self.eof[index] = true,
                }
            }
        }
        // 选择 DTS 最早的一包（双流按解码进度交错，保证音画同步）。
        // 视频含 B 帧时 PTS 在解码序下会回退，故必须用 DTS 做比较基准。
        let mut chosen: Option<usize> = None;
        for index in 0..self.inputs.len() {
            let Some(packet) = self.pending[index].as_ref() else {
                continue;
            };
            let decode_ts = packet
                .dts()
                .or_else(|| packet.pts())
                .map(|ts| ts.seconds())
                .unwrap_or(f64::MIN);
            let is_earlier = match chosen {
                None => true,
                Some(current) => {
                    let current_ts = self.pending[current]
                        .as_ref()
                        .and_then(Packet::dts)
                        .or_else(|| self.pending[current].as_ref().and_then(Packet::pts))
                        .map(|ts| ts.seconds())
                        .unwrap_or(f64::MIN);
                    decode_ts < current_ts
                }
            };
            if is_earlier {
                chosen = Some(index);
            }
        }
        let Some(input_index) = chosen else {
            return Ok(None);
        };
        let mut packet = self.pending[input_index].take().expect("pending ensured");
        packet.set_stream_index(packet.stream_index() + self.inputs[input_index].stream_offset);
        if self.selection.accepts(packet.stream_index()) {
            Ok(Some(packet))
        } else {
            self.read_packet()
        }
    }

    pub fn seek(&mut self, position: Duration) -> Result<()> {
        let relative_target = position.as_micros().min(i64::MAX as u128) as i64;
        // 分输入打点：HTTP 源上 `av_seek_frame` 不是纯内存操作——fMP4 要靠 moof
        // 定关键帧，且 AVIO 的 seek 钩子会在这里触发 `prefetch_hint` 重设下载
        // 窗口，所以它既耗时又读字节。video / audio 两个输入是**串行** seek 的，
        // 把每个输入的耗时 + 读取量单独报出来，才能看出"预滚那几秒"里有多少
        // 花在 seek 本身、有多少是 seek 之后等目标窗口的字节。
        let seek_started = std::time::Instant::now();
        let bytes_before = self.input_bytes_read();
        for index in 0..self.inputs.len() {
            let input_started = std::time::Instant::now();
            self.seek_input(index, relative_target)?;
            emit_seek_stage(
                "av_seek_input",
                position,
                input_started.elapsed(),
                Some(index),
                &bytes_before,
                &self.input_bytes_read(),
            );
            self.pending[index] = None;
            self.eof[index] = false;
        }
        emit_seek_stage(
            "demux_seek_total",
            position,
            seek_started.elapsed(),
            None,
            &bytes_before,
            &self.input_bytes_read(),
        );
        Ok(())
    }

    /// 对单个输入执行 seek：优先选输入内视频流做 backward keyframe 锚点，
    /// 无视频流（如音频输入）则整文件 seek；seek 后清掉该输入的交错缓存。
    fn seek_input(&mut self, index: usize, relative_target: i64) -> Result<()> {
        let absolute_target =
            absolute_seek_target_micros(relative_target, self.inputs[index].timeline_origin_micros);
        let seek_stream = self.selected_video_seek_stream();
        let selected_streams = match &self.selection {
            StreamSelection::All => None,
            StreamSelection::Only(streams) => Some(streams.iter().copied().collect::<Vec<_>>()),
        };
        let available_video_streams = self
            .probe
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        let (stream_index, target) = seek_stream.map_or((-1, absolute_target), |stream_index| {
            // 视频流只属于 video 输入；音频输入用整文件 seek（stream=-1）
            let Some(local) = self.inputs[index].local_index(stream_index) else {
                return (-1, absolute_target);
            };
            let time_base = self
                .stream_time_base(stream_index)
                .expect("selected video stream has a time base");
            (
                local,
                rescale_microseconds_to_time_base(absolute_target, time_base),
            )
        });
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "ffmpeg_seek",
                "stage": "backward_keyframe_request",
                "inputIndex": index,
                "streamIndex": stream_index,
                "relativeTargetMicros": relative_target,
                "timelineOriginMicros": self.inputs[index].timeline_origin_micros,
                "absoluteTargetMicros": absolute_target,
                "targetInSeekTimeBase": target,
                "selectedStreams": selected_streams,
                "availableVideoStreams": available_video_streams,
            })
            .to_string(),
        );
        let backward = unsafe {
            sys::av_seek_frame(
                self.inputs[index].context.as_mut_ptr(),
                stream_index,
                target,
                sys::AVSEEK_FLAG_BACKWARD as i32,
            )
        };
        if backward < 0 {
            // Queue-backed subtitle demuxers reject a backward seek before the
            // first cue (for example target 0 with the first SRT cue at 500 ms).
            // A full-range seek can select that first future cue while retaining
            // the normal backward/keyframe behavior for media demuxers that
            // accepted the primary request.
            let fallback = unsafe {
                sys::avformat_seek_file(
                    self.inputs[index].context.as_mut_ptr(),
                    stream_index,
                    i64::MIN,
                    target,
                    i64::MAX,
                    0,
                )
            };
            if fallback < 0 {
                check(backward, "av_seek_frame")?;
            }
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "ffmpeg_seek",
                    "stage": "full_range_fallback",
                    "inputIndex": index,
                    "streamIndex": stream_index,
                    "relativeTargetMicros": relative_target,
                    "timelineOriginMicros": self.inputs[index].timeline_origin_micros,
                    "absoluteTargetMicros": absolute_target,
                    "targetInSeekTimeBase": target,
                    "backwardError": error_string(backward),
                })
                .to_string(),
            );
        }
        check(
            unsafe { sys::avformat_flush(self.inputs[index].context.as_mut_ptr()) },
            "avformat_flush",
        )?;
        Ok(())
    }

    fn selected_video_seek_stream(&self) -> Option<i32> {
        self.probe
            .tracks
            .iter()
            .find(|track| {
                track.kind == TrackKind::Video
                    && i32::try_from(track.id)
                        .ok()
                        .is_some_and(|stream| self.selection.accepts(stream))
            })
            .and_then(|track| i32::try_from(track.id).ok())
    }

    fn has_stream(&self, stream_index: i32) -> bool {
        self.stream_time_base(stream_index).is_some()
            || self
                .probe
                .tracks
                .iter()
                .any(|track| track.id == stream_index as i64)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CodecParameters<'a> {
    ptr: *const sys::AVCodecParameters,
    stream_index: i32,
    time_base: TimeBase,
    _owner: PhantomData<&'a Demuxer>,
}

impl CodecParameters<'_> {
    pub fn stream_index(self) -> i32 {
        self.stream_index
    }

    pub fn time_base(self) -> TimeBase {
        self.time_base
    }

    pub fn codec_name(self) -> Option<String> {
        unsafe { codec_name((*self.ptr).codec_id) }
    }

    pub fn kind(self) -> Option<TrackKind> {
        unsafe { track_kind((*self.ptr).codec_type) }
    }

    /// Container-signalled Dolby Vision configuration profile (`dvcC`/`dvvC`
    /// carried as `AV_PKT_DATA_DOVI_CONF` codec parameters side data), used to
    /// steer decode backend selection.
    pub fn dolby_vision_profile(self) -> Option<u8> {
        unsafe { codec_parameters_dolby_vision_profile(self.ptr) }
    }
}

pub struct OwnedCodecParameters {
    ptr: *mut sys::AVCodecParameters,
    stream_index: i32,
    time_base: TimeBase,
}

unsafe impl Send for OwnedCodecParameters {}

impl OwnedCodecParameters {
    /// Container-signalled Dolby Vision configuration profile (`dvcC`/`dvvC`
    /// carried as `AV_PKT_DATA_DOVI_CONF` codec parameters side data), used to
    /// steer decode backend selection.
    pub fn dolby_vision_profile(&self) -> Option<u8> {
        unsafe { codec_parameters_dolby_vision_profile(self.ptr) }
    }

    fn copy_from(parameters: CodecParameters<'_>) -> Result<Self> {
        let ptr = unsafe { sys::avcodec_parameters_alloc() };
        if ptr.is_null() {
            return Err(FfmpegError::NullPointer("avcodec_parameters_alloc"));
        }
        check(
            unsafe { sys::avcodec_parameters_copy(ptr, parameters.ptr) },
            "avcodec_parameters_copy",
        )?;
        Ok(Self {
            ptr,
            stream_index: parameters.stream_index,
            time_base: parameters.time_base,
        })
    }

    pub fn stream_index(&self) -> i32 {
        self.stream_index
    }

    pub fn time_base(&self) -> TimeBase {
        self.time_base
    }

    pub fn codec_name(&self) -> Option<String> {
        unsafe { codec_name((*self.ptr).codec_id) }
    }

    pub fn kind(&self) -> Option<TrackKind> {
        unsafe { track_kind((*self.ptr).codec_type) }
    }
}

impl Drop for OwnedCodecParameters {
    fn drop(&mut self) {
        unsafe { sys::avcodec_parameters_free(&mut self.ptr) };
    }
}

pub struct SubtitleDecoder {
    context: *mut sys::AVCodecContext,
    stream_index: i32,
    time_base: TimeBase,
    track: SubtitleTrackConfig,
    ass_track: Option<Arc<AssTrackResources>>,
    missing_ass_header_reported: bool,
}

unsafe impl Send for SubtitleDecoder {}

impl SubtitleDecoder {
    pub fn open(parameters: CodecParameters<'_>) -> Result<Self> {
        Self::open_raw_with_fonts(
            parameters.ptr,
            parameters.stream_index,
            parameters.time_base,
            Arc::from([]),
        )
    }

    pub fn open_owned(parameters: &OwnedCodecParameters) -> Result<Self> {
        Self::open_owned_with_fonts(parameters, Arc::from([]))
    }

    pub fn open_owned_with_fonts(
        parameters: &OwnedCodecParameters,
        fonts: Arc<[SubtitleFontAttachment]>,
    ) -> Result<Self> {
        Self::open_raw_with_fonts(
            parameters.ptr,
            parameters.stream_index,
            parameters.time_base,
            fonts,
        )
    }

    fn open_raw_with_fonts(
        parameters_ptr: *const sys::AVCodecParameters,
        stream_index: i32,
        time_base: TimeBase,
        fonts: Arc<[SubtitleFontAttachment]>,
    ) -> Result<Self> {
        if unsafe { track_kind((*parameters_ptr).codec_type) } != Some(TrackKind::Subtitle) {
            return Err(FfmpegError::ExpectedSubtitleStream);
        }

        let codec_id = unsafe { (*parameters_ptr).codec_id };
        let codec = unsafe { sys::avcodec_find_decoder(codec_id) };
        if codec.is_null() {
            return Err(FfmpegError::NullPointer("avcodec_find_decoder(subtitle)"));
        }
        let context = unsafe { sys::avcodec_alloc_context3(codec) };
        if context.is_null() {
            return Err(FfmpegError::NullPointer("avcodec_alloc_context3(subtitle)"));
        }
        let mut decoder = Self {
            context,
            stream_index,
            time_base,
            track: SubtitleTrackConfig::embedded(i64::from(stream_index), i64::from(stream_index)),
            ass_track: None,
            missing_ass_header_reported: false,
        };
        check(
            unsafe { sys::avcodec_parameters_to_context(decoder.context, parameters_ptr) },
            "avcodec_parameters_to_context(subtitle)",
        )?;
        unsafe {
            (*decoder.context).pkt_timebase = time_base.to_av_rational();
        }
        check(
            unsafe { sys::avcodec_open2(decoder.context, codec, ptr::null_mut()) },
            "avcodec_open2(subtitle)",
        )?;
        let subtitle_header_size = unsafe { (*decoder.context).subtitle_header_size };
        let codec_private = if subtitle_header_size > 0 {
            unsafe { subtitle_header_bytes(decoder.context, stream_index) }
        } else {
            unsafe { codec_parameter_extradata(parameters_ptr, stream_index) }
        };
        if let Some(codec_private) = codec_private {
            decoder.ass_track = Some(Arc::new(AssTrackResources::new(
                i64::from(stream_index),
                codec_private,
                fonts,
            )));
        } else if unsafe { subtitle_codec_is_text(codec_id) } {
            decoder.missing_ass_header_reported = true;
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "subtitle_ass_track",
                    "stage": "codec_private_missing",
                    "trackId": stream_index,
                    "sourceStreamIndex": stream_index,
                })
                .to_string(),
            );
        }
        Ok(decoder)
    }

    pub fn stream_index(&self) -> i32 {
        self.stream_index
    }

    pub fn time_base(&self) -> TimeBase {
        self.time_base
    }

    pub fn track(&self) -> &SubtitleTrackConfig {
        &self.track
    }

    pub fn decode_packet(&mut self, packet: &Packet) -> Result<Option<DecodedSubtitleFrame>> {
        if packet.stream_index() != self.stream_index {
            return Err(FfmpegError::StreamMismatch {
                decoder_stream: self.stream_index,
                packet_stream: packet.stream_index(),
            });
        }

        let mut subtitle = sys::AVSubtitle::default();
        let mut got_subtitle = 0;
        let code = unsafe {
            sys::avcodec_decode_subtitle2(
                self.context,
                &mut subtitle,
                &mut got_subtitle,
                packet.as_ptr().cast_mut(),
            )
        };
        if code < 0 {
            if got_subtitle != 0 {
                unsafe { sys::avsubtitle_free(&mut subtitle) };
            }
            check(code, "avcodec_decode_subtitle2")?;
        }
        if got_subtitle == 0 {
            return Ok(None);
        }

        let canvas = unsafe {
            (
                (*self.context).width.max(0) as u32,
                (*self.context).height.max(0) as u32,
            )
        };
        let frame = unsafe {
            import_av_subtitle(
                i64::from(self.stream_index),
                packet,
                &subtitle,
                canvas,
                self.ass_track.clone(),
            )
        };
        unsafe { sys::avsubtitle_free(&mut subtitle) };
        let frame = frame?;
        if frame.has_ass_chunks() && frame.ass_track.is_none() && !self.missing_ass_header_reported
        {
            self.missing_ass_header_reported = true;
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "subtitle_ass_track",
                    "stage": "codec_private_missing",
                    "trackId": frame.track_id,
                    "sourceStreamIndex": self.stream_index,
                })
                .to_string(),
            );
        }
        Ok(Some(frame))
    }

    pub fn flush(&mut self) {
        unsafe { sys::avcodec_flush_buffers(self.context) };
    }
}

impl Drop for SubtitleDecoder {
    fn drop(&mut self) {
        unsafe { sys::avcodec_free_context(&mut self.context) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderOutput {
    Frame,
    NeedMoreInput,
    EndOfStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderBackend {
    Software,
    Cuda,
    Vaapi,
    VideoToolbox,
    D3d11va,
    MediaCodec,
    AvCodec,
}

impl DecoderBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Cuda => "cuda",
            Self::Vaapi => "vaapi",
            Self::VideoToolbox => "videotoolbox",
            Self::D3d11va => "d3d11va",
            Self::MediaCodec => "mediacodec",
            Self::AvCodec => "avcodec",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderConfig {
    pub backend: DecoderBackend,
    pub mediacodec_surface: bool,
    /// D3D11VA 硬件帧池的内存预算（字节）。`None` 使用
    /// `D3D11VA_POOL_BUDGET_BYTES`。Windows 之外的平台忽略。
    pub d3d11va_pool_budget_bytes: Option<usize>,
    /// D3D11VA 硬件帧池深度的下界覆盖值。`None` 使用按 codec 推导的
    /// `d3d11va_pool_floor`。
    ///
    /// 只有在**同时**下调了视频帧 handoff 队列容量
    /// （`PlayerConfig::video_frame_queue_capacity`）时才应该跟着下调：
    /// 下界里的消费端预留就是那个队列的容量加渲染器常驻的 1 片。队列还是 8 时
    /// 把它改成 21 就是复现 `get_buffer failed`。
    ///
    /// 一般情况下**不要设它**：正常路径由
    /// `d3d11va_consumer_queue_frames` 把队列容量喂进来，下界自动跟着队列走，
    /// 不会漂移。
    pub d3d11va_pool_min_frames: Option<i32>,
    /// 视频帧 handoff 队列容量（片），用来推导池下界里的消费端预留。
    ///
    /// 由 `Player` 在创建解码器时填成它实际使用的
    /// `PlayerConfig::video_frame_queue_capacity`；默认值
    /// `DEFAULT_VIDEO_FRAME_QUEUE_CAPACITY` 只用于没有 Player 层队列的直接
    /// 调用（`PlaybackSession` / `VideoPlaybackEngine::open`）。
    /// Windows 之外的平台忽略。
    pub d3d11va_consumer_queue_frames: i32,
}

impl DecoderConfig {
    pub fn cuda() -> Self {
        Self {
            backend: DecoderBackend::Cuda,
            mediacodec_surface: false,
            ..Self::default()
        }
    }

    pub fn vaapi() -> Self {
        Self {
            backend: DecoderBackend::Vaapi,
            mediacodec_surface: false,
            ..Self::default()
        }
    }

    pub fn software() -> Self {
        Self {
            backend: DecoderBackend::Software,
            mediacodec_surface: false,
            ..Self::default()
        }
    }

    pub fn videotoolbox() -> Self {
        Self {
            backend: DecoderBackend::VideoToolbox,
            mediacodec_surface: false,
            ..Self::default()
        }
    }

    pub fn d3d11va() -> Self {
        Self {
            backend: DecoderBackend::D3d11va,
            mediacodec_surface: false,
            ..Self::default()
        }
    }

    pub fn mediacodec() -> Self {
        Self {
            backend: DecoderBackend::MediaCodec,
            mediacodec_surface: true,
            ..Self::default()
        }
    }

    pub fn mediacodec_byte_buffer() -> Self {
        Self {
            backend: DecoderBackend::MediaCodec,
            mediacodec_surface: false,
            ..Self::default()
        }
    }

    pub fn avcodec() -> Self {
        Self {
            backend: DecoderBackend::AvCodec,
            mediacodec_surface: false,
            ..Self::default()
        }
    }
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            backend: DecoderBackend::Software,
            mediacodec_surface: false,
            d3d11va_pool_budget_bytes: None,
            d3d11va_pool_min_frames: None,
            d3d11va_consumer_queue_frames: crate::core::DEFAULT_VIDEO_FRAME_QUEUE_CAPACITY as i32,
        }
    }
}

struct HardwareDecoderState {
    device_ref: *mut sys::AVBufferRef,
    frames_ref: *mut sys::AVBufferRef,
    pixel_format: sys::AVPixelFormat,
    /// D3D11VA 池预算、下界覆盖值与消费端队列容量，随 `DecoderConfig` 一起下发到
    /// `select_hw_format` → `ensure_d3d11va_frames_for_context` 的建池路径。
    pool_budget_bytes: usize,
    pool_min_frames: Option<i32>,
    pool_consumer_queue_frames: i32,
}

#[cfg(target_os = "android")]
#[derive(Clone)]
struct AndroidMediaCodecFrameState {
    source: Arc<AndroidMediaCodecFrameSource>,
    delivery: Arc<Mutex<AndroidMediaCodecFrameDelivery>>,
}

#[cfg(target_os = "android")]
#[derive(Default)]
struct AndroidMediaCodecFrameDelivery {
    released: bool,
    result: Option<std::result::Result<Arc<AndroidHardwareBufferImage>, String>>,
}

impl Drop for HardwareDecoderState {
    fn drop(&mut self) {
        unsafe { sys::av_buffer_unref(&mut self.frames_ref) };
        unsafe { sys::av_buffer_unref(&mut self.device_ref) };
    }
}

pub struct Decoder {
    context: *mut sys::AVCodecContext,
    stream_index: i32,
    time_base: TimeBase,
    backend: DecoderBackend,
    mediacodec_surface: bool,
    eof_sent: bool,
    end_of_stream: bool,
    hw_state: Option<Box<HardwareDecoderState>>,
    #[cfg(target_os = "android")]
    mediacodec_source: Option<Arc<AndroidMediaCodecFrameSource>>,
    #[cfg(target_env = "ohos")]
    ohos_decoder: Option<OhosVideoDecoder>,
    #[cfg(target_env = "ohos")]
    ohos_color_properties: OhosFrameColorProperties,
}

#[cfg(target_env = "ohos")]
#[derive(Debug, Clone, Copy)]
struct OhosFrameColorProperties {
    range: sys::AVColorRange,
    primaries: sys::AVColorPrimaries,
    transfer: sys::AVColorTransferCharacteristic,
    space: sys::AVColorSpace,
    chroma_location: sys::AVChromaLocation,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn open(parameters: CodecParameters<'_>) -> Result<Self> {
        Self::open_with_config(parameters, DecoderConfig::default())
    }

    pub fn open_owned(parameters: &OwnedCodecParameters) -> Result<Self> {
        Self::open_owned_with_config(parameters, DecoderConfig::default())
    }

    pub fn open_owned_with_config(
        parameters: &OwnedCodecParameters,
        config: DecoderConfig,
    ) -> Result<Self> {
        Self::open_raw(
            parameters.ptr,
            parameters.stream_index,
            parameters.time_base,
            config,
        )
    }

    #[cfg(target_env = "ohos")]
    pub(crate) fn open_owned_with_ohos_avcodec_surface(
        parameters: &OwnedCodecParameters,
        surface: Option<Arc<crate::ohos::avcodec::OhosAvCodecSurface>>,
    ) -> Result<Self> {
        let codec_id = unsafe { (*parameters.ptr).codec_id };
        Self::open_ohos_avcodec(
            parameters.ptr,
            parameters.stream_index,
            parameters.time_base,
            codec_id,
            surface,
        )
    }

    pub fn open_with_config(
        parameters: CodecParameters<'_>,
        config: DecoderConfig,
    ) -> Result<Self> {
        Self::open_raw(
            parameters.ptr,
            parameters.stream_index,
            parameters.time_base,
            config,
        )
    }

    fn open_raw(
        parameters_ptr: *const sys::AVCodecParameters,
        stream_index: i32,
        time_base: TimeBase,
        config: DecoderConfig,
    ) -> Result<Self> {
        configure_ffmpeg_debug_logging();
        let codec_id = unsafe { (*parameters_ptr).codec_id };
        #[cfg(target_env = "ohos")]
        if config.backend == DecoderBackend::AvCodec {
            return Self::open_ohos_avcodec(
                parameters_ptr,
                stream_index,
                time_base,
                codec_id,
                None,
            );
        }
        let (codec, find_operation) = match config.backend {
            DecoderBackend::Software => software_decoder(codec_id),
            DecoderBackend::MediaCodec => (
                mediacodec_decoder(codec_id),
                "avcodec_find_decoder_by_name(MediaCodec)",
            ),
            DecoderBackend::VideoToolbox
            | DecoderBackend::D3d11va
            | DecoderBackend::Cuda
            | DecoderBackend::Vaapi => ffmpeg_hardware_decoder(codec_id),
            DecoderBackend::AvCodec => (
                ptr::null(),
                "HarmonyOS AVCodec is unavailable on this target",
            ),
        };
        if codec.is_null() {
            return Err(FfmpegError::NullPointer(find_operation));
        }
        let context = unsafe { sys::avcodec_alloc_context3(codec) };
        if context.is_null() {
            return Err(FfmpegError::NullPointer("avcodec_alloc_context3"));
        }
        let decoder = Self {
            context,
            stream_index,
            time_base,
            backend: config.backend,
            mediacodec_surface: config.mediacodec_surface,
            eof_sent: false,
            end_of_stream: false,
            hw_state: None,
            #[cfg(target_os = "android")]
            mediacodec_source: None,
            #[cfg(target_env = "ohos")]
            ohos_decoder: None,
            #[cfg(target_env = "ohos")]
            ohos_color_properties: OhosFrameColorProperties {
                range: unsafe { (*parameters_ptr).color_range },
                primaries: unsafe { (*parameters_ptr).color_primaries },
                transfer: unsafe { (*parameters_ptr).color_trc },
                space: unsafe { (*parameters_ptr).color_space },
                chroma_location: unsafe { (*parameters_ptr).chroma_location },
            },
        };
        check(
            unsafe { sys::avcodec_parameters_to_context(decoder.context, parameters_ptr) },
            "avcodec_parameters_to_context",
        )?;
        // Packets are normalized to the stream time base before being sent
        // (Packet::normalize_timeline), so declare that base the way the
        // subtitle decoder does. Decoders that pass timestamps through are
        // unaffected; a decoder that rescales packet timestamps gets frame pts
        // back in the stream time base the rest of the engine expects instead
        // of an undefined base.
        unsafe {
            (*decoder.context).pkt_timebase = time_base.to_av_rational();
        }
        let mut decoder = decoder;
        match config.backend {
            DecoderBackend::Software => {}
            DecoderBackend::MediaCodec =>
            {
                #[cfg(target_os = "android")]
                if config.mediacodec_surface {
                    decoder.configure_mediacodec(codec)?;
                }
            }
            DecoderBackend::VideoToolbox => decoder.configure_videotoolbox(codec)?,
            DecoderBackend::D3d11va => decoder.configure_d3d11va(codec, &config)?,
            DecoderBackend::Cuda => decoder.configure_hardware(
                codec,
                sys::AVHWDeviceType_AV_HWDEVICE_TYPE_CUDA,
                "CUDA/NVDEC codec hardware configuration",
                "create CUDA/NVDEC device",
            )?,
            DecoderBackend::Vaapi => decoder.configure_hardware(
                codec,
                sys::AVHWDeviceType_AV_HWDEVICE_TYPE_VAAPI,
                "VA-API codec hardware configuration",
                "create VA-API device",
            )?,
            DecoderBackend::AvCodec => {}
        }
        let mut codec_options = ptr::null_mut();
        #[cfg(target_os = "android")]
        if config.backend == DecoderBackend::MediaCodec {
            let option_result = check(
                unsafe {
                    sys::av_dict_set(
                        &mut codec_options,
                        c"erika_nonblocking".as_ptr(),
                        c"1".as_ptr(),
                        0,
                    )
                },
                "av_dict_set(erika_nonblocking=1)",
            );
            if let Err(error) = option_result {
                unsafe { sys::av_dict_free(&mut codec_options) };
                return Err(error);
            }
        }
        #[cfg(target_os = "android")]
        if config.backend == DecoderBackend::MediaCodec && config.mediacodec_surface {
            // FFmpeg's JNI MediaCodec wrapper can only configure a jobject
            // Surface. The AImageReader route supplies an ANativeWindow, which
            // is consumed correctly only by the NDK wrapper. FFmpeg otherwise
            // auto-selects JNI whenever a JavaVM is registered and silently
            // configures a byte-buffer decoder with no output Surface.
            let option_result = check(
                unsafe {
                    sys::av_dict_set(&mut codec_options, c"ndk_codec".as_ptr(), c"1".as_ptr(), 0)
                },
                "av_dict_set(ndk_codec=1)",
            );
            if let Err(error) = option_result {
                unsafe { sys::av_dict_free(&mut codec_options) };
                return Err(error);
            }
            let delay_flush_result = check(
                unsafe {
                    sys::av_dict_set(
                        &mut codec_options,
                        c"delay_flush".as_ptr(),
                        c"1".as_ptr(),
                        0,
                    )
                },
                "av_dict_set(delay_flush=1)",
            );
            if let Err(error) = delay_flush_result {
                unsafe { sys::av_dict_free(&mut codec_options) };
                return Err(error);
            }
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "android_mediacodec_surface",
                    "stage": "decoder_api_selected",
                    "codecApi": "ndk",
                    "options": ["ndk_codec=1", "delay_flush=1", "erika_nonblocking=1"],
                    "reason": "AImageReader output is supplied as ANativeWindow",
                })
                .to_string(),
            );
        }
        let open_code = unsafe { sys::avcodec_open2(decoder.context, codec, &mut codec_options) };
        #[cfg(target_os = "android")]
        let erika_nonblocking_unconsumed = config.backend == DecoderBackend::MediaCodec
            && !unsafe {
                sys::av_dict_get(codec_options, c"erika_nonblocking".as_ptr(), ptr::null(), 0)
            }
            .is_null();
        #[cfg(target_os = "android")]
        let surface_option_unconsumed = config.backend == DecoderBackend::MediaCodec
            && config.mediacodec_surface
            && (!unsafe { sys::av_dict_get(codec_options, c"ndk_codec".as_ptr(), ptr::null(), 0) }
                .is_null()
                || !unsafe {
                    sys::av_dict_get(codec_options, c"delay_flush".as_ptr(), ptr::null(), 0)
                }
                .is_null());
        unsafe { sys::av_dict_free(&mut codec_options) };
        #[cfg_attr(not(target_os = "android"), allow(unused_mut))]
        let mut open_result = check(open_code, "avcodec_open2");
        #[cfg(target_os = "android")]
        if open_result.is_ok()
            && config.backend == DecoderBackend::MediaCodec
            && erika_nonblocking_unconsumed
        {
            open_result = Err(FfmpegError::AndroidMediaCodec(
                "FFmpeg did not consume required decoder option erika_nonblocking=1; rebuild Android native dependencies with the Erika FFmpeg 8.1.2 patch set"
                    .to_string(),
            ));
        }
        #[cfg(target_os = "android")]
        if open_result.is_ok()
            && config.backend == DecoderBackend::MediaCodec
            && config.mediacodec_surface
            && surface_option_unconsumed
        {
            open_result = Err(FfmpegError::AndroidMediaCodec(
                "FFmpeg did not consume required Surface decoder options ndk_codec=1 and delay_flush=1"
                    .to_string(),
            ));
        }
        #[cfg(target_os = "android")]
        if config.backend == DecoderBackend::MediaCodec {
            if open_result.is_ok() {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "android_mediacodec_decoder",
                        "stage": "nonblocking_dequeue_enabled",
                        "mode": if config.mediacodec_surface {
                            "surface_ahardwarebuffer"
                        } else {
                            "bytebuffer_cpu_upload"
                        },
                        "option": "erika_nonblocking",
                        "value": true,
                        "verification": "avcodec_open2_consumed",
                        "patchVersion": "v2",
                        "inputDequeueTimeoutMicros": 0,
                        "outputDequeueTimeoutMicros": 0,
                        "drainOutputDequeueTimeoutMicros": 0,
                        "reason": "positive NDK dequeue timeouts can remain blocked inside the synchronous MediaCodec framework request after a codec transition",
                        "retryOwner": "rust_playback_worker",
                    })
                    .to_string(),
                );
            }
            if let Err(error) = &open_result {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "android_mediacodec_decoder",
                        "stage": "decoder_open_failed",
                        "mode": if config.mediacodec_surface {
                            "surface_ahardwarebuffer"
                        } else {
                            "bytebuffer_cpu_upload"
                        },
                        "reason": error.to_string(),
                    })
                    .to_string(),
                );
            }
        }
        open_result?;
        if config.backend == DecoderBackend::Software && codec_id == sys::AVCodecID_AV_CODEC_ID_AV1
        {
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "video_decoder",
                    "stage": "software_decoder_selected",
                    "codec": "av1",
                    "decoder": "libdav1d",
                })
                .to_string(),
            );
        }
        Ok(decoder)
    }

    pub fn stream_index(&self) -> i32 {
        self.stream_index
    }

    pub fn time_base(&self) -> TimeBase {
        self.time_base
    }

    pub fn backend(&self) -> DecoderBackend {
        self.backend
    }

    pub fn uses_mediacodec_surface(&self) -> bool {
        self.backend == DecoderBackend::MediaCodec && self.mediacodec_surface
    }

    pub fn uses_avcodec_surface(&self) -> bool {
        #[cfg(target_env = "ohos")]
        {
            self.backend == DecoderBackend::AvCodec
                && self
                    .ohos_decoder
                    .as_ref()
                    .is_some_and(OhosVideoDecoder::uses_surface)
        }
        #[cfg(not(target_env = "ohos"))]
        {
            false
        }
    }

    pub fn eof_sent(&self) -> bool {
        self.eof_sent
    }

    pub fn is_end_of_stream(&self) -> bool {
        self.end_of_stream
    }

    pub fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.stream_index() != self.stream_index {
            return Err(FfmpegError::StreamMismatch {
                decoder_stream: self.stream_index,
                packet_stream: packet.stream_index(),
            });
        }
        #[cfg(target_env = "ohos")]
        if let Some(decoder) = self.ohos_decoder.as_mut() {
            let pts_micros = packet
                .pts()
                .or_else(|| packet.dts())
                .map(timestamp_to_microseconds)
                .unwrap_or(0);
            return match decoder
                .send_packet(packet.data(), pts_micros, packet.is_key())
                .map_err(FfmpegError::OhosAvCodec)?
            {
                true => Ok(()),
                false => check(av_error(EAGAIN), "OH_VideoDecoder_PushInputBuffer(wait)"),
            };
        }
        check(
            unsafe { sys::avcodec_send_packet(self.context, packet.as_ptr()) },
            "avcodec_send_packet",
        )
    }

    pub fn send_eof(&mut self) -> Result<()> {
        if self.eof_sent {
            return Ok(());
        }
        #[cfg(target_env = "ohos")]
        if let Some(decoder) = self.ohos_decoder.as_mut() {
            if decoder.send_eof().map_err(FfmpegError::OhosAvCodec)? {
                self.eof_sent = true;
                return Ok(());
            }
            return check(
                av_error(EAGAIN),
                "OH_VideoDecoder_PushInputBuffer(eof wait)",
            );
        }
        let code = unsafe { sys::avcodec_send_packet(self.context, ptr::null()) };
        if code == AVERROR_EOF {
            self.eof_sent = true;
            self.end_of_stream = true;
            return Ok(());
        }
        check(code, "avcodec_send_packet_eof")?;
        self.eof_sent = true;
        Ok(())
    }

    pub fn receive_frame(&mut self) -> Result<DecoderOutputFrame> {
        #[cfg(target_env = "ohos")]
        if let Some(decoder) = self.ohos_decoder.as_mut() {
            let time_base = self.time_base;
            let color = self.ohos_color_properties;
            if decoder.uses_surface() {
                return match decoder
                    .receive_surface_frame()
                    .map_err(FfmpegError::OhosAvCodec)?
                {
                    OhosDecoderOutput::NeedMoreInput => Ok(DecoderOutputFrame::NeedMoreInput),
                    OhosDecoderOutput::Frame(decoded) => Ok(DecoderOutputFrame::Frame(
                        Frame::from_ohos_surface(decoded, time_base, color)?,
                    )),
                    OhosDecoderOutput::EndOfStream => {
                        self.end_of_stream = true;
                        Ok(DecoderOutputFrame::EndOfStream)
                    }
                };
            }
            return match decoder
                .receive_frame(|decoded| {
                    Frame::from_ohos_nv12(decoded, time_base, color)
                        .map_err(|error| error.to_string())
                })
                .map_err(FfmpegError::OhosAvCodec)?
            {
                OhosDecoderOutput::NeedMoreInput => Ok(DecoderOutputFrame::NeedMoreInput),
                OhosDecoderOutput::Frame(frame) => Ok(DecoderOutputFrame::Frame(frame)),
                OhosDecoderOutput::EndOfStream => {
                    self.end_of_stream = true;
                    Ok(DecoderOutputFrame::EndOfStream)
                }
            };
        }
        #[cfg_attr(not(target_os = "android"), allow(unused_mut))]
        let mut frame = Frame::alloc(self.time_base)?;
        let code = unsafe { sys::avcodec_receive_frame(self.context, frame.ptr) };
        if code == av_error(EAGAIN) {
            return Ok(DecoderOutputFrame::NeedMoreInput);
        }
        if code == AVERROR_EOF {
            self.end_of_stream = true;
            return Ok(DecoderOutputFrame::EndOfStream);
        }
        check(code, "avcodec_receive_frame")?;
        #[cfg(target_os = "android")]
        if self.backend == DecoderBackend::MediaCodec && frame.is_mediacodec() {
            let source = self.mediacodec_source.clone().ok_or_else(|| {
                FfmpegError::AndroidMediaCodec(
                    "decoder returned AV_PIX_FMT_MEDIACODEC without an AImageReader source"
                        .to_string(),
                )
            })?;
            frame.mediacodec = Some(AndroidMediaCodecFrameState {
                source,
                delivery: Arc::new(Mutex::new(AndroidMediaCodecFrameDelivery::default())),
            });
        }
        Ok(DecoderOutputFrame::Frame(frame))
    }

    pub fn flush(&mut self) {
        #[cfg(target_env = "ohos")]
        if let Some(decoder) = self.ohos_decoder.as_mut() {
            if let Err(error) = decoder.flush() {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "ohos_avcodec_decoder",
                        "stage": "flush_failed",
                        "reason": error,
                    })
                    .to_string(),
                );
            }
            self.eof_sent = false;
            self.end_of_stream = false;
            return;
        }
        unsafe { sys::avcodec_flush_buffers(self.context) };
        self.eof_sent = false;
        self.end_of_stream = false;
    }

    #[cfg(target_env = "ohos")]
    fn open_ohos_avcodec(
        parameters_ptr: *const sys::AVCodecParameters,
        stream_index: i32,
        time_base: TimeBase,
        codec_id: sys::AVCodecID,
        surface: Option<Arc<crate::ohos::avcodec::OhosAvCodecSurface>>,
    ) -> Result<Self> {
        let codec_kind = match codec_id {
            sys::AVCodecID_AV_CODEC_ID_H264 => OhosVideoCodec::Avc,
            sys::AVCodecID_AV_CODEC_ID_HEVC => OhosVideoCodec::Hevc,
            _ => {
                return Err(FfmpegError::OhosAvCodec(format!(
                    "unsupported codec id {codec_id}; AVCodec hardware decoding currently supports H.264 and HEVC"
                )));
            }
        };
        let parameters = unsafe { &*parameters_ptr };
        let width = parameters.width.max(0) as u32;
        let height = parameters.height.max(0) as u32;
        let codec_config = if parameters.extradata.is_null() || parameters.extradata_size <= 0 {
            &[]
        } else {
            unsafe {
                slice::from_raw_parts(parameters.extradata, parameters.extradata_size as usize)
            }
        };
        let ohos_decoder = OhosVideoDecoder::new(codec_kind, width, height, codec_config, surface)
            .map_err(FfmpegError::OhosAvCodec)?;
        let output_mode = if ohos_decoder.uses_surface() {
            "surface_native_buffer"
        } else {
            "buffer_nv12_direct_frame_copy"
        };
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "ohos_avcodec_decoder",
                "stage": "decoder_opened",
                "codec": codec_kind.as_str(),
                "mode": output_mode,
                "width": width,
                "height": height,
                "codecConfigBytes": codec_config.len(),
            })
            .to_string(),
        );
        Ok(Self {
            context: ptr::null_mut(),
            stream_index,
            time_base,
            backend: DecoderBackend::AvCodec,
            mediacodec_surface: false,
            eof_sent: false,
            end_of_stream: false,
            hw_state: None,
            #[cfg(target_os = "android")]
            mediacodec_source: None,
            ohos_decoder: Some(ohos_decoder),
            ohos_color_properties: OhosFrameColorProperties {
                range: parameters.color_range,
                primaries: parameters.color_primaries,
                transfer: parameters.color_trc,
                space: parameters.color_space,
                chroma_location: parameters.chroma_location,
            },
        })
    }

    fn configure_videotoolbox(&mut self, codec: *const sys::AVCodec) -> Result<()> {
        self.configure_hardware(
            codec,
            sys::AVHWDeviceType_AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
            "avcodec_get_hw_config(VideoToolbox)",
            "av_hwdevice_ctx_create(VideoToolbox)",
        )
    }

    fn configure_d3d11va(
        &mut self,
        codec: *const sys::AVCodec,
        config: &DecoderConfig,
    ) -> Result<()> {
        self.configure_hardware(
            codec,
            sys::AVHWDeviceType_AV_HWDEVICE_TYPE_D3D11VA,
            "avcodec_get_hw_config(D3D11VA)",
            "av_hwdevice_ctx_create(D3D11VA)",
        )?;
        // 池预算随 HW 状态一起下发：建池发生在 `select_hw_format` →
        // `ensure_d3d11va_frames_for_context`，那条路径只能通过
        // `AVCodecContext::opaque` 拿到 HW 状态，拿不到 `DecoderConfig`。
        if let Some(state) = self.hw_state.as_mut() {
            state.pool_budget_bytes = config
                .d3d11va_pool_budget_bytes
                .unwrap_or(D3D11VA_POOL_BUDGET_BYTES);
            state.pool_min_frames = config.d3d11va_pool_min_frames;
            state.pool_consumer_queue_frames = config.d3d11va_consumer_queue_frames;
        }
        // 硬解固定单线程解码。
        //
        // 为什么必须在这里收敛：Erika 从不设置 `thread_count`，FFmpeg 于是取默认值
        // `FFMIN(nb_cpus + 1, 16)` 并启用**帧级多线程**
        // （`third_party/.../libavcodec/pthread.c:49-64`、
        // `pthread_frame.c:923-930`），同时把 `avctx->delay` 设为
        // `thread_count - 1`（同文件 955）。帧级多线程的意义是让 CPU 侧的解码
        // 工作并行，但 D3D11VA 的宏块重建本来就在 GPU 上，CPU 只剩位流解析，
        // 并行收益极小；代价却是每个线程各占输出帧槽（`decoded_frames_get_free`，
        // 同文件 192/284）和 15 帧的解码流水线延迟（30fps 下首帧就要多等 0.5s，
        // seek/切清晰度同理）。
        //
        // 更重要的是池：帧级多线程让"同时在飞的切片数"与线程数成正比，
        // 官方自己建池时会在 `libavcodec/decode.c:1148-1151` 补 `+ thread_count`
        // 片，而 Erika 在 `select_hw_format` 里用自建 `hw_frames_ctx` 覆写，
        // **整段补偿被绕过**。实测"1080p 池降到 48 片就 get_buffer failed"
        // 正是这么来的。把硬解收敛成单线程后，解码器内部的池需求回到 DXVA
        // 规范基线（`dxva2.c:619-634` 的 1 + 参考帧），池深才可能按内存预算下调。
        //
        // 软解/其它后端不受影响：它们不走这条分支，仍然拿满 CPU 并行度。
        #[cfg(target_os = "windows")]
        unsafe {
            (*self.context).thread_count = 1;
        }
        Ok(())
    }

    #[cfg(target_os = "android")]
    fn configure_mediacodec(&mut self, codec: *const sys::AVCodec) -> Result<()> {
        let result = self.configure_mediacodec_inner(codec);
        if let Err(error) = &result {
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "android_mediacodec_surface",
                    "stage": "decoder_surface_configuration_failed",
                    "reason": error.to_string(),
                })
                .to_string(),
            );
        }
        result
    }

    #[cfg(target_os = "android")]
    fn configure_mediacodec_inner(&mut self, codec: *const sys::AVCodec) -> Result<()> {
        let pixel_format =
            hardware_pixel_format(codec, sys::AVHWDeviceType_AV_HWDEVICE_TYPE_MEDIACODEC)
                .ok_or_else(|| {
                    FfmpegError::AndroidMediaCodec(
                        "avcodec_get_hw_config did not expose a MediaCodec HW device format"
                            .to_string(),
                    )
                })?;
        if pixel_format != sys::AVPixelFormat_AV_PIX_FMT_MEDIACODEC {
            return Err(FfmpegError::AndroidMediaCodec(format!(
                "unexpected MediaCodec hardware pixel format {pixel_format}"
            )));
        }
        let width = unsafe {
            (*self.context)
                .width
                .max((*self.context).coded_width)
                .max(0) as u32
        };
        let height = unsafe {
            (*self.context)
                .height
                .max((*self.context).coded_height)
                .max(0) as u32
        };
        let source = Arc::new(
            AndroidMediaCodecFrameSource::new(width, height)
                .map_err(|error| FfmpegError::AndroidMediaCodec(error.to_string()))?,
        );

        let mut device_ref =
            unsafe { sys::av_hwdevice_ctx_alloc(sys::AVHWDeviceType_AV_HWDEVICE_TYPE_MEDIACODEC) };
        if device_ref.is_null() {
            return Err(FfmpegError::NullPointer(
                "av_hwdevice_ctx_alloc(MediaCodec)",
            ));
        }
        let setup = (|| {
            let device_context =
                unsafe { (*device_ref).data.cast::<sys::AVHWDeviceContext>().as_mut() }
                    .ok_or(FfmpegError::NullPointer("AVHWDeviceContext(MediaCodec)"))?;
            let mediacodec_context = unsafe {
                device_context
                    .hwctx
                    .cast::<sys::AVMediaCodecDeviceContext>()
                    .as_mut()
            }
            .ok_or(FfmpegError::NullPointer("AVMediaCodecDeviceContext"))?;
            mediacodec_context.surface = ptr::null_mut();
            mediacodec_context.native_window = source.native_window().as_ptr();
            mediacodec_context.create_window = 0;
            check(
                unsafe { sys::av_hwdevice_ctx_init(device_ref) },
                "av_hwdevice_ctx_init(MediaCodec)",
            )?;
            let context_device_ref = unsafe { sys::av_buffer_ref(device_ref) };
            if context_device_ref.is_null() {
                return Err(FfmpegError::NullPointer(
                    "av_buffer_ref(MediaCodec hw_device_ctx)",
                ));
            }
            let mut hw_state = Box::new(HardwareDecoderState {
                device_ref,
                frames_ref: ptr::null_mut(),
                pixel_format,
                pool_budget_bytes: D3D11VA_POOL_BUDGET_BYTES,
                pool_min_frames: None,
                pool_consumer_queue_frames: crate::core::DEFAULT_VIDEO_FRAME_QUEUE_CAPACITY as i32,
            });
            unsafe {
                (*self.context).hw_device_ctx = context_device_ref;
                (*self.context).opaque = (&mut *hw_state) as *mut HardwareDecoderState as *mut _;
                (*self.context).get_format = Some(select_hw_format);
            }
            self.hw_state = Some(hw_state);
            self.mediacodec_source = Some(source.clone());
            device_ref = ptr::null_mut();
            Ok(())
        })();
        if !device_ref.is_null() {
            unsafe { sys::av_buffer_unref(&mut device_ref) };
        }
        setup?;
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "android_mediacodec_surface",
                "stage": "decoder_surface_configured",
                "width": source.width(),
                "height": source.height(),
                "pixelFormat": "mediacodec",
            })
            .to_string(),
        );
        Ok(())
    }

    fn configure_hardware(
        &mut self,
        codec: *const sys::AVCodec,
        device_type: sys::AVHWDeviceType,
        hw_config_operation: &'static str,
        hw_device_operation: &'static str,
    ) -> Result<()> {
        let pixel_format = hardware_pixel_format(codec, device_type)
            .ok_or_else(|| FfmpegError::NullPointer(hw_config_operation))?;
        // FFmpeg selects a default device when unset. A render-node path lets
        // Intel/AMD multi-GPU systems choose the desired VA-API device; CUDA
        // accepts an ordinal such as 0. No vendor driver is bundled here.
        let device = match self.backend {
            DecoderBackend::Cuda => std::env::var("ERIKA_CUDA_DEVICE").ok(),
            DecoderBackend::Vaapi => std::env::var("ERIKA_VAAPI_DEVICE").ok(),
            _ => None,
        }
        .map(std::ffi::CString::new)
        .transpose()
        .map_err(|_| FfmpegError::InteriorNul)?;
        let mut device_ref = ptr::null_mut();
        check(
            unsafe {
                sys::av_hwdevice_ctx_create(
                    &mut device_ref,
                    device_type,
                    device.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
                    ptr::null_mut(),
                    0,
                )
            },
            hw_device_operation,
        )?;
        if device_ref.is_null() {
            return Err(FfmpegError::NullPointer(hw_device_operation));
        }

        let context_device_ref = unsafe { sys::av_buffer_ref(device_ref) };
        if context_device_ref.is_null() {
            unsafe { sys::av_buffer_unref(&mut device_ref) };
            return Err(FfmpegError::NullPointer("av_buffer_ref(hw_device_ctx)"));
        }

        let mut hw_state = Box::new(HardwareDecoderState {
            device_ref,
            frames_ref: ptr::null_mut(),
            pixel_format,
            pool_budget_bytes: D3D11VA_POOL_BUDGET_BYTES,
            pool_min_frames: None,
            pool_consumer_queue_frames: crate::core::DEFAULT_VIDEO_FRAME_QUEUE_CAPACITY as i32,
        });

        unsafe {
            (*self.context).hw_device_ctx = context_device_ref;
            (*self.context).opaque = (&mut *hw_state) as *mut HardwareDecoderState as *mut _;
            (*self.context).get_format = Some(select_hw_format);
        }
        self.hw_state = Some(hw_state);
        Ok(())
    }
}

fn software_decoder(codec_id: sys::AVCodecID) -> (*const sys::AVCodec, &'static str) {
    if codec_id == sys::AVCodecID_AV_CODEC_ID_AV1 {
        return (
            unsafe { sys::avcodec_find_decoder_by_name(c"libdav1d".as_ptr()) },
            "avcodec_find_decoder_by_name(libdav1d)",
        );
    }
    (
        unsafe { sys::avcodec_find_decoder(codec_id) },
        "avcodec_find_decoder",
    )
}

fn ffmpeg_hardware_decoder(codec_id: sys::AVCodecID) -> (*const sys::AVCodec, &'static str) {
    // FFmpeg prefers libdav1d when both AV1 decoders are installed. Select
    // its hardware wrapper explicitly for VideoToolbox and D3D11VA.
    if codec_id == sys::AVCodecID_AV_CODEC_ID_AV1 {
        return (
            unsafe { sys::avcodec_find_decoder_by_name(c"av1".as_ptr()) },
            "avcodec_find_decoder_by_name(av1)",
        );
    }
    (
        unsafe { sys::avcodec_find_decoder(codec_id) },
        "avcodec_find_decoder",
    )
}

fn mediacodec_decoder(codec_id: sys::AVCodecID) -> *const sys::AVCodec {
    let name = if codec_id == sys::AVCodecID_AV_CODEC_ID_H264 {
        b"h264_mediacodec\0".as_slice()
    } else if codec_id == sys::AVCodecID_AV_CODEC_ID_HEVC {
        b"hevc_mediacodec\0".as_slice()
    } else if codec_id == sys::AVCodecID_AV_CODEC_ID_MPEG2VIDEO {
        b"mpeg2_mediacodec\0".as_slice()
    } else if codec_id == sys::AVCodecID_AV_CODEC_ID_MPEG4 {
        b"mpeg4_mediacodec\0".as_slice()
    } else if codec_id == sys::AVCodecID_AV_CODEC_ID_VP8 {
        b"vp8_mediacodec\0".as_slice()
    } else if codec_id == sys::AVCodecID_AV_CODEC_ID_VP9 {
        b"vp9_mediacodec\0".as_slice()
    } else if codec_id == sys::AVCodecID_AV_CODEC_ID_AV1 {
        b"av1_mediacodec\0".as_slice()
    } else {
        return ptr::null();
    };
    unsafe { sys::avcodec_find_decoder_by_name(name.as_ptr().cast()) }
}

#[cfg(target_os = "windows")]
unsafe fn ensure_d3d11va_frames_for_context(
    context: *mut sys::AVCodecContext,
    state: *mut HardwareDecoderState,
) {
    if !unsafe { (*state).frames_ref }.is_null() {
        return;
    }
    const D3D11_BIND_SHADER_RESOURCE: u32 = 0x8;
    const D3D11_BIND_DECODER: u32 = 0x200;
    const D3D11_RESOURCE_MISC_SHARED: u32 = 0x2;

    let mut frames_ref = unsafe { sys::av_hwframe_ctx_alloc((*state).device_ref) };
    if frames_ref.is_null() {
        trace_ffmpeg("av_hwframe_ctx_alloc(D3D11VA) returned null");
        return;
    }

    let init_result = (|| {
        let frames_ctx = unsafe { (*frames_ref).data.cast::<sys::AVHWFramesContext>().as_mut() }?;
        let d3d11_frames = unsafe {
            frames_ctx
                .hwctx
                .cast::<sys::AVD3D11VAFramesContext>()
                .as_mut()
        }?;
        frames_ctx.format = unsafe { (*state).pixel_format };
        let sw_format = d3d11va_sw_format_for_context(context);
        frames_ctx.sw_format = sw_format;
        let alignment = d3d11va_surface_alignment(unsafe { (*context).codec_id });
        let aligned_width = align_i32(unsafe { (*context).coded_width }, alignment);
        let aligned_height = align_i32(unsafe { (*context).coded_height }, alignment);
        frames_ctx.width = aligned_width;
        frames_ctx.height = aligned_height;
        frames_ctx.initial_pool_size = d3d11va_pool_size(
            context,
            aligned_width,
            aligned_height,
            sw_format,
            unsafe { (*state).pool_budget_bytes },
            unsafe { (*state).pool_min_frames },
            unsafe { (*state).pool_consumer_queue_frames },
        );
        d3d11_frames.BindFlags = D3D11_BIND_DECODER | D3D11_BIND_SHADER_RESOURCE;
        d3d11_frames.MiscFlags = D3D11_RESOURCE_MISC_SHARED;
        let code = unsafe { sys::av_hwframe_ctx_init(frames_ref) };
        if code < 0 {
            trace_ffmpeg("av_hwframe_ctx_init(D3D11VA) failed");
            return None;
        }
        Some(())
    })();

    if init_result.is_some() {
        unsafe {
            (*state).frames_ref = frames_ref;
        }
        frames_ref = ptr::null_mut();
    }
    if !frames_ref.is_null() {
        unsafe { sys::av_buffer_unref(&mut frames_ref) };
    }
}

#[cfg(target_os = "windows")]
fn d3d11va_sw_format_for_context(context: *const sys::AVCodecContext) -> sys::AVPixelFormat {
    match unsafe { (*context).sw_pix_fmt } {
        sys::AVPixelFormat_AV_PIX_FMT_YUV420P10LE
        | sys::AVPixelFormat_AV_PIX_FMT_YUV420P10BE
        | sys::AVPixelFormat_AV_PIX_FMT_P010LE
        | sys::AVPixelFormat_AV_PIX_FMT_P010BE => sys::AVPixelFormat_AV_PIX_FMT_P010LE,
        _ => sys::AVPixelFormat_AV_PIX_FMT_NV12,
    }
}

#[cfg(target_os = "windows")]
fn d3d11va_surface_alignment(codec_id: sys::AVCodecID) -> i32 {
    if codec_id == sys::AVCodecID_AV_CODEC_ID_MPEG2VIDEO {
        32
    } else if codec_id == sys::AVCodecID_AV_CODEC_ID_HEVC
        || codec_id == sys::AVCodecID_AV_CODEC_ID_AV1
    {
        128
    } else {
        16
    }
}

/// D3D11VA 硬件帧池的默认内存预算（字节）。
///
/// 池是**一次性全量预分配**：`av_hwframe_ctx_init` 会执行一次
/// `CreateTexture2D`，`ArraySize = initial_pool_size`，把池里每个切片都建成
/// 同一张纹理的一个 array slice
/// （`third_party/src/FFmpeg-n8.1.2/libavutil/hwcontext_d3d11va.c:298-330`）。
/// 之后 `d3d11va_pool_alloc` 只在池里没有空闲切片时才被调用，一旦
/// `nb_surfaces_used >= ArraySize` 就直接返回 NULL（同文件 262-265 的
/// "Static surface pool size exceeded"），`d3d11va_get_buffer` 随之返回
/// `AVERROR(ENOMEM)`，上层看到的就是 `get_buffer() failed`。
///
/// **所以"池深 × 单片字节"就是实打实的常驻内存，而池深同时又是并发上限。**
/// 这两件事被同一个数字绑定，正是本函数要按分辨率重新折算它的原因。
///
/// 64MiB 的取值依据：
/// - 1080P H.264（coded 1920x1088，NV12 1.5B/px）单片 3.13MB →
///   64MiB / 3.13MB ≈ 21 片；低于下界时由下界兜底，所以 1080P 实际取
///   `d3d11va_pool_floor`（队列 3 → 21 片 ≈ 66MB，队列 8 → 26 片 ≈ 81MB）；
/// - 720P 以下折算值高于下界，预算直接生效（720P ≈ 48 片 × 1.38MB ≈ 66MB），
///   并被 `d3d11va_pool_legacy_pool_size` 的上限夹住，不会因为预算富裕就
///   无限多占内存；
/// - 1440P/4K 的折算值低于下界，最终由 `d3d11va_pool_floor` 决定——高分辨率
///   省内存靠的是"不再乘 60 片"，而不是击穿正确性下界。
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const D3D11VA_POOL_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// 消费端在池里占用的常驻切片数 = 视频帧 handoff 队列容量 + 渲染器常驻那一片。
///
/// 这些切片不属于解码器，但同样从池里取，池深必须把它们算进去，否则解码器
/// 自己的 `get_buffer` 就会撞上限：
/// - `core.rs` 的 `subscribe_video_frames` 按
///   `PlayerConfig::video_frame_queue_capacity` 建的有界队列，队列满时这几片
///   全部被 pin 住；队列再满则 `emit_video_frame_from_worker` 用 `try_send`
///   丢弃而不是阻塞解码器，所以队列容量就是这一项的上限，不会再涨，但也不会
///   自己降下来。**渲染停摆（宿主 `pause_rendering` 的 open 窗口）正是队列会
///   满的场景**，所以这一项不能按稳态的 0~2 计；
/// - `renderer/d3d11.rs` 的 `try_clone_ref` 把当前帧的 AVFrame 引用存进
///   `ImportedVideoFrame::_frame`，直到下一次 import 或 clear 才释放，稳态占
///   1 片。`preserve_current_frame_for_transition` 是空实现，transition 期间
///   **不释放**这一片。
///
/// 不重复计算 presenter 正在 import 的那一片：它是刚从队列里 `try_recv`
/// 出来的，队列计数已经减一，两者是同一片。
///
/// 注意会话内部还有一条解码输出队列（`playback.rs` 的
/// `D3D11VA_VIDEO_FRAME_QUEUE_LIMIT = 8`），它**两种配置下都存在**，所以它是
/// 常量而不是本项的自变量；它吃掉的余量见 `d3d11va_pool_floor` 的推导。
#[cfg(target_os = "windows")]
fn d3d11va_consumer_headroom_frames(queue_frames: i32) -> i32 {
    queue_frames.max(1) + D3D11VA_RENDERER_RESIDENT_FRAMES
}

/// 渲染器常驻的那一片（见 `d3d11va_consumer_headroom_frames`）。
#[cfg(target_os = "windows")]
const D3D11VA_RENDERER_RESIDENT_FRAMES: i32 = 1;

/// DXVA 规范基线里的工作切片数：1 片解码工作面。
///
/// `third_party/src/FFmpeg-n8.1.2/libavcodec/dxva2.c:619-628` 里 FFmpeg 自己
/// 也是这么算的（`num_surfaces = 1` 起算，再按 codec 加参考帧）。
#[cfg(target_os = "windows")]
const D3D11VA_BASE_WORK_SURFACES: i32 = 1;

/// 建池路径读 `AVCodecContext::codec_id` 的唯一入口（附 null 契约断言）。
///
/// 单独一个函数而不是散落三处 `unsafe { (*ctx).codec_id }`：这三个函数都
/// 要求 `context` 非空（真实建池路径永远如此，单测也改传伪造 context 而不再
/// 传 null——见 `d3d11va_pool_size` 的 `context` 参数文档）。把"绝不为 null"
/// 这条契约和那个 `unsafe` 收在一处，将来要加检查只改这一行。
#[cfg(target_os = "windows")]
fn d3d11va_context_codec_id(context: *const sys::AVCodecContext) -> sys::AVCodecID {
    debug_assert!(
        !context.is_null(),
        "建池路径的 AVCodecContext 绝不能为 null"
    );
    unsafe { (*context).codec_id }
}

/// 解码器 DPB（参考帧）需要预留的切片数，按 codec 取规范上限。
///
/// 与 `dxva2.c:623-628` 完全一致：H264/HEVC 16，VP9/AV1 8，其它 2。
///
/// 这是**兜底值**，只在 [`d3d11va_sps_reference_surfaces`] 拿不到真实值时用
/// （见那里的可用性判据）。
#[cfg(target_os = "windows")]
fn d3d11va_reference_surfaces(codec_id: sys::AVCodecID) -> i32 {
    if codec_id == sys::AVCodecID_AV_CODEC_ID_H264 || codec_id == sys::AVCodecID_AV_CODEC_ID_HEVC {
        16
    } else if codec_id == sys::AVCodecID_AV_CODEC_ID_VP9
        || codec_id == sys::AVCodecID_AV_CODEC_ID_AV1
    {
        8
    } else {
        2
    }
}

/// `d3d11va_reference_surfaces` 允许的下限：低于它的"实测值"不可信。
///
/// DPB 里至少有当前正在解码的那一片（它也是一条参考链的起点），所以规范上
/// 不存在 ref < 1 的合法片源。留 1 的下界而不是直接采信，是为了让"读到 0"
/// 这类异常（解码器还没解 SPS）自然落回兜底分支，而不是建出池深 1+ 的池。
#[cfg(target_os = "windows")]
const D3D11VA_MIN_SPS_REFERENCE_FRAMES: i32 = 1;

/// 从 `AVCodecContext::refs` 读该片源**真实**的参考帧数（H.264 专有）。
///
/// # 这个数为什么可信
///
/// FFmpeg 自己就把 SPS 解出来的 `num_ref_frames` 回填到 `avctx->refs`
/// （`third_party/src/FFmpeg-n8.1.2/libavcodec/h264_slice.c:1101`：
/// `h->avctx->refs = sps->ref_frame_count;`），而 `avctx->refs` 的字段文档就是
/// "number of reference frames / decoding: Set by lavc"（`avcodec.h:692-697`）。
///
/// **时序上也来得及**：`select_hw_format` 由 `avctx->get_format` 回调触发，
/// 它在 `h264_init_ps` 的第 1155 行（`get_pixel_format` → `ff_get_format`）
/// 被调用，而 `avctx->refs` 的赋值在**同一个函数**的第 1101 行——赋值先于回调。
/// 也就是说建池那一刻，SPS 早已解析完毕，不需要我们自己解 NAL。
///
/// # 为什么 H.264 之外一律返回 `None`
///
/// 全 FFmpeg 树里给 `avctx->refs` 赋值的只有两处：`h264_slice.c:1101` 和
/// `apv_decode.c:116`（后者恒置 0）。**HEVC / VP9 / AV1 / VP8 的解码器都不
/// 写这个字段**，它会停在 `options_table.h` 里 `refs` 选项的默认值 1。
///
/// 所以对非 H.264 调用本函数，读到的"真实值"其实是那个默认 1——按它建池会
/// 少算 15 片参考帧，是必现的 `get_buffer failed`。必须返回 `None` 退回规范
/// 上限，不能"看着像个数字就用"。
///
/// HEVC 其实也有等价信息（`sps->temporal_layer[].max_dec_pic_buffering`，
/// 见 `hevc/hevcdec.c:3385-3386`），但它只存在于 `HEVCSPS` 私有结构里，
/// `AVCodecContext` 上没有对应字段——要读只能改 FFmpeg 或自己解 SPS。当前
/// 不做（见下）。
///
/// `context` 绝不能为 null（契约见 [`d3d11va_context_codec_id`]）。
///
/// 单测不再用 `ptr::null()` 表示"拿不到 SPS 实测值"，而是传一个**只填了
/// `codec_id` 与 `refs` 的伪造 context**（refs 填 0，或 codec 非 H.264）——
/// 那才是真实建池时"拿不到实测值"的两种成因，也顺带把 null 路径从测试里
/// 彻底去掉，省得留一条只在测试里存在的分支。
#[cfg(target_os = "windows")]
fn d3d11va_sps_reference_surfaces(context: *const sys::AVCodecContext) -> Option<i32> {
    if d3d11va_context_codec_id(context) != sys::AVCodecID_AV_CODEC_ID_H264 {
        return None;
    }
    let refs = unsafe { (*context).refs };
    (refs >= D3D11VA_MIN_SPS_REFERENCE_FRAMES).then_some(refs)
}

/// 实际用于建池的参考帧切片数：能用 SPS 实测值就用，否则用规范上限。
///
/// # 收益：**只在 1440P / 4K 上，1080P 及以下为 0**
///
/// `pool = clamp(预算 / 单片字节, 下界, 旧固定值)` —— 参考帧只影响**下界**，
/// 而 64MiB 预算在低分辨率上折算出的片数**高于**旧下界，所以那一档根本不
/// 受下界影响。下界只在预算折算值掉到它以下时（即单片够大）才成为约束。
///
/// 实测（队列 3、`num_ref_frames` 实测 4 vs 规范上限 16）：
///
/// | 档位 | 单片 | 预算折算 | 旧池(ref16) | 新池(ref4) | 旧内存 | 新内存 |
/// |---|---|---|---|---|---|---|
/// | 720P NV12 | 1.32MB | 48 | 48 | 48 | 63MB | 63MB |
/// | 1080P NV12 | 2.99MB | 21 | 21 | **21** | 63MB | **63MB** |
/// | 1440P NV12 | 5.27MB | 12 | 21 | **12** | 111MB | **63MB** |
/// | 4K P010 | 23.91MB | 2 | 21 | **9** | 502MB | **215MB** |
///
/// **1080P 一格都没省** —— 预算折算 21 片恰好等于旧下界 21，两边都取 21。
/// （本文档初稿曾按"21→9、省 38MB"估算，那是错的：漏了预算这一层的 clamp，
/// 是单测实测把它打出来的。）真正省下的是 1440P（−48MB）与 4K（−287MB）。
///
/// 换句话说：这个优化救的是**高分辨率**。而 4K 那 502MB 恰好就是内存报告里
/// "4K−1080P 差 254MB"的来源量级——高分辨率下帧池是唯一的大头。
/// 要让 1080P 也降，得动 `D3D11VA_POOL_BUDGET_BYTES` 预算本身，那是另一个
/// 更激进的旋钮（预算正是防止低分辨率池过小的护栏），本轮不碰。
///
/// # 为什么不能更激进
///
/// 池在 open 时定容、之后**无法增长**（D3D11VA 的帧数组用
/// `ArraySize = initial_pool_size` 一次建好，见本文件 `d3d11va_init_hw_frames`
/// 的注释），低估一次就是一次 `get_buffer failed` —— 表现是"有声音无画面、
/// 宿主换源重开、进度归零"，正是当年把池从 20 加到 60 的那次故障。所以：
///
/// - 只在 `avctx->refs` 确定已被 lavc 填过时才采信（H.264，见上）；
/// - 采信时**仍然保留消费端余量**（`d3d11va_consumer_headroom_frames` 不变），
///   因为真正会打爆池的是"解码器 pin 住多少片"，参考帧只是其中一项；
/// - 实测值只调 `d3d11va_reference_surfaces` 这一项，`D3D11VA_POOL_BUDGET_BYTES`
///   的预算折算与旧固定上限都不动 —— 后两者是另外两条独立的保守约束。
#[cfg(target_os = "windows")]
fn d3d11va_effective_reference_surfaces(context: *const sys::AVCodecContext) -> i32 {
    d3d11va_sps_reference_surfaces(context)
        .unwrap_or_else(|| d3d11va_reference_surfaces(d3d11va_context_codec_id(context)))
}

/// 池深下界：低于它的池在某些播放路径上必然耗尽。
///
/// = DXVA 基线（1 工作面 + 参考帧上限）+ 消费端常驻（handoff 队列 + 渲染器 1 片）。
///
/// # 为什么 handoff 队列容量 8→3 能让池 26→21 而不损失安全性
///
/// 池在 open 时定容、之后不能增长，所以下界必须覆盖"同时被签出切片数"的峰值：
///
/// ```text
/// 峰值 = 工作面(1) + 参考帧(R) + 会话内解码输出队列(≤8) + handoff 队列(Q) + 渲染器(1)
/// ```
///
/// `R` 是片源相关的量（规范上限 16，B站 1080P H.264 实测 ref 通常 2~4），建池
/// 时拿不到，所以换个角度看：把规范上限 16 记为 R_max，则池深减掉"除参考帧以外
/// 的确定项"就是**留给参考帧的余量**
///
/// ```text
/// R_余量 = 池深 - 1(工作面) - 8(会话内队列) - Q - 1(渲染器)
/// ```
///
/// - 旧配置（池 26，Q=8）：26 - 1 - 8 - 8 - 1 = 8
/// - 新配置（池 21，Q=3）：21 - 1 - 8 - 3 - 1 = 8
///
/// 两者留下来的参考帧余量**完全相同**。也就是说把 handoff 队列从 8 降到 3 时，
/// 池里那 5 片（26→21，1080P 15MB）本来就是"为队列多买的"，退回去不是降级。
///
/// **两个数字必须一起动**：只把池压到 21 而队列仍是 8，余量只剩 3，参考帧稍多
/// 的片源就会撞 `get_buffer failed`（当年固定池 48 片崩溃是同一种故障）。
/// 正因如此，这里不接受"魔法数字"而是由 `d3d11va_consumer_headroom_frames`
/// 从队列容量推导——调用方把实际队列容量传进来，两者不可能对不上。
/// 池深下界：低于它的池在某些播放路径上必然耗尽。
///
/// = DXVA 基线（1 工作面 + 参考帧）+ 消费端常驻（handoff 队列 + 渲染器 1 片）。
///
/// 参考帧那一项走 [`d3d11va_effective_reference_surfaces`]：H.264 能拿到 SPS
/// 实测值就用实测值，其余 codec 落回规范上限。**只有这一项被 SPS 影响**——
/// 工作面与消费端两项是本项目自己的并发模型，与片源无关。
#[cfg(target_os = "windows")]
fn d3d11va_pool_floor(context: *const sys::AVCodecContext, consumer_queue_frames: i32) -> i32 {
    D3D11VA_BASE_WORK_SURFACES
        + d3d11va_effective_reference_surfaces(context)
        + d3d11va_consumer_headroom_frames(consumer_queue_frames)
}

/// 单片的显存字节数（对齐后尺寸 × 每像素字节）。
///
/// D3D11VA 的 NV12 是 1.5B/px；10bit 走 P010（每样本 16bit，4:2:0）→ 3B/px。
#[cfg(target_os = "windows")]
fn d3d11va_slice_bytes(width: i32, height: i32, sw_format: sys::AVPixelFormat) -> usize {
    let pixels = (width.max(1) as usize) * (height.max(1) as usize);
    if sw_format == sys::AVPixelFormat_AV_PIX_FMT_P010LE {
        pixels * 3
    } else {
        // NV12: pixels * 3 / 2。先乘后除，避免整除截断。
        pixels * 3 / 2
    }
}

/// 旧的固定池深，现在只作为上限使用（见 `d3d11va_pool_size` 的注释）。
#[cfg(target_os = "windows")]
fn d3d11va_pool_legacy_pool_size(codec_id: sys::AVCodecID) -> i32 {
    if codec_id == sys::AVCodecID_AV_CODEC_ID_H264 || codec_id == sys::AVCodecID_AV_CODEC_ID_HEVC {
        60
    } else if codec_id == sys::AVCodecID_AV_CODEC_ID_VP9
        || codec_id == sys::AVCodecID_AV_CODEC_ID_AV1
    {
        40
    } else {
        10
    }
}

/// 按内存预算折算 D3D11VA 硬件帧池深度。
///
/// `pool = clamp(预算 / 单片字节, 下界, 旧固定值)`
///
/// 下界由 `d3d11va_pool_floor` 决定，而它把 `consumer_queue_frames`（视频帧
/// handoff 队列容量）算成消费端预留——队列容量与池深必须同源，理由见
/// `d3d11va_pool_floor` 的推导。1080P H.264（**按规范上限 16 参考帧**，即
/// 拿不到 SPS 实测值的 codec 走这条）：
/// - 队列 8 → 下界 26 片 ≈ 78MB；
/// - 队列 3 → 下界 21 片 ≈ 63MB（Erika 默认值）；
/// - 队列 4 → 下界 22 片 ≈ 66MB。
///
/// ⚠️ **上表是"下界"，不是最终池深**。1080P 的 64MiB 预算恰好折算出 21 片，
/// 与队列 3 的下界相等，所以实际就是 21 片；队列 8 时预算 21 < 下界 26，
/// 取 26。H.264 拿到 SPS 实测 `num_ref_frames` 后下界按实测走，但**只有在
/// 预算折算值低于下界时才会改变池深** —— 即只有 1440P / 4K。收益与实测
/// 数据见 [`d3d11va_effective_reference_surfaces`]。
///
/// 历史结论（保留，别删）：这个池曾经是固定值，且是**从 20 加到 60/40** 的。
/// 当时宿主在快 CDN 源下遇到 `get_buffer failed`（表现为有声音无画面，随后宿主
/// 换源重开、进度归零），把池从 20 加到 60/40 才稳住，因此 48 片一度被认为
/// "减不得"。那个结论是对的，但它描述的是**当时的并发模型**，不是池深本身：
/// 池耗尽的根因是解码器侧同时 pin 住了远超 48 片的切片，其中最大一块是
/// FFmpeg 的**帧级多线程**。Erika 从未设置 `thread_count`，于是 FFmpeg 取默认值
/// `FFMIN(nb_cpus + 1, MAX_AUTO_THREADS=16)`
/// （`libavcodec/pthread_frame.c:923-930`），并设 `avctx->delay = thread_count - 1`
/// （同文件 955），每个解码线程还会各自占用输出帧槽
/// （`decoded_frames_get_free`，同文件 192/284）。本机 16 逻辑核时那是 16 个
/// 线程、15 帧流水线延迟、最多 32 片在飞——叠加 DXVA 基线 17 片与消费端 10 片，
/// 峰值正好落在 48~60 之间，这就是"48 片会崩、60 片没事"的由来。
///
/// 因此现在改成两件事一起做：
/// 1. 硬解路径把 `thread_count` 收敛为 1（见 `configure_d3d11va`），把解码器
///    内部的池需求压回 DXVA 规范基线；
/// 2. 池深按"内存预算 / 单片字节"折算，下界由 `d3d11va_pool_floor` 保证，
///    而后者由消费端队列容量与**片源实测参考帧数**推导。
///    只有 1 和 2 同时成立时，小于 48 的池深才是安全的；单独调小池深就是
///    复现当年那次崩溃。
///
/// `context` 用于两件事：读 SPS 实测参考帧数，以及取 `codec_id` 决定参考帧
/// 上限与旧固定池深。**绝不能为 null**（契约见 [`d3d11va_context_codec_id`]）；
/// 过去 `codec_id` 是独立入参，那有两个问题：8 个参数触发 clippy 的
/// `too_many_arguments`，且 `codec_id` 与 `context.codec_id` 理论上可以对不上
/// ——现在两者同源，那个分歧从类型上就不存在了。
#[cfg(target_os = "windows")]
fn d3d11va_pool_size(
    context: *const sys::AVCodecContext,
    width: i32,
    height: i32,
    sw_format: sys::AVPixelFormat,
    budget_bytes: usize,
    min_frames_override: Option<i32>,
    consumer_queue_frames: i32,
) -> i32 {
    let slice_bytes = d3d11va_slice_bytes(width, height, sw_format);
    let by_budget = (budget_bytes / slice_bytes.max(1)) as i32;
    let floor = min_frames_override
        .map(|value| value.max(1))
        .unwrap_or_else(|| d3d11va_pool_floor(context, consumer_queue_frames));
    let legacy = d3d11va_pool_legacy_pool_size(d3d11va_context_codec_id(context));
    by_budget.clamp(floor, legacy)
}

#[cfg(target_os = "windows")]
fn align_i32(value: i32, alignment: i32) -> i32 {
    let value = value.max(1);
    ((value + alignment - 1) / alignment) * alignment
}

fn configure_ffmpeg_debug_logging() {
    if std::env::var_os("ERIKA_FFMPEG_DEBUG").is_some() {
        unsafe { sys::av_log_set_level(sys::AV_LOG_DEBUG as c_int) };
    }
}

/// Registers the process Java VM used by FFmpeg's MediaCodec bridge.
///
/// FFmpeg cannot attach decoder worker threads to JNI until this has run. A
/// missing registration makes every MediaCodec decoder fail with "No Java
/// virtual machine has been registered", which would otherwise look like a
/// normal hardware-to-software fallback.
///
/// # Safety
///
/// `java_vm` must be the live `JavaVM*` supplied by Android to `JNI_OnLoad`.
#[cfg(target_os = "android")]
pub unsafe fn register_android_java_vm(java_vm: *mut c_void) -> Result<()> {
    if java_vm.is_null() {
        let error = FfmpegError::NullPointer("JNI_OnLoad(JavaVM)");
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "ffmpeg_jni",
                "stage": "java_vm_registration_failed",
                "reason": error.to_string(),
            })
            .to_string(),
        );
        return Err(error);
    }

    let result = check(
        unsafe { sys::av_jni_set_java_vm(java_vm, ptr::null_mut()) },
        "av_jni_set_java_vm",
    );
    match &result {
        Ok(()) => crate::trace::diagnostic(
            serde_json::json!({
                "event": "ffmpeg_jni",
                "stage": "java_vm_registered",
            })
            .to_string(),
        ),
        Err(error) => crate::trace::diagnostic(
            serde_json::json!({
                "event": "ffmpeg_jni",
                "stage": "java_vm_registration_failed",
                "reason": error.to_string(),
            })
            .to_string(),
        ),
    }
    result
}

#[cfg(target_os = "windows")]
fn trace_ffmpeg(message: &str) {
    if std::env::var_os("ERIKA_FFMPEG_DEBUG").is_some() {
        eprintln!("erika ffmpeg: {message}");
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { sys::avcodec_free_context(&mut self.context) };
    }
}

pub enum DecoderOutputFrame {
    Frame(Frame),
    NeedMoreInput,
    EndOfStream,
}

pub struct Frame {
    ptr: *mut sys::AVFrame,
    time_base: TimeBase,
    #[cfg(target_os = "android")]
    mediacodec: Option<AndroidMediaCodecFrameState>,
    #[cfg(target_env = "ohos")]
    ohos_surface: Option<OhosAvCodecSurfaceFrameState>,
}

#[cfg(target_env = "ohos")]
#[derive(Clone)]
struct OhosAvCodecSurfaceFrameState {
    image: Arc<OhosNativeBufferImage>,
}

pub struct AudioResampler {
    context: *mut sys::SwrContext,
    output_format: PcmFormat,
    next_output_pts: Option<Duration>,
    #[allow(dead_code)]
    output_layout: ChannelLayout,
}

const AUDIO_RESAMPLER_PTS_DISCONTINUITY_TOLERANCE: Duration = Duration::from_millis(250);

unsafe impl Send for AudioResampler {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoToolboxPixelBuffer<'a> {
    raw: *mut c_void,
    width: u32,
    height: u32,
    _frame: PhantomData<&'a Frame>,
}

impl VideoToolboxPixelBuffer<'_> {
    pub fn raw(self) -> *mut c_void {
        self.raw
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct D3d11vaTexture<'a> {
    raw_texture: *mut c_void,
    array_index: u32,
    width: u32,
    height: u32,
    _frame: PhantomData<&'a Frame>,
}

impl D3d11vaTexture<'_> {
    pub fn raw_texture(self) -> *mut c_void {
        self.raw_texture
    }

    pub fn array_index(self) -> u32 {
        self.array_index
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }
}

unsafe impl Send for Frame {}

impl Frame {
    fn alloc(time_base: TimeBase) -> Result<Self> {
        let ptr = unsafe { sys::av_frame_alloc() };
        if ptr.is_null() {
            return Err(FfmpegError::NullPointer("av_frame_alloc"));
        }
        Ok(Self {
            ptr,
            time_base,
            #[cfg(target_os = "android")]
            mediacodec: None,
            #[cfg(target_env = "ohos")]
            ohos_surface: None,
        })
    }

    #[cfg(target_env = "ohos")]
    fn from_ohos_surface(
        decoded: DecodedSurfaceFrame,
        time_base: TimeBase,
        color: OhosFrameColorProperties,
    ) -> Result<Self> {
        if decoded.width == 0
            || decoded.height == 0
            || decoded.width > i32::MAX as u32
            || decoded.height > i32::MAX as u32
        {
            return Err(FfmpegError::OhosAvCodec(format!(
                "invalid decoded Surface dimensions {}x{}",
                decoded.width, decoded.height
            )));
        }
        let mut frame = Self::alloc(time_base)?;
        unsafe {
            (*frame.ptr).format = sys::AVPixelFormat_AV_PIX_FMT_NONE;
            (*frame.ptr).width = decoded.width as i32;
            (*frame.ptr).height = decoded.height as i32;
            (*frame.ptr).pts = rescale_microseconds_to_time_base(decoded.pts_micros, time_base);
            (*frame.ptr).color_range = color.range;
            (*frame.ptr).color_primaries = color.primaries;
            (*frame.ptr).color_trc = color.transfer;
            (*frame.ptr).colorspace = color.space;
            (*frame.ptr).chroma_location = color.chroma_location;
        }
        frame.ohos_surface = Some(OhosAvCodecSurfaceFrameState {
            image: decoded.image,
        });
        Ok(frame)
    }

    #[cfg(target_env = "ohos")]
    fn from_ohos_nv12(
        decoded: DecodedNv12FrameView<'_>,
        time_base: TimeBase,
        color: OhosFrameColorProperties,
    ) -> Result<Self> {
        let frame = Self::alloc(time_base)?;
        if decoded.width == 0
            || decoded.height == 0
            || decoded.width > i32::MAX as u32
            || decoded.height > i32::MAX as u32
        {
            return Err(FfmpegError::OhosAvCodec(format!(
                "invalid decoded NV12 dimensions {}x{}",
                decoded.width, decoded.height
            )));
        }
        let width = decoded.width as usize;
        let height = decoded.height as usize;
        let chroma_width = width.div_ceil(2) * 2;
        let chroma_rows = height.div_ceil(2);
        let expected_luma_size = decoded.luma_stride.checked_mul(height).ok_or_else(|| {
            FfmpegError::OhosAvCodec("decoded NV12 luma size overflowed".to_string())
        })?;
        let expected_chroma_size =
            decoded
                .chroma_stride
                .checked_mul(chroma_rows)
                .ok_or_else(|| {
                    FfmpegError::OhosAvCodec("decoded NV12 chroma size overflowed".to_string())
                })?;
        if decoded.luma_stride < width
            || decoded.chroma_stride < chroma_width
            || decoded.luma.len() < expected_luma_size
            || decoded.chroma.len() < expected_chroma_size
        {
            return Err(FfmpegError::OhosAvCodec(format!(
                "decoded NV12 frame has invalid source layout: luma {}/{expected_luma_size} bytes (stride {}), chroma {}/{expected_chroma_size} bytes (stride {})",
                decoded.luma.len(),
                decoded.luma_stride,
                decoded.chroma.len(),
                decoded.chroma_stride,
            )));
        }

        unsafe {
            (*frame.ptr).format = sys::AVPixelFormat_AV_PIX_FMT_NV12 as i32;
            (*frame.ptr).width = decoded.width as i32;
            (*frame.ptr).height = decoded.height as i32;
            (*frame.ptr).pts = rescale_microseconds_to_time_base(decoded.pts_micros, time_base);
            (*frame.ptr).color_range = color.range;
            (*frame.ptr).color_primaries = color.primaries;
            (*frame.ptr).color_trc = color.transfer;
            (*frame.ptr).colorspace = color.space;
            (*frame.ptr).chroma_location = color.chroma_location;
        }
        check(
            unsafe { sys::av_frame_get_buffer(frame.ptr, 32) },
            "av_frame_get_buffer(OHOS NV12)",
        )?;

        let source_luma = decoded.luma.as_ptr();
        let source_chroma = decoded.chroma.as_ptr();
        unsafe {
            let destination_luma = (*frame.ptr).data[0];
            let destination_chroma = (*frame.ptr).data[1];
            let destination_luma_stride = (*frame.ptr).linesize[0].max(0) as usize;
            let destination_chroma_stride = (*frame.ptr).linesize[1].max(0) as usize;
            if destination_luma.is_null()
                || destination_chroma.is_null()
                || destination_luma_stride < width
                || destination_chroma_stride < chroma_width
            {
                return Err(FfmpegError::OhosAvCodec(
                    "FFmpeg allocated an invalid NV12 frame layout".to_string(),
                ));
            }
            for row in 0..height {
                ptr::copy_nonoverlapping(
                    source_luma.add(row * decoded.luma_stride),
                    destination_luma.add(row * destination_luma_stride),
                    width,
                );
            }
            for row in 0..chroma_rows {
                ptr::copy_nonoverlapping(
                    source_chroma.add(row * decoded.chroma_stride),
                    destination_chroma.add(row * destination_chroma_stride),
                    chroma_width,
                );
            }
        }
        Ok(frame)
    }

    pub fn try_clone_ref(&self) -> Result<Self> {
        #[cfg_attr(
            not(any(target_os = "android", target_env = "ohos")),
            allow(unused_mut)
        )]
        let mut frame = Frame::alloc(self.time_base)?;
        check(
            unsafe { sys::av_frame_ref(frame.ptr, self.ptr) },
            "av_frame_ref",
        )?;
        #[cfg(target_os = "android")]
        {
            frame.mediacodec = self.mediacodec.clone();
        }
        #[cfg(target_env = "ohos")]
        {
            frame.ohos_surface = self.ohos_surface.clone();
        }
        Ok(frame)
    }

    pub fn as_ptr(&self) -> *const sys::AVFrame {
        self.ptr
    }

    pub fn width(&self) -> u32 {
        unsafe { (*self.ptr).width.max(0) as u32 }
    }

    pub fn height(&self) -> u32 {
        unsafe { (*self.ptr).height.max(0) as u32 }
    }

    pub fn sample_rate(&self) -> u32 {
        unsafe { (*self.ptr).sample_rate.max(0) as u32 }
    }

    pub fn channel_count(&self) -> u32 {
        #[cfg(erika_ffmpeg_legacy_channel_layout)]
        unsafe {
            sys::av_frame_get_channels(self.ptr).max(0) as u32
        }
        #[cfg(not(erika_ffmpeg_legacy_channel_layout))]
        unsafe {
            (*self.ptr).ch_layout.nb_channels.max(0) as u32
        }
    }

    pub fn sample_count(&self) -> usize {
        unsafe { (*self.ptr).nb_samples.max(0) as usize }
    }

    pub fn sample_format(&self) -> Option<String> {
        unsafe { sample_format_name((*self.ptr).format) }
    }

    pub fn raw_sample_format(&self) -> sys::AVSampleFormat {
        unsafe { (*self.ptr).format }
    }

    pub fn is_audio(&self) -> bool {
        self.sample_rate() > 0 && self.sample_count() > 0 && self.channel_count() > 0
    }

    pub fn pixel_format(&self) -> Option<String> {
        unsafe { pixel_format_name((*self.ptr).format) }
    }

    pub fn raw_pixel_format(&self) -> i32 {
        unsafe { (*self.ptr).format }
    }

    pub fn line_sizes(&self) -> [i32; 4] {
        unsafe {
            let linesize = (*self.ptr).linesize;
            [linesize[0], linesize[1], linesize[2], linesize[3]]
        }
    }

    /// Zero-copy view of a software frame's luma plane.
    ///
    /// `yuv420p10le` and `p010le` both store two bytes per sample but pack
    /// the 10-bit code differently: software Main10 (`yuv420p10le`) keeps the
    /// code in bits `[9:0]`, while P010 left-aligns it in bits `[15:6]`.
    /// Callers must respect [`LumaPlaneView::sample_layout`] — treating both
    /// as P010 silently mis-measures software HDR10.
    pub fn luma_plane_view(&self) -> Option<LumaPlaneView<'_>> {
        let width = self.width() as usize;
        let height = self.height() as usize;
        if width == 0 || height == 0 {
            return None;
        }
        let format = self.raw_pixel_format();
        let (bytes_per_sample, layout) = if format == sys::AVPixelFormat_AV_PIX_FMT_YUV420P
            || format == sys::AVPixelFormat_AV_PIX_FMT_NV12
        {
            (1, LumaSampleLayout::U8)
        } else if format == sys::AVPixelFormat_AV_PIX_FMT_YUV420P10LE {
            (2, LumaSampleLayout::Packed10)
        } else if format == sys::AVPixelFormat_AV_PIX_FMT_P010LE {
            (2, LumaSampleLayout::P010)
        } else {
            return None;
        };
        unsafe {
            let frame = &*self.ptr;
            let data = frame.data[0] as *const u8;
            if data.is_null() {
                return None;
            }
            let stride = usize::try_from(frame.linesize[0]).ok()?;
            if stride < width.checked_mul(bytes_per_sample)? {
                return None;
            }
            Some(LumaPlaneView {
                data,
                stride,
                width,
                height,
                bytes_per_sample,
                layout,
                _frame: std::marker::PhantomData,
            })
        }
    }

    pub fn is_videotoolbox(&self) -> bool {
        self.raw_pixel_format() == sys::AVPixelFormat_AV_PIX_FMT_VIDEOTOOLBOX
    }

    pub fn is_d3d11va(&self) -> bool {
        self.raw_pixel_format() == sys::AVPixelFormat_AV_PIX_FMT_D3D11
    }

    #[cfg(target_os = "android")]
    pub fn is_mediacodec(&self) -> bool {
        self.raw_pixel_format() == sys::AVPixelFormat_AV_PIX_FMT_MEDIACODEC
    }

    #[cfg(target_os = "android")]
    pub(crate) fn render_mediacodec_image(&self) -> Result<Arc<AndroidHardwareBufferImage>> {
        if !self.is_mediacodec() {
            return Err(FfmpegError::AndroidMediaCodec(format!(
                "expected AV_PIX_FMT_MEDIACODEC, received {:?}",
                self.pixel_format()
            )));
        }
        let state = self.mediacodec.as_ref().ok_or_else(|| {
            FfmpegError::AndroidMediaCodec(
                "MediaCodec frame is not associated with its AImageReader source".to_string(),
            )
        })?;
        let expected_media_timestamp_ns = self.raw_pts().and_then(|pts| {
            let denominator = i128::from(self.time_base.den);
            if denominator == 0 {
                return None;
            }
            let timestamp_ns = i128::from(pts)
                .checked_mul(i128::from(self.time_base.num))?
                .checked_mul(1_000_000_000)?
                / denominator;
            i64::try_from(timestamp_ns).ok()
        });
        let mut delivery = state.delivery.lock().map_err(|_| {
            FfmpegError::AndroidMediaCodec(
                "MediaCodec frame delivery state lock was poisoned".to_string(),
            )
        })?;
        if let Some(result) = delivery.result.as_ref() {
            return result
                .as_ref()
                .map(Arc::clone)
                .map_err(|error| FfmpegError::AndroidMediaCodec(error.clone()));
        }
        let _delivery_guard = state
            .source
            .lock_delivery()
            .map_err(|error| FfmpegError::AndroidMediaCodec(error.to_string()))?;
        if !delivery.released {
            if let Err(error @ AndroidMediaCodecError::ImageBackpressure { .. }) = state
                .source
                .ensure_image_capacity(expected_media_timestamp_ns)
            {
                return Err(FfmpegError::AndroidMediaCodecBackpressure(
                    error.to_string(),
                ));
            }
            let buffer = unsafe { (*self.ptr).data[3] }.cast::<sys::AVMediaCodecBuffer>();
            if buffer.is_null() {
                let error = FfmpegError::NullPointer("AVMediaCodecBuffer");
                delivery.result = Some(Err(error.to_string()));
                return Err(error);
            }
            // ImageReader is an off-screen consumer, not a display surface. Release
            // exactly one codec buffer for immediate rendering and then acquire the
            // next queued image. Display-time scheduling is unnecessary here; the
            // per-source delivery lock keeps release/acquire pairs in queue order.
            let render_result = unsafe { sys::av_mediacodec_release_buffer(buffer, 1) };
            if render_result < 0 {
                let error = api_error("av_mediacodec_release_buffer(render=1)", render_result);
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "android_mediacodec_surface",
                        "stage": "render_buffer_failed",
                        "renderMode": "immediate",
                        "expectedMediaTimestampNs": expected_media_timestamp_ns,
                        "reason": error.to_string(),
                    })
                    .to_string(),
                );
                delivery.result = Some(Err(error.to_string()));
                return Err(error);
            }
            delivery.released = true;
        }
        match state
            .source
            .acquire_next_rendered_image(expected_media_timestamp_ns)
        {
            Ok(image) => {
                delivery.result = Some(Ok(Arc::clone(&image)));
                Ok(image)
            }
            Err(error @ AndroidMediaCodecError::ImageBackpressure { .. }) => Err(
                FfmpegError::AndroidMediaCodecBackpressure(error.to_string()),
            ),
            Err(error) => {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "android_mediacodec_surface",
                        "stage": "image_acquire_failed",
                        "renderMode": "immediate",
                        "expectedMediaTimestampNs": expected_media_timestamp_ns,
                        "reason": error.to_string(),
                    })
                    .to_string(),
                );
                let error = error.to_string();
                delivery.result = Some(Err(error.clone()));
                Err(FfmpegError::AndroidMediaCodec(error))
            }
        }
    }

    #[cfg(target_env = "ohos")]
    pub fn is_ohos_avcodec_surface(&self) -> bool {
        self.ohos_surface.is_some()
    }

    #[cfg(target_env = "ohos")]
    pub(crate) fn prepared_ohos_native_buffer(&self) -> Result<Arc<OhosNativeBufferImage>> {
        let state = self.ohos_surface.as_ref().ok_or_else(|| {
            FfmpegError::OhosAvCodec(
                "decoded frame is not associated with an AVCodec Surface".to_string(),
            )
        })?;
        Ok(Arc::clone(&state.image))
    }

    #[cfg(target_os = "android")]
    pub(crate) fn prepare_mediacodec_image(&self) -> Result<()> {
        if !self.is_mediacodec() {
            return Ok(());
        }
        self.render_mediacodec_image().map(|_| ())
    }

    #[cfg(target_os = "android")]
    pub(crate) fn prepared_mediacodec_image(&self) -> Result<Arc<AndroidHardwareBufferImage>> {
        if !self.is_mediacodec() {
            return Err(FfmpegError::AndroidMediaCodec(format!(
                "expected AV_PIX_FMT_MEDIACODEC, received {:?}",
                self.pixel_format()
            )));
        }
        let state = self.mediacodec.as_ref().ok_or_else(|| {
            FfmpegError::AndroidMediaCodec(
                "MediaCodec frame is not associated with its AImageReader source".to_string(),
            )
        })?;
        let delivery = state.delivery.lock().map_err(|_| {
            FfmpegError::AndroidMediaCodec(
                "MediaCodec frame delivery state lock was poisoned".to_string(),
            )
        })?;
        match delivery.result.as_ref() {
            Some(Ok(image)) => Ok(Arc::clone(image)),
            Some(Err(error)) => Err(FfmpegError::AndroidMediaCodec(error.clone())),
            None => Err(FfmpegError::AndroidMediaCodec(
                "MediaCodec image was not prepared on the playback worker".to_string(),
            )),
        }
    }

    pub fn has_hw_frames_context(&self) -> bool {
        unsafe { !(*self.ptr).hw_frames_ctx.is_null() }
    }

    pub fn videotoolbox_pixel_buffer(&self) -> Option<VideoToolboxPixelBuffer<'_>> {
        if !self.is_videotoolbox() {
            return None;
        }
        let raw = unsafe { (*self.ptr).data[3] }.cast::<c_void>();
        if raw.is_null() {
            return None;
        }
        Some(VideoToolboxPixelBuffer {
            raw,
            width: self.width(),
            height: self.height(),
            _frame: PhantomData,
        })
    }

    pub fn d3d11va_texture(&self) -> Option<D3d11vaTexture<'_>> {
        if !self.is_d3d11va() {
            return None;
        }
        let raw_texture = unsafe { (*self.ptr).data[0] }.cast::<c_void>();
        if raw_texture.is_null() {
            return None;
        }
        let array_index = unsafe { (*self.ptr).data[1] as usize }.try_into().ok()?;
        Some(D3d11vaTexture {
            raw_texture,
            array_index,
            width: self.width(),
            height: self.height(),
            _frame: PhantomData,
        })
    }

    pub fn raw_pts(&self) -> Option<i64> {
        timestamp_value(unsafe { (*self.ptr).pts })
    }

    pub fn pts(&self) -> Option<PacketTimestamp> {
        Some(PacketTimestamp {
            raw: self.raw_pts()?,
            time_base: self.time_base,
        })
    }

    pub fn color_primaries(&self) -> ColorPrimaries {
        unsafe { color_primaries((*self.ptr).color_primaries) }
    }

    pub fn transfer_function(&self) -> TransferFunction {
        unsafe { transfer_function((*self.ptr).color_trc) }
    }

    pub fn color_range(&self) -> ColorRange {
        unsafe { color_range((*self.ptr).color_range) }
    }

    pub fn matrix_coefficients(&self) -> MatrixCoefficients {
        unsafe { matrix_coefficients((*self.ptr).colorspace) }
    }

    pub fn hdr_metadata(&self) -> Option<HdrMetadata> {
        unsafe { frame_hdr_metadata(self.ptr) }
    }

    /// Per-frame Dolby Vision RPU metadata parsed by the HEVC decoder
    /// (`AV_FRAME_DATA_DOVI_METADATA`). Present on RPU-carrying streams such
    /// as profiles 5, 7, and 8, on both software and hardware decoded frames
    /// (FFmpeg parses the RPU on the CPU and attaches it regardless).
    pub fn dovi_metadata(&self) -> Option<DoviSourceMetadata> {
        unsafe {
            frame_dovi_metadata_result(self.ptr)
                .ok()
                .map(|(metadata, _)| metadata)
        }
    }

    /// Reports the enhancement-layer residual an RPU asks for but this renderer
    /// cannot compose (Profile 7 FEL/MEL). The base-layer mapping still applies
    /// to such frames, so this is diagnostic signal rather than a rejection.
    /// Returns `None` when the frame carries no usable RPU at all.
    pub fn dovi_el_status(&self) -> Option<DoviElStatus> {
        unsafe { frame_dovi_metadata_result(self.ptr).ok().map(|(_, el)| el) }
    }

    /// Returns `None` when this frame's Dolby Vision metadata can feed the
    /// mapping path. Otherwise reports why it cannot. Frames without any RPU
    /// side data report [`DoviRejectReason::MissingSideData`], which callers
    /// must interpret against the stream's Dolby Vision profile: absent RPUs
    /// are normal on Profile 8, but Profile 5 has no HDR10-compatible base
    /// layer, so a missing RPU leaves the frame unmappable.
    pub fn dovi_unavailable_reason(&self) -> Option<DoviRejectReason> {
        match self.dovi_mapping_probe() {
            Ok(_) => None,
            Err(reason) => Some(reason),
        }
    }

    /// One-pass probe of this frame's RPU: `Ok` carries the enhancement-layer
    /// status when the mapping path can consume the RPU, `Err` reports why it
    /// cannot. Prefer this over calling [`Frame::dovi_unavailable_reason`]
    /// and [`Frame::dovi_el_status`] back-to-back, which re-parses the side
    /// data.
    pub fn dovi_mapping_probe(&self) -> std::result::Result<DoviElStatus, DoviRejectReason> {
        if self.ptr.is_null() {
            return Err(DoviRejectReason::MissingSideData);
        }
        let has_side_data = unsafe {
            !sys::av_frame_get_side_data(
                self.ptr,
                sys::AVFrameSideDataType_AV_FRAME_DATA_DOVI_METADATA,
            )
            .is_null()
        };
        if !has_side_data {
            return Err(DoviRejectReason::MissingSideData);
        }
        unsafe { frame_dovi_metadata_result(self.ptr).map(|(_, el)| el) }
    }

    pub fn transfer_to_system_memory(&self) -> Result<Frame> {
        let frame = Frame::alloc(self.time_base)?;
        check(
            unsafe { sys::av_hwframe_transfer_data(frame.ptr, self.ptr, 0) },
            "av_hwframe_transfer_data",
        )?;
        check(
            unsafe { sys::av_frame_copy_props(frame.ptr, self.ptr) },
            "av_frame_copy_props",
        )?;
        Ok(frame)
    }

    /// Repack a software-decoded 8-bit 4:2:0 frame (yuv420p or nv12) into tightly
    /// packed NV12 planes, resolving the source row stride. Returns `None` for
    /// hardware frames or unsupported pixel formats (e.g. 10-bit P010).
    pub fn to_nv12(&self) -> Option<Nv12Frame> {
        let width = self.width() as usize;
        let height = self.height() as usize;
        if width == 0 || height == 0 {
            return None;
        }
        let format = self.raw_pixel_format();
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);

        // SAFETY: `self.ptr` is a valid AVFrame for this Frame's lifetime. For a
        // software 4:2:0 frame `data[0]` is the luma plane and `data[1..]` the
        // chroma plane(s); each row spans `linesize[i]` bytes with at least the
        // visible width of valid samples. We read only the visible region, row by
        // row, after checking the pointers are non-null and the strides are wide
        // enough.
        unsafe {
            let frame = &*self.ptr;
            let luma_ptr = frame.data[0] as *const u8;
            if luma_ptr.is_null() {
                return None;
            }
            let luma_stride = frame.linesize[0].max(0) as usize;
            if luma_stride < width {
                return None;
            }
            let mut luma = vec![0u8; width * height];
            for row in 0..height {
                let src = std::slice::from_raw_parts(luma_ptr.add(row * luma_stride), width);
                luma[row * width..row * width + width].copy_from_slice(src);
            }

            let mut chroma = vec![0u8; chroma_width * chroma_height * 2];
            if format == sys::AVPixelFormat_AV_PIX_FMT_YUV420P {
                let u_ptr = frame.data[1] as *const u8;
                let v_ptr = frame.data[2] as *const u8;
                if u_ptr.is_null() || v_ptr.is_null() {
                    return None;
                }
                let u_stride = frame.linesize[1].max(0) as usize;
                let v_stride = frame.linesize[2].max(0) as usize;
                if u_stride < chroma_width || v_stride < chroma_width {
                    return None;
                }
                for row in 0..chroma_height {
                    let u = std::slice::from_raw_parts(u_ptr.add(row * u_stride), chroma_width);
                    let v = std::slice::from_raw_parts(v_ptr.add(row * v_stride), chroma_width);
                    for col in 0..chroma_width {
                        let idx = (row * chroma_width + col) * 2;
                        chroma[idx] = u[col];
                        chroma[idx + 1] = v[col];
                    }
                }
            } else if format == sys::AVPixelFormat_AV_PIX_FMT_NV12 {
                let uv_ptr = frame.data[1] as *const u8;
                if uv_ptr.is_null() {
                    return None;
                }
                let uv_stride = frame.linesize[1].max(0) as usize;
                let row_bytes = chroma_width * 2;
                if uv_stride < row_bytes {
                    return None;
                }
                for row in 0..chroma_height {
                    let src = std::slice::from_raw_parts(uv_ptr.add(row * uv_stride), row_bytes);
                    chroma[row * row_bytes..row * row_bytes + row_bytes].copy_from_slice(src);
                }
            } else {
                return None;
            }

            Some(Nv12Frame {
                width: width as u32,
                height: height as u32,
                luma,
                chroma,
            })
        }
    }

    /// Repack a software-decoded frame into GPU-ready planes. Native 4:2:0 inputs
    /// preserve NV12/P010; other CPU pixel formats fall back through swscale to
    /// 8-bit NV12 so unusual RGB, 4:2:2, or 4:4:4 sources can still play.
    /// Returns `None` for hardware frames or formats swscale cannot convert.
    pub fn to_planar_frame(&self) -> Option<PlanarFrame> {
        let format = self.raw_pixel_format();
        if format == sys::AVPixelFormat_AV_PIX_FMT_YUV420P
            || format == sys::AVPixelFormat_AV_PIX_FMT_NV12
        {
            let nv12 = self.to_nv12()?;
            return Some(PlanarFrame {
                format: PlanarPixelFormat::Nv12,
                width: nv12.width,
                height: nv12.height,
                luma: nv12.luma,
                chroma: nv12.chroma,
            });
        }

        let width = self.width() as usize;
        let height = self.height() as usize;
        if width == 0 || height == 0 {
            return None;
        }
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);

        // SAFETY: `self.ptr` is a valid AVFrame. For 10-bit planar 4:2:0 the planes
        // hold 16-bit little-endian samples spanning `linesize[i]` bytes per row; the
        // helpers read only the visible region after checking pointers and strides.
        unsafe {
            let frame = &*self.ptr;
            if format == sys::AVPixelFormat_AV_PIX_FMT_YUV420P10LE {
                let luma =
                    read_10bit_plane_as_p010(frame.data[0], frame.linesize[0], width, height)?;
                let chroma = read_10bit_chroma_as_p010(
                    frame.data[1],
                    frame.linesize[1],
                    frame.data[2],
                    frame.linesize[2],
                    chroma_width,
                    chroma_height,
                )?;
                Some(PlanarFrame {
                    format: PlanarPixelFormat::P010,
                    width: width as u32,
                    height: height as u32,
                    luma,
                    chroma,
                })
            } else if format == sys::AVPixelFormat_AV_PIX_FMT_P010LE {
                let luma = copy_16bit_rows(frame.data[0], frame.linesize[0], width, height)?;
                let chroma = copy_16bit_rows(
                    frame.data[1],
                    frame.linesize[1],
                    chroma_width * 2,
                    chroma_height,
                )?;
                Some(PlanarFrame {
                    format: PlanarPixelFormat::P010,
                    width: width as u32,
                    height: height as u32,
                    luma,
                    chroma,
                })
            } else {
                self.to_nv12_with_swscale()
            }
        }
    }

    fn to_nv12_with_swscale(&self) -> Option<PlanarFrame> {
        let width = self.width() as usize;
        let height = self.height() as usize;
        if width == 0 || height == 0 {
            return None;
        }
        let width_i32 = i32::try_from(width).ok()?;
        let height_i32 = i32::try_from(height).ok()?;
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        let chroma_row_bytes = chroma_width.checked_mul(2)?;
        let chroma_linesize = i32::try_from(chroma_row_bytes).ok()?;
        let format = self.raw_pixel_format();
        if format < 0
            || format == sys::AVPixelFormat_AV_PIX_FMT_VIDEOTOOLBOX
            || format == sys::AVPixelFormat_AV_PIX_FMT_NONE
        {
            return None;
        }

        unsafe {
            let frame = &*self.ptr;
            let context = sys::sws_getContext(
                width_i32,
                height_i32,
                format,
                width_i32,
                height_i32,
                sys::AVPixelFormat_AV_PIX_FMT_NV12,
                sys::ERIKA_SWS_BILINEAR,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if context.is_null() {
                return None;
            }

            let mut luma = vec![0u8; width * height];
            let mut chroma = vec![0u8; chroma_row_bytes * chroma_height];
            let mut dst_data = [
                luma.as_mut_ptr(),
                chroma.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            ];
            let mut dst_linesize = [width_i32, chroma_linesize, 0, 0];
            let converted = sys::sws_scale(
                context,
                frame.data.as_ptr() as *const *const u8,
                frame.linesize.as_ptr(),
                0,
                height_i32,
                dst_data.as_mut_ptr(),
                dst_linesize.as_mut_ptr(),
            );
            sys::sws_freeContext(context);
            if converted != height_i32 {
                return None;
            }

            Some(PlanarFrame {
                format: PlanarPixelFormat::Nv12,
                width: width as u32,
                height: height as u32,
                luma,
                chroma,
            })
        }
    }
}

/// Tightly packed NV12 planes produced by [`Frame::to_nv12`]: an 8-bit luma plane
/// (`width * height`) and an interleaved Cb/Cr plane at half resolution
/// (`ceil(width / 2) * ceil(height / 2) * 2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12Frame {
    pub width: u32,
    pub height: u32,
    pub luma: Vec<u8>,
    pub chroma: Vec<u8>,
}

/// How a luma plane stores each sample. 10-bit frames share two bytes per
/// sample but not the same bit packing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LumaSampleLayout {
    /// 8-bit `yuv420p` / `nv12`.
    U8,
    /// 10-bit packed LE (`yuv420p10le`): code lives in bits `[9:0]`.
    Packed10,
    /// 10-bit P010 LE: code left-aligned in bits `[15:6]`.
    P010,
}

impl LumaSampleLayout {
    /// Decode one little-endian sample into a 10-bit code in `[0, 1023]`.
    /// Returns `None` for the 8-bit layout.
    pub fn decode_10bit(self, sample: u16) -> Option<u16> {
        match self {
            Self::U8 => None,
            Self::Packed10 => Some(sample & 0x03FF),
            Self::P010 => Some(sample >> 6),
        }
    }
}

/// Strided, zero-copy view of a software frame's luma plane (see
/// [`Frame::luma_plane_view`]).
#[derive(Clone, Copy)]
pub struct LumaPlaneView<'a> {
    data: *const u8,
    stride: usize,
    width: usize,
    height: usize,
    bytes_per_sample: usize,
    layout: LumaSampleLayout,
    _frame: std::marker::PhantomData<&'a Frame>,
}

impl<'a> LumaPlaneView<'a> {
    pub fn width(&self) -> u32 {
        self.width as u32
    }

    pub fn height(&self) -> u32 {
        self.height as u32
    }

    /// 10-bit samples (two bytes per sample)?
    pub fn is_10bit(&self) -> bool {
        self.bytes_per_sample == 2
    }

    pub fn sample_layout(&self) -> LumaSampleLayout {
        self.layout
    }

    /// Test-only view over a packed luma buffer: `stride == width *
    /// bytes_per_sample`. Tests that care about stride padding lay the rows
    /// out manually and point `data` at row 0.
    #[cfg(test)]
    pub(crate) fn from_packed(
        data: &'a [u8],
        width: u32,
        height: u32,
        is_10bit: bool,
    ) -> Option<Self> {
        let layout = if is_10bit {
            LumaSampleLayout::P010
        } else {
            LumaSampleLayout::U8
        };
        Self::from_packed_with_layout(data, width, height, layout)
    }

    /// Test-only view with an explicit sample layout.
    #[cfg(test)]
    pub(crate) fn from_packed_with_layout(
        data: &'a [u8],
        width: u32,
        height: u32,
        layout: LumaSampleLayout,
    ) -> Option<Self> {
        let width = width as usize;
        let height = height as usize;
        let bytes_per_sample = match layout {
            LumaSampleLayout::U8 => 1,
            LumaSampleLayout::Packed10 | LumaSampleLayout::P010 => 2,
        };
        if width == 0 || height == 0 || data.len() < width * height * bytes_per_sample {
            return None;
        }
        let stride = width * bytes_per_sample;
        Some(Self::from_strided_with_layout(
            data,
            stride,
            width as u32,
            height as u32,
            layout,
        ))
    }

    /// Test-only view over rows of `width` samples separated by `stride`
    /// bytes (the decoder-alignment layout `Frame::luma_plane_view` exposes).
    #[cfg(test)]
    pub(crate) fn from_strided(
        data: &'a [u8],
        stride: usize,
        width: u32,
        height: u32,
        is_10bit: bool,
    ) -> Self {
        let layout = if is_10bit {
            LumaSampleLayout::P010
        } else {
            LumaSampleLayout::U8
        };
        Self::from_strided_with_layout(data, stride, width, height, layout)
    }

    /// Test-only view with an explicit sample layout.
    #[cfg(test)]
    pub(crate) fn from_strided_with_layout(
        data: &'a [u8],
        stride: usize,
        width: u32,
        height: u32,
        layout: LumaSampleLayout,
    ) -> Self {
        assert!(data.len() >= stride * height as usize);
        Self {
            data: data.as_ptr(),
            stride,
            width: width as usize,
            height: height as usize,
            bytes_per_sample: match layout {
                LumaSampleLayout::U8 => 1,
                LumaSampleLayout::Packed10 | LumaSampleLayout::P010 => 2,
            },
            layout,
            _frame: std::marker::PhantomData,
        }
    }

    /// Row `index` of the luma plane: `width` samples of 1 or 2 bytes each.
    /// `None` when the index is out of bounds.
    pub fn row(&self, index: usize) -> Option<&'a [u8]> {
        if index >= self.height {
            return None;
        }
        let row_bytes = self.width.checked_mul(self.bytes_per_sample)?;
        let offset = index.checked_mul(self.stride)?.checked_add(row_bytes)?;
        let total = self.stride.checked_mul(self.height)?;
        if offset > total {
            return None;
        }
        // SAFETY: `data` points at a valid luma plane whose rows hold at
        // least `row_bytes` bytes every `stride` bytes (validated when the
        // view was built), and the frame outlives `'a`.
        Some(unsafe { std::slice::from_raw_parts(self.data.add(index * self.stride), row_bytes) })
    }
}

unsafe impl Send for LumaPlaneView<'_> {}

/// GPU upload format for a repacked planar frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanarPixelFormat {
    /// 8-bit NV12: R8 luma + interleaved Rg8 chroma.
    Nv12,
    /// 10-bit P010 (values MSB-aligned in 16-bit LE): R16 luma + Rg16 chroma.
    P010,
}

/// Tightly packed planar frame produced by [`Frame::to_planar_frame`]. `luma` and
/// `chroma` hold raw bytes: 1 byte/sample for [`PlanarPixelFormat::Nv12`], 2 bytes
/// (little-endian) for [`PlanarPixelFormat::P010`]. `chroma` is interleaved Cb/Cr at
/// half resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanarFrame {
    pub format: PlanarPixelFormat,
    pub width: u32,
    pub height: u32,
    pub luma: Vec<u8>,
    pub chroma: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanarFrameConversionError {
    #[error("P010 to NV12 conversion requires P010 input, got {format:?}")]
    ExpectedP010 { format: PlanarPixelFormat },
    #[error("P010 {plane} plane is {actual} bytes, expected {expected} for {width}x{height}")]
    InvalidP010PlaneSize {
        plane: &'static str,
        actual: usize,
        expected: usize,
        width: u32,
        height: u32,
    },
    #[error("P010 plane dimensions overflow host address space for {width}x{height}")]
    DimensionsOverflow { width: u32, height: u32 },
}

impl PlanarFrame {
    /// Down-converts tightly packed P010 into tightly packed NV12 on the CPU.
    ///
    /// P010 stores each 10-bit code in the high bits of a little-endian 16-bit
    /// word. Rounding the 10-bit code by two bits preserves the conventional
    /// limited-range anchors exactly (`64 -> 16`, `940 -> 235`, `960 -> 240`)
    /// while also mapping full-range `1023` to `255`.
    pub fn downconvert_p010_to_nv12(self) -> std::result::Result<Self, PlanarFrameConversionError> {
        if self.format != PlanarPixelFormat::P010 {
            return Err(PlanarFrameConversionError::ExpectedP010 {
                format: self.format,
            });
        }

        let luma_samples = (self.width as usize)
            .checked_mul(self.height as usize)
            .ok_or(PlanarFrameConversionError::DimensionsOverflow {
                width: self.width,
                height: self.height,
            })?;
        let chroma_samples = (self.width.div_ceil(2) as usize)
            .checked_mul(self.height.div_ceil(2) as usize)
            .and_then(|samples| samples.checked_mul(2))
            .ok_or(PlanarFrameConversionError::DimensionsOverflow {
                width: self.width,
                height: self.height,
            })?;
        let expected_luma =
            luma_samples
                .checked_mul(2)
                .ok_or(PlanarFrameConversionError::DimensionsOverflow {
                    width: self.width,
                    height: self.height,
                })?;
        let expected_chroma = chroma_samples.checked_mul(2).ok_or(
            PlanarFrameConversionError::DimensionsOverflow {
                width: self.width,
                height: self.height,
            },
        )?;
        if self.luma.len() != expected_luma {
            return Err(PlanarFrameConversionError::InvalidP010PlaneSize {
                plane: "luma",
                actual: self.luma.len(),
                expected: expected_luma,
                width: self.width,
                height: self.height,
            });
        }
        if self.chroma.len() != expected_chroma {
            return Err(PlanarFrameConversionError::InvalidP010PlaneSize {
                plane: "chroma",
                actual: self.chroma.len(),
                expected: expected_chroma,
                width: self.width,
                height: self.height,
            });
        }

        Ok(Self {
            format: PlanarPixelFormat::Nv12,
            width: self.width,
            height: self.height,
            luma: downconvert_p010_samples_to_u8(&self.luma),
            chroma: downconvert_p010_samples_to_u8(&self.chroma),
        })
    }
}

fn downconvert_p010_samples_to_u8(source: &[u8]) -> Vec<u8> {
    source
        .chunks_exact(2)
        .map(|bytes| {
            let code = u16::from_le_bytes([bytes[0], bytes[1]]) >> 6;
            (((u32::from(code) + 2) >> 2).min(u32::from(u8::MAX))) as u8
        })
        .collect()
}

/// Reads a 10-bit little-endian plane and rewrites it as P010 (value `<< 6`, so the
/// 10 bits occupy the high bits of each 16-bit sample), tightly packed.
///
/// # Safety
/// `ptr` must point to a plane with at least `stride` bytes per row for `height`
/// rows and at least `width` 16-bit samples of valid data per row.
unsafe fn read_10bit_plane_as_p010(
    ptr: *mut u8,
    stride: i32,
    width: usize,
    height: usize,
) -> Option<Vec<u8>> {
    let ptr = ptr as *const u8;
    if ptr.is_null() {
        return None;
    }
    let stride = stride.max(0) as usize;
    if stride < width * 2 {
        return None;
    }
    let mut out = Vec::with_capacity(width * height * 2);
    for row in 0..height {
        let row_ptr = unsafe { ptr.add(row * stride) };
        for col in 0..width {
            let lo = unsafe { *row_ptr.add(col * 2) };
            let hi = unsafe { *row_ptr.add(col * 2 + 1) };
            let sample = (u16::from_le_bytes([lo, hi]) & 0x03FF) << 6;
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }
    Some(out)
}

/// Interleaves two 10-bit LE chroma planes (Cb, Cr) into P010 (`value << 6`) order.
///
/// # Safety
/// `u_ptr`/`v_ptr` must each point to at least `cw` 16-bit samples per row for `ch`
/// rows, spanning the given strides.
unsafe fn read_10bit_chroma_as_p010(
    u_ptr: *mut u8,
    u_stride: i32,
    v_ptr: *mut u8,
    v_stride: i32,
    cw: usize,
    ch: usize,
) -> Option<Vec<u8>> {
    let u_ptr = u_ptr as *const u8;
    let v_ptr = v_ptr as *const u8;
    if u_ptr.is_null() || v_ptr.is_null() {
        return None;
    }
    let u_stride = u_stride.max(0) as usize;
    let v_stride = v_stride.max(0) as usize;
    if u_stride < cw * 2 || v_stride < cw * 2 {
        return None;
    }
    let mut out = Vec::with_capacity(cw * ch * 4);
    for row in 0..ch {
        let u_row = unsafe { u_ptr.add(row * u_stride) };
        let v_row = unsafe { v_ptr.add(row * v_stride) };
        for col in 0..cw {
            let u = (u16::from_le_bytes([unsafe { *u_row.add(col * 2) }, unsafe {
                *u_row.add(col * 2 + 1)
            }]) & 0x03FF)
                << 6;
            let v = (u16::from_le_bytes([unsafe { *v_row.add(col * 2) }, unsafe {
                *v_row.add(col * 2 + 1)
            }]) & 0x03FF)
                << 6;
            out.extend_from_slice(&u.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    Some(out)
}

/// Copies `samples_per_row` 16-bit samples per row for `rows` rows, resolving stride.
///
/// # Safety
/// `ptr` must point to at least `stride` bytes per row for `rows` rows with at least
/// `samples_per_row` 16-bit samples of valid data per row.
unsafe fn copy_16bit_rows(
    ptr: *mut u8,
    stride: i32,
    samples_per_row: usize,
    rows: usize,
) -> Option<Vec<u8>> {
    let ptr = ptr as *const u8;
    if ptr.is_null() {
        return None;
    }
    let stride = stride.max(0) as usize;
    let row_bytes = samples_per_row * 2;
    if stride < row_bytes {
        return None;
    }
    let mut out = Vec::with_capacity(row_bytes * rows);
    for row in 0..rows {
        let src = unsafe { std::slice::from_raw_parts(ptr.add(row * stride), row_bytes) };
        out.extend_from_slice(src);
    }
    Some(out)
}

impl AudioResampler {
    pub fn new_from_frame(frame: &Frame, output_format: PcmFormat) -> Result<Self> {
        if !frame.is_audio() {
            return Err(FfmpegError::ExpectedAudioFrame);
        }
        let output_layout = ChannelLayout::default_for_channels(output_format.channels)?;
        let context = unsafe { allocate_swr_context(frame, output_format, &output_layout)? };
        check(unsafe { sys::swr_init(context) }, "swr_init")?;
        Ok(Self {
            context,
            output_format,
            next_output_pts: None,
            output_layout,
        })
    }

    pub fn output_format(&self) -> PcmFormat {
        self.output_format
    }

    pub fn convert(&mut self, frame: &Frame) -> Result<PcmAudioFrame> {
        if !frame.is_audio() {
            return Err(FfmpegError::ExpectedAudioFrame);
        }
        let input_pts = frame.pts().and_then(|pts| pts.as_duration());
        let (pts, reanchored) = select_resampler_output_pts(self.next_output_pts, input_pts);
        if reanchored {
            let discarded_delay_frames =
                unsafe { sys::swr_get_delay(self.context, self.output_format.sample_rate as i64) }
                    .max(0);
            unsafe { sys::swr_close(self.context) };
            check(
                unsafe { sys::swr_init(self.context) },
                "swr_init(discontinuity)",
            )?;
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "audio_resampler_timeline",
                    "stage": "discontinuity_reanchor",
                    "expectedSeconds": self.next_output_pts.map(|pts| pts.as_secs_f64()),
                    "inputSeconds": input_pts.map(|pts| pts.as_secs_f64()),
                    "discardedDelayFrames": discarded_delay_frames,
                    "toleranceSeconds": AUDIO_RESAMPLER_PTS_DISCONTINUITY_TOLERANCE.as_secs_f64(),
                })
                .to_string(),
            );
        }
        let input_samples = frame.sample_count().min(i32::MAX as usize) as i32;
        let delay = unsafe { sys::swr_get_delay(self.context, frame.sample_rate() as i64) }.max(0);
        let output_capacity = unsafe {
            sys::av_rescale_rnd(
                delay + input_samples as i64,
                self.output_format.sample_rate as i64,
                frame.sample_rate() as i64,
                sys::AVRounding_AV_ROUND_UP,
            )
        }
        .max(1)
        .min(i32::MAX as i64) as i32;
        let channels = self.output_format.channels.max(1) as usize;
        let mut samples = vec![0.0f32; output_capacity as usize * channels];
        let mut output_planes = [samples.as_mut_ptr().cast::<u8>()];
        let input = unsafe { (*frame.ptr).extended_data as *mut *const u8 };
        if input.is_null() {
            return Err(FfmpegError::NullPointer("AVFrame.extended_data"));
        }
        let converted = unsafe {
            sys::swr_convert(
                self.context,
                output_planes.as_mut_ptr(),
                output_capacity,
                input,
                input_samples,
            )
        };
        check(converted, "swr_convert")?;
        let frames = converted.max(0) as usize;
        samples.truncate(frames * channels);
        self.next_output_pts = pcm_end_pts(pts, frames, self.output_format.sample_rate);
        Ok(PcmAudioFrame {
            format: self.output_format,
            pts,
            frames,
            samples,
        })
    }

    pub fn drain(&mut self) -> Result<Option<PcmAudioFrame>> {
        let output_capacity =
            unsafe { sys::swr_get_delay(self.context, self.output_format.sample_rate as i64) }
                .max(0)
                .min(i32::MAX as i64) as i32;
        if output_capacity == 0 {
            return Ok(None);
        }

        let channels = self.output_format.channels.max(1) as usize;
        let mut samples = vec![0.0f32; output_capacity as usize * channels];
        let mut output_planes = [samples.as_mut_ptr().cast::<u8>()];
        let converted = unsafe {
            sys::swr_convert(
                self.context,
                output_planes.as_mut_ptr(),
                output_capacity,
                ptr::null_mut(),
                0,
            )
        };
        check(converted, "swr_convert(drain)")?;
        let frames = converted.max(0) as usize;
        if frames == 0 {
            return Ok(None);
        }
        samples.truncate(frames * channels);
        let pts = self.next_output_pts;
        self.next_output_pts = pcm_end_pts(pts, frames, self.output_format.sample_rate);
        Ok(Some(PcmAudioFrame {
            format: self.output_format,
            pts,
            frames,
            samples,
        }))
    }
}

fn pcm_end_pts(pts: Option<Duration>, frames: usize, sample_rate: u32) -> Option<Duration> {
    let pts = pts?;
    if frames == 0 || sample_rate == 0 {
        return Some(pts);
    }
    Some(pts.saturating_add(Duration::from_secs_f64(frames as f64 / sample_rate as f64)))
}

fn select_resampler_output_pts(
    expected: Option<Duration>,
    input: Option<Duration>,
) -> (Option<Duration>, bool) {
    let Some(expected) = expected else {
        return (input, false);
    };
    let Some(input) = input else {
        return (Some(expected), false);
    };
    let drift = if expected >= input {
        expected - input
    } else {
        input - expected
    };
    if drift > AUDIO_RESAMPLER_PTS_DISCONTINUITY_TOLERANCE {
        (Some(input), true)
    } else {
        (Some(expected), false)
    }
}

impl Drop for AudioResampler {
    fn drop(&mut self) {
        unsafe { sys::swr_free(&mut self.context) };
    }
}

#[cfg(not(erika_ffmpeg_legacy_channel_layout))]
unsafe fn allocate_swr_context(
    frame: &Frame,
    output_format: PcmFormat,
    output_layout: &ChannelLayout,
) -> Result<*mut sys::SwrContext> {
    let mut context = ptr::null_mut();
    check(
        unsafe {
            sys::swr_alloc_set_opts2(
                &mut context,
                output_layout.as_ptr(),
                sys::AVSampleFormat_AV_SAMPLE_FMT_FLT,
                output_format.sample_rate as i32,
                &(*frame.ptr).ch_layout,
                frame.raw_sample_format(),
                frame.sample_rate() as i32,
                0,
                ptr::null_mut(),
            )
        },
        "swr_alloc_set_opts2",
    )?;
    if context.is_null() {
        return Err(FfmpegError::NullPointer("swr_alloc_set_opts2"));
    }
    unsafe {
        configure_downmix_normalization(context, frame.channel_count(), output_format.channels)
    };
    Ok(context)
}

#[cfg(erika_ffmpeg_legacy_channel_layout)]
unsafe fn allocate_swr_context(
    frame: &Frame,
    output_format: PcmFormat,
    output_layout: &ChannelLayout,
) -> Result<*mut sys::SwrContext> {
    let input_layout = unsafe { legacy_frame_channel_layout(frame) };
    let context = unsafe {
        sys::swr_alloc_set_opts(
            ptr::null_mut(),
            output_layout.as_raw(),
            sys::AVSampleFormat_AV_SAMPLE_FMT_FLT,
            output_format.sample_rate as i32,
            input_layout,
            frame.raw_sample_format(),
            frame.sample_rate() as i32,
            0,
            ptr::null_mut(),
        )
    };
    if context.is_null() {
        return Err(FfmpegError::NullPointer("swr_alloc_set_opts"));
    }
    unsafe {
        configure_downmix_normalization(context, frame.channel_count(), output_format.channels)
    };
    Ok(context)
}

// `libavutil/opt.h` is not pulled in by the erika_ffmpeg_sys wrapper header, so
// bindgen does not emit this binding even though `av_.*` is allowlisted. The
// symbol is exported by the bundled static libavutil, so declare it directly.
unsafe extern "C" {
    fn av_opt_set_double(
        obj: *mut c_void,
        name: *const c_char,
        val: f64,
        search_flags: c_int,
    ) -> c_int;
}

/// Returns whether a swr context that maps `input_channels` down to
/// `output_channels` needs its rematrix normalized to avoid clipping.
///
/// FFmpeg's default downmix matrices sum to more than unity per output channel
/// (e.g. 5.1 -> stereo: FL_out = FL + 0.707*FC + 0.707*BL, coefficient sum
/// ~2.41), so full-scale f32 input exceeds +/-1.0 and hard-clips downstream.
/// Upmix and equal-channel conversions keep the default behavior so stereo
/// pass-through loudness is unaffected.
fn downmix_requires_normalization(input_channels: u32, output_channels: u32) -> bool {
    output_channels < input_channels
}

/// Coefficient-sum ceiling handed to swresample's `rematrix_maxval` when the
/// channel count is reduced.
///
/// swresample normalizes the downmix matrix so no output row sums above this
/// value. The obvious choice, 1.0, guarantees that even fully correlated
/// full-scale input cannot clip -- but it pays for that guarantee everywhere:
/// the 5.1 -> stereo row sums to ~2.41, so every surround track is scaled by
/// 0.41 (-7.7 dB) whether or not it was ever going to clip.
///
/// That worst case assumes the summed channels are identical signals. Real
/// multichannel masters are largely decorrelated, so they add as power rather
/// than amplitude: sqrt(1^2 + 0.707^2 + 0.707^2) = sqrt(2). Normalizing to the
/// power sum instead keeps realistic content below full scale while recovering
/// ~3 dB over the amplitude-sum bound. Measured 5.1 -> stereo peaks at this
/// setting: typical mixed levels 0.74, music-heavy 0.82, dialogue-heavy 0.57.
///
/// Only a synthetic signal holding every channel at digital full scale still
/// exceeds unity here; catching that too would need a limiter on the output
/// stage rather than a static matrix scale.
const DOWNMIX_REMATRIX_MAXVAL: f64 = std::f64::consts::SQRT_2;

/// Caps the downmix matrix at [`DOWNMIX_REMATRIX_MAXVAL`] so surround content
/// stops clipping on stereo output without being needlessly attenuated. Only
/// applies when channels are reduced; failures are traced but non-fatal (the
/// context falls back to the default un-normalized matrix).
///
/// # Safety
/// `context` must point to a valid, allocated `SwrContext` that has not been
/// initialized with `swr_init` yet.
unsafe fn configure_downmix_normalization(
    context: *mut sys::SwrContext,
    input_channels: u32,
    output_channels: u32,
) {
    if !downmix_requires_normalization(input_channels, output_channels) {
        return;
    }
    let code = unsafe {
        av_opt_set_double(
            context.cast::<c_void>(),
            c"rematrix_maxval".as_ptr(),
            DOWNMIX_REMATRIX_MAXVAL,
            0,
        )
    };
    if code < 0 {
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "audio_resampler_downmix",
                "stage": "rematrix_maxval_failed",
                "inputChannels": input_channels,
                "outputChannels": output_channels,
                "code": code,
                "message": error_string(code),
            })
            .to_string(),
        );
    }
}

#[cfg(erika_ffmpeg_legacy_channel_layout)]
unsafe fn legacy_frame_channel_layout(frame: &Frame) -> i64 {
    let layout = unsafe { sys::av_frame_get_channel_layout(frame.ptr) };
    if layout != 0 {
        layout
    } else {
        let channels = frame.channel_count().min(i32::MAX as u32) as i32;
        unsafe { sys::av_get_default_channel_layout(channels) }
    }
}

#[cfg(not(erika_ffmpeg_legacy_channel_layout))]
struct ChannelLayout {
    raw: sys::AVChannelLayout,
}

#[cfg(not(erika_ffmpeg_legacy_channel_layout))]
impl ChannelLayout {
    fn default_for_channels(channels: u32) -> Result<Self> {
        let channels = channels.min(i32::MAX as u32) as i32;
        let mut raw = sys::AVChannelLayout::default();
        unsafe { sys::av_channel_layout_default(&mut raw, channels) };
        if unsafe { sys::av_channel_layout_check(&raw) } == 0 {
            unsafe { sys::av_channel_layout_uninit(&mut raw) };
            return Err(FfmpegError::Api {
                operation: "av_channel_layout_default",
                code: -1,
                message: "invalid channel layout".to_string(),
            });
        }
        Ok(Self { raw })
    }

    fn as_ptr(&self) -> *const sys::AVChannelLayout {
        &self.raw
    }
}

#[cfg(not(erika_ffmpeg_legacy_channel_layout))]
impl Drop for ChannelLayout {
    fn drop(&mut self) {
        unsafe { sys::av_channel_layout_uninit(&mut self.raw) };
    }
}

#[cfg(erika_ffmpeg_legacy_channel_layout)]
struct ChannelLayout {
    raw: i64,
}

#[cfg(erika_ffmpeg_legacy_channel_layout)]
impl ChannelLayout {
    fn default_for_channels(channels: u32) -> Result<Self> {
        let channels = channels.min(i32::MAX as u32) as i32;
        let raw = unsafe { sys::av_get_default_channel_layout(channels) };
        if raw == 0 {
            return Err(FfmpegError::Api {
                operation: "av_get_default_channel_layout",
                code: -1,
                message: "invalid channel layout".to_string(),
            });
        }
        Ok(Self { raw })
    }

    fn as_raw(&self) -> i64 {
        self.raw
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { sys::av_frame_free(&mut self.ptr) };
    }
}

pub struct Packet {
    ptr: *mut sys::AVPacket,
    time_base: Option<TimeBase>,
}

unsafe impl Send for Packet {}

impl Packet {
    fn alloc() -> Result<Self> {
        let ptr = unsafe { sys::av_packet_alloc() };
        if ptr.is_null() {
            return Err(FfmpegError::NullPointer("av_packet_alloc"));
        }
        Ok(Self {
            ptr,
            time_base: None,
        })
    }

    fn normalize_timeline(&mut self, timeline_origin_micros: i64) {
        let Some(time_base) = self.time_base.filter(|_| timeline_origin_micros != 0) else {
            return;
        };
        let origin = rescale_microseconds_to_time_base(timeline_origin_micros, time_base);
        unsafe {
            if timestamp_value((*self.ptr).pts).is_some() {
                (*self.ptr).pts = (*self.ptr).pts.saturating_sub(origin);
            }
            if timestamp_value((*self.ptr).dts).is_some() {
                (*self.ptr).dts = (*self.ptr).dts.saturating_sub(origin);
            }
        }
    }

    pub fn as_ptr(&self) -> *const sys::AVPacket {
        self.ptr
    }

    pub fn as_mut_ptr(&mut self) -> *mut sys::AVPacket {
        self.ptr
    }

    pub fn stream_index(&self) -> i32 {
        unsafe { (*self.ptr).stream_index }
    }

    /// 改写流索引。DASH 双流中两个输入内部流 index 都是 0 起，需平移到
    /// 全局 index（video=0，audio=1）后才能与解码器/路由对齐。
    pub fn set_stream_index(&mut self, stream_index: i32) {
        unsafe { (*self.ptr).stream_index = stream_index };
    }

    pub fn size(&self) -> usize {
        unsafe { (*self.ptr).size.max(0) as usize }
    }

    pub fn data(&self) -> &[u8] {
        let size = self.size();
        let data = unsafe { (*self.ptr).data };
        if data.is_null() || size == 0 {
            return &[];
        }
        unsafe { slice::from_raw_parts(data, size) }
    }

    pub fn flags(&self) -> PacketFlags {
        PacketFlags {
            bits: unsafe { (*self.ptr).flags },
        }
    }

    pub fn is_key(&self) -> bool {
        self.flags().is_key()
    }

    pub fn pos(&self) -> Option<i64> {
        let pos = unsafe { (*self.ptr).pos };
        if pos < 0 { None } else { Some(pos) }
    }

    pub fn raw_pts(&self) -> Option<i64> {
        timestamp_value(unsafe { (*self.ptr).pts })
    }

    pub fn raw_dts(&self) -> Option<i64> {
        timestamp_value(unsafe { (*self.ptr).dts })
    }

    pub fn raw_duration(&self) -> Option<i64> {
        let duration = unsafe { (*self.ptr).duration };
        if duration <= 0 { None } else { Some(duration) }
    }

    pub fn time_base(&self) -> Option<TimeBase> {
        self.time_base
    }

    pub fn pts(&self) -> Option<PacketTimestamp> {
        Some(PacketTimestamp {
            raw: self.raw_pts()?,
            time_base: self.time_base?,
        })
    }

    pub fn dts(&self) -> Option<PacketTimestamp> {
        Some(PacketTimestamp {
            raw: self.raw_dts()?,
            time_base: self.time_base?,
        })
    }

    pub fn duration_seconds(&self) -> Option<f64> {
        Some(self.time_base?.seconds_from_timestamp(self.raw_duration()?))
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        unsafe { sys::av_packet_free(&mut self.ptr) };
    }
}

pub fn version() -> String {
    unsafe {
        let ptr = sys::av_version_info();
        if ptr.is_null() {
            return "unknown".to_string();
        }
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

pub fn probe_path(path: impl AsRef<Path>) -> Result<MediaProbe> {
    let uri = path.as_ref().to_string_lossy().into_owned();
    probe_uri(&uri)
}

pub fn probe_uri(uri: &str) -> Result<MediaProbe> {
    Ok(Demuxer::open_uri(uri)?.probe().clone())
}

struct FormatContext {
    ptr: *mut sys::AVFormatContext,
    avio: Option<Box<CustomAvio>>,
}

impl FormatContext {
    fn new(ptr: *mut sys::AVFormatContext) -> Result<Self> {
        if ptr.is_null() {
            return Err(FfmpegError::NullPointer("avformat_open_input"));
        }
        Ok(Self { ptr, avio: None })
    }

    fn new_with_custom_io(ptr: *mut sys::AVFormatContext, avio: Box<CustomAvio>) -> Result<Self> {
        if ptr.is_null() {
            return Err(FfmpegError::NullPointer("avformat_open_input"));
        }
        Ok(Self {
            ptr,
            avio: Some(avio),
        })
    }

    fn as_ptr(&self) -> *const sys::AVFormatContext {
        self.ptr
    }

    fn as_mut_ptr(&mut self) -> *mut sys::AVFormatContext {
        self.ptr
    }

    /// 诊断读数 `(已消费字节总量, 读过的最大偏移)`；非自定义 AVIO（本地
    /// 路径 `open_format_context`）返回 None。
    fn avio_diag(&self) -> Option<(u64, u64)> {
        self.avio
            .as_ref()
            .map(|avio| (avio.bytes_read, avio.max_offset))
    }
}

impl Drop for FormatContext {
    fn drop(&mut self) {
        unsafe { sys::avformat_close_input(&mut self.ptr) };
        let _ = self.avio.take();
    }
}

// SAFETY: `FormatContext` 独占持有它的 `AVFormatContext` 与 AVIO 上下文；把它
// 整体移动到另一个线程不会造成对同一上下文的共享访问（Rust 的所有权保证独占，
// 移动即转移，不存在别名）。FFmpeg 的格式上下文没有线程亲和性要求（不像 GUI
// 对象），同 crate 的 `Demuxer` 已经用同样的论证声明了 `unsafe impl Send`。
// 本 impl 的唯一用途是把 DASH 音频输入从它的 open 工作线程交回调用线程
// （见 `Demuxer::open_video_audio` 的并行 open）。
unsafe impl Send for FormatContext {}

/// 一个输入在并行工作线程里完成 open + find_stream_info 后的产物。
struct OpenedInput {
    context: FormatContext,
    /// `avformat_open_input` 自身耗时（含格式探测读取）。
    open_ms: f64,
    /// 该阶段结束时该输入累计消费的字节（格式探测读完的量）。
    bytes_after_open: u64,
    /// `avformat_find_stream_info` 耗时（含为判定流信息的追加读取）。
    probe_ms: f64,
    stream_count: usize,
}

struct CustomAvio {
    context: *mut sys::AVIOContext,
    source: Box<dyn MediaSource>,
    offset: u64,
    last_error: Option<String>,
    /// 诊断计数：本输入自创建起从源消费的字节总量（含 seek 后重读的区段）。
    /// 存在的理由：`avformat_open_input` / `avformat_find_stream_info` 都不是
    /// "看一眼文件头就返回"——它们会按 `probesize`/`analyzeduration` 读入
    /// 可观的流数据才能判定流信息。宿主侧只看到"网络在下载、时长还是 0"，
    /// 无法区分「网络慢」与「内核在解析」；本计数把后者变成可读数字
    /// （见 `Demuxer::open_video_audio` / `PlaybackSession` 的 open 阶段打点）。
    bytes_read: u64,
    /// 诊断计数：本输入读到过的最大文件偏移。能区分"顺序读完 N 字节"与
    /// "跳到大偏移再读"（后者在 HTTP 源上是另一次 Range 往返）。
    max_offset: u64,
}

fn normalize_avio_seek_whence(whence: c_int) -> c_int {
    whence & !(sys::AVSEEK_FORCE as c_int)
}

impl CustomAvio {
    const BUFFER_SIZE: usize = 64 * 1024;

    fn new(source: Box<dyn MediaSource>) -> Result<Box<Self>> {
        let buffer = unsafe { sys::av_malloc(Self::BUFFER_SIZE) }.cast::<u8>();
        if buffer.is_null() {
            return Err(FfmpegError::NullPointer("av_malloc(avio buffer)"));
        }

        let mut avio = Box::new(Self {
            context: ptr::null_mut(),
            source,
            offset: 0,
            last_error: None,
            bytes_read: 0,
            max_offset: 0,
        });
        let context = unsafe {
            sys::avio_alloc_context(
                buffer,
                Self::BUFFER_SIZE as c_int,
                0,
                (&mut *avio) as *mut CustomAvio as *mut c_void,
                Some(custom_avio_read_packet),
                None,
                Some(custom_avio_seek),
            )
        };
        if context.is_null() {
            unsafe { sys::av_free(buffer.cast::<c_void>()) };
            return Err(FfmpegError::NullPointer("avio_alloc_context"));
        }
        if !avio.source.seekable() {
            // 直播流等不可 seek 源：avio_alloc_context 因传入了 seek 回调而
            // 默认置 AVIO_SEEKABLE_NORMAL，这里清掉。flv demuxer 据 seekable
            // 位进入 live 模式（顺序读、不建索引、不 seek 探测 duration）。
            // 注意 avio_seek 本身不检查 seekable 位，真正的防跳转由 seek()
            // 对不可 seek 源的拒绝 + read_range 的 offset 校验兜底。
            unsafe {
                (*context).seekable = 0;
            }
        }
        avio.context = context;
        Ok(avio)
    }

    fn context(&self) -> *mut sys::AVIOContext {
        self.context
    }

    fn read_packet(&mut self, buffer: *mut u8, buffer_size: c_int) -> c_int {
        if buffer.is_null() || buffer_size <= 0 {
            return av_error(EINVAL);
        }
        let length = buffer_size as u64;
        match self.source.read_range(ByteRange {
            start: self.offset,
            length: Some(length),
        }) {
            Ok(bytes) if bytes.is_empty() => {
                // A source that answered is not the cause of any later failure:
                // a sticky `last_error` would be appended to an unrelated one.
                self.last_error = None;
                AVERROR_EOF
            }
            Ok(bytes) => {
                self.last_error = None;
                let copy_len = bytes.len().min(buffer_size as usize);
                unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, copy_len) };
                self.offset = self.offset.saturating_add(copy_len as u64);
                self.bytes_read = self.bytes_read.saturating_add(copy_len as u64);
                self.max_offset = self.max_offset.max(self.offset);
                copy_len as c_int
            }
            Err(error) => {
                self.last_error = Some(error.to_string());
                crate::trace::log(format!(
                    "Erika custom AVIO read failed at offset {} for {} bytes: {error}",
                    self.offset, length
                ));
                av_error(EIO)
            }
        }
    }

    fn seek(&mut self, offset: i64, whence: c_int) -> i64 {
        // FFmpeg may OR AVSEEK_FORCE into ordinary SEEK_SET/SEEK_CUR/SEEK_END
        // requests. It is a hint to the protocol layer rather than a distinct
        // seek origin, so custom AVIO implementations must ignore it.
        let whence = normalize_avio_seek_whence(whence);
        if whence == sys::AVSEEK_SIZE as c_int {
            return match self.source.len() {
                Ok(Some(length)) => length.min(i64::MAX as u64) as i64,
                Ok(None) => av_error(ESPIPE) as i64,
                Err(error) => {
                    self.last_error = Some(error.to_string());
                    av_error(EIO) as i64
                }
            };
        }

        let target = match whence {
            SEEK_SET => offset,
            SEEK_CUR => self.offset as i64 + offset,
            SEEK_END => match self.source.len() {
                Ok(Some(length)) => length.min(i64::MAX as u64) as i64 + offset,
                Ok(None) => return av_error(ESPIPE) as i64,
                Err(error) => {
                    self.last_error = Some(error.to_string());
                    return av_error(EIO) as i64;
                }
            },
            _ => {
                let error = format!("unsupported seek origin {whence:#x} at offset {offset}");
                self.last_error = Some(error.clone());
                crate::trace::log(format!("Erika custom AVIO rejected {error}"));
                return av_error(EINVAL) as i64;
            }
        };
        if target < 0 {
            return av_error(EINVAL) as i64;
        }
        if !self.source.seekable() && target as u64 != self.offset {
            // 直播流等不可 seek 源：只放行原地 seek（avformat 可能用它查询
            // 当前位置）；任何真正的跳转都拒绝。avio_seek 不检查 seekable
            // 位，静默改 offset 会让下一次 read_range 的校验报出令人费解的
            // EIO，这里在 seek 阶段就明确失败并记录原因。
            let error = format!("live source is not seekable (seek target {target})");
            self.last_error = Some(error.clone());
            crate::trace::log(format!("Erika custom AVIO rejected {error}"));
            return av_error(ESPIPE) as i64;
        }
        if whence == SEEK_SET {
            // 窗口外 seek 的目标预取提示：avformat 即将从 target 开始读
            // （seek 到目标关键帧），把目标窗口提前拉入后台多段预取，使
            // seek 后 read 走 take_prefetch（join 小段）而非同步单请求。
            // HttpRangeSource 内部判断 target 是否超出缓存窗口并防抖。
            self.source.prefetch_hint(target as u64);
        }
        if crate::trace::enabled() && whence != sys::AVSEEK_SIZE as c_int {
            crate::trace::log(format!(
                "[erika-avio] stage=seek whence={whence} target={target} prev_offset={}",
                self.offset
            ));
        }
        self.offset = target as u64;
        self.last_error = None;
        self.offset.min(i64::MAX as u64) as i64
    }
}

impl Drop for CustomAvio {
    fn drop(&mut self) {
        if !self.context.is_null() {
            let buffer = unsafe { (*self.context).buffer };
            let mut context = self.context;
            unsafe { sys::avio_context_free(&mut context) };
            if !buffer.is_null() {
                unsafe { sys::av_free(buffer.cast::<c_void>()) };
            }
            self.context = ptr::null_mut();
        }
    }
}

unsafe extern "C" fn custom_avio_read_packet(
    opaque: *mut c_void,
    buffer: *mut u8,
    buffer_size: c_int,
) -> c_int {
    if opaque.is_null() {
        return av_error(EINVAL);
    }
    unsafe { (&mut *(opaque.cast::<CustomAvio>())).read_packet(buffer, buffer_size) }
}

unsafe extern "C" fn custom_avio_seek(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    if opaque.is_null() {
        return av_error(EINVAL) as i64;
    }
    unsafe { (&mut *(opaque.cast::<CustomAvio>())).seek(offset, whence) }
}

fn open_format_context(uri: &str) -> Result<FormatContext> {
    let input = CString::new(uri).map_err(|_| FfmpegError::InteriorNul)?;
    let mut format_context = ptr::null_mut();
    check(
        unsafe {
            sys::avformat_open_input(
                &mut format_context,
                input.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        },
        "avformat_open_input",
    )?;
    FormatContext::new(format_context)
}

/// 自定义 AVIO 输入的可选 FFmpeg 选项（诊断/实验开关；**默认不设置 =
/// 与从前完全一致，不做任何静默降级**）。
///
/// 存在的理由：`avformat_open_input` + `avformat_find_stream_info` 会按
/// `probesize`（FFmpeg 默认 5MB）与 `analyzeduration`（默认 5 秒）读入流数据
/// 才能判定流信息。对 B 站 m4s 的本地实测（`tests/open_stages_diag.rs`）是
/// 每个输入约 0.9MB——在 300KB/s 的链路上一读就是数秒，这正是宿主侧
/// 「网络在下载、`duration` 还是 0」那段 `Opening` 的主要构成。
///
/// 这两个环境变量让「读取量是否随解析窗口收紧」变成可测的事实：
/// - `ERIKA_OPEN_ANALYZEDURATION_US`：微秒，对应 demuxer 的 `analyzeduration`；
/// - `ERIKA_OPEN_PROBESIZE`：字节，对应 `probesize`。
///
/// 只做实验/诊断用：调小它们会改变「解析多久的流信息」这一契约，是否仍然
/// 拿得到正确的 `duration`/轨道必须由实测定夺（见诊断测试的输出对照）。
fn open_input_options() -> Result<*mut sys::AVDictionary> {
    let mut options: *mut sys::AVDictionary = ptr::null_mut();
    for (env_name, key) in [
        ("ERIKA_OPEN_ANALYZEDURATION_US", c"analyzeduration"),
        ("ERIKA_OPEN_PROBESIZE", c"probesize"),
    ] {
        let Some(value) = std::env::var(env_name).ok().filter(|v| !v.is_empty()) else {
            continue;
        };
        let value = CString::new(value).map_err(|_| FfmpegError::InteriorNul)?;
        if let Err(error) = check(
            unsafe { sys::av_dict_set(&mut options, key.as_ptr(), value.as_ptr(), 0) },
            "av_dict_set(open_input_options)",
        ) {
            unsafe { sys::av_dict_free(&mut options) };
            return Err(error);
        }
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "erika_open_input_option",
                "name": env_name,
                "value": value.to_string_lossy(),
            })
            .to_string(),
        );
    }
    Ok(options)
}

fn open_source_format_context(source: Box<dyn MediaSource>) -> Result<FormatContext> {
    let uri = CString::new(source.uri()).map_err(|_| FfmpegError::InteriorNul)?;
    let avio = CustomAvio::new(source)?;
    let format_context = unsafe { sys::avformat_alloc_context() };
    if format_context.is_null() {
        return Err(FfmpegError::NullPointer("avformat_alloc_context"));
    }
    unsafe {
        (*format_context).pb = avio.context();
        (*format_context).flags |= sys::AVFMT_FLAG_CUSTOM_IO as c_int;
    }

    let mut options = open_input_options()?;
    let mut opened_context = format_context;
    let open_result = check(
        unsafe {
            sys::avformat_open_input(
                &mut opened_context,
                uri.as_ptr(),
                ptr::null_mut(),
                &mut options,
            )
        },
        "avformat_open_input(custom_io)",
    );
    // `avformat_open_input` 会把它认识的选项从字典里摘走；剩下的必须显式
    // 释放，否则字典连同其中的字符串一起泄漏。
    unsafe { sys::av_dict_free(&mut options) };
    match open_result {
        Ok(()) => FormatContext::new_with_custom_io(opened_context, avio),
        Err(error) => {
            if !opened_context.is_null() {
                unsafe { sys::avformat_close_input(&mut opened_context) };
            }
            match avio.last_error.as_deref() {
                Some(source_error) => Err(FfmpegError::Source(format!(
                    "{error}; custom AVIO: {source_error}"
                ))),
                None => Err(error),
            }
        }
    }
}

fn find_stream_info(context: &mut FormatContext) -> Result<()> {
    check(
        unsafe { sys::avformat_find_stream_info(context.as_mut_ptr(), ptr::null_mut()) },
        "avformat_find_stream_info",
    )
}

fn inspect_format_context(
    uri: &str,
    raw: *const sys::AVFormatContext,
) -> (MediaProbe, Vec<Option<TimeBase>>) {
    let duration = unsafe { duration_from_av((*raw).duration) };
    let stream_count = unsafe { (*raw).nb_streams as usize };
    let mut tracks = Vec::with_capacity(stream_count);
    let mut video = Vec::new();
    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    let mut subtitle_fonts = Vec::new();
    let mut stream_time_bases = Vec::with_capacity(stream_count);

    for index in 0..stream_count {
        let stream = unsafe { *(*raw).streams.add(index) };
        if stream.is_null() {
            stream_time_bases.push(None);
            continue;
        }

        stream_time_bases.push(Some(TimeBase::from_av(unsafe { (*stream).time_base })));

        let codecpar = unsafe { (*stream).codecpar };
        if codecpar.is_null() {
            continue;
        }

        if unsafe { (*codecpar).codec_type } == sys::AVMediaType_AVMEDIA_TYPE_ATTACHMENT {
            let accepted_bytes = subtitle_fonts
                .iter()
                .map(SubtitleFontAttachment::byte_len)
                .sum();
            if let Some(font) = unsafe {
                subtitle_font_attachment(stream, codecpar, subtitle_fonts.len(), accepted_bytes)
            } {
                subtitle_fonts.push(font);
            }
            continue;
        }

        let Some(kind) = (unsafe { track_kind((*codecpar).codec_type) }) else {
            continue;
        };

        // Embedded cover art (ID3 APIC, METADATA_BLOCK_PICTURE, ...) is exposed
        // as a video stream with the attached-picture disposition. It is not
        // playable content: decoding it as the main video track dead-ends the
        // video path on a single still frame, and selecting it as the seek
        // stream breaks seeking on demuxers without a real video index.
        if kind == TrackKind::Video
            && unsafe { (*stream).disposition } & sys::AV_DISPOSITION_ATTACHED_PIC as i32 != 0
        {
            continue;
        }

        let codec = unsafe { codec_name((*codecpar).codec_id) };
        let mut track = TrackInfo::embedded(unsafe { (*stream).index as i64 }, kind);
        track.title = metadata_value(unsafe { (*stream).metadata }, "title");
        track.language = metadata_value(unsafe { (*stream).metadata }, "language");
        track.codec = codec.clone();
        track.bit_rate = u64::try_from(unsafe { (*codecpar).bit_rate })
            .ok()
            .filter(|bit_rate| *bit_rate > 0);

        if kind == TrackKind::Video {
            track.frame_rate = unsafe { stream_frame_rate(raw, stream) };
            let probe = unsafe { video_probe(&track, codecpar) };
            track.width = probe.params.width;
            track.height = probe.params.height;
            track.pixel_format = probe.pixel_format.clone();
            track.profile = probe.profile.clone();
            track.level = probe.level;
            video.push(probe);
        }
        if kind == TrackKind::Audio {
            let probe = unsafe { audio_probe(&track, codecpar) };
            track.sample_rate = probe.sample_rate;
            track.channels = probe.channels;
            track.sample_format = probe.sample_format.clone();
            audio.push(probe);
        }
        if kind == TrackKind::Subtitle {
            subtitles.push(subtitle_probe(&track));
        }

        tracks.push(track);
    }

    let container_bit_rate = u64::try_from(unsafe { (*raw).bit_rate })
        .ok()
        .filter(|bit_rate| *bit_rate > 0);
    infer_single_video_bit_rate(&mut tracks, container_bit_rate);

    (
        MediaProbe {
            uri: uri.to_string(),
            duration,
            tracks,
            video,
            audio,
            subtitles,
            subtitle_fonts: Arc::from(subtitle_fonts),
        },
        stream_time_bases,
    )
}

fn infer_single_video_bit_rate(tracks: &mut [TrackInfo], container_bit_rate: Option<u64>) {
    let Some(container_bit_rate) = container_bit_rate else {
        return;
    };
    let mut video_indexes = tracks
        .iter()
        .enumerate()
        .filter_map(|(index, track)| (track.kind == TrackKind::Video).then_some(index));
    let Some(video_index) = video_indexes.next() else {
        return;
    };
    if video_indexes.next().is_some() || tracks[video_index].bit_rate.is_some() {
        return;
    }
    let other_media_bit_rates = tracks
        .iter()
        .enumerate()
        .filter(|(index, track)| {
            *index != video_index
                && matches!(
                    track.kind,
                    TrackKind::Video | TrackKind::Audio | TrackKind::Subtitle
                )
        })
        .map(|(_, track)| track.bit_rate)
        .collect::<Option<Vec<_>>>();
    tracks[video_index].bit_rate = other_media_bit_rates
        .and_then(|values| values.into_iter().try_fold(0_u64, u64::checked_add))
        .and_then(|other_bit_rate| container_bit_rate.checked_sub(other_bit_rate))
        .filter(|value| *value > 0);
}

fn check(code: i32, operation: &'static str) -> Result<()> {
    if code >= 0 {
        Ok(())
    } else {
        Err(api_error(operation, code))
    }
}

fn api_error(operation: &'static str, code: i32) -> FfmpegError {
    FfmpegError::Api {
        operation,
        code,
        message: error_string(code),
    }
}

fn error_string(code: i32) -> String {
    let mut buffer = [0 as c_char; 256];
    unsafe {
        if sys::av_strerror(code, buffer.as_mut_ptr(), buffer.len()) == 0 {
            CStr::from_ptr(buffer.as_ptr())
                .to_string_lossy()
                .into_owned()
        } else {
            "unknown FFmpeg error".to_string()
        }
    }
}

unsafe fn duration_from_av(duration: i64) -> Option<Duration> {
    if duration <= 0 || duration == i64::MIN {
        return None;
    }
    let micros = duration as u64;
    Some(Duration::from_micros(micros))
}

unsafe fn track_kind(kind: sys::AVMediaType) -> Option<TrackKind> {
    match kind {
        sys::AVMediaType_AVMEDIA_TYPE_VIDEO => Some(TrackKind::Video),
        sys::AVMediaType_AVMEDIA_TYPE_AUDIO => Some(TrackKind::Audio),
        sys::AVMediaType_AVMEDIA_TYPE_SUBTITLE => Some(TrackKind::Subtitle),
        _ => None,
    }
}

unsafe fn codec_name(codec_id: sys::AVCodecID) -> Option<String> {
    let descriptor = unsafe { sys::avcodec_descriptor_get(codec_id) };
    if descriptor.is_null() {
        return None;
    }
    let name = unsafe { (*descriptor).name };
    if name.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
    )
}

unsafe fn stream_frame_rate(
    format: *const sys::AVFormatContext,
    stream: *const sys::AVStream,
) -> Option<FrameRate> {
    let average = unsafe { (*stream).avg_frame_rate };
    frame_rate_from_av(average)
        .or_else(|| frame_rate_from_av(unsafe { (*stream).r_frame_rate }))
        .or_else(|| {
            frame_rate_from_av(unsafe {
                sys::av_guess_frame_rate(format.cast_mut(), stream.cast_mut(), ptr::null_mut())
            })
        })
}

fn frame_rate_from_av(value: sys::AVRational) -> Option<FrameRate> {
    let numerator = u32::try_from(value.num).ok()?;
    let denominator = u32::try_from(value.den).ok()?;
    FrameRate::new(numerator, denominator)
}

unsafe fn video_probe(track: &TrackInfo, codecpar: *const sys::AVCodecParameters) -> VideoProbe {
    let width = unsafe { (*codecpar).width.max(0) as u32 };
    let height = unsafe { (*codecpar).height.max(0) as u32 };
    let pixel_format = unsafe { pixel_format_name((*codecpar).format) };
    let codec = track.codec.clone();
    let profile = codec
        .as_deref()
        .and_then(|codec_name| unsafe { profile_name(codecpar, codec_name) });
    VideoProbe {
        track_id: track.id,
        params: VideoParams {
            width,
            height,
            primaries: unsafe { color_primaries((*codecpar).color_primaries) },
            transfer: unsafe { transfer_function((*codecpar).color_trc) },
        },
        codec,
        pixel_format,
        profile,
        level: Some(unsafe { (*codecpar).level }).filter(|level| *level > 0),
    }
}

unsafe fn audio_probe(track: &TrackInfo, codecpar: *const sys::AVCodecParameters) -> AudioProbe {
    AudioProbe {
        track_id: track.id,
        codec: track.codec.clone(),
        sample_rate: unsafe { (*codecpar).sample_rate.max(0) as u32 },
        #[cfg(erika_ffmpeg_legacy_channel_layout)]
        channels: unsafe { (*codecpar).channels.max(0) as u32 },
        #[cfg(not(erika_ffmpeg_legacy_channel_layout))]
        channels: unsafe { (*codecpar).ch_layout.nb_channels.max(0) as u32 },
        sample_format: unsafe { sample_format_name((*codecpar).format) },
    }
}

fn subtitle_probe(track: &TrackInfo) -> SubtitleTrackConfig {
    let mut config = SubtitleTrackConfig::embedded(track.id, track.id);
    config.language = track.language.clone();
    config.title = track.title.clone();
    config
}

unsafe fn subtitle_font_attachment(
    stream: *const sys::AVStream,
    codecpar: *const sys::AVCodecParameters,
    accepted_count: usize,
    accepted_bytes: usize,
) -> Option<SubtitleFontAttachment> {
    let stream_index = unsafe { (*stream).index };
    let name = metadata_value(unsafe { (*stream).metadata }, "filename")
        .unwrap_or_else(|| format!("attachment-{stream_index}"));
    let mime_type = metadata_value(unsafe { (*stream).metadata }, "mimetype");
    let codec = unsafe { codec_name((*codecpar).codec_id) };
    if !is_font_attachment_candidate(&name, mime_type.as_deref(), codec.as_deref()) {
        return None;
    }

    let size = unsafe { (*codecpar).extradata_size };
    let data = unsafe { (*codecpar).extradata };
    if size <= 0 || data.is_null() {
        log_subtitle_font_rejected(
            stream_index,
            &name,
            mime_type.as_deref(),
            0,
            "attachment has no font data",
        );
        return None;
    }
    let size = size as usize;
    if accepted_count >= MAX_SUBTITLE_FONT_ATTACHMENTS {
        log_subtitle_font_rejected(
            stream_index,
            &name,
            mime_type.as_deref(),
            size,
            "container font attachment count limit exceeded",
        );
        return None;
    }
    if size > MAX_SUBTITLE_FONT_ATTACHMENT_BYTES {
        log_subtitle_font_rejected(
            stream_index,
            &name,
            mime_type.as_deref(),
            size,
            "font attachment byte limit exceeded",
        );
        return None;
    }
    if accepted_bytes
        .checked_add(size)
        .is_none_or(|total| total > MAX_SUBTITLE_FONT_TOTAL_BYTES)
    {
        log_subtitle_font_rejected(
            stream_index,
            &name,
            mime_type.as_deref(),
            size,
            "container font attachment total byte limit exceeded",
        );
        return None;
    }
    let bytes = unsafe { slice::from_raw_parts(data, size) }.to_vec();
    let mut database = fontdb::Database::new();
    database.load_font_data(bytes.clone());
    let faces = database.faces().collect::<Vec<_>>();
    if faces.is_empty() {
        log_subtitle_font_rejected(
            stream_index,
            &name,
            mime_type.as_deref(),
            bytes.len(),
            "font parser found no faces",
        );
        return None;
    }

    let mut families = BTreeSet::new();
    for face in faces {
        families.extend(face.families.iter().map(|(family, _)| family.clone()));
        if !face.post_script_name.is_empty() {
            families.insert(face.post_script_name.clone());
        }
    }
    let families = families.into_iter().collect::<Vec<_>>();
    crate::trace::diagnostic(
        serde_json::json!({
            "event": "subtitle_font_attachment",
            "stage": "validated",
            "sourceStreamIndex": stream_index,
            "name": name,
            "mimeType": mime_type,
            "families": families,
            "bytes": bytes.len(),
        })
        .to_string(),
    );
    Some(SubtitleFontAttachment::new(
        name,
        mime_type,
        families,
        Arc::<[u8]>::from(bytes),
    ))
}

fn is_font_attachment_candidate(name: &str, mime_type: Option<&str>, codec: Option<&str>) -> bool {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    let mime_type = mime_type.map(str::to_ascii_lowercase);
    matches!(extension.as_deref(), Some("ttf" | "otf" | "ttc" | "otc"))
        || mime_type.as_deref().is_some_and(|mime| {
            mime.starts_with("font/") || mime.contains("truetype") || mime.contains("opentype")
        })
        || matches!(codec, Some("ttf" | "otf"))
}

fn log_subtitle_font_rejected(
    stream_index: i32,
    name: &str,
    mime_type: Option<&str>,
    bytes: usize,
    reason: &str,
) {
    crate::trace::diagnostic(
        serde_json::json!({
            "event": "subtitle_font_attachment",
            "stage": "rejected",
            "sourceStreamIndex": stream_index,
            "name": name,
            "mimeType": mime_type,
            "bytes": bytes,
            "reason": reason,
        })
        .to_string(),
    );
}

unsafe fn subtitle_header_bytes(
    context: *const sys::AVCodecContext,
    stream_index: i32,
) -> Option<Arc<[u8]>> {
    let size = unsafe { (*context).subtitle_header_size };
    let data = unsafe { (*context).subtitle_header };
    copy_ass_codec_private(data, size, stream_index, "subtitle_header")
}

unsafe fn codec_parameter_extradata(
    parameters: *const sys::AVCodecParameters,
    stream_index: i32,
) -> Option<Arc<[u8]>> {
    let size = unsafe { (*parameters).extradata_size };
    let data = unsafe { (*parameters).extradata };
    copy_ass_codec_private(data, size, stream_index, "codec_extradata")
}

fn copy_ass_codec_private(
    data: *const u8,
    size: i32,
    stream_index: i32,
    source: &str,
) -> Option<Arc<[u8]>> {
    if size <= 0 || data.is_null() {
        return None;
    }
    let size = size as usize;
    if size > MAX_ASS_CODEC_PRIVATE_BYTES {
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "subtitle_ass_track",
                "stage": "codec_private_rejected",
                "sourceStreamIndex": stream_index,
                "source": source,
                "bytes": size,
                "limitBytes": MAX_ASS_CODEC_PRIVATE_BYTES,
                "reason": "ASS CodecPrivate byte limit exceeded",
            })
            .to_string(),
        );
        return None;
    }
    Some(Arc::from(unsafe { slice::from_raw_parts(data, size) }))
}

unsafe fn subtitle_codec_is_text(codec_id: sys::AVCodecID) -> bool {
    let descriptor = unsafe { sys::avcodec_descriptor_get(codec_id) };
    !descriptor.is_null()
        && unsafe { (*descriptor).props } & sys::AV_CODEC_PROP_TEXT_SUB as i32 != 0
}

unsafe fn import_av_subtitle(
    track_id: i64,
    packet: &Packet,
    subtitle: &sys::AVSubtitle,
    canvas: (u32, u32),
    ass_track: Option<Arc<AssTrackResources>>,
) -> Result<DecodedSubtitleFrame> {
    let start = subtitle_start_time(packet, subtitle);
    let start_offset = Duration::from_millis(u64::from(subtitle.start_display_time));
    let start = start.map(|pts| pts.saturating_add(start_offset));
    let end = subtitle_end_time(start, subtitle);
    let mut frame = DecodedSubtitleFrame::new(track_id, start, end);
    let mut has_ass = false;

    let rect_count = subtitle.num_rects as usize;
    if rect_count == 0 || subtitle.rects.is_null() {
        return Ok(frame);
    }

    for index in 0..rect_count {
        let rect = unsafe { *subtitle.rects.add(index) };
        if rect.is_null() {
            continue;
        }
        let rect = unsafe { &*rect };
        let forced = subtitle_rect_forced(rect);
        match rect.type_ {
            sys::AVSubtitleType_SUBTITLE_TEXT => {
                if let Some(text) = unsafe { subtitle_c_string(rect.text) } {
                    frame.push_text(
                        SubtitleTextSegment::new(SubtitleTextFormat::PlainText, text)
                            .with_forced(forced),
                    );
                }
            }
            sys::AVSubtitleType_SUBTITLE_ASS => {
                if let Some(text) = unsafe { subtitle_c_string(rect.ass) } {
                    has_ass = true;
                    frame.push_text(
                        SubtitleTextSegment::new(SubtitleTextFormat::Ass, text).with_forced(forced),
                    );
                }
            }
            sys::AVSubtitleType_SUBTITLE_BITMAP => {
                // One malformed bitmap rect must not abort the whole track: a
                // single damaged PGS/DVB packet would otherwise end subtitle
                // decoding for the stream. Text and ASS rects above already
                // degrade per rect, so skip the unusable one and keep going.
                match unsafe { subtitle_bitmap_rect_to_rgba_plane(rect) } {
                    Ok(Some(plane)) => {
                        frame.push_bitmap_plane(plane.with_canvas(canvas.0, canvas.1), forced);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        crate::trace::diagnostic(
                            serde_json::json!({
                                "event": "subtitle_bitmap_rect_skipped",
                                "trackId": track_id,
                                "error": error.to_string(),
                            })
                            .to_string(),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    if has_ass {
        frame.ass_track = ass_track;
    }

    Ok(frame)
}

fn subtitle_start_time(packet: &Packet, subtitle: &sys::AVSubtitle) -> Option<Duration> {
    if subtitle.pts != i64::MIN {
        let seconds = subtitle.pts as f64 / f64::from(sys::AV_TIME_BASE);
        if seconds.is_finite() && seconds >= 0.0 {
            return Some(Duration::from_secs_f64(seconds));
        }
    }
    packet.pts().and_then(PacketTimestamp::as_duration)
}

fn subtitle_end_time(start: Option<Duration>, subtitle: &sys::AVSubtitle) -> Option<Duration> {
    let start = start?;
    if subtitle.end_display_time <= subtitle.start_display_time
        || subtitle.end_display_time == u32::MAX
    {
        return None;
    }
    let duration_ms = subtitle
        .end_display_time
        .saturating_sub(subtitle.start_display_time);
    Some(start.saturating_add(Duration::from_millis(u64::from(duration_ms))))
}

unsafe fn subtitle_c_string(ptr: *const libc::c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let text = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    (!text.is_empty()).then_some(text)
}

fn subtitle_rect_forced(rect: &sys::AVSubtitleRect) -> bool {
    rect.flags & sys::AV_SUBTITLE_FLAG_FORCED as i32 != 0
}

unsafe fn subtitle_bitmap_rect_to_rgba_plane(
    rect: &sys::AVSubtitleRect,
) -> Result<Option<SubtitleBitmapPlane>> {
    if rect.w <= 0 || rect.h <= 0 {
        return Ok(None);
    }
    if rect.data[0].is_null()
        || rect.data[1].is_null()
        || rect.linesize[0] < rect.w
        || rect.nb_colors <= 0
        || rect.nb_colors > sys::AVPALETTE_COUNT as i32
    {
        return Err(FfmpegError::InvalidSubtitleBitmap {
            width: rect.w,
            height: rect.h,
            stride: rect.linesize[0],
            colors: rect.nb_colors,
        });
    }

    let width = rect.w as usize;
    let height = rect.h as usize;
    let stride = rect.linesize[0] as usize;
    let mut rgba = vec![0u8; width.saturating_mul(height).saturating_mul(4)];
    let palette =
        unsafe { std::slice::from_raw_parts(rect.data[1].cast::<u32>(), rect.nb_colors as usize) };

    for y in 0..height {
        let row = unsafe { std::slice::from_raw_parts(rect.data[0].add(y * stride), width) };
        for (x, index) in row.iter().copied().enumerate() {
            let color = palette.get(index as usize).copied().unwrap_or(0);
            let dst = &mut rgba[(y * width + x) * 4..][..4];
            dst.copy_from_slice(&palette_color_to_rgba(color));
        }
    }

    Ok(Some(SubtitleBitmapPlane::new(
        rect.x,
        rect.y,
        rect.w as u32,
        rect.h as u32,
        rgba,
    )))
}

fn palette_color_to_rgba(color: u32) -> [u8; 4] {
    [
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
        ((color >> 24) & 0xff) as u8,
    ]
}

unsafe fn pixel_format_name(format: i32) -> Option<String> {
    if format < 0 {
        return None;
    }
    let name = unsafe { sys::av_get_pix_fmt_name(format) };
    if name.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
    )
}

unsafe fn sample_format_name(format: i32) -> Option<String> {
    if format < 0 {
        return None;
    }
    let name = unsafe { sys::av_get_sample_fmt_name(format) };
    if name.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
    )
}

unsafe fn profile_name(
    codecpar: *const sys::AVCodecParameters,
    codec_name: &str,
) -> Option<String> {
    let codec_id = unsafe { (*codecpar).codec_id };
    let profile = unsafe { (*codecpar).profile };
    if profile == sys::ERIKA_PROFILE_UNKNOWN {
        return None;
    }
    let name = unsafe { sys::av_get_profile_name(sys::avcodec_find_decoder(codec_id), profile) };
    if name.is_null() {
        return Some(format!("{codec_name}:{profile}"));
    }
    Some(
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
    )
}

unsafe fn color_primaries(value: sys::AVColorPrimaries) -> ColorPrimaries {
    match value {
        sys::AVColorPrimaries_AVCOL_PRI_BT709 => ColorPrimaries::Bt709,
        sys::AVColorPrimaries_AVCOL_PRI_SMPTE432 => ColorPrimaries::DisplayP3,
        sys::AVColorPrimaries_AVCOL_PRI_BT2020 => ColorPrimaries::Bt2020,
        _ => ColorPrimaries::Unknown,
    }
}

unsafe fn transfer_function(value: sys::AVColorTransferCharacteristic) -> TransferFunction {
    match value {
        sys::AVColorTransferCharacteristic_AVCOL_TRC_IEC61966_2_1 => TransferFunction::Srgb,
        sys::AVColorTransferCharacteristic_AVCOL_TRC_BT709 => TransferFunction::Bt1886,
        sys::AVColorTransferCharacteristic_AVCOL_TRC_SMPTE2084 => TransferFunction::Pq,
        sys::AVColorTransferCharacteristic_AVCOL_TRC_ARIB_STD_B67 => TransferFunction::Hlg,
        _ => TransferFunction::Unknown,
    }
}

unsafe fn color_range(value: sys::AVColorRange) -> ColorRange {
    match value {
        sys::AVColorRange_AVCOL_RANGE_MPEG => ColorRange::Limited,
        sys::AVColorRange_AVCOL_RANGE_JPEG => ColorRange::Full,
        _ => ColorRange::Unspecified,
    }
}

unsafe fn matrix_coefficients(value: sys::AVColorSpace) -> MatrixCoefficients {
    match value {
        sys::AVColorSpace_AVCOL_SPC_RGB => MatrixCoefficients::Identity,
        sys::AVColorSpace_AVCOL_SPC_BT709 => MatrixCoefficients::Bt709,
        sys::AVColorSpace_AVCOL_SPC_BT470BG | sys::AVColorSpace_AVCOL_SPC_SMPTE170M => {
            MatrixCoefficients::Bt601
        }
        sys::AVColorSpace_AVCOL_SPC_BT2020_NCL => MatrixCoefficients::Bt2020NonConstantLuminance,
        _ => MatrixCoefficients::Unspecified,
    }
}

unsafe fn frame_hdr_metadata(frame: *const sys::AVFrame) -> Option<HdrMetadata> {
    let mastering_display = unsafe { mastering_display_metadata(frame) };
    let content_light = unsafe { content_light_metadata(frame) };
    if mastering_display.is_none() && content_light.is_none() {
        return None;
    }
    Some(HdrMetadata::new(mastering_display, content_light))
}

/// Reads the container's Dolby Vision configuration record from codec
/// parameters side data (`AV_PKT_DATA_DOVI_CONF` in ffmpeg 8), returning the
/// Dolby Vision profile number.
unsafe fn codec_parameters_dolby_vision_profile(
    parameters: *const sys::AVCodecParameters,
) -> Option<u8> {
    if parameters.is_null() {
        return None;
    }
    let side_data = unsafe {
        sys::av_packet_side_data_get(
            (*parameters).coded_side_data,
            (*parameters).nb_coded_side_data,
            sys::AVPacketSideDataType_AV_PKT_DATA_DOVI_CONF,
        )
    };
    if side_data.is_null() {
        return None;
    }
    let data = unsafe { (*side_data).data };
    if data.is_null() {
        return None;
    }
    let size = unsafe { (*side_data).size };
    if usize::try_from(size).ok()? < mem::size_of::<sys::AVDOVIDecoderConfigurationRecord>() {
        return None;
    }
    let record = data as *const sys::AVDOVIDecoderConfigurationRecord;
    Some(unsafe { (*record).dv_profile })
}

/// Why a decoded frame cannot feed the Dolby Vision mapping path.
///
/// Reportable through [`Frame::dovi_unavailable_reason`] so a stream whose
/// RPUs are missing or rejected stays diagnosable instead of silently falling
/// back to the base layer — Profile 5 in particular has no HDR10-compatible
/// base layer, and an unmapped Profile 8 frame with a rejected RPU displays
/// with wrong colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoviRejectReason {
    /// The frame carries no RPU side data at all. Normal for profiles whose
    /// base layer is HDR10-compatible (e.g. Profile 8); always wrong for
    /// Profile 5.
    MissingSideData,
    /// Side data exists but failed structural validation (size, alignment,
    /// sub-structure offsets).
    MalformedSideData,
    /// Base-layer bit depth outside FFmpeg's 8..=16 range.
    UnsupportedBitDepth,
    /// `coef_log2_denom` beyond FFmpeg's fixed-point range (above 32).
    UnsupportedCoefDenom,
    /// Reshaping curves invalid or absent (pivots, orders, unknown methods).
    InvalidCurves,
    /// Color matrices/offsets invalid or the RGB→LMS matrix is singular.
    InvalidColorMetadata,
}

impl DoviRejectReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::MissingSideData => "rpu side data missing",
            Self::MalformedSideData => "rpu side data malformed",
            Self::UnsupportedBitDepth => "base layer bit depth outside 8..=16",
            Self::UnsupportedCoefDenom => "coef_log2_denom above 32",
            Self::InvalidCurves => "invalid reshaping curves",
            Self::InvalidColorMetadata => "invalid color metadata",
        }
    }
}

/// Enhancement-layer information an RPU carries that this renderer does not
/// compose. Profile 7 keeps its enhancement layer in a second HEVC layer that
/// this renderer never decodes, so the NLQ residual these fields describe has
/// nothing to be added to. libplacebo draws the same line: it exposes
/// `nlq_active` and documents that "consumers that have not bound an
/// enhancement layer must not look at these fields", while still applying the
/// base-layer reshaping, color matrices, and trims.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DoviElStatus {
    /// `disable_residual_flag == 0`: the RPU expects its residual to be
    /// composed with an enhancement layer (Profile 7 FEL/MEL).
    pub residual_requested: bool,
    /// The RPU carries a non-trivial NLQ definition for that residual.
    pub nlq_nontrivial: bool,
}

impl DoviElStatus {
    /// Whether the RPU asked for anything this renderer cannot compose.
    pub fn is_active(self) -> bool {
        self.residual_requested || self.nlq_nontrivial
    }
}

/// Reads the decoder's parsed Dolby Vision RPU side data and converts it into
/// shader-ready floats, mirroring libplacebo's `pl_map_dovi_metadata`: pivots
/// are normalized by the base-layer bit depth and curve coefficients by
/// `2^-coef_log2_denom`. The second value reports any enhancement-layer
/// residual the RPU asked for but this renderer cannot compose; it never
/// rejects an otherwise valid RPU.
unsafe fn frame_dovi_metadata_result(
    frame: *const sys::AVFrame,
) -> std::result::Result<(DoviSourceMetadata, DoviElStatus), DoviRejectReason> {
    if frame.is_null() {
        return Err(DoviRejectReason::MissingSideData);
    }
    let side_data = unsafe {
        sys::av_frame_get_side_data(frame, sys::AVFrameSideDataType_AV_FRAME_DATA_DOVI_METADATA)
    };
    if side_data.is_null() {
        return Err(DoviRejectReason::MissingSideData);
    }
    let data = unsafe { (*side_data).data };
    if data.is_null() {
        return Err(DoviRejectReason::MalformedSideData);
    }
    let size = usize::try_from(unsafe { (*side_data).size })
        .ok()
        .ok_or(DoviRejectReason::MalformedSideData)?;
    if size < mem::size_of::<sys::AVDOVIMetadata>() {
        return Err(DoviRejectReason::MalformedSideData);
    }
    if (data as usize) % mem::align_of::<sys::AVDOVIMetadata>() != 0 {
        return Err(DoviRejectReason::MalformedSideData);
    }
    // The sub-structures live behind byte offsets inside this side data buffer
    // (`av_dovi_get_header` and friends are C inline helpers bindgen does not
    // emit), so resolve them by the same pointer arithmetic here with bounds checks.
    let metadata = unsafe { *data.cast::<sys::AVDOVIMetadata>() };
    let data_address = data as usize;
    let aligned = |offset: usize, alignment: usize| {
        data_address
            .checked_add(offset)
            .is_some_and(|address| address % alignment == 0)
    };
    let in_bounds = |offset: usize, structure: usize| {
        offset.checked_add(structure).is_some_and(|end| end <= size)
    };
    if !in_bounds(
        metadata.header_offset,
        mem::size_of::<sys::AVDOVIRpuDataHeader>(),
    ) || !in_bounds(
        metadata.mapping_offset,
        mem::size_of::<sys::AVDOVIDataMapping>(),
    ) || !in_bounds(
        metadata.color_offset,
        mem::size_of::<sys::AVDOVIColorMetadata>(),
    ) || !aligned(
        metadata.header_offset,
        mem::align_of::<sys::AVDOVIRpuDataHeader>(),
    ) || !aligned(
        metadata.mapping_offset,
        mem::align_of::<sys::AVDOVIDataMapping>(),
    ) || !aligned(
        metadata.color_offset,
        mem::align_of::<sys::AVDOVIColorMetadata>(),
    ) {
        return Err(DoviRejectReason::MalformedSideData);
    }
    let header = unsafe {
        &*data
            .add(metadata.header_offset)
            .cast::<sys::AVDOVIRpuDataHeader>()
    };
    let mapping = unsafe {
        &*data
            .add(metadata.mapping_offset)
            .cast::<sys::AVDOVIDataMapping>()
    };
    let color = unsafe {
        &*data
            .add(metadata.color_offset)
            .cast::<sys::AVDOVIColorMetadata>()
    };

    let bl_bit_depth = usize::from(header.bl_bit_depth);
    let coef_denom = u32::from(header.coef_log2_denom);
    // Validate bit depth and coefficient denominator before using them in
    // shifts below; malformed side data must be rejected, never panic.
    // FFmpeg uses coef_log2_denom up to 32 for float RPU coefficients.
    if !(8..=16).contains(&bl_bit_depth) {
        return Err(DoviRejectReason::UnsupportedBitDepth);
    }
    if coef_denom > 32 {
        return Err(DoviRejectReason::UnsupportedCoefDenom);
    }
    // This renderer has no enhancement-layer input, so the NLQ residual below
    // is never composed. That does not make the RPU unusable: the base-layer
    // reshaping curves, color matrices, and L1 trims still apply, and dropping
    // the whole RPU would throw them away for every Profile 7 FEL frame.
    // libplacebo keeps the same separation (it exposes `nlq_active` but only
    // composes it when an enhancement layer is bound).
    //
    // FFmpeg marks an absent NLQ with the AV_DOVI_NLQ_NONE sentinel (-1);
    // method 0 with all-neutral parameters is likewise an identity mapping.
    let nlq_nontrivial = match mapping.nlq_method_idc as i32 {
        -1 => false,
        0 => mapping.nlq.iter().any(|params| {
            params.nlq_offset != 0
                || params.linear_deadzone_slope != 0
                || params.linear_deadzone_threshold != 0
                || (params.vdr_in_max != 0
                    && params.vdr_in_max != (1_u64 << u32::from(header.coef_log2_denom)))
        }),
        _ => true,
    };
    let el = DoviElStatus {
        residual_requested: header.disable_residual_flag == 0,
        nlq_nontrivial,
    };

    let pivot_scale = 1.0_f32 / (((1_usize << bl_bit_depth) - 1) as f32);
    let coefficient_scale = 2.0_f32.powi(-(coef_denom as i32));

    let mut reshaping = [DoviComponentCurve::default(); 3];
    let mut has_curve = false;
    for (component, curve) in reshaping.iter_mut().enumerate() {
        let source = &mapping.curves[component];
        let num_pivots = usize::from(source.num_pivots);
        if num_pivots == 0 {
            continue;
        }
        has_curve = true;
        if !(2..=DOVI_MAX_PIECES + 1).contains(&num_pivots) {
            return Err(DoviRejectReason::InvalidCurves);
        }
        let max_pivot = ((1_usize << bl_bit_depth) - 1) as u16;
        let mut previous = None;
        for &pivot in source.pivots[..num_pivots].iter() {
            if pivot > max_pivot || previous.is_some_and(|previous| pivot <= previous) {
                return Err(DoviRejectReason::InvalidCurves);
            }
            previous = Some(pivot);
        }
        curve.num_pivots = num_pivots as u8;
        for (slot, &pivot) in curve.pivots[..num_pivots]
            .iter_mut()
            .zip(source.pivots[..num_pivots].iter())
        {
            *slot = pivot_scale * f32::from(pivot);
        }
        for segment in 0..num_pivots - 1 {
            match source.mapping_idc[segment] {
                sys::AVDOVIMappingMethod_AV_DOVI_MAPPING_MMR => {
                    let order = usize::from(source.mmr_order[segment]);
                    if order == 0 || order > DOVI_MAX_MMR_ORDER {
                        return Err(DoviRejectReason::InvalidCurves);
                    }
                    curve.mmr_orders[segment] = order as u8;
                    curve.mmr_constants[segment] =
                        coefficient_scale * source.mmr_constant[segment] as f32;
                    let destination = &mut curve.mmr_coeffs[segment][..order];
                    for (order_index, coefficients) in destination.iter_mut().enumerate() {
                        for (index, coefficient) in coefficients.iter_mut().enumerate() {
                            *coefficient = coefficient_scale
                                * source.mmr_coef[segment][order_index][index] as f32;
                        }
                    }
                }
                sys::AVDOVIMappingMethod_AV_DOVI_MAPPING_POLYNOMIAL => {
                    let poly_order = usize::from(source.poly_order[segment]);
                    if !(1..=2).contains(&poly_order) {
                        return Err(DoviRejectReason::InvalidCurves);
                    }
                    let coefficients = &source.poly_coef[segment];
                    for (order_index, slot) in curve.poly_coeffs[segment].iter_mut().enumerate() {
                        *slot = if order_index <= poly_order {
                            coefficient_scale * coefficients[order_index] as f32
                        } else {
                            0.0
                        };
                    }
                }
                _ => return Err(DoviRejectReason::InvalidCurves),
            }
        }
    }
    if !has_curve {
        return Err(DoviRejectReason::InvalidCurves);
    }

    let rational_matrix = |values: &[sys::AVRational; 9]| -> Option<[[f32; 3]; 3]> {
        let mut matrix = [[0.0_f32; 3]; 3];
        for row in 0..3 {
            for col in 0..3 {
                let value = &values[row * 3 + col];
                if value.den == 0 {
                    return None;
                }
                let value = value.num as f32 / value.den as f32;
                if !value.is_finite() {
                    return None;
                }
                matrix[row][col] = value;
            }
        }
        Some(matrix)
    };

    let mut nonlinear_offset = [0.0_f32; 3];
    for (index, slot) in nonlinear_offset.iter_mut().enumerate() {
        let value = &color.ycc_to_rgb_offset[index];
        if value.den == 0 {
            return Err(DoviRejectReason::InvalidColorMetadata);
        }
        *slot = value.num as f32 / value.den as f32;
        if !slot.is_finite() {
            return Err(DoviRejectReason::InvalidColorMetadata);
        }
    }

    if color.source_min_pq > 4095
        || color.source_max_pq > 4095
        || (color.source_max_pq != 0 && color.source_min_pq > color.source_max_pq)
    {
        return Err(DoviRejectReason::InvalidColorMetadata);
    }

    let nonlinear_matrix =
        rational_matrix(&color.ycc_to_rgb_matrix).ok_or(DoviRejectReason::InvalidColorMetadata)?;
    let rgb_to_lms =
        rational_matrix(&color.rgb_to_lms_matrix).ok_or(DoviRejectReason::InvalidColorMetadata)?;
    let determinant = rgb_to_lms[0][0]
        * (rgb_to_lms[1][1] * rgb_to_lms[2][2] - rgb_to_lms[1][2] * rgb_to_lms[2][1])
        - rgb_to_lms[0][1]
            * (rgb_to_lms[1][0] * rgb_to_lms[2][2] - rgb_to_lms[1][2] * rgb_to_lms[2][0])
        + rgb_to_lms[0][2]
            * (rgb_to_lms[1][0] * rgb_to_lms[2][1] - rgb_to_lms[1][1] * rgb_to_lms[2][0]);
    if !determinant.is_finite() || determinant == 0.0 {
        return Err(DoviRejectReason::InvalidColorMetadata);
    }

    // Level 1 per-frame brightness metadata lives in the DM extension blocks
    // that the RPU decoder appends right after the color structure. `av_dovi_find_level`
    // is exported by FFmpeg but performs an unchecked pointer walk, so validate
    // the block region here like the sub-structures above. L1 is optional
    // signal quality metadata: an invalid or absent block never rejects the RPU.
    let l1 = unsafe { frame_dovi_level1(data, &metadata, size) };

    Ok((
        DoviSourceMetadata {
            reshaping,
            nonlinear_matrix: RgbMatrix::new(nonlinear_matrix),
            nonlinear_offset,
            rgb_to_lms: RgbMatrix::new(rgb_to_lms),
            source_min_pq: color.source_min_pq,
            source_max_pq: color.source_max_pq,
            l1,
        },
        el,
    ))
}

/// Reads the dynamic DM level 1 block (per-frame min/max/avg luminance in
/// 12-bit PQ codes) from the validated ext-block region, matching
/// `av_dovi_get_ext`'s pointer arithmetic.
unsafe fn frame_dovi_level1(
    data: *const u8,
    metadata: &sys::AVDOVIMetadata,
    size: usize,
) -> Option<DoviFramePq> {
    let count = usize::try_from(metadata.num_ext_blocks).ok()?;
    if count == 0 {
        return None;
    }
    let block_size = metadata.ext_block_size;
    if block_size < mem::size_of::<sys::AVDOVIDmData>() {
        return None;
    }
    let total = block_size.checked_mul(count)?;
    let end = metadata.ext_block_offset.checked_add(total)?;
    if end > size {
        return None;
    }
    if (metadata.ext_block_offset as usize) % mem::align_of::<sys::AVDOVIDmData>() != 0 {
        return None;
    }
    let ext = unsafe { data.add(metadata.ext_block_offset) };
    for index in 0..count {
        let block = unsafe { &*ext.add(block_size * index).cast::<sys::AVDOVIDmData>() };
        if block.level != 1 {
            continue;
        }
        let l1 = unsafe { block.__bindgen_anon_1.l1 };
        if l1.max_pq == 0 || (l1.min_pq != 0 && l1.min_pq > l1.max_pq) {
            return None;
        }
        return Some(DoviFramePq {
            min_pq: l1.min_pq,
            max_pq: l1.max_pq,
            avg_pq: l1.avg_pq,
        });
    }
    None
}

unsafe fn mastering_display_metadata(
    frame: *const sys::AVFrame,
) -> Option<MasteringDisplayMetadata> {
    let metadata = unsafe {
        read_frame_side_data::<sys::AVMasteringDisplayMetadata>(
            frame,
            sys::AVFrameSideDataType_AV_FRAME_DATA_MASTERING_DISPLAY_METADATA,
        )
    }?;

    let has_primaries = metadata.has_primaries != 0;
    let has_luminance = metadata.has_luminance != 0;
    let display_primaries = has_primaries
        .then(|| {
            Some([
                rational_chromaticity(metadata.display_primaries[0])?,
                rational_chromaticity(metadata.display_primaries[1])?,
                rational_chromaticity(metadata.display_primaries[2])?,
            ])
        })
        .flatten();
    let white_point = has_primaries
        .then(|| rational_chromaticity(metadata.white_point))
        .flatten();
    let min_luminance_nits = has_luminance
        .then(|| rational_to_positive_f32(metadata.min_luminance))
        .flatten();
    let max_luminance_nits = has_luminance
        .then(|| rational_to_positive_f32(metadata.max_luminance))
        .flatten();

    if display_primaries.is_none()
        && white_point.is_none()
        && min_luminance_nits.is_none()
        && max_luminance_nits.is_none()
    {
        return None;
    }

    Some(MasteringDisplayMetadata {
        display_primaries,
        white_point,
        min_luminance_nits,
        max_luminance_nits,
    })
}

unsafe fn content_light_metadata(frame: *const sys::AVFrame) -> Option<ContentLightMetadata> {
    let metadata = unsafe {
        read_frame_side_data::<sys::AVContentLightMetadata>(
            frame,
            sys::AVFrameSideDataType_AV_FRAME_DATA_CONTENT_LIGHT_LEVEL,
        )
    }?;
    if metadata.MaxCLL == 0 && metadata.MaxFALL == 0 {
        return None;
    }
    Some(ContentLightMetadata {
        max_content_light_level_nits: metadata.MaxCLL,
        max_frame_average_light_level_nits: metadata.MaxFALL,
    })
}

#[allow(irrefutable_let_patterns)]
unsafe fn read_frame_side_data<T: Copy>(
    frame: *const sys::AVFrame,
    side_data_type: sys::AVFrameSideDataType,
) -> Option<T> {
    if frame.is_null() {
        return None;
    }
    let side_data = unsafe { sys::av_frame_get_side_data(frame, side_data_type) };
    if side_data.is_null() {
        return None;
    }
    let data = unsafe { (*side_data).data };
    let size = unsafe { (*side_data).size };
    let Ok(size) = usize::try_from(size) else {
        return None;
    };
    if data.is_null() || size < mem::size_of::<T>() {
        return None;
    }
    Some(unsafe { ptr::read_unaligned(data.cast::<T>()) })
}

fn rational_chromaticity(values: [sys::AVRational; 2]) -> Option<Chromaticity> {
    Some(Chromaticity::new(
        rational_to_positive_f32(values[0])?,
        rational_to_positive_f32(values[1])?,
    ))
}

fn rational_to_positive_f32(value: sys::AVRational) -> Option<f32> {
    if value.den == 0 {
        return None;
    }
    let value = value.num as f32 / value.den as f32;
    if value.is_finite() && value > 0.0 {
        Some(value)
    } else {
        None
    }
}

fn metadata_value(metadata: *mut sys::AVDictionary, key: &str) -> Option<String> {
    let key = CString::new(key).ok()?;
    unsafe {
        let entry = sys::av_dict_get(metadata, key.as_ptr(), ptr::null(), 0);
        if entry.is_null() || (*entry).value.is_null() {
            return None;
        }
        Some(
            CStr::from_ptr((*entry).value)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

fn timestamp_value(value: i64) -> Option<i64> {
    if value == i64::MIN { None } else { Some(value) }
}

fn format_timeline_origin_micros(raw: *const sys::AVFormatContext) -> i64 {
    if raw.is_null() {
        return 0;
    }
    timestamp_value(unsafe { (*raw).start_time }).unwrap_or(0)
}

fn trace_timeline_origin(timeline_origin_micros: i64) {
    if timeline_origin_micros == 0 {
        return;
    }
    crate::trace::diagnostic(
        serde_json::json!({
            "event": "ffmpeg_timeline",
            "stage": "normalize_origin",
            "timelineOriginMicros": timeline_origin_micros,
        })
        .to_string(),
    );
}

fn absolute_seek_target_micros(relative_target: i64, timeline_origin_micros: i64) -> i64 {
    timeline_origin_micros.saturating_add(relative_target)
}

fn rescale_microseconds_to_time_base(microseconds: i64, time_base: TimeBase) -> i64 {
    unsafe {
        sys::av_rescale_q(
            microseconds,
            sys::AVRational {
                num: 1,
                den: sys::AV_TIME_BASE as i32,
            },
            time_base.to_av_rational(),
        )
    }
}

#[cfg(target_env = "ohos")]
fn timestamp_to_microseconds(timestamp: PacketTimestamp) -> i64 {
    unsafe {
        sys::av_rescale_q(
            timestamp.raw,
            timestamp.time_base.to_av_rational(),
            sys::AVRational {
                num: 1,
                den: sys::AV_TIME_BASE as i32,
            },
        )
    }
}

fn av_error(errno: i32) -> i32 {
    -errno
}

fn hardware_pixel_format(
    codec: *const sys::AVCodec,
    device_type: sys::AVHWDeviceType,
) -> Option<sys::AVPixelFormat> {
    let mut index = 0;
    loop {
        let config = unsafe { sys::avcodec_get_hw_config(codec, index) };
        if config.is_null() {
            return None;
        }
        let supports_device_ctx = unsafe {
            (*config).device_type == device_type
                && ((*config).methods & sys::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32) != 0
        };
        if supports_device_ctx {
            return Some(unsafe { (*config).pix_fmt });
        }
        index += 1;
    }
}

unsafe extern "C" fn select_hw_format(
    context: *mut sys::AVCodecContext,
    formats: *const sys::AVPixelFormat,
) -> sys::AVPixelFormat {
    let state = unsafe { (*context).opaque as *const HardwareDecoderState };
    if !state.is_null() {
        let target = unsafe { (*state).pixel_format };
        let mut index = 0usize;
        loop {
            let format = unsafe { *formats.add(index) };
            if format == sys::AVPixelFormat_AV_PIX_FMT_NONE {
                break;
            }
            if format == target {
                #[cfg(target_os = "windows")]
                if target == sys::AVPixelFormat_AV_PIX_FMT_D3D11 {
                    unsafe { ensure_d3d11va_frames_for_context(context, state.cast_mut()) };
                }
                let frames_ref = unsafe { (*state).frames_ref };
                if !frames_ref.is_null() {
                    let context_frames = unsafe { &mut (*context).hw_frames_ctx };
                    if !(*context_frames).is_null() {
                        unsafe { sys::av_buffer_unref(context_frames) };
                    }
                    let frames_ref = unsafe { sys::av_buffer_ref(frames_ref) };
                    if !frames_ref.is_null() {
                        *context_frames = frames_ref;
                    }
                }
                return format;
            }
            index += 1;
        }
        // Never report a software frame as NVDEC/VA-API. Return an error to
        // the playback layer, which owns the explicit, observable fallback.
        if matches!(
            target,
            sys::AVPixelFormat_AV_PIX_FMT_CUDA | sys::AVPixelFormat_AV_PIX_FMT_VAAPI
        ) {
            return sys::AVPixelFormat_AV_PIX_FMT_NONE;
        }
    }
    unsafe { sys::avcodec_default_get_format(context, formats) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 池深单测共用的伪造 `AVCodecContext`：只填 `codec_id` 与 `refs`。
    ///
    /// 为什么不用 `avcodec_alloc_context3()`：那会真分配几百 KB，而这几个函数
    /// 只读那两个字段，不碰其余任何东西。用 `MaybeUninit::zeroed()` 造一个
    /// 最小可用实例即可（`refs = 0` 就是"lavc 还没解 SPS"的真实状态）。
    ///
    /// 顺带这也是 `d3d11va_pool_size` 的 `context` 参数**不允许为 null** 的
    /// 由来——单测改传伪造实例后，`ptr::null()` 那条只在测试里存在的分支
    /// 就没有调用者了。
    #[cfg(target_os = "windows")]
    fn fake_ctx(codec_id: sys::AVCodecID, refs: i32) -> *const sys::AVCodecContext {
        use std::mem::MaybeUninit;
        let mut raw = MaybeUninit::<sys::AVCodecContext>::zeroed();
        let ptr = raw.as_mut_ptr();
        unsafe {
            (*ptr).codec_id = codec_id;
            (*ptr).refs = refs;
            ptr
        }
    }

    /// "拿不到 SPS 实测值"的兜底实例：H.264 但 `refs = 0`。
    ///
    /// 覆盖的是 `d3d11va_sps_reference_surfaces` 里那条
    /// `refs < D3D11VA_MIN_SPS_REFERENCE_FRAMES` 分支——也就是解码器还没解出
    /// SPS（建池早于 SPS 解析）时的样子。
    #[cfg(target_os = "windows")]
    fn fallback_ctx_h264() -> *const sys::AVCodecContext {
        fake_ctx(sys::AVCodecID_AV_CODEC_ID_H264, 0)
    }

    #[test]
    fn linked_ffmpeg_reports_version() {
        assert!(!version().is_empty());
    }

    /// 池深按"预算 / 单片字节"折算，但不许击穿下界、也不许超过旧的固定值。
    ///
    /// **全部传 `fallback_ctx_h264()`** —— 即"拿不到 SPS 实测值"的保守情形，
    /// 走规范上限。这让本测试锁定的仍是**兜底行为**（真实世界里等于
    /// HEVC / VP9 / AV1，它们永远走这条），不受 H.264 实测路径影响。H.264 的
    /// 实测路径由 `d3d11va_pool_size_uses_sps_reference_frames_for_h264` 覆盖。
    #[cfg(target_os = "windows")]
    #[test]
    fn d3d11va_pool_size_follows_budget_between_floor_and_legacy_ceiling() {
        use sys::{
            AVCodecID_AV_CODEC_ID_AV1, AVPixelFormat_AV_PIX_FMT_NV12,
            AVPixelFormat_AV_PIX_FMT_P010LE,
        };

        // 1080P H.264（coded 1920x1088，NV12 2.99MB/片）：64MiB 折算 21 片，
        // 低于队列 8 时的下界 26（1 工作面 + 16 参考帧 + 8 队列 + 1 渲染器）
        // → 取 26，≈78MB。
        assert_eq!(
            d3d11va_pool_size(
                fallback_ctx_h264(),
                1920,
                1088,
                AVPixelFormat_AV_PIX_FMT_NV12,
                D3D11VA_POOL_BUDGET_BYTES,
                None,
                8
            ),
            26
        );

        // 4K 10bit（P010 23.91MB/片）：预算只折算得出 2 片，仍然不能击穿下界。
        // 这是刻意的：高分辨率省内存靠的是"不再乘 60 片"，不是靠赌参考帧数。
        assert_eq!(
            d3d11va_pool_size(
                fallback_ctx_h264(),
                3840,
                2176,
                AVPixelFormat_AV_PIX_FMT_P010LE,
                D3D11VA_POOL_BUDGET_BYTES,
                None,
                8
            ),
            26
        );

        // 720P（1.38MB/片）：预算折算 48 片，落在那下界与旧固定值之间 → 用预算值，
        // 池总内存仍被 64MiB 预算框住。
        assert_eq!(
            d3d11va_pool_size(
                fallback_ctx_h264(),
                1280,
                720,
                AVPixelFormat_AV_PIX_FMT_NV12,
                D3D11VA_POOL_BUDGET_BYTES,
                None,
                8
            ),
            48
        );

        // VP9/AV1 的参考帧上限是 8，队列 8 时下界降到 18；1080P 的预算折算 21
        // 高于下界，所以取 21。
        assert_eq!(
            d3d11va_pool_size(
                fake_ctx(AVCodecID_AV_CODEC_ID_AV1, 0),
                1920,
                1088,
                AVPixelFormat_AV_PIX_FMT_NV12,
                D3D11VA_POOL_BUDGET_BYTES,
                None,
                8
            ),
            21
        );
    }

    /// 下界跟着 handoff 队列容量走：队列 8→3 时 1080P 池 26→21 片（≈78→63MB），
    /// 两者留给参考帧的余量相同（见 `d3d11va_pool_floor` 的推导）。
    ///
    /// 这条测试就是"队列与池深必须同源"的回归线：谁把队列容量从
    /// `DecoderConfig` 到建池的那条链路改断，这里立刻红。全部传
    /// `fallback_ctx_h264()` 固定在"无 SPS 实测值"的保守分支。
    #[cfg(target_os = "windows")]
    #[test]
    fn d3d11va_pool_floor_tracks_the_consumer_queue_capacity() {
        use sys::AVCodecID_AV_CODEC_ID_AV1;

        // 1080P H.264：下界 = 1 + 16 + (队列 + 1)。
        for (queue, expected) in [(3, 21), (4, 22), (8, 26), (16, 34)] {
            assert_eq!(
                d3d11va_pool_floor(fallback_ctx_h264(), queue),
                expected,
                "queue={queue}"
            );
        }
        // VP9/AV1 参考帧上限 8：1080P 折算 21 片，队列 3 时下界 13 < 21 → 取预算值。
        assert_eq!(
            d3d11va_pool_floor(fake_ctx(AVCodecID_AV_CODEC_ID_AV1, 0), 3),
            13
        );
        assert_eq!(
            d3d11va_pool_size(
                fake_ctx(AVCodecID_AV_CODEC_ID_AV1, 0),
                1920,
                1088,
                sys::AVPixelFormat_AV_PIX_FMT_NV12,
                D3D11VA_POOL_BUDGET_BYTES,
                None,
                3
            ),
            21
        );
        // 1080P H.264 + 队列 3：预算折算 21 片 == 下界 21 → 21 片 ≈ 63MB。
        assert_eq!(
            d3d11va_pool_size(
                fallback_ctx_h264(),
                1920,
                1088,
                sys::AVPixelFormat_AV_PIX_FMT_NV12,
                D3D11VA_POOL_BUDGET_BYTES,
                None,
                3
            ),
            21
        );
    }

    /// H.264 走 SPS 实测参考帧数：池深按实测 `num_ref_frames` 降下来。
    ///
    /// **实测值只影响"下界"，而下界只在预算折算值低于它时才决定池深** ——
    /// 这是本测试最重要的一条断言（见下面 1080P 那一段）。B 站 1080P 的
    /// `num_ref_frames` 多为 2~4，但 1080P 的池深**一点没变**；真正省下的是
    /// 1440P 与 4K。
    ///
    /// 伪造 context 的做法与理由见同模块的 `fake_ctx` / `fallback_ctx_h264`。
    #[cfg(target_os = "windows")]
    #[test]
    fn d3d11va_pool_size_uses_sps_reference_frames_for_h264() {
        use sys::{AVCodecID_AV_CODEC_ID_AV1, AVCodecID_AV_CODEC_ID_H264};

        // 1080P H.264 + SPS 实测 ref=4 + 队列 3：
        // 下界 = 1 + 4 + (3 + 1) = 9 —— 实测值确实进了下界公式。
        let h264 = fake_ctx(AVCodecID_AV_CODEC_ID_H264, 4);
        assert_eq!(d3d11va_sps_reference_surfaces(h264), Some(4));
        assert_eq!(d3d11va_pool_floor(h264, 3), 9);
        // ⚠️ **1080P 的池深并没有因此变浅**：预算 64MiB / 2.99MB 折算出 21 片，
        // 而旧下界也是 21（1+16+4）→ 两者都取 21，内存不变（63MB）。
        //
        // 这一点必须写死成断言，否则很容易像本文档初稿那样误以为"降参考帧 =
        // 降池深"。真实结论：**1080P 的池深由 `D3D11VA_POOL_BUDGET_BYTES`
        // 预算封顶，不是由参考帧下界封顶** —— 预算 21 片恰好等于旧下界 21。
        // 收益只出现在预算折算值**低于**下界的分辨率上（1440P / 4K）。
        assert_eq!(
            d3d11va_pool_size(
                h264,
                1920,
                1088,
                sys::AVPixelFormat_AV_PIX_FMT_NV12,
                D3D11VA_POOL_BUDGET_BYTES,
                None,
                3
            ),
            21,
            "1080P 由预算封顶，SPS 实测值在此不改变池深"
        );

        // 4K 10bit（P010 23.91MB/片）：预算只折算出 2 片，**下界是唯一约束**，
        // 于是实测参考帧直接决定池深 —— 这才是本项优化的真实收益面。
        // ref=16（旧）→ 下界 26 → 622MB；ref=4（实测）→ 下界 14 → 335MB。
        assert_eq!(
            D3D11VA_POOL_BUDGET_BYTES
                / d3d11va_slice_bytes(3840, 2176, sys::AVPixelFormat_AV_PIX_FMT_P010LE),
            2,
            "4K 预算折算恒为 2 片（下界才是约束）"
        );
        assert_eq!(
            d3d11va_pool_size(
                fake_ctx(AVCodecID_AV_CODEC_ID_H264, 16),
                3840,
                2176,
                sys::AVPixelFormat_AV_PIX_FMT_P010LE,
                D3D11VA_POOL_BUDGET_BYTES,
                None,
                8
            ),
            26,
            "4K + 队列 8 + ref 16：下界 26（= 旧行为）"
        );
        assert_eq!(
            d3d11va_pool_size(
                fake_ctx(AVCodecID_AV_CODEC_ID_H264, 4),
                3840,
                2176,
                sys::AVPixelFormat_AV_PIX_FMT_P010LE,
                D3D11VA_POOL_BUDGET_BYTES,
                None,
                8
            ),
            14,
            "4K + 队列 8 + 实测 ref 4：下界 1+4+(8+1)=14（旧为 26）→ 省 12 片 ≈ 287MB"
        );

        // 实测 ref=16 必须与旧的规范上限路径完全一致（不回归）。
        assert_eq!(
            d3d11va_pool_size(
                fake_ctx(AVCodecID_AV_CODEC_ID_H264, 16),
                1920,
                1088,
                sys::AVPixelFormat_AV_PIX_FMT_NV12,
                D3D11VA_POOL_BUDGET_BYTES,
                None,
                8
            ),
            26
        );

        // 非 H.264 一律忽略 avctx->refs（哪怕它是 16 这个"看起来很真"的值）——
        // 因为 HEVC/VP9/AV1 的解码器根本不写该字段，写进去的是选项默认值 1。
        // 误采信会少算 15 片参考帧 = 必现 get_buffer failed。
        for codec in [
            AVCodecID_AV_CODEC_ID_AV1,
            sys::AVCodecID_AV_CODEC_ID_HEVC,
            sys::AVCodecID_AV_CODEC_ID_VP9,
        ] {
            let fake = fake_ctx(codec, 16);
            assert_eq!(
                d3d11va_sps_reference_surfaces(fake),
                None,
                "codec={codec:?} 不该采信 avctx->refs"
            );
            assert_eq!(
                d3d11va_effective_reference_surfaces(fake),
                d3d11va_reference_surfaces(codec),
                "非 H.264 必须落回规范上限"
            );
        }

        // refs 还没被解码器填过（0）→ 不可信 → 落回规范上限。
        assert_eq!(d3d11va_sps_reference_surfaces(fallback_ctx_h264()), None);
    }

    /// `DecoderConfig` 的兜底队列容量必须与 `PlayerConfig` 的默认值一致，
    /// 否则"没有 Player 层队列"的直接调用会拿到与默认 Player 不同的池。
    #[test]
    fn decoder_config_default_queue_matches_player_config_default() {
        assert_eq!(
            DecoderConfig::default().d3d11va_consumer_queue_frames,
            crate::core::PlayerConfig::default().video_frame_queue_capacity as i32
        );
    }

    /// 配置项：下界覆盖值生效，预算放大也不会超过旧的固定池深。
    #[cfg(target_os = "windows")]
    #[test]
    fn d3d11va_pool_size_respects_overrides_and_legacy_ceiling() {
        use sys::AVPixelFormat_AV_PIX_FMT_NV12;

        // 下界覆盖值优先于队列推导值（逃生舱：救援/诊断时手工钉死池深）。
        assert_eq!(
            d3d11va_pool_size(
                fallback_ctx_h264(),
                1920,
                1088,
                AVPixelFormat_AV_PIX_FMT_NV12,
                D3D11VA_POOL_BUDGET_BYTES,
                Some(21),
                8
            ),
            21
        );

        // 预算再大也封顶在旧固定值 60/40/10，不会因为预算富裕而多占内存。
        assert_eq!(
            d3d11va_pool_size(
                fallback_ctx_h264(),
                1280,
                720,
                AVPixelFormat_AV_PIX_FMT_NV12,
                1 << 40,
                None,
                8
            ),
            60
        );

        // 预算小到折算为 0 片时仍然保底在下界。
        assert_eq!(
            d3d11va_pool_size(
                fallback_ctx_h264(),
                1280,
                720,
                AVPixelFormat_AV_PIX_FMT_NV12,
                1 << 20,
                None,
                8
            ),
            26
        );
    }

    #[test]
    fn avseek_force_does_not_change_seek_origin() {
        let forced_set = SEEK_SET | sys::AVSEEK_FORCE as c_int;
        let forced_size = sys::AVSEEK_SIZE as c_int | sys::AVSEEK_FORCE as c_int;

        assert_eq!(normalize_avio_seek_whence(forced_set), SEEK_SET);
        assert_eq!(
            normalize_avio_seek_whence(forced_size),
            sys::AVSEEK_SIZE as c_int
        );
    }

    #[test]
    fn software_av1_preserves_hardware_decoder_selection() {
        let (codec, operation) = ffmpeg_hardware_decoder(sys::AVCodecID_AV_CODEC_ID_AV1);
        assert_eq!(operation, "avcodec_find_decoder_by_name(av1)");
        assert!(!codec.is_null());
        let (software, _) = software_decoder(sys::AVCodecID_AV_CODEC_ID_AV1);
        assert!(!software.is_null());
        assert_ne!(codec, software);
    }

    #[test]
    fn software_uses_available_ffmpeg_av1_decoder() {
        let (codec, operation) = software_decoder(sys::AVCodecID_AV_CODEC_ID_AV1);
        assert_eq!(operation, "avcodec_find_decoder_by_name(libdav1d)");
        assert!(!codec.is_null());
    }

    #[test]
    fn software_av1_decodes_eight_and_ten_bit_frames() {
        for (fixture, format, bytes_per_sample) in [
            ("av1-8bit.ivf", PlanarPixelFormat::Nv12, 1),
            ("av1-10bit.ivf", PlanarPixelFormat::P010, 2),
        ] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("testdata/software")
                .join(fixture);
            let mut demuxer = Demuxer::open_path(path).unwrap();
            let mut decoder = demuxer.open_decoder(0).unwrap();
            assert_eq!(decoder.backend(), DecoderBackend::Software);
            let mut count = 0;
            let mut check_frame = |frame: Frame| {
                assert!(!frame.has_hw_frames_context());
                let planar = frame.to_planar_frame().expect("CPU AV1 frame upload");
                assert_eq!(planar.format, format);
                assert_eq!((planar.width, planar.height), (64, 48));
                assert_eq!(planar.luma.len(), 64 * 48 * bytes_per_sample);
                assert_eq!(planar.chroma.len(), 32 * 24 * 2 * bytes_per_sample);
                assert!(planar.luma.iter().any(|value| *value != planar.luma[0]));
                count += 1;
            };
            while let Some(packet) = demuxer.read_packet().unwrap() {
                decoder.send_packet(&packet).unwrap();
                while let DecoderOutputFrame::Frame(frame) = decoder.receive_frame().unwrap() {
                    check_frame(frame);
                }
            }
            decoder.send_eof().unwrap();
            while let DecoderOutputFrame::Frame(frame) = decoder.receive_frame().unwrap() {
                check_frame(frame);
            }
            assert!(decoder.is_end_of_stream());
            assert_eq!(count, 2, "{fixture}");
        }
    }

    #[test]
    fn mp3_attached_picture_is_not_a_video_track() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/software/mp3-attached-pic.mp3");
        let probe = probe_path(path).unwrap();
        assert!(
            probe
                .tracks
                .iter()
                .all(|track| track.kind != TrackKind::Video),
            "embedded cover art must not appear as a video track: {:?}",
            probe.tracks,
        );
        assert!(
            probe.video.is_empty(),
            "no video probe entries for cover art"
        );
        assert_eq!(
            probe
                .tracks
                .iter()
                .filter(|track| track.kind == TrackKind::Audio)
                .count(),
            1,
            "the audio track stays discoverable"
        );
    }

    #[test]
    fn playback_fixture_exposes_video_technical_metadata() {
        let path = std::env::var_os("ERIKA_PLAYBACK_FIXTURE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/playback/playback-fixture.mkv")
            });
        let probe = probe_path(path).unwrap();
        let video = probe
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap();

        assert_eq!(video.codec.as_deref(), Some("mpeg4"));
        assert_eq!((video.width, video.height), (160, 90));
        assert_eq!(video.bit_rate, None);
        assert_eq!(video.frame_rate, FrameRate::new(30, 1));
    }

    #[test]
    fn video_bit_rate_inference_requires_unambiguous_track_metadata() {
        let mut video = TrackInfo::embedded(0, TrackKind::Video);
        let mut audio = TrackInfo::embedded(1, TrackKind::Audio);
        audio.bit_rate = Some(128_000);
        let mut tracks = vec![video.clone(), audio.clone()];

        infer_single_video_bit_rate(&mut tracks, Some(1_128_000));

        assert_eq!(tracks[0].bit_rate, Some(1_000_000));

        let mut subtitle = TrackInfo::embedded(2, TrackKind::Subtitle);
        subtitle.bit_rate = Some(128_000);
        let mut tracks = vec![video.clone(), audio.clone(), subtitle.clone()];
        infer_single_video_bit_rate(&mut tracks, Some(1_256_000));
        assert_eq!(tracks[0].bit_rate, Some(1_000_000));

        subtitle.bit_rate = None;
        let mut tracks = vec![video.clone(), audio.clone(), subtitle];
        infer_single_video_bit_rate(&mut tracks, Some(1_256_000));
        assert_eq!(tracks[0].bit_rate, None);

        audio.bit_rate = None;
        let mut tracks = vec![video.clone(), audio];
        infer_single_video_bit_rate(&mut tracks, Some(1_128_000));
        assert_eq!(tracks[0].bit_rate, None);

        video.bit_rate = Some(900_000);
        let mut tracks = vec![video, TrackInfo::embedded(2, TrackKind::Video)];
        infer_single_video_bit_rate(&mut tracks, Some(1_128_000));
        assert_eq!(tracks[1].bit_rate, None);
    }

    #[test]
    fn time_base_converts_packet_timestamps() {
        let timestamp = PacketTimestamp {
            raw: 24,
            time_base: TimeBase { num: 1, den: 24 },
        };
        assert_eq!(timestamp.seconds(), 1.0);
        assert_eq!(timestamp.as_duration(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn downmix_normalization_only_applies_when_channels_are_reduced() {
        assert!(downmix_requires_normalization(6, 2));
        assert!(downmix_requires_normalization(8, 2));
        assert!(downmix_requires_normalization(6, 5));
        assert!(!downmix_requires_normalization(2, 2));
        assert!(!downmix_requires_normalization(1, 2));
        assert!(!downmix_requires_normalization(2, 6));
    }

    #[cfg(not(erika_ffmpeg_legacy_channel_layout))]
    /// Builds an interleaved f32 frame holding a constant level per channel.
    /// `levels` is in the default layout order for its length (5.1 is
    /// FL FR FC LFE BL BR).
    fn audio_frame_with_levels(levels: &[f32], sample_rate: i32, samples: i32) -> Frame {
        let channels = levels.len();
        let frame = Frame::alloc(TimeBase {
            num: 1,
            den: sample_rate,
        })
        .unwrap();
        unsafe {
            (*frame.ptr).format = sys::AVSampleFormat_AV_SAMPLE_FMT_FLT;
            (*frame.ptr).sample_rate = sample_rate;
            (*frame.ptr).nb_samples = samples;
            sys::av_channel_layout_default(&mut (*frame.ptr).ch_layout, channels as i32);
            check(
                sys::av_frame_get_buffer(frame.ptr, 0),
                "av_frame_get_buffer",
            )
            .unwrap();
            let data = (*frame.ptr).extended_data.read().cast::<f32>();
            for sample in 0..samples as usize {
                for (channel, level) in levels.iter().enumerate() {
                    data.add(sample * channels + channel).write(*level);
                }
            }
        }
        frame
    }

    fn full_scale_audio_frame(channels: i32, sample_rate: i32, samples: i32) -> Frame {
        audio_frame_with_levels(&vec![1.0; channels as usize], sample_rate, samples)
    }

    #[cfg(not(erika_ffmpeg_legacy_channel_layout))]
    fn downmix_peak_to_stereo(levels: &[f32]) -> f32 {
        let frame = audio_frame_with_levels(levels, 48_000, 1024);
        let mut resampler =
            AudioResampler::new_from_frame(&frame, PcmFormat::f32_interleaved(48_000, 2)).unwrap();
        let pcm = resampler.convert(&frame).unwrap();
        assert!(pcm.frames > 0);
        pcm.samples
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
    }

    #[cfg(not(erika_ffmpeg_legacy_channel_layout))]
    #[test]
    fn surround_downmix_to_stereo_does_not_clip_realistic_content() {
        // FL FR FC LFE BL BR. Un-normalized these peak at 1.27 / 1.40 / 0.98,
        // so the first two clip against FFmpeg's default matrix.
        for (label, levels) in [
            ("typical mixed levels", [0.7, 0.7, 0.5, 0.0, 0.3, 0.3]),
            ("music-heavy", [0.9, 0.9, 0.2, 0.0, 0.5, 0.5]),
            ("dialogue-heavy", [0.2, 0.2, 1.0, 0.0, 0.1, 0.1]),
        ] {
            let peak = downmix_peak_to_stereo(&levels);
            assert!(
                peak <= 1.0 + 1e-4,
                "{label}: downmixed peak {peak} exceeds full scale"
            );
        }
    }

    #[cfg(not(erika_ffmpeg_legacy_channel_layout))]
    #[test]
    fn surround_downmix_normalizes_to_the_power_sum_not_the_amplitude_sum() {
        // Every channel at digital full scale is the fully correlated worst
        // case the amplitude sum guards against. Normalizing to it would cost
        // 7.7 dB on all surround content; we normalize to the power sum
        // instead, so this synthetic signal is allowed to reach the ceiling.
        let peak = downmix_peak_to_stereo(&[1.0, 1.0, 1.0, 0.0, 1.0, 1.0]);
        assert!(
            (f64::from(peak) - DOWNMIX_REMATRIX_MAXVAL).abs() < 1e-3,
            "worst-case peak {peak} should sit at the rematrix ceiling {DOWNMIX_REMATRIX_MAXVAL}"
        );

        // Guard the loudness side. Fronts-only input passes through the matrix
        // at unity, so what is left is purely the normalization scale:
        // the amplitude-sum bound (maxval 1.0) would leave 0.41 here (-7.7 dB),
        // the power-sum bound leaves 0.59 (-4.6 dB).
        let fronts = downmix_peak_to_stereo(&[1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        assert!(
            (0.55..0.62).contains(&fronts),
            "fronts-only downmix {fronts} left the expected -4.6 dB window"
        );
    }

    #[cfg(not(erika_ffmpeg_legacy_channel_layout))]
    #[test]
    fn stereo_passthrough_keeps_unity_gain() {
        let frame = full_scale_audio_frame(2, 48_000, 512);
        let mut resampler =
            AudioResampler::new_from_frame(&frame, PcmFormat::f32_interleaved(48_000, 2)).unwrap();
        let pcm = resampler.convert(&frame).unwrap();
        assert!(pcm.frames > 0);
        for sample in &pcm.samples {
            assert!(
                (sample - 1.0).abs() < 1e-4,
                "stereo pass-through altered sample: {sample}"
            );
        }
    }

    #[test]
    fn resampler_pts_keeps_continuous_output_and_reanchors_real_discontinuities() {
        let expected = Duration::from_secs(1);
        assert_eq!(
            select_resampler_output_pts(None, Some(expected)),
            (Some(expected), false)
        );
        assert_eq!(
            select_resampler_output_pts(Some(expected), Some(expected + Duration::from_millis(8)),),
            (Some(expected), false)
        );
        assert_eq!(
            select_resampler_output_pts(Some(expected), Some(expected + Duration::from_secs(2)),),
            (Some(expected + Duration::from_secs(2)), true)
        );
        assert_eq!(
            select_resampler_output_pts(Some(expected), None),
            (Some(expected), false)
        );
    }

    #[test]
    fn mpeg_ts_timeline_origin_is_rescaled_into_stream_ticks() {
        assert_eq!(
            rescale_microseconds_to_time_base(
                600_000_000,
                TimeBase {
                    num: 1,
                    den: 90_000,
                },
            ),
            54_000_000
        );
    }

    #[test]
    fn relative_seek_targets_include_the_container_timeline_origin() {
        assert_eq!(
            absolute_seek_target_micros(15_000_000, 600_000_000),
            615_000_000
        );
        assert_eq!(absolute_seek_target_micros(15_000_000, 0), 15_000_000);
    }

    #[test]
    fn stream_selection_accepts_expected_streams() {
        let selection = StreamSelection::only([0, 2]);
        assert!(selection.accepts(0));
        assert!(!selection.accepts(1));
        assert!(selection.accepts(2));
    }

    #[test]
    fn maps_ffmpeg_color_ranges() {
        assert_eq!(
            unsafe { color_range(sys::AVColorRange_AVCOL_RANGE_MPEG) },
            ColorRange::Limited
        );
        assert_eq!(
            unsafe { color_range(sys::AVColorRange_AVCOL_RANGE_JPEG) },
            ColorRange::Full
        );
        assert_eq!(
            unsafe { color_range(sys::AVColorRange_AVCOL_RANGE_UNSPECIFIED) },
            ColorRange::Unspecified
        );
    }

    #[test]
    fn maps_ffmpeg_matrix_coefficients() {
        assert_eq!(
            unsafe { matrix_coefficients(sys::AVColorSpace_AVCOL_SPC_BT709) },
            MatrixCoefficients::Bt709
        );
        assert_eq!(
            unsafe { matrix_coefficients(sys::AVColorSpace_AVCOL_SPC_SMPTE170M) },
            MatrixCoefficients::Bt601
        );
        assert_eq!(
            unsafe { matrix_coefficients(sys::AVColorSpace_AVCOL_SPC_BT470BG) },
            MatrixCoefficients::Bt601
        );
        assert_eq!(
            unsafe { matrix_coefficients(sys::AVColorSpace_AVCOL_SPC_BT2020_NCL) },
            MatrixCoefficients::Bt2020NonConstantLuminance
        );
        assert_eq!(
            unsafe { matrix_coefficients(sys::AVColorSpace_AVCOL_SPC_RGB) },
            MatrixCoefficients::Identity
        );
        assert_eq!(
            unsafe { matrix_coefficients(sys::AVColorSpace_AVCOL_SPC_UNSPECIFIED) },
            MatrixCoefficients::Unspecified
        );
    }

    #[test]
    fn subtitle_probe_marks_embedded_tracks_non_removable() {
        let mut track = TrackInfo::embedded(3, TrackKind::Subtitle);
        track.title = Some("Signs".to_string());
        track.language = Some("jpn".to_string());
        track.codec = Some("hdmv_pgs_subtitle".to_string());

        let config = subtitle_probe(&track);

        assert_eq!(config.id, 3);
        assert_eq!(config.language.as_deref(), Some("jpn"));
        assert_eq!(config.title.as_deref(), Some("Signs"));
        assert!(config.source.is_embedded());
        assert!(!config.can_remove());
    }

    #[test]
    fn imports_real_matroska_ass_chunk_without_rewriting() {
        let packet = Packet::alloc().unwrap();
        let plain = CString::new("hello").unwrap();
        let raw_ass = "1,0,Song - CN,sign,12,34,56,banner,{\\pos(100,200)\\clip(0,0,300,200)\\t(0,500,\\blur6)\\fad(100,200)}hi";
        let ass = CString::new(raw_ass).unwrap();
        let resources = Arc::new(AssTrackResources::new(
            7,
            Arc::<[u8]>::from(
                b"[Script Info]\n[V4+ Styles]\nStyle: Song - CN,Arial,32\n[Events]\n".as_slice(),
            ),
            Arc::<[SubtitleFontAttachment]>::from([]),
        ));
        let mut text_rect = sys::AVSubtitleRect {
            type_: sys::AVSubtitleType_SUBTITLE_TEXT,
            text: plain.as_ptr().cast_mut(),
            flags: sys::AV_SUBTITLE_FLAG_FORCED as i32,
            ..sys::AVSubtitleRect::default()
        };
        let mut ass_rect = sys::AVSubtitleRect {
            type_: sys::AVSubtitleType_SUBTITLE_ASS,
            ass: ass.as_ptr().cast_mut(),
            ..sys::AVSubtitleRect::default()
        };
        let mut rects = [&mut text_rect as *mut _, &mut ass_rect as *mut _];
        let subtitle = sys::AVSubtitle {
            start_display_time: 250,
            end_display_time: 1250,
            num_rects: rects.len() as u32,
            rects: rects.as_mut_ptr(),
            pts: 1_000_000,
            ..sys::AVSubtitle::default()
        };

        let frame =
            unsafe { import_av_subtitle(7, &packet, &subtitle, (0, 0), Some(resources.clone())) }
                .unwrap();

        assert_eq!(frame.track_id, 7);
        assert_eq!(frame.start, Some(Duration::from_millis(1250)));
        assert_eq!(frame.end, Some(Duration::from_millis(2250)));
        assert_eq!(frame.text.len(), 2);
        assert_eq!(frame.text[0].format, SubtitleTextFormat::PlainText);
        assert_eq!(frame.text[0].text, "hello");
        assert!(frame.text[0].forced);
        assert_eq!(frame.text[1].format, SubtitleTextFormat::Ass);
        assert_eq!(frame.text[1].text, raw_ass);
        assert!(Arc::ptr_eq(frame.ass_track.as_ref().unwrap(), &resources));
        assert!(frame.forced);
    }

    #[test]
    fn imports_palette_bitmap_subtitle_as_rgba_plane() {
        let packet = Packet::alloc().unwrap();
        let pixels = [0u8, 1, 2, 1];
        let palette = [0x00000000u32, 0x804020ff, 0xff00ff80];
        let mut rect = sys::AVSubtitleRect {
            x: 11,
            y: 22,
            w: 2,
            h: 2,
            nb_colors: palette.len() as i32,
            data: [
                pixels.as_ptr().cast_mut(),
                palette.as_ptr().cast::<u8>().cast_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ],
            linesize: [2, 0, 0, 0],
            type_: sys::AVSubtitleType_SUBTITLE_BITMAP,
            ..sys::AVSubtitleRect::default()
        };
        let mut rects = [&mut rect as *mut _];
        let subtitle = sys::AVSubtitle {
            num_rects: 1,
            rects: rects.as_mut_ptr(),
            pts: 2_000_000,
            start_display_time: 0,
            end_display_time: 500,
            ..sys::AVSubtitle::default()
        };

        let frame =
            unsafe { import_av_subtitle(9, &packet, &subtitle, (1920, 1080), None) }.unwrap();

        assert_eq!(frame.start, Some(Duration::from_secs(2)));
        assert_eq!(frame.end, Some(Duration::from_millis(2500)));
        assert_eq!(frame.bitmap.planes.len(), 1);
        let plane = &frame.bitmap.planes[0];
        assert_eq!(
            (plane.x, plane.y, plane.width, plane.height),
            (11, 22, 2, 2)
        );
        assert_eq!((plane.canvas_width, plane.canvas_height), (1920, 1080));
        assert_eq!(
            plane.rgba,
            vec![
                0, 0, 0, 0, 0x40, 0x20, 0xff, 0x80, 0x00, 0xff, 0x80, 0xff, 0x40, 0x20, 0xff, 0x80,
            ]
        );
    }

    #[test]
    fn ass_codec_private_copy_enforces_byte_limit() {
        let header = b"[Script Info]\n";
        let copied = copy_ass_codec_private(header.as_ptr(), header.len() as i32, 2, "test")
            .expect("small header should be copied");
        assert_eq!(&*copied, header);

        assert!(
            copy_ass_codec_private(
                header.as_ptr(),
                (MAX_ASS_CODEC_PRIVATE_BYTES + 1) as i32,
                2,
                "test",
            )
            .is_none()
        );
    }

    #[test]
    fn validates_true_type_attachment_and_extracts_family_names() {
        let mut stream = sys::AVStream {
            index: 4,
            ..sys::AVStream::default()
        };
        let mut parameters = sys::AVCodecParameters {
            codec_type: sys::AVMediaType_AVMEDIA_TYPE_ATTACHMENT,
            codec_id: sys::AVCodecID_AV_CODEC_ID_TTF,
            extradata: crate::NIPAPLAY_FALLBACK_FONT.as_ptr().cast_mut(),
            extradata_size: crate::NIPAPLAY_FALLBACK_FONT.len() as i32,
            ..sys::AVCodecParameters::default()
        };

        let font = unsafe { subtitle_font_attachment(&mut stream, &mut parameters, 0, 0) }
            .expect("bundled TTF should validate");

        assert_eq!(font.byte_len(), crate::NIPAPLAY_FALLBACK_FONT.len());
        assert!(
            font.families
                .iter()
                .any(|family| family == "Droid Sans Fallback")
        );
    }

    #[test]
    fn rejects_font_before_copy_when_attachment_limits_are_exceeded() {
        let mut stream = sys::AVStream {
            index: 9,
            ..sys::AVStream::default()
        };
        let byte = 0u8;
        let mut parameters = sys::AVCodecParameters {
            codec_type: sys::AVMediaType_AVMEDIA_TYPE_ATTACHMENT,
            codec_id: sys::AVCodecID_AV_CODEC_ID_TTF,
            extradata: (&byte as *const u8).cast_mut(),
            extradata_size: 1,
            ..sys::AVCodecParameters::default()
        };

        assert!(
            unsafe {
                subtitle_font_attachment(
                    &mut stream,
                    &mut parameters,
                    MAX_SUBTITLE_FONT_ATTACHMENTS,
                    0,
                )
            }
            .is_none()
        );

        parameters.extradata_size = (MAX_SUBTITLE_FONT_ATTACHMENT_BYTES + 1) as i32;
        assert!(unsafe { subtitle_font_attachment(&mut stream, &mut parameters, 0, 0) }.is_none());

        parameters.extradata_size = 1;
        assert!(
            unsafe {
                subtitle_font_attachment(
                    &mut stream,
                    &mut parameters,
                    0,
                    MAX_SUBTITLE_FONT_TOTAL_BYTES,
                )
            }
            .is_none()
        );
    }

    #[test]
    fn playback_fixture_has_no_dolby_vision_profile() {
        let path = std::env::var_os("ERIKA_PLAYBACK_FIXTURE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/playback/playback-fixture.mkv")
            });
        let demuxer = Demuxer::open_path(&path).unwrap();
        let video = demuxer
            .probe()
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap();

        let parameters = demuxer.owned_codec_parameters(video.id as i32).unwrap();
        assert_eq!(parameters.dolby_vision_profile(), None);
    }

    #[test]
    fn dv_sample_reports_dolby_vision_profile() {
        let Some(path) = std::env::var_os("ERIKA_DV_SAMPLE") else {
            return;
        };
        let demuxer = Demuxer::open_path(&path).unwrap();
        let video = demuxer
            .probe()
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .expect("DV sample must contain a video stream");

        let parameters = demuxer.owned_codec_parameters(video.id as i32).unwrap();
        assert_eq!(parameters.dolby_vision_profile(), Some(5));
    }

    #[test]
    fn dv_profile_8_sample_reports_profile() {
        let Some(path) = std::env::var_os("ERIKA_DV_PROFILE_8_SAMPLE") else {
            return;
        };
        let demuxer = Demuxer::open_path(&path).unwrap();
        let video = demuxer
            .probe()
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .expect("DV Profile 8 sample must contain a video stream");

        let parameters = demuxer.owned_codec_parameters(video.id as i32).unwrap();
        assert_eq!(parameters.dolby_vision_profile(), Some(8));
    }

    #[test]
    fn real_ass_container_preserves_header_fonts_and_matroska_chunk_when_env_is_set() {
        let Ok(path) = std::env::var("ERIKA_ASS_SAMPLE") else {
            return;
        };
        let mut demuxer = Demuxer::open_path(&path).unwrap();
        let subtitle_stream = demuxer
            .probe()
            .tracks
            .iter()
            .find(|track| {
                track.kind == TrackKind::Subtitle && track.codec.as_deref() == Some("ass")
            })
            .map(|track| track.id as i32)
            .expect("sample should contain an ASS subtitle stream");
        assert!(!demuxer.probe().subtitle_fonts.is_empty());
        assert!(
            demuxer
                .probe()
                .subtitle_fonts
                .iter()
                .all(|font| !font.families.is_empty() && !font.data.is_empty())
        );

        let mut decoder = demuxer.open_subtitle_decoder(subtitle_stream).unwrap();
        demuxer
            .set_stream_selection(StreamSelection::only([subtitle_stream]))
            .unwrap();
        let frame = loop {
            let packet = demuxer
                .read_packet()
                .unwrap()
                .expect("sample should contain an ASS packet");
            if let Some(frame) = decoder.decode_packet(&packet).unwrap() {
                if frame.has_ass_chunks() {
                    break frame;
                }
            }
        };

        let resources = frame
            .ass_track
            .as_ref()
            .expect("decoded ASS frame should retain track resources");
        let header = std::str::from_utf8(&resources.codec_private).unwrap();
        assert!(header.contains("[Script Info]"));
        assert!(header.contains("[V4+ Styles]"));
        assert!(header.contains("[Events]"));
        assert_eq!(resources.fonts.len(), demuxer.probe().subtitle_fonts.len());
        let chunk = frame
            .text
            .iter()
            .find(|segment| segment.format == SubtitleTextFormat::Ass)
            .expect("decoded frame should contain an ASS chunk");
        assert_eq!(chunk.text.splitn(9, ',').count(), 9);
        assert!(!chunk.text.starts_with("Dialogue:"));
        assert!(chunk.text.contains("{\\"));
    }

    #[test]
    fn rejects_malformed_bitmap_subtitle_rect() {
        let mut rect = sys::AVSubtitleRect {
            w: 4,
            h: 2,
            linesize: [3, 0, 0, 0],
            nb_colors: 1,
            type_: sys::AVSubtitleType_SUBTITLE_BITMAP,
            ..sys::AVSubtitleRect::default()
        };
        let pixels = [0u8; 8];
        let palette = [0xffffffffu32];
        rect.data[0] = pixels.as_ptr().cast_mut();
        rect.data[1] = palette.as_ptr().cast::<u8>().cast_mut();

        let error = unsafe { subtitle_bitmap_rect_to_rgba_plane(&rect) }.unwrap_err();

        assert!(matches!(error, FfmpegError::InvalidSubtitleBitmap { .. }));
    }

    #[test]
    fn import_av_subtitle_skips_a_malformed_bitmap_rect_instead_of_failing() {
        let mut malformed = sys::AVSubtitleRect {
            w: 4,
            h: 2,
            linesize: [3, 0, 0, 0],
            nb_colors: 1,
            type_: sys::AVSubtitleType_SUBTITLE_BITMAP,
            ..sys::AVSubtitleRect::default()
        };
        let pixels = [0u8; 8];
        let palette = [0xffffffffu32];
        malformed.data[0] = pixels.as_ptr().cast_mut();
        malformed.data[1] = palette.as_ptr().cast::<u8>().cast_mut();

        let caption = CString::new("caption").unwrap();
        let mut text_rect = sys::AVSubtitleRect {
            type_: sys::AVSubtitleType_SUBTITLE_TEXT,
            text: caption.as_ptr().cast_mut(),
            ..sys::AVSubtitleRect::default()
        };
        let mut rects: [*mut sys::AVSubtitleRect; 2] = [&mut malformed, &mut text_rect];
        let subtitle = sys::AVSubtitle {
            num_rects: 2,
            rects: rects.as_mut_ptr(),
            ..sys::AVSubtitle::default()
        };

        let packet = Packet::alloc().unwrap();
        let frame = unsafe {
            import_av_subtitle(7, &packet, &subtitle, (64, 48), None)
                .expect("one malformed bitmap rect must not abort the whole subtitle frame")
        };
        assert!(frame.bitmap.planes.is_empty());
        assert_eq!(frame.text.len(), 1);
        assert_eq!(frame.text[0].text, "caption");
    }

    #[test]
    fn p010_downconversion_rounds_to_nv12_and_preserves_video_range_anchors() {
        let pack = |codes: &[u16]| {
            codes
                .iter()
                .flat_map(|code| (*code << 6).to_le_bytes())
                .collect::<Vec<_>>()
        };
        let frame = PlanarFrame {
            format: PlanarPixelFormat::P010,
            width: 4,
            height: 2,
            luma: pack(&[0, 1, 2, 3, 64, 512, 940, 1023]),
            chroma: pack(&[64, 960, 512, 1023]),
        };

        let converted = frame.downconvert_p010_to_nv12().unwrap();

        assert_eq!(converted.format, PlanarPixelFormat::Nv12);
        assert_eq!((converted.width, converted.height), (4, 2));
        assert_eq!(converted.luma, vec![0, 0, 1, 1, 16, 128, 235, 255]);
        assert_eq!(converted.chroma, vec![16, 240, 128, 255]);
    }

    #[test]
    fn p010_downconversion_rejects_malformed_planes() {
        let error = PlanarFrame {
            format: PlanarPixelFormat::P010,
            width: 2,
            height: 2,
            luma: vec![0; 6],
            chroma: vec![0; 4],
        }
        .downconvert_p010_to_nv12()
        .unwrap_err();

        assert!(matches!(
            error,
            PlanarFrameConversionError::InvalidP010PlaneSize {
                plane: "luma",
                actual: 6,
                expected: 8,
                ..
            }
        ));
    }

    #[test]
    fn bgra_software_frame_falls_back_to_nv12() {
        let frame = Frame::alloc(TimeBase { num: 1, den: 1 }).unwrap();
        let mut pixels = vec![
            0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 255, 255, 0, 255, 0, 255,
        ];
        unsafe {
            (*frame.ptr).width = 2;
            (*frame.ptr).height = 2;
            (*frame.ptr).format = sys::AVPixelFormat_AV_PIX_FMT_BGRA;
            (*frame.ptr).data[0] = pixels.as_mut_ptr();
            (*frame.ptr).linesize[0] = 8;
        }

        let planar = frame.to_planar_frame().unwrap();

        assert_eq!(planar.format, PlanarPixelFormat::Nv12);
        assert_eq!((planar.width, planar.height), (2, 2));
        assert_eq!(planar.luma.len(), 4);
        assert_eq!(planar.chroma.len(), 2);
    }

    #[test]
    fn yuv444p_software_frame_falls_back_to_nv12() {
        let frame = Frame::alloc(TimeBase { num: 1, den: 1 }).unwrap();
        let mut y = vec![16, 64, 128, 235];
        let mut u = vec![128; 4];
        let mut v = vec![128; 4];
        unsafe {
            (*frame.ptr).width = 2;
            (*frame.ptr).height = 2;
            (*frame.ptr).format = sys::AVPixelFormat_AV_PIX_FMT_YUV444P;
            (*frame.ptr).data[0] = y.as_mut_ptr();
            (*frame.ptr).data[1] = u.as_mut_ptr();
            (*frame.ptr).data[2] = v.as_mut_ptr();
            (*frame.ptr).linesize[0] = 2;
            (*frame.ptr).linesize[1] = 2;
            (*frame.ptr).linesize[2] = 2;
        }

        let planar = frame.to_planar_frame().unwrap();

        assert_eq!(planar.format, PlanarPixelFormat::Nv12);
        assert_eq!((planar.width, planar.height), (2, 2));
        assert_eq!(planar.luma.len(), 4);
        assert_eq!(planar.chroma.len(), 2);
    }

    #[test]
    fn odd_sized_yuv420p_frame_preserves_ceil_chroma_extent() {
        let frame = Frame::alloc(TimeBase { num: 1, den: 1 }).unwrap();
        let mut y = vec![16, 32, 48, 64, 80, 96, 112, 128, 144];
        let mut u = vec![100, 110, 120, 130];
        let mut v = vec![140, 150, 160, 170];
        unsafe {
            (*frame.ptr).width = 3;
            (*frame.ptr).height = 3;
            (*frame.ptr).format = sys::AVPixelFormat_AV_PIX_FMT_YUV420P;
            (*frame.ptr).data[0] = y.as_mut_ptr();
            (*frame.ptr).data[1] = u.as_mut_ptr();
            (*frame.ptr).data[2] = v.as_mut_ptr();
            (*frame.ptr).linesize[0] = 3;
            (*frame.ptr).linesize[1] = 2;
            (*frame.ptr).linesize[2] = 2;
        }

        let planar = frame.to_planar_frame().unwrap();

        assert_eq!((planar.width, planar.height), (3, 3));
        assert_eq!(planar.luma, y);
        assert_eq!(planar.chroma, vec![100, 140, 110, 150, 120, 160, 130, 170]);
    }

    #[test]
    fn frame_reads_hdr_side_data() {
        let frame = Frame::alloc(TimeBase { num: 1, den: 1 }).unwrap();
        unsafe {
            let mastering = sys::av_mastering_display_metadata_create_side_data(frame.ptr);
            assert!(!mastering.is_null());
            (*mastering).has_primaries = 1;
            (*mastering).display_primaries[0] = [rational(708, 1000), rational(292, 1000)];
            (*mastering).display_primaries[1] = [rational(170, 1000), rational(797, 1000)];
            (*mastering).display_primaries[2] = [rational(131, 1000), rational(46, 1000)];
            (*mastering).white_point = [rational(3127, 10000), rational(3290, 10000)];
            (*mastering).has_luminance = 1;
            (*mastering).min_luminance = rational(5, 1000);
            (*mastering).max_luminance = rational(1000, 1);

            let content_light = sys::av_content_light_metadata_create_side_data(frame.ptr);
            assert!(!content_light.is_null());
            (*content_light).MaxCLL = 4000;
            (*content_light).MaxFALL = 450;
        }

        let metadata = frame.hdr_metadata().unwrap();
        let mastering = metadata.mastering_display.unwrap();
        let content_light = metadata.content_light.unwrap();

        assert_eq!(metadata.nominal_peak_nits(), Some(1000.0));
        assert_eq!(content_light.max_content_light_level_nits, 4000);
        assert_eq!(content_light.max_frame_average_light_level_nits, 450);
        assert_close(mastering.max_luminance_nits.unwrap(), 1000.0);
        assert_close(mastering.min_luminance_nits.unwrap(), 0.005);
        assert_close(mastering.display_primaries.unwrap()[0].x, 0.708);
        assert_close(mastering.white_point.unwrap().y, 0.329);
    }

    fn rational(num: i32, den: i32) -> sys::AVRational {
        sys::AVRational { num, den }
    }

    #[test]
    fn frame_reads_dovi_side_data() {
        let frame = Frame::alloc(TimeBase { num: 1, den: 1 }).unwrap();
        unsafe {
            let mut size = 0_usize;
            let metadata = sys::av_dovi_metadata_alloc(&mut size);
            assert!(!metadata.is_null());
            assert!(size >= mem::size_of::<sys::AVDOVIMetadata>());
            let header = (metadata as *mut u8)
                .add((*metadata).header_offset)
                .cast::<sys::AVDOVIRpuDataHeader>();
            let mapping = (metadata as *mut u8)
                .add((*metadata).mapping_offset)
                .cast::<sys::AVDOVIDataMapping>();
            let color = (metadata as *mut u8)
                .add((*metadata).color_offset)
                .cast::<sys::AVDOVIColorMetadata>();

            (*header).bl_bit_depth = 10;
            (*header).el_bit_depth = 10;
            (*header).coef_log2_denom = 13;
            (*header).disable_residual_flag = 1;

            let curve = &mut (*mapping).curves[0];
            curve.num_pivots = 3;
            curve.pivots[0] = 0;
            curve.pivots[1] = 256;
            curve.pivots[2] = 1023;
            curve.mapping_idc[0] = sys::AVDOVIMappingMethod_AV_DOVI_MAPPING_POLYNOMIAL;
            curve.poly_order[0] = 1;
            curve.poly_coef[0][0] = 0;
            curve.poly_coef[0][1] = 1 << 13;
            curve.poly_coef[0][2] = 0;
            curve.mapping_idc[1] = sys::AVDOVIMappingMethod_AV_DOVI_MAPPING_MMR;
            curve.mmr_order[1] = 1;
            curve.mmr_constant[1] = 1024;
            curve.mmr_coef[1][0][0] = 4096;
            curve.mmr_coef[1][0][6] = -8192;

            (*color).ycc_to_rgb_matrix = [
                rational(9575, 8192),
                rational(0, 8192),
                rational(14742, 8192),
                rational(9575, 8192),
                rational(1754, 8192),
                rational(4383, 8192),
                rational(9575, 8192),
                rational(17372, 8192),
                rational(0, 8192),
            ];
            (*color).ycc_to_rgb_offset = [rational(1, 4), rational(2, 1), rational(2, 1)];
            (*color).rgb_to_lms_matrix = [
                rational(5845, 16384),
                rational(9702, 16384),
                rational(837, 16384),
                rational(2568, 16384),
                rational(12256, 16384),
                rational(1561, 16384),
                rational(0, 16384),
                rational(679, 16384),
                rational(15705, 16384),
            ];
            (*color).source_min_pq = 62;
            (*color).source_max_pq = 3079;

            let side_data = sys::av_frame_new_side_data(
                frame.ptr,
                sys::AVFrameSideDataType_AV_FRAME_DATA_DOVI_METADATA,
                size,
            );
            assert!(!side_data.is_null());
            ptr::copy_nonoverlapping(metadata.cast::<u8>(), (*side_data).data, size);
            sys::av_free(metadata.cast());
        }

        let dovi = frame.dovi_metadata().unwrap();
        let luma = &dovi.reshaping[0];
        assert_eq!(luma.num_pivots, 3);
        assert_close(luma.pivots[1], 256.0 / 1023.0);
        assert_close(luma.pivots[2], 1.0);
        assert_close(luma.poly_coeffs[0][1], 1.0);
        assert_eq!(luma.mmr_orders[1], 1);
        assert_close(luma.mmr_constants[1], 0.125);
        assert_close(luma.mmr_coeffs[1][0][0], 0.5);
        assert_close(luma.mmr_coeffs[1][0][6], -1.0);
        // Chroma and luma curves without pivots stay empty.
        assert_eq!(dovi.reshaping[1].num_pivots, 0);
        assert_close(dovi.nonlinear_matrix.rows()[0][0], 9575.0 / 8192.0);
        assert_close(dovi.nonlinear_offset[0], 0.25);
        assert_close(dovi.rgb_to_lms.rows()[2][2], 15705.0 / 16384.0);
        assert_eq!(dovi.source_min_pq, 62);
        assert_eq!(dovi.source_max_pq, 3079);

        unsafe {
            let side_data = sys::av_frame_get_side_data(
                frame.ptr,
                sys::AVFrameSideDataType_AV_FRAME_DATA_DOVI_METADATA,
            );
            assert!(!side_data.is_null());
            let metadata = *(*side_data).data.cast::<sys::AVDOVIMetadata>();
            let header = &mut *((*side_data)
                .data
                .add(metadata.header_offset)
                .cast::<sys::AVDOVIRpuDataHeader>());
            let mapping = &mut *((*side_data)
                .data
                .add(metadata.mapping_offset)
                .cast::<sys::AVDOVIDataMapping>());
            mapping.curves[0].mmr_order[1] = 0;
            assert_eq!(frame.dovi_metadata(), None);
            header.disable_residual_flag = 0;
            assert_eq!(frame.dovi_metadata(), None);
        }
    }

    #[test]
    fn frame_rejects_truncated_or_invalid_dovi_side_data() {
        let frame = Frame::alloc(TimeBase { num: 1, den: 1 }).unwrap();
        unsafe {
            let side_data = sys::av_frame_new_side_data(
                frame.ptr,
                sys::AVFrameSideDataType_AV_FRAME_DATA_DOVI_METADATA,
                4,
            );
            assert!(!side_data.is_null());
        }
        assert_eq!(frame.dovi_metadata(), None);
    }

    /// Attaches a known-good RPU: 10-bit base layer, `coef_log2_denom = 32`
    /// with the identity polynomial scaled to match, no residual, identity
    /// matrices, valid PQ range.
    unsafe fn attach_valid_dovi_rpu(frame: &Frame) {
        unsafe {
            let mut size = 0_usize;
            let metadata = sys::av_dovi_metadata_alloc(&mut size);
            assert!(!metadata.is_null());
            let header = (metadata as *mut u8)
                .add((*metadata).header_offset)
                .cast::<sys::AVDOVIRpuDataHeader>();
            let mapping = (metadata as *mut u8)
                .add((*metadata).mapping_offset)
                .cast::<sys::AVDOVIDataMapping>();
            let color = (metadata as *mut u8)
                .add((*metadata).color_offset)
                .cast::<sys::AVDOVIColorMetadata>();

            (*header).bl_bit_depth = 10;
            (*header).el_bit_depth = 10;
            (*header).coef_log2_denom = 32;
            (*header).disable_residual_flag = 1;

            let curve = &mut (*mapping).curves[0];
            curve.num_pivots = 2;
            curve.pivots[0] = 0;
            curve.pivots[1] = 1023;
            curve.mapping_idc[0] = sys::AVDOVIMappingMethod_AV_DOVI_MAPPING_POLYNOMIAL;
            curve.poly_order[0] = 1;
            curve.poly_coef[0][0] = 0;
            curve.poly_coef[0][1] = 1_i64 << 32;
            curve.poly_coef[0][2] = 0;

            (*color).ycc_to_rgb_matrix = [
                rational(1, 1),
                rational(0, 1),
                rational(0, 1),
                rational(0, 1),
                rational(1, 1),
                rational(0, 1),
                rational(0, 1),
                rational(0, 1),
                rational(1, 1),
            ];
            (*color).ycc_to_rgb_offset = [rational(0, 1), rational(0, 1), rational(0, 1)];
            (*color).rgb_to_lms_matrix = [
                rational(1, 1),
                rational(0, 1),
                rational(0, 1),
                rational(0, 1),
                rational(1, 1),
                rational(0, 1),
                rational(0, 1),
                rational(0, 1),
                rational(1, 1),
            ];
            (*color).source_min_pq = 0;
            (*color).source_max_pq = 3079;

            let side_data = sys::av_frame_new_side_data(
                frame.ptr,
                sys::AVFrameSideDataType_AV_FRAME_DATA_DOVI_METADATA,
                size,
            );
            assert!(!side_data.is_null());
            ptr::copy_nonoverlapping(metadata.cast::<u8>(), (*side_data).data, size);
            sys::av_free(metadata.cast());
        }
    }

    unsafe fn mutate_dovi_rpu(
        frame: &Frame,
        mutate: impl FnOnce(
            &mut sys::AVDOVIRpuDataHeader,
            &mut sys::AVDOVIDataMapping,
            &mut sys::AVDOVIColorMetadata,
        ),
    ) {
        unsafe {
            let side_data = sys::av_frame_get_side_data(
                frame.ptr,
                sys::AVFrameSideDataType_AV_FRAME_DATA_DOVI_METADATA,
            );
            assert!(!side_data.is_null());
            let metadata = *(*side_data).data.cast::<sys::AVDOVIMetadata>();
            let header = &mut *((*side_data)
                .data
                .add(metadata.header_offset)
                .cast::<sys::AVDOVIRpuDataHeader>());
            let mapping = &mut *((*side_data)
                .data
                .add(metadata.mapping_offset)
                .cast::<sys::AVDOVIDataMapping>());
            let color = &mut *((*side_data)
                .data
                .add(metadata.color_offset)
                .cast::<sys::AVDOVIColorMetadata>());
            mutate(header, mapping, color);
        }
    }

    /// Writes a single dynamic DM level 1 ext block into the side data.
    /// `av_dovi_metadata_alloc` reserves the full ext block array inside the
    /// same allocation, so the first block lands at `ext_block_offset`.
    unsafe fn with_dovi_l1(frame: &Frame, min_pq: u16, max_pq: u16, avg_pq: u16) {
        unsafe {
            let side_data = sys::av_frame_get_side_data(
                frame.ptr,
                sys::AVFrameSideDataType_AV_FRAME_DATA_DOVI_METADATA,
            );
            assert!(!side_data.is_null());
            let metadata = &mut *(*side_data).data.cast::<sys::AVDOVIMetadata>();
            let block = &mut *((*side_data)
                .data
                .add(metadata.ext_block_offset)
                .cast::<sys::AVDOVIDmData>());
            block.level = 1;
            block.__bindgen_anon_1.l1 = sys::AVDOVIDmLevel1 {
                min_pq,
                max_pq,
                avg_pq,
            };
            metadata.num_ext_blocks = 1;
        }
    }

    #[test]
    fn dovi_l1_brightness_metadata_is_parsed() {
        let frame = Frame::alloc(TimeBase { num: 1, den: 1 }).unwrap();
        unsafe { attach_valid_dovi_rpu(&frame) };
        assert_eq!(frame.dovi_metadata().unwrap().l1, None);

        unsafe { with_dovi_l1(&frame, 62, 2200, 1500) };
        let l1 = frame
            .dovi_metadata()
            .unwrap()
            .l1
            .expect("level 1 block should be parsed");
        assert_eq!(
            l1,
            DoviFramePq {
                min_pq: 62,
                max_pq: 2200,
                avg_pq: 1500
            }
        );

        // Inverted min/max and an all-zero block are treated as absent.
        unsafe { with_dovi_l1(&frame, 2200, 100, 1500) };
        assert_eq!(frame.dovi_metadata().unwrap().l1, None);
        unsafe { with_dovi_l1(&frame, 0, 0, 0) };
        assert_eq!(frame.dovi_metadata().unwrap().l1, None);
    }

    #[test]
    fn dovi_malformed_ext_region_skips_l1_but_keeps_rpu() {
        let frame = Frame::alloc(TimeBase { num: 1, den: 1 }).unwrap();
        unsafe {
            attach_valid_dovi_rpu(&frame);
            let side_data = sys::av_frame_get_side_data(
                frame.ptr,
                sys::AVFrameSideDataType_AV_FRAME_DATA_DOVI_METADATA,
            );
            assert!(!side_data.is_null());
            let metadata = &mut *(*side_data).data.cast::<sys::AVDOVIMetadata>();
            metadata.ext_block_offset = usize::MAX;
            metadata.ext_block_size = mem::size_of::<sys::AVDOVIDmData>();
            metadata.num_ext_blocks = 5;
        }
        let dovi = frame
            .dovi_metadata()
            .expect("RPU must remain usable when the ext region is malformed");
        assert_eq!(dovi.l1, None);
    }

    #[test]
    fn frame_reads_dovi_side_data_with_32bit_coef_denom() {
        let frame = Frame::alloc(TimeBase { num: 1, den: 1 }).unwrap();
        unsafe { attach_valid_dovi_rpu(&frame) };

        let dovi = frame
            .dovi_metadata()
            .expect("32-bit coef denominator should be accepted");
        let luma = &dovi.reshaping[0];
        assert_eq!(luma.num_pivots, 2);
        assert_close(luma.poly_coeffs[0][1], 1.0);

        // Test with 31-bit denominator
        unsafe {
            mutate_dovi_rpu(&frame, |header, mapping, _| {
                header.coef_log2_denom = 31;
                mapping.curves[0].poly_coef[0][1] = 1_i64 << 31;
            });
        }
        let dovi31 = frame
            .dovi_metadata()
            .expect("31-bit coef denominator should be accepted");
        assert_close(dovi31.reshaping[0].poly_coeffs[0][1], 1.0);

        // Test with invalid 33-bit denominator (should be rejected)
        unsafe {
            mutate_dovi_rpu(&frame, |header, _, _| header.coef_log2_denom = 33);
        }
        assert_eq!(frame.dovi_metadata(), None);
    }

    #[test]
    fn dovi_unavailable_reason_classifies_rejections() {
        let frame = Frame::alloc(TimeBase { num: 1, den: 1 }).unwrap();
        assert_eq!(
            frame.dovi_unavailable_reason(),
            Some(DoviRejectReason::MissingSideData)
        );

        unsafe { attach_valid_dovi_rpu(&frame) };
        assert_eq!(frame.dovi_unavailable_reason(), None);

        unsafe {
            mutate_dovi_rpu(&frame, |header, _, _| header.coef_log2_denom = 33);
        }
        assert_eq!(
            frame.dovi_unavailable_reason(),
            Some(DoviRejectReason::UnsupportedCoefDenom)
        );
        unsafe {
            mutate_dovi_rpu(&frame, |header, _, _| header.coef_log2_denom = 32);
        }

        unsafe {
            mutate_dovi_rpu(&frame, |header, _, _| header.disable_residual_flag = 0);
        }
        // A residual request is reported through `dovi_el_status`, not rejected:
        // the base-layer mapping still applies (see
        // `fel_residual_keeps_the_base_layer_mapping`).
        assert_eq!(frame.dovi_unavailable_reason(), None);
        unsafe {
            mutate_dovi_rpu(&frame, |header, _, _| header.disable_residual_flag = 1);
        }

        unsafe {
            mutate_dovi_rpu(&frame, |_, mapping, _| mapping.curves[0].pivots[1] = 0);
        }
        assert_eq!(
            frame.dovi_unavailable_reason(),
            Some(DoviRejectReason::InvalidCurves)
        );
        unsafe {
            mutate_dovi_rpu(&frame, |_, mapping, _| mapping.curves[0].pivots[1] = 1023);
        }

        unsafe {
            mutate_dovi_rpu(&frame, |_, _, color| {
                color.rgb_to_lms_matrix = [
                    rational(1, 1),
                    rational(0, 1),
                    rational(0, 1),
                    rational(0, 1),
                    rational(0, 1),
                    rational(0, 1),
                    rational(0, 1),
                    rational(0, 1),
                    rational(0, 1),
                ];
            });
        }
        assert_eq!(
            frame.dovi_unavailable_reason(),
            Some(DoviRejectReason::InvalidColorMetadata)
        );

        // Restoring the matrix makes the RPU usable again, and the reason
        // tracks the metadata rather than sticky stream state.
        unsafe {
            mutate_dovi_rpu(&frame, |_, _, color| {
                color.rgb_to_lms_matrix = [
                    rational(1, 1),
                    rational(0, 1),
                    rational(0, 1),
                    rational(0, 1),
                    rational(1, 1),
                    rational(0, 1),
                    rational(0, 1),
                    rational(0, 1),
                    rational(1, 1),
                ];
            });
        }
        assert_eq!(frame.dovi_unavailable_reason(), None);
        assert!(frame.dovi_metadata().is_some());
    }

    #[test]
    fn fel_residual_keeps_the_base_layer_mapping() {
        let frame = Frame::alloc(TimeBase { num: 1, den: 1 }).unwrap();
        unsafe { attach_valid_dovi_rpu(&frame) };

        // Profile 7 FEL: the RPU asks for residual composition, which this
        // renderer cannot do (no enhancement-layer input). The RPU itself is
        // still valid, so the base-layer reshaping must survive; only the
        // un-composable part is reported.
        unsafe {
            mutate_dovi_rpu(&frame, |header, _, _| header.disable_residual_flag = 0);
        }
        assert_eq!(frame.dovi_unavailable_reason(), None);
        let metadata = frame.dovi_metadata().expect("FEL RPU stays usable");
        assert_eq!(metadata.reshaping[0].num_pivots, 2);
        assert_close(metadata.reshaping[0].pivots[1], 1.0);
        let status = frame.dovi_el_status().expect("FEL status is reported");
        assert!(status.residual_requested);
        assert!(!status.nlq_nontrivial);
        assert!(status.is_active());

        // A non-trivial NLQ definition is likewise reported, never rejected.
        unsafe {
            mutate_dovi_rpu(&frame, |_, mapping, _| mapping.nlq_method_idc = 1);
        }
        assert_eq!(frame.dovi_unavailable_reason(), None);
        assert!(frame.dovi_metadata().is_some());
        let status = frame.dovi_el_status().expect("NLQ status is reported");
        assert!(status.nlq_nontrivial);
        assert!(status.is_active());

        // Profile 8 style RPUs (no residual, no NLQ) report an inactive status
        // so callers do not diagnose them.
        unsafe {
            mutate_dovi_rpu(&frame, |header, mapping, _| {
                header.disable_residual_flag = 1;
                mapping.nlq_method_idc = -1;
            });
        }
        assert_eq!(frame.dovi_el_status(), Some(DoviElStatus::default()));
        assert!(!frame.dovi_el_status().unwrap().is_active());
    }

    #[test]
    fn frame_rate_normalizes_positive_rationals() {
        assert_eq!(
            frame_rate_from_av(rational(60_000, 2_002)),
            FrameRate::new(30_000, 1_001)
        );
        assert_eq!(frame_rate_from_av(rational(0, 1)), None);
        assert_eq!(frame_rate_from_av(rational(30, 0)), None);
        assert_eq!(frame_rate_from_av(rational(-30, 1)), None);
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.0001,
            "expected {expected}, got {actual}"
        );
    }
}
