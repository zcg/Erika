#[cfg(target_os = "android")]
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
#[cfg(target_os = "android")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(target_os = "android")]
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::core::MediaSourceHint;
use crate::trace;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("io error: {0}")]
    Io(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("unsupported source URI: {0}")]
    Unsupported(String),
    #[error("invalid owned file descriptor URI: {0}")]
    InvalidFileDescriptorUri(String),
}

pub type Result<T> = std::result::Result<T, SourceError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub length: Option<u64>,
}

impl ByteRange {
    pub fn suffix_from(start: u64) -> Self {
        Self {
            start,
            length: None,
        }
    }
}

/// 宿主可轮询的 `HttpRangeSource` 实时诊断快照（网络/缓冲状态）。
///
/// demux 线程写、宿主 UI 线程读：单播放窗口约束下通过全局槽传递，
/// 复用 Erika 现有的进程级诊断先例（`trace`）。宿主用它渲染"流诊断
/// 浮窗"——显示缓存窗口覆盖到哪里、还有多少段在下载、累计下载量
/// （差分得实时速度），从而判断"为什么卡"（网络慢 / 窗口外 seek 等预取）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SourceDiagSnapshot {
    /// 媒体总长度（字节），HEAD/Content-Range 得到；未知为 None。
    pub content_length: Option<u64>,
    /// 当前缓存窗口起点（字节）。
    pub cache_start: u64,
    /// 当前缓存窗口终点（字节，含）。
    pub cache_end: u64,
    /// 正在下载的预取段数（多连接并行 in-flight）。
    pub prefetch_in_flight: usize,
    /// 累计已下载字节（同步 fetch + 已消费预取段；用于差分下载速度）。
    pub downloaded_bytes: u64,
    /// 源内带宽估算（字节/秒，2s 滑动 EMA）。基于每次 fetch 完成的字节
    /// 累加差分，与宿主 poll 时机无关——宿主轮询差分在慢网下（一段
    /// 2s+ 才完成一次 publish）大部分采样为 0，ABR/诊断会误判"无供给"
    /// 而永不降档（弱网死扛高画质 → 周期饥饿卡顿）。
    pub est_bps: f64,
}

/// 诊断槽数。DASH 双流（video + audio）各占一槽独立发布/清空，互不覆盖；
/// 单流/直播只用槽 0。见 `HttpRangeSource::with_http_headers_and_diag_slot`。
const SOURCE_DIAG_SLOTS: usize = 2;

static SOURCE_DIAG: Mutex<[SourceDiagSnapshot; SOURCE_DIAG_SLOTS]> = Mutex::new([
    SourceDiagSnapshot {
        content_length: None,
        cache_start: 0,
        cache_end: 0,
        prefetch_in_flight: 0,
        downloaded_bytes: 0,
        est_bps: 0.0,
    },
    SourceDiagSnapshot {
        content_length: None,
        cache_start: 0,
        cache_end: 0,
        prefetch_in_flight: 0,
        downloaded_bytes: 0,
        est_bps: 0.0,
    },
]);

/// 读取当前 `HttpRangeSource` 诊断快照（宿主轮询用；无播放时返回零值/上次刷新）。
/// 兼容旧单槽语义：返回槽 0（主流/单流/直播）。
pub fn source_diag_snapshot() -> SourceDiagSnapshot {
    source_diag_snapshot_slot(0)
}

/// 读取指定槽位的诊断快照。DASH 双流约定 video=0、audio=1，
/// 各源独立发布/清空，宿主据此分别显示两条流的缓冲状态。
/// 槽位越界或无对应源时返回零值。
pub fn source_diag_snapshot_slot(slot: usize) -> SourceDiagSnapshot {
    SOURCE_DIAG
        .lock()
        .unwrap()
        .get(slot)
        .cloned()
        .unwrap_or_default()
}

pub trait MediaSource: Send {
    fn uri(&self) -> &str;
    fn len(&mut self) -> Result<Option<u64>>;
    /// 是否支持任意位置随机读取（seek）。默认 `true`；直播流等不可 seek
    /// 源返回 `false`，AVIO 据此清掉 `seekable` 位（flv demuxer 进入 live
    /// 模式），且 `avformat` 内部的 seek 会被拒绝。
    fn seekable(&self) -> bool {
        true
    }
    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>>;
    /// 播放层在已知目标读取位置时给出预取提示（例如 seek 到缓存窗口外时
    /// avformat 即将从该字节位置开始读）。默认无操作；`HttpRangeSource` 用它
    /// 把目标窗口提前拉入后台多段预取，避免窗口外 seek 后 demux 线程同步阻塞
    /// 在单个慢 HTTP 请求上。
    fn prefetch_hint(&mut self, _target: u64) {}
}

#[derive(Debug)]
pub struct LocalFileSource {
    uri: String,
    path: PathBuf,
}

impl LocalFileSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let uri = format!("file://{}", path.display());
        Ok(Self { uri, path })
    }
}

impl MediaSource for LocalFileSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        let metadata =
            std::fs::metadata(&self.path).map_err(|error| SourceError::Io(error.to_string()))?;
        Ok(Some(metadata.len()))
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let mut file =
            File::open(&self.path).map_err(|error| SourceError::Io(error.to_string()))?;
        file.seek(SeekFrom::Start(range.start))
            .map_err(|error| SourceError::Io(error.to_string()))?;
        let mut reader: Box<dyn Read> = match range.length {
            Some(length) => Box::new(file.take(length)),
            None => Box::new(file),
        };
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| SourceError::Io(error.to_string()))?;
        Ok(bytes)
    }
}

/// A seekable Android content descriptor owned by the media source.
///
/// The descriptor is closed automatically when this value is dropped. `offset`
/// and `length` expose an `AssetFileDescriptor` slice as a zero-based media file.
#[cfg(target_os = "android")]
#[derive(Debug)]
pub struct OwnedFileDescriptorSource {
    uri: String,
    file: File,
    offset: u64,
    length: Option<u64>,
}

/// Keeps an Android-owned descriptor registered until a synchronous native
/// source call either adopts it or returns an error.
///
/// Dropping the registration closes the descriptor when no `MediaSource`
/// consumed it. This closes the ownership gap between JNI validation and the
/// point where playback constructs `OwnedFileDescriptorSource`.
#[cfg(target_os = "android")]
#[derive(Debug)]
pub struct AndroidOwnedFdRegistration {
    fd: RawFd,
}

#[cfg(target_os = "android")]
impl Drop for AndroidOwnedFdRegistration {
    fn drop(&mut self) {
        if let Ok(mut registry) = android_owned_fd_registry().lock() {
            let _ = registry.remove(&self.fd);
        }
    }
}

/// Registers a descriptor transferred by the Android host for one synchronous
/// native invocation. `source_from_uri` consumes the registered `File`; if the
/// invocation fails before that boundary, the returned guard closes it.
#[cfg(target_os = "android")]
pub fn register_android_owned_fd(file: File) -> Result<AndroidOwnedFdRegistration> {
    let fd = file.as_raw_fd();
    if fd < 0 {
        return Err(SourceError::InvalidFileDescriptorUri(format!(
            "negative descriptor {fd}"
        )));
    }
    let mut registry = android_owned_fd_registry()
        .lock()
        .map_err(|_| SourceError::Io("Android owned-fd registry mutex poisoned".to_string()))?;
    if registry.contains_key(&fd) {
        // The existing entry already owns this raw descriptor. Closing a second
        // File wrapper here would invalidate that entry, so discard only the
        // duplicate wrapper and preserve the original ownership.
        std::mem::forget(file);
        return Err(SourceError::InvalidFileDescriptorUri(format!(
            "descriptor {fd} is already awaiting source adoption"
        )));
    }
    registry.insert(fd, file);
    Ok(AndroidOwnedFdRegistration { fd })
}

#[cfg(target_os = "android")]
fn android_owned_fd_registry() -> &'static Mutex<HashMap<RawFd, File>> {
    static REGISTRY: OnceLock<Mutex<HashMap<RawFd, File>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(target_os = "android")]
fn take_registered_android_owned_fd(fd: RawFd) -> Option<File> {
    android_owned_fd_registry().lock().ok()?.remove(&fd)
}

#[cfg(target_os = "android")]
impl OwnedFileDescriptorSource {
    /// Takes ownership of `fd`; callers must not close or reuse it afterwards.
    ///
    /// # Safety
    ///
    /// `fd` must be a valid, uniquely-owned, seekable descriptor.
    pub unsafe fn from_owned_fd(
        fd: RawFd,
        offset: u64,
        length: Option<u64>,
        uri: impl Into<String>,
    ) -> Result<Self> {
        if fd < 0 {
            return Err(SourceError::InvalidFileDescriptorUri(format!(
                "negative descriptor {fd}"
            )));
        }
        // SAFETY: ownership is transferred by the function contract.
        let file = unsafe { File::from_raw_fd(fd) };
        Self::from_owned_file(file, offset, length, uri.into())
    }

    fn from_owned_file(file: File, offset: u64, length: Option<u64>, uri: String) -> Result<Self> {
        let metadata = file
            .metadata()
            .map_err(|error| SourceError::Io(error.to_string()))?;
        let length = length.or_else(|| metadata.len().checked_sub(offset));
        Ok(Self {
            uri,
            file,
            offset,
            length,
        })
    }

    unsafe fn open_uri(uri: &str) -> Result<Self> {
        let fd = parse_owned_fd(uri)?;
        // Safe URI dispatch may only consume descriptors registered by the JNI
        // transferred-fd contract. Never adopt a registry miss by raw number:
        // that could seize or double-close an unrelated process descriptor.
        // Direct native callers with unique ownership must use `from_owned_fd`.
        let file = take_registered_android_owned_fd(fd).ok_or_else(|| {
            SourceError::InvalidFileDescriptorUri(format!(
                "{uri} (descriptor was not explicitly transferred)"
            ))
        })?;
        let spec = parse_fd_uri(uri)?;
        Self::from_owned_file(file, spec.offset, spec.length, uri.to_string())
    }
}

#[cfg(target_os = "android")]
impl MediaSource for OwnedFileDescriptorSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        Ok(self.length)
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let length = match self.length {
            Some(total) if range.start >= total => return Ok(Vec::new()),
            Some(total) => Some(
                range
                    .length
                    .unwrap_or_else(|| total.saturating_sub(range.start))
                    .min(total.saturating_sub(range.start)),
            ),
            None => range.length,
        };
        let absolute_start = self.offset.checked_add(range.start).ok_or_else(|| {
            SourceError::Io("owned descriptor seek offset overflowed u64".to_string())
        })?;
        self.file
            .seek(SeekFrom::Start(absolute_start))
            .map_err(|error| SourceError::Io(error.to_string()))?;
        let mut bytes = Vec::new();
        match length {
            Some(length) => (&mut self.file)
                .take(length)
                .read_to_end(&mut bytes)
                .map_err(|error| SourceError::Io(error.to_string()))?,
            None => self
                .file
                .read_to_end(&mut bytes)
                .map_err(|error| SourceError::Io(error.to_string()))?,
        };
        Ok(bytes)
    }
}

#[cfg(any(target_os = "android", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnedFdUri {
    fd: i32,
    offset: u64,
    length: Option<u64>,
}

#[cfg(any(target_os = "android", test))]
fn parse_fd_uri(uri: &str) -> Result<OwnedFdUri> {
    let body = uri
        .strip_prefix("fd://")
        .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))?;
    let (fd, query) = body.split_once('?').unwrap_or((body, ""));
    let fd = parse_owned_fd_value(fd, uri)?;
    let mut offset = None;
    let mut length = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))?;
        match key {
            "offset" => {
                if offset.is_some() {
                    return Err(SourceError::InvalidFileDescriptorUri(uri.to_string()));
                }
                offset = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| SourceError::InvalidFileDescriptorUri(uri.to_string()))?,
                );
            }
            "length" => {
                if length.is_some() {
                    return Err(SourceError::InvalidFileDescriptorUri(uri.to_string()));
                }
                length = Some(if value.is_empty() || value == "-1" {
                    None
                } else {
                    Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| SourceError::InvalidFileDescriptorUri(uri.to_string()))?,
                    )
                });
            }
            // Display names/URIs may be appended by the Android host for diagnostics.
            "name" | "display_uri" => {}
            _ => return Err(SourceError::InvalidFileDescriptorUri(uri.to_string())),
        }
    }
    Ok(OwnedFdUri {
        fd,
        offset: offset.unwrap_or(0),
        length: length.flatten(),
    })
}

#[cfg(target_os = "android")]
fn parse_owned_fd(uri: &str) -> Result<i32> {
    let body = uri
        .strip_prefix("fd://")
        .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))?;
    let fd = body.split_once(['?', '/', '#']).map_or(body, |(fd, _)| fd);
    parse_owned_fd_value(fd, uri)
}

#[cfg(any(target_os = "android", test))]
fn parse_owned_fd_value(value: &str, uri: &str) -> Result<i32> {
    value
        .parse::<i32>()
        .ok()
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| SourceError::InvalidFileDescriptorUri(uri.to_string()))
}

pub struct HttpRangeSource {
    uri: String,
    agent: ureq::Agent,
    http_headers: Vec<(String, String)>,
    content_length: Option<u64>,
    /// 诊断槽位：DASH 双流时 video=0 / audio=1，各源独立发布/清空诊断
    /// 快照互不覆盖（单流/直播默认 0）。
    diag_slot: usize,
    /// 缓存窗口起点（cache_bytes[0] 对应的字节 offset）。顺序播放推进时
    /// 保留已读数据末尾 back_bytes 作为后向缓冲，起点随之后移——因此这个
    /// 起点可能小于"当前播放位置"。
    cache_start: u64,
    cache_bytes: Vec<u8>,
    /// 后向缓冲上限（mpv `--demuxer-max-back-bytes` 等价）：窗口重设时保留
    /// 已读数据末尾这一段，seek 回退到该范围命中缓存不重新下载。默认 4MB
    /// （对齐 PiliPlus/mpv 默认）。seek 回退超过该范围仍重新拉流。
    back_bytes: u64,
    read_ahead_bytes: u64,
    /// 本源的并行预取上限。DASH 音频流（码率 ~40KB/s）由宿主传 2，
    /// 避免与视频流抢带宽；视频/单流默认 8（`MAX_PREFETCH_IN_FLIGHT`）。
    max_prefetch_in_flight: usize,
    /// 实测下载带宽估算（EMA，字节/秒）。所有同步 fetch 完成与预取段被
    /// 消费时累加字节，按 2s 窗口差分更新。驱动段大小/join 超时/read_ahead
    /// 自适应——固定参数在总带宽 <~400KB/s 时预取段必超时（一顿一顿），
    /// 自适应后任何带宽下预取机制都成立。0 表示冷启动（未测得数据）。
    est_bps: f64,
    /// 带宽估算窗口起点。跨窗口差分：窗口内所有完成段（含并行段）的字节
    /// 都累计进 `est_window_bytes`，因此并行下载的叠加吞吐被正确计入。
    last_est_at: Instant,
    est_window_bytes: u64,
    /// 多段并行预取队列。窗口重设（cache_start/cache_bytes 变更）时清空，
    /// `maybe_start_prefetch` 按需从 cache_end 向后填充多段 in-flight 下载。
    prefetch: Vec<PendingHttpFetch>,
    /// 最近一次窗口外预取提示的目标位置（`prefetch_hint`）。用于防抖：
    /// `av_seek_frame` 内部可能对相近位置多次 seek，同一窗口内不重复预取。
    prefetch_target: Option<u64>,
    /// 累计已下载字节（同步 fetch 完成 + 预取段被消费时累加）。仅供诊断
    /// 快照差分计算实时下载速度，不参与任何取数逻辑。
    downloaded_bytes: u64,
}

struct PendingHttpFetch {
    range: ByteRange,
    handle: JoinHandle<()>,
    receiver: mpsc::Receiver<Result<HttpRangeResponse>>,
}

/// Bytes fetched for one HTTP range request plus the resource total reported
/// by the server (`Content-Range` on 206, `Content-Length` on a whole-file
/// 200). The total lets callers backfill `content_length` when HEAD is
/// unavailable (e.g. servers answering HEAD with 405).
struct HttpRangeResponse {
    bytes: Vec<u8>,
    total_length: Option<u64>,
}

impl HttpRangeSource {
    const DEFAULT_READ_AHEAD_BYTES: u64 = 2 * 1024 * 1024;
    /// 后向缓冲默认大小（对齐 mpv `--demuxer-max-back-bytes` 默认 4MB，
    /// 与 PiliPlus `Pref.bufferSize` 默认一致）。可用环境变量
    /// `ERIKA_HTTP_BACK_BYTES` 覆盖。
    const DEFAULT_BACK_BUFFER_BYTES: u64 = 4 * 1024 * 1024;
    /// 并行预取的最大 in-flight 段数。多连接叠加带宽：单连接（B站 CDN）
    /// 吞吐约 1-3MB/s 且波动，read_range 在 demux 线程同步阻塞——缓存耗尽
    /// 即冻结播放（一顿一顿）。多段并行让总吞吐翻数倍，缓存窗口内永远有数据。
    /// 5 段 → 8 段：慢网（总带宽 ~1MB/s，均摊每段 ~125KB/s）时 256KB 小段约
    /// 2s 完成，显著小于 join 超时（5s），预取段不再被超时放弃——慢网下
    /// 预取机制依然成立，窗口持续大步扩张（对齐 PiliPlus 激进预缓冲）。
    const MAX_PREFETCH_IN_FLIGHT: usize = 8;
    /// 顺序预取/窗口外 seek 的目标预取统一用小段 256KB：慢网络（实测
    /// ~1MB/s 级，均摊到 N 段后单段吞吐更低）下大段（2MB+）join 必超时 →
    /// read 放弃预取段走同步 fetch → 缓存窗口退化成同步小步拉取（一顿一顿
    /// 的根因）。256KB 段在总带宽 ≥0.5MB/s 时单段完成时间显著小于 join
    /// 超时（5s），多段滚动叠加带宽后总吞吐远超播放消费，窗口持续向前大步
    /// 缓冲（对齐 PiliPlus：看一点、缓冲一大截）。段小也让 seek 后目标段
    /// 更快就绪（256KB ≈ 慢网 1-2s）。
    const SEEK_PREFETCH_SEGMENT_BYTES: u64 = 256 * 1024;
    /// 预取段 join 上限。超过即放弃该段（read_range 走同步小范围 fetch 兜底），
    /// 防止慢段无限阻塞 demux 线程（日志实测 seek 后 join 11MB 段卡 28.9s）。
    /// 同步兜底只取当前读取所需（≤256KB），比等 10MB 级慢段快得多。
    /// 5s：256KB 段在 ≥0.5MB/s 的总带宽下均可在超时内完成，慢网不被误杀。
    const PREFETCH_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
    /// 自适应段大小下限：带宽极低（<~100KB/s）时 32KB 段（8 段整窗 256KB）
    /// 约 2.5s 完成，仍在 join 超时内；再小 HTTP 请求开销占比失控。
    const MIN_PREFETCH_SEGMENT_BYTES: u64 = 32 * 1024;
    /// 带宽估算差分窗口：每 2s 用窗口内累计下载字节更新一次 EMA。
    /// 太短（<1s）会被单次请求的突发抖动带偏；太长响应迟缓。
    const BANDWIDTH_ESTIMATE_WINDOW: Duration = Duration::from_secs(2);

    pub fn new(uri: impl Into<String>) -> Self {
        Self::with_http_headers(uri, Vec::new())
    }

    pub fn with_http_headers(uri: impl Into<String>, http_headers: Vec<(String, String)>) -> Self {
        Self::with_http_headers_and_diag_slot(uri, http_headers, 0)
    }

    /// 指定诊断槽位构造（视频 0 / 音频 1）。单流/直播默认槽 0；
    /// 双流宿主要通过 `source_from_uri_with_hint_and_headers_slot` 创建。
    pub fn with_http_headers_and_diag_slot(
        uri: impl Into<String>,
        http_headers: Vec<(String, String)>,
        diag_slot: usize,
    ) -> Self {
        Self::with_http_headers_and_diag_slot_limits(
            uri,
            http_headers,
            diag_slot,
            Self::MAX_PREFETCH_IN_FLIGHT,
        )
    }

    /// 同 `with_http_headers_and_diag_slot`，但可指定并行预取上限
    /// （DASH 音频源传小值避免与视频抢带宽）。
    pub fn with_http_headers_and_diag_slot_limits(
        uri: impl Into<String>,
        http_headers: Vec<(String, String)>,
        diag_slot: usize,
        max_prefetch_in_flight: usize,
    ) -> Self {
        let agent = http_agent();
        Self {
            uri: uri.into(),
            agent,
            http_headers,
            content_length: None,
            diag_slot,
            cache_start: 0,
            cache_bytes: Vec::new(),
            back_bytes: http_back_bytes(),
            read_ahead_bytes: http_read_ahead_bytes(),
            max_prefetch_in_flight: max_prefetch_in_flight.max(1),
            est_bps: 0.0,
            last_est_at: Instant::now(),
            est_window_bytes: 0,
            prefetch: Vec::new(),
            prefetch_target: None,
            downloaded_bytes: 0,
        }
    }

    /// 累计已下载字节进带宽估算窗口；跨 2s 差分做 EMA 更新。
    /// 同步 fetch 与预取段消费两处都调用——窗口内并行段全部计入，
    /// 估算反映多连接叠加后的真实总吞吐。
    fn record_downloaded(&mut self, bytes: u64) {
        self.est_window_bytes = self.est_window_bytes.saturating_add(bytes);
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_est_at);
        if elapsed >= Self::BANDWIDTH_ESTIMATE_WINDOW {
            let inst_bps = self.est_window_bytes as f64 / elapsed.as_secs_f64();
            self.est_bps = if self.est_bps <= 0.0 {
                inst_bps
            } else {
                self.est_bps * 0.7 + inst_bps * 0.3
            };
            self.est_window_bytes = 0;
            self.last_est_at = now;
        }
    }

    /// 自适应预取段大小：目标单段（顺序预取 = 1 段在途）约 3s 完成
    /// （join 超时 5s 内留余量）。带宽低自动缩段——固定 256KB 在总带宽
    /// <~85KB/s 时单段完成 >3s；下限 32KB 防 HTTP 开销占比失控；冷启动
    /// （带宽未知）保持 256KB 原行为。
    fn prefetch_segment_bytes(&self) -> u64 {
        if self.est_bps <= 0.0 {
            return Self::SEEK_PREFETCH_SEGMENT_BYTES;
        }
        let seg = self.est_bps * 3.0;
        seg.clamp(
            Self::MIN_PREFETCH_SEGMENT_BYTES as f64,
            Self::SEEK_PREFETCH_SEGMENT_BYTES as f64,
        ) as u64
    }

    /// 自适应 join 超时：`max(5s, 单段完成时间 × 1.5)`。段大小已按带宽
    /// 缩放，此值只在带宽估算低于实际（段比预期慢）时兜底放宽，避免
    /// 慢段被误杀后同步 fetch 反而更卡。
    fn prefetch_join_timeout(&self) -> Duration {
        if self.est_bps <= 0.0 {
            return Self::PREFETCH_JOIN_TIMEOUT;
        }
        let seg = self.prefetch_segment_bytes() as f64;
        let needed = seg / self.est_bps * 1.5;
        Self::PREFETCH_JOIN_TIMEOUT.max(Duration::from_secs_f64(needed))
    }

    /// 从窗口取 `[range.start, range.start + requested)` 的字节。
    ///
    /// **不能直接用 `cache_bytes[..n]`**：`reset_cache` 可能走 keep_back 分支把
    /// `cache_start` 回退到 `keep_from(< range.start)`（保留已读后向缓冲），此时
    /// 窗口前缀是旧数据、`cache_bytes[0]` 对应文件 `cache_start` 而非
    /// `range.start` —— 直接取前缀会把窗口开头的旧字节当成本次请求的数据，
    /// 造成 64KB 级错位（慢网 + 顺序预取下实测：读 393216 返回文件 0 处的
    /// ftyp，avcodec Invalid data 坏包的根因）。
    fn window_slice(&self, range: ByteRange, requested_length: u64) -> Option<Vec<u8>> {
        if range.start < self.cache_start {
            return None;
        }
        let offset = usize::try_from(range.start - self.cache_start).ok()?;
        if offset >= self.cache_bytes.len() {
            return Some(Vec::new());
        }
        let n =
            usize::try_from(requested_length).unwrap_or(0).min(self.cache_bytes.len() - offset);
        Some(self.cache_bytes[offset..offset + n].to_vec())
    }

    fn cache_end(&self) -> u64 {
        self.cache_start
            .saturating_add(self.cache_bytes.len() as u64)
    }

    /// 把当前网络/缓冲状态发布到自己的诊断槽（宿主 UI 轮询读取）。
    /// DASH 双流（video=0/audio=1）各发布各的，互不覆盖。
    fn publish_diag(&self) {
        if let Ok(mut slots) = SOURCE_DIAG.lock() {
            if let Some(slot) = slots.get_mut(self.diag_slot) {
                *slot = SourceDiagSnapshot {
                    content_length: self.content_length,
                    cache_start: self.cache_start,
                    cache_end: self.cache_end(),
                    prefetch_in_flight: self.prefetch.len(),
                    downloaded_bytes: self.downloaded_bytes,
                    est_bps: self.est_bps,
                };
            }
        }
    }

    fn cached_slice(&self, range: ByteRange) -> Option<Vec<u8>> {
        let length = range.length?;
        let end = range.start.checked_add(length)?;
        if range.start < self.cache_start || end > self.cache_end() {
            return None;
        }
        let start_index = usize::try_from(range.start - self.cache_start).ok()?;
        let length = usize::try_from(length).ok()?;
        let end_index = start_index.checked_add(length)?;
        Some(self.cache_bytes[start_index..end_index].to_vec())
    }

    fn cached_prefix(&self, range: ByteRange) -> Option<Vec<u8>> {
        let length = range.length?;
        let end = range.start.checked_add(length)?;
        let cache_end = self.cache_end();
        if range.start < self.cache_start || range.start >= cache_end || end <= cache_end {
            return None;
        }
        let start_index = usize::try_from(range.start - self.cache_start).ok()?;
        let end_index = usize::try_from(cache_end - self.cache_start).ok()?;
        Some(self.cache_bytes[start_index..end_index].to_vec())
    }

    fn fetch_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let response = fetch_http_range(
            &self.agent,
            &self.uri,
            &self.http_headers,
            range,
            "http_range",
        )?;
        if self.content_length.is_none() {
            self.content_length = response.total_length;
        }
        let n = response.bytes.len() as u64;
        self.downloaded_bytes = self.downloaded_bytes.saturating_add(n);
        self.record_downloaded(n);
        self.publish_diag();
        Ok(response.bytes)
    }

    fn fetch_length(&mut self, range: ByteRange) -> Result<Option<u64>> {
        let requested_length = range.length.unwrap_or(0);
        Ok(match range.length {
            Some(length) => {
                // 同步请求（demux 正在等数据）用小段：读多段（64KB 的
                // demux 粒度）会让网络往返频繁拖慢；更大的段（如 1MB）在慢网
                // （实测 ~1MB/s 及以下）下同步阻塞达到 1s+，播放冻结时间过长。
                // 段大小按实测带宽自适应（慢网自动缩段），配合常驻并行预取
                // 窗口平滑覆盖。持续大步预取由 maybe_start_prefetch 的常驻段负责。
                let mut length = length.max(self.prefetch_segment_bytes());
                if let Some(total) = self.content_length.or_else(|| self.len().ok().flatten()) {
                    if range.start >= total {
                        return Ok(Some(0));
                    }
                    length = length.min(total.saturating_sub(range.start));
                }
                Some(length.max(requested_length))
            }
            None => None,
        })
    }

    fn take_prefetch(&mut self, range: ByteRange) -> Option<Result<(u64, Vec<u8>)>> {
        // 多段队列：只取覆盖请求的那一段，其余段保留（它们预取的是未来数据）。
        let idx = self
            .prefetch
            .iter()
            .position(|pending| range_contains(pending.range, range))?;
        let pending = self.prefetch.remove(idx);
        if !pending.is_finished() {
            // The pending fetch covers the requested bytes and they are needed
            // now: joining the in-flight thread is expected to be much cheaper
            // than issuing a duplicate synchronous download of the same range.
            http_trace_log(format!(
                "{{\"event\":\"http_prefetch_pending\",\"decision\":\"join\",\"start\":{},\"length\":{},\"requested_start\":{},\"requested_length\":{}}}",
                pending.range.start,
                pending
                    .range
                    .length
                    .map_or_else(|| "null".to_string(), |length| length.to_string()),
                range.start,
                range
                    .length
                    .map_or_else(|| "null".to_string(), |length| length.to_string()),
            ));
        }

        let join_started = Instant::now();
        let start = pending.range.start;
        // 带超时 join：慢段会卡死 demux 线程。超过上限放弃该段（后台线程
        // 继续下载、结果丢弃），返回 None 让 read_range 走同步小范围 fetch
        // 兜底——同步只取当前读取所需（≤1MB），比等 10MB 级慢段快得多。
        // 超时按实测带宽自适应：慢网下段大小已同步缩小，整窗可在超时内
        // 完成，正常情况不触发放弃（避免退化同步 fetch 的"一顿一顿"）。
        let result = match pending.receiver.recv_timeout(self.prefetch_join_timeout()) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                http_trace_log(format!(
                    "{{\"event\":\"http_prefetch_join_timeout\",\"start\":{},\"length\":{},\"waited_ms\":{:.3}}}",
                    start,
                    pending
                        .range
                        .length
                        .map_or_else(|| "null".to_string(), |length| length.to_string()),
                    join_started.elapsed().as_secs_f64() * 1000.0,
                ));
                return None;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Some(Err(SourceError::Http(
                    "http prefetch thread panicked".to_string(),
                )));
            }
        };
        http_trace_log(format!(
            "{{\"event\":\"http_prefetch_join\",\"start\":{},\"length\":{},\"elapsed_ms\":{:.3}}}",
            start,
            pending
                .range
                .length
                .map_or_else(|| "null".to_string(), |length| length.to_string()),
            join_started.elapsed().as_secs_f64() * 1000.0,
        ));
        Some(result.map(|response| {
            if self.content_length.is_none() {
                self.content_length = response.total_length;
            }
            let n = response.bytes.len() as u64;
            self.downloaded_bytes = self.downloaded_bytes.saturating_add(n);
            self.record_downloaded(n);
            self.publish_diag();
            (start, response.bytes)
        }))
    }

    /// 重设缓存窗口。保留窗口末尾之后仍在下载的预取段（顺序读取时它们是
    /// 未来数据——无脑清空会浪费在飞下载，每次消费一段就丢两段进度）；
    /// 清掉与新窗口重叠的作废段（seek 跳走后旧位置段无用了）。
    ///
    /// **后向缓冲**（mpv `--demuxer-max-back-bytes` 等价）：新数据起点严格落在
    /// 旧窗口内（`start > cache_start`，即顺序续读或窗口内回退读）时，保留旧
    /// 窗口紧邻 `start` 的已读数据（至多 `back_bytes`）拼到新数据前 → 窗口
    /// 起点 `cache_start` 回退到 `start - back_bytes`，seek 回退到该范围命中
    /// 缓存不重新下载。`start == cache_start`（同区域重读，如 prefetch 短读
    /// 后同步补拉同一段）不保留——旧数据与新数据同起点，保留会数据重复。
    /// 回退超过该范围（或向前跳到窗口外）则丢弃旧数据、从 `start` 全新建窗。
    ///
    /// **旧窗口只保留到 `start` 为止**：`bytes` 的起点恒等于 `start`（调用方
    /// 保证：同步 fetch 从 `range.start` 起、prefetch 段从 `prefetch_start` 起、
    /// prefix 拼接从 `range.start` 起）。若保留到旧窗口终点（`cache_end`），
    /// 而 `start` 落在窗口内（cached_prefix 部分命中场景：读 `[400381, 64KB]`
    /// 时窗口 `[0, X)`、`X` 在请求中途），旧窗口的 `[start..X)` 与新数据的
    /// 起始部分完全重复 → 窗口内容从 `start` 起错位（实测：残留坏包的数据
    /// 是文件 `start` 处的旧字节，错位 `X - start`）。
    fn reset_cache(&mut self, start: u64, bytes: Vec<u8>) {
        let new_end = start.saturating_add(bytes.len() as u64);
        let old_start = self.cache_start;
        let old_bytes = self.cache_bytes.len() as u64;
        self.prefetch.retain(|p| p.range.start >= new_end);
        if self.back_bytes > 0 && start > self.cache_start && start <= self.cache_end() {
            let old_len = self.cache_bytes.len();
            if old_len > 0 {
                let keep_from = start.saturating_sub(self.back_bytes).max(old_start);
                let keep_begin = usize::try_from(keep_from - old_start).unwrap_or(0);
                // 旧窗口只保留到新数据起点 `start`（见函数注释：保留更多会重复）
                let keep_end =
                    usize::try_from(start - old_start).unwrap_or(0).min(old_len);
                if keep_begin < keep_end {
                    let mut merged = self.cache_bytes[keep_begin..keep_end].to_vec();
                    merged.extend_from_slice(&bytes);
                    self.cache_start = keep_from;
                    self.cache_bytes = merged;
                    http_trace_log(format!(
                        "{{\"event\":\"http_cache_reset\",\"mode\":\"keep_back\",\"req_start\":{},\"new_start\":{},\"new_end\":{},\"old_start\":{},\"old_bytes\":{},\"keep_from\":{},\"keep_end\":{},\"head\":\"{}\"}}",
                        start,
                        self.cache_start,
                        self.cache_end(),
                        old_start,
                        old_bytes,
                        keep_from,
                        keep_end,
                        self.cache_bytes
                            .iter()
                            .take(8)
                            .map(|b| format!("{b:02X}"))
                            .collect::<Vec<_>>()
                            .join(""),
                    ));
                    self.publish_diag();
                    return;
                }
            }
        }
        self.cache_start = start;
        self.cache_bytes = bytes;
        http_trace_log(format!(
            "{{\"event\":\"http_cache_reset\",\"mode\":\"fresh\",\"req_start\":{},\"new_start\":{},\"new_end\":{},\"old_start\":{},\"old_bytes\":{},\"head\":\"{}\"}}",
            start,
            self.cache_start,
            self.cache_end(),
            old_start,
            old_bytes,
            self.cache_bytes
                .iter()
                .take(8)
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(""),
        ));
        self.publish_diag();
    }

    fn maybe_start_prefetch(&mut self, range: ByteRange) {
        if self.cache_bytes.is_empty() {
            return;
        }
        let Some(length) = range.length else {
            return;
        };
        let Some(total) = self.content_length else {
            return;
        };
        let Some(end) = range.start.checked_add(length) else {
            return;
        };
        let cache_end = self.cache_end();
        if end > cache_end || cache_end >= total {
            return;
        }
        let remaining = cache_end.saturating_sub(end);
        // 激进预缓冲：只要还有存量数据即在飞满额并行预取，始终把 cache_end
        // 后方的 read_ahead 窗口铺满 parallel 段（对齐 PiliPlus：播放中持续
        // 大步向前缓冲）。不设"剩余 > 半窗才停"的门限——那会导致慢网下
        // 半窗未到预取团无法启动，且窗口被消费后才补段，预取永远落后。
        // spawn_prefetch_window 内部以 MAX_PREFETCH_IN_FLIGHT 为限自然停滞
        // （段已铺满时不重复发起），这里每次缓存命中后检查即可。
        // 多连接并行：把 cache_end 后方 read_ahead 窗口拆成小段同时下载。
        // 单连接吞吐（B站 CDN 约 1-3MB/s）波动时，一段跟不上就缓存耗尽 →
        // demux 线程同步阻塞 → 一顿一顿。N 段并行让总吞吐翻数倍。
        // **段大小固定 256KB**（与 seek 预取同款）：慢网络（总带宽 ~1MB/s、
        // 均摊每段 ~125KB/s）下 2MB 大段 3s+ 必超时、预取段被放弃 → read
        // 退化同步 fetch、窗口卡 1MB。256KB 段均摊下约 2s 完成（<5s join
        // 超时），多段滚动叠加带宽，总吞吐持续远超播放消费。
        let seg_len = self.prefetch_segment_bytes();
        if self.prefetch.is_empty() {
            http_trace_log(format!(
                "{{\"event\":\"http_prefetch_fill\",\"cause\":\"remaining_low\",\"cache_end\":{},\"remaining\":{},\"read_ahead\":{},\"in_flight\":{}}}",
                cache_end,
                remaining,
                self.read_ahead_bytes,
                self.prefetch.len(),
            ));
        }
        self.spawn_prefetch_window(cache_end, seg_len);
    }

    /// 从指定字节位置起预取**一段**（顺序预取，PiliPlus mpv cache 式：
    /// 播一段预取一段）。顺序预取（`maybe_start_prefetch`）与窗口外 seek
    /// 预取（`prefetch_hint`）共用。
    ///
    /// **只发一段、任何时刻至多 1 个在途段**（`!self.prefetch.is_empty()`
    /// 即返回）：多段并行拼接（8 段 × 32KB）在慢网/CDN 并发下实测出现
    /// 数据错位（avcodec Invalid data 坏包的根因——HTTP 双流 demux 与
    /// 本地文件在相同偏移读到不同字节，时序相关：同代码同用例有时
    /// 0 错误有时 30 坏包）。顺序单段下载数据天然连续，杜绝拼接错位；
    /// 带宽不足由自适应段大小（`prefetch_segment_bytes`，32KB~256KB）
    /// 兜底，段消费后自动补下一段。
    fn spawn_prefetch_window(&mut self, from: u64, seg_len: u64) {
        let Some(total) = self.content_length else {
            return;
        };
        if from >= total || !self.prefetch.is_empty() {
            return;
        }
        // 已有 in-flight 段覆盖该起点（顺序预取与 seek 预取可能重叠）→ 不重复发
        if self
            .prefetch
            .iter()
            .any(|p| p.range.start < from.saturating_add(seg_len) && from < p.range_end())
        {
            return;
        }
        let len = seg_len.min(total.saturating_sub(from));
        if len == 0 {
            return;
        }
        let prefetch_range = ByteRange {
            start: from,
            length: Some(len),
        };
        http_trace_log(format!(
            "{{\"event\":\"http_prefetch_start\",\"start\":{},\"length\":{},\"in_flight\":{}}}",
            prefetch_range.start,
            len,
            self.prefetch.len(),
        ));
        self.prefetch.push(PendingHttpFetch::spawn(
            self.uri.clone(),
            self.http_headers.clone(),
            prefetch_range,
        ));
        self.publish_diag();
    }

    /// 窗口外 seek 的目标预取提示（由 AVIO seek 钩子调用）。target 超出
    /// 已缓存窗口时，清掉旧窗口的顺序预取段，从 target 前一个小段起用 2MB
    /// 小段并行拉目标窗口——seek 后 demux 第一次 read 命中这些小段并 join，
    /// 从"同步单请求抓大段（可卡 60s）"降为"join 2MB 小段（约 1s 就绪）"。
    fn prefetch_hint(&mut self, target: u64) {
        let Some(total) = self.content_length else {
            return;
        };
        if target >= total {
            return;
        }
        let cache_end = self.cache_end();
        // 目标仍在已缓存窗口内（含向前留一个小段给 BACKWARD 关键帧），无需预取。
        if target < cache_end.saturating_add(Self::SEEK_PREFETCH_SEGMENT_BYTES) {
            http_trace_log(format!(
                "{{\"event\":\"http_prefetch_hint\",\"decision\":\"in_cache\",\"target\":{},\"cache_end\":{}}}",
                target, cache_end,
            ));
            return;
        }
        // 防抖：`av_seek_frame` 内部可能对相近位置多次 seek，同一窗口不重复预取。
        if let Some(prev) = self.prefetch_target {
            if target < prev.saturating_add(Self::SEEK_PREFETCH_SEGMENT_BYTES) {
                http_trace_log(format!(
                    "{{\"event\":\"http_prefetch_hint\",\"decision\":\"debounced\",\"target\":{},\"prev\":{}}}",
                    target, prev,
                ));
                return;
            }
        }
        self.prefetch_target = Some(target);
        // seek 跳走：旧窗口的顺序预取段作废。in-flight 线程独立运行，drop
        // 只丢弃结果（避免无谓的 join 等待），带宽浪费可接受。
        let discarded = self.prefetch.len();
        self.prefetch.clear();
        let from = target.saturating_sub(self.prefetch_segment_bytes());
        http_trace_log(format!(
            "{{\"event\":\"http_prefetch_hint\",\"decision\":\"spawn\",\"target\":{},\"from\":{},\"discarded_in_flight\":{}}}",
            target, from, discarded,
        ));
        self.spawn_prefetch_window(from, self.prefetch_segment_bytes());
    }
}

impl PendingHttpFetch {
    fn spawn(uri: String, http_headers: Vec<(String, String)>, range: ByteRange) -> Self {
        http_trace_log(format!(
            "{{\"event\":\"http_prefetch_start\",\"start\":{},\"length\":{}}}",
            range.start,
            range
                .length
                .map_or_else(|| "null".to_string(), |length| length.to_string()),
        ));
        // 结果经 channel 送达：`take_prefetch` 用 recv_timeout 支持超时放弃
        // 慢段（无限 join 会卡死 demux 线程），线程跑完后自动退出。
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let agent = http_agent();
            let result =
                fetch_http_range(&agent, &uri, &http_headers, range, "http_prefetch_range");
            let _ = sender.send(result);
        });
        Self {
            range,
            handle,
            receiver,
        }
    }

    fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    fn range_end(&self) -> u64 {
        self.range
            .length
            .map_or(u64::MAX, |length| self.range.start.saturating_add(length))
    }
}

impl Drop for HttpRangeSource {
    fn drop(&mut self) {
        // source 销毁（播放停止/换源）→ 清空**自己的**诊断槽，宿主读到
        // 零值即"该流无播放"。只清本槽：DASH 双流另一条流的诊断不受影响。
        if let Ok(mut slots) = SOURCE_DIAG.lock() {
            if let Some(slot) = slots.get_mut(self.diag_slot) {
                *slot = SourceDiagSnapshot::default();
            }
        }
    }
}

/// 直播流源：不可 seek 的 HTTP 流（HTTP-FLV over HTTPS）。
///
/// B 站直播 CDN 支持 HTTP-FLV，但**不支持 Range、无 Content-Length、
/// 连接长驻**——与 `HttpRangeSource`（可 Range seek 的 VOD）是完全不同的
/// 访问模型。本实现 GET 建立连接后从响应 body 顺序读取，demux 线程按需
/// 阻塞取字节，绝不把无限流一次性读进内存。
///
/// `len()` 恒返回 `None`（无总长度）、`seekable()` 恒返回 `false`：AVIO
/// 清掉 seekable 位后 flv demuxer 进入 live 模式（不 seek、按墙钟推进），
/// 播放层不会因"EOF"而结束（连接正常关闭才触发 EOF）。
pub struct HttpLiveStreamSource {
    uri: String,
    /// GET 响应 body 的读取器（连接长驻、数据持续推送）。`None` 表示连接已关闭。
    /// ureq 的 `Response` 类型是私有的，这里持 `Body::as_reader()` 返回的
    /// `'static` reader（内部 Arc 共享读取位置，可脱离 Response 独立持有）。
    reader: Option<Box<dyn Read + Send + Sync>>,
    /// 已消费字节数。直播流不可 seek：`read_range` 校验请求起点 == 此值，
    /// 失配（AVIO seek 后）报错，防止把流当作可跳转文件读。
    offset: u64,
    /// 累计已下载字节（诊断快照差分实时速度）。
    downloaded_bytes: u64,
}

impl HttpLiveStreamSource {
    /// 单次读取上限。AVIO buffer 即 64KB，demux 粒度足够；截断大 `length`
    /// （如误用 `suffix_from(0)`）避免把无限流一次读进内存。
    const MAX_READ_BYTES: usize = 256 * 1024;

    pub fn open(uri: &str, http_headers: Vec<(String, String)>) -> Result<Self> {
        // 直播专用 agent（无 recv_response/recv_body 总时长超时，见 http_live_agent）
        let agent = http_live_agent();
        let mut request = agent.get(uri);
        for (name, value) in &http_headers {
            request = request.header(name, value);
        }
        let response = request.call().map_err(|error| {
            http_trace_log(format!(
                "{{\"event\":\"http_live_open_error\",\"error\":\"{}\"}}",
                json_escape(&error.to_string()),
            ));
            SourceError::Http(error.to_string())
        })?;
        let status = response.status().as_u16();
        if status >= 400 {
            http_trace_log(format!(
                "{{\"event\":\"http_live_open_status\",\"status\":{}}}",
                status,
            ));
            return Err(SourceError::Http(format!(
                "live stream HTTP status {status}"
            )));
        }
        http_trace_log(format!(
            "{{\"event\":\"http_live_open\",\"status\":{},\"content_length\":{}}}",
            status,
            response
                .headers()
                .get("content-length")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
                .unwrap_or_else(|| "null".to_string()),
        ));
        let reader = response.into_body().into_reader();
        Ok(Self {
            uri: uri.to_string(),
            reader: Some(Box::new(reader)),
            offset: 0,
            downloaded_bytes: 0,
        })
    }

    fn publish_diag(&self) {
        if let Ok(mut slots) = SOURCE_DIAG.lock() {
            if let Some(slot) = slots.get_mut(0) {
                *slot = SourceDiagSnapshot {
                    content_length: None,
                    cache_start: 0,
                    cache_end: self.offset,
                    prefetch_in_flight: 0,
                    downloaded_bytes: self.downloaded_bytes,
                    est_bps: 0.0,
                };
            }
        }
    }
}

impl MediaSource for HttpLiveStreamSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        Ok(None)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        if range.start != self.offset {
            // AVIO seek 改动 offset 后的下一次读会走到这里：直播流不可跳转。
            http_trace_log(format!(
                "{{\"event\":\"http_live_seek_rejected\",\"requested\":{},\"stream_at\":{}}}",
                range.start, self.offset,
            ));
            return Err(SourceError::Io(format!(
                "live stream is not seekable (requested {} at stream offset {})",
                range.start, self.offset,
            )));
        }
        let Some(reader) = self.reader.as_mut() else {
            return Err(SourceError::Io(
                "live stream connection is closed".to_string(),
            ));
        };
        let length = usize::try_from(range.length.unwrap_or(Self::MAX_READ_BYTES as u64))
            .unwrap_or(Self::MAX_READ_BYTES)
            .min(Self::MAX_READ_BYTES);
        let mut buf = vec![0u8; length.max(1)];
        // 阻塞读：直播流持续推送，read 等到有数据才返回；0 表示服务器关闭
        // （断流 → AVIO EOF → 播放层按流结束处理）。
        let n = reader.read(&mut buf).map_err(|error| {
            let message = format!("live stream read error: {error}");
            http_trace_log(format!(
                "{{\"event\":\"http_live_read_error\",\"error\":\"{}\"}}",
                json_escape(&error.to_string()),
            ));
            SourceError::Http(message)
        })?;
        buf.truncate(n);
        if n == 0 {
            // 直播源没有"正常结束"概念：read 返回 0 即服务器关闭连接（断流）。
            // 记录流位置供宿主诊断断流点；空 buf → AVIO EOF → 播放层按流结束
            // 处理。这里不清空 reader：EOF 后继续 read 仍返回 Ok(0)，保持
            // ffmpeg 干净的 EOF 语义；清空会让下一次读报 Io 错误、破坏结束路径。
            http_trace_log(format!(
                "{{\"event\":\"http_live_eof\",\"stream_offset\":{},\"downloaded_bytes\":{}}}",
                self.offset, self.downloaded_bytes,
            ));
        }
        self.offset = self.offset.saturating_add(n as u64);
        self.downloaded_bytes = self.downloaded_bytes.saturating_add(n as u64);
        self.publish_diag();
        Ok(buf)
    }
}

impl Drop for HttpLiveStreamSource {
    fn drop(&mut self) {
        // 直播流用槽 0；销毁时清空自己的诊断槽。
        if let Ok(mut slots) = SOURCE_DIAG.lock() {
            if let Some(slot) = slots.get_mut(0) {
                *slot = SourceDiagSnapshot::default();
            }
        }
    }
}

fn http_agent() -> ureq::Agent {
    // bili-rs 宿主可能以 IP 直连（绕过 DNS 污染/IPv6 不可达），
    // 证书按 IP 校验会失败——关闭证书校验（播放器场景可接受）
    let tls = ureq::tls::TlsConfig::builder()
        .disable_verification(true)
        .build();
    ureq::Agent::config_builder()
        // 连接超时 4s：宿主在 UI 线程同步 open，超时太长会卡死界面
        // （慢 CDN 快速失败 → bili-rs 回退官方源）
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_response(Some(Duration::from_secs(15)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .tls_config(tls)
        .build()
        .into()
}

/// 直播流专用 HTTP agent。**不能复用 `http_agent()`**：ureq 3.x 的
/// `timeout_recv_response`/`timeout_recv_body` 是**自响应头起算的总时长**
/// deadline（`next_timeout(RecvBody)` 同时检查 RecvResponse 的剩余时间并取
/// 更早者），直播流是无限长 body，读取超过 15s 后每次读都立即超时 →
/// 直播必在播放约 15s 后断流（"播着播着就自动重连"的根因，与网络无关）。
/// 直播源只保留连接超时（慢源快速失败）；body 读取无总时长限制，交由
/// demuxer/播放层消费。CDN 主动断开（FIN/RST）时读返回 EOF/错误 → 宿主
/// 断流检测触发自动重连（新 URL 换源）。
fn http_live_agent() -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .disable_verification(true)
        .build();
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .tls_config(tls)
        .build()
        .into()
}

const HTTP_FETCH_MAX_ATTEMPTS: u32 = 3;
const HTTP_FETCH_RETRY_BACKOFF: [Duration; 2] =
    [Duration::from_millis(200), Duration::from_secs(1)];
/// Wall-clock ceiling on one logical fetch, retries and backoff included.
///
/// `read_range` runs on the demuxer thread, so every retry freezes playback.
/// The per-request timeouts already allow 10 s to connect and 15 s for response
/// headers, which three attempts would stretch past a minute; this bounds the
/// stall instead, at the cost of giving up on origins that are merely very slow.
const HTTP_FETCH_TOTAL_BUDGET: Duration = Duration::from_secs(20);

fn http_retry_backoff(attempt: u32) -> Duration {
    let index = usize::try_from(attempt.saturating_sub(1)).unwrap_or(0);
    HTTP_FETCH_RETRY_BACKOFF
        .get(index)
        .copied()
        .unwrap_or(Duration::from_secs(1))
}

/// Whether another attempt (plus its backoff) still fits inside the budget.
/// Checked before sleeping so a retry is never armed only to blow the deadline.
fn http_retry_fits_budget(deadline_started: Instant, backoff: Duration) -> bool {
    deadline_started
        .elapsed()
        .saturating_add(backoff)
        .lt(&HTTP_FETCH_TOTAL_BUDGET)
}

/// Whether a failed HTTP exchange is worth retrying: transport errors and 5xx
/// responses are transient; 4xx responses are deterministic client errors.
fn http_error_is_retryable(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::StatusCode(status) => *status >= 500,
        _ => true,
    }
}

/// Parses the `total` out of a `Content-Range: bytes start-end/total` header.
/// Returns `None` for missing headers, unsatisfied-range (`*/total` still
/// yields the total), and unknown totals (`bytes 0-1/*`).
fn parse_content_range_total(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (_, total) = rest.rsplit_once('/')?;
    total.trim().parse::<u64>().ok()
}

/// Parses the first byte offset out of a `Content-Range: bytes start-end/total`
/// header. `None` for an unsatisfied-range form (`bytes */total`), which names
/// no offset.
fn parse_content_range_start(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, _) = rest.rsplit_once('/')?;
    let (start, _) = range.trim().split_once('-')?;
    start.trim().parse::<u64>().ok()
}

/// The strongest entity validator the response offers, preferred in the order
/// RFC 9110 recommends for `If-Range`.
fn response_entity_validator<T>(response: &ureq::http::Response<T>) -> Option<String> {
    ["etag", "last-modified"].into_iter().find_map(|name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

/// Learns the total length from a one-byte GET, for origins that reject HEAD.
///
/// The body is deliberately never read. An origin that rejects HEAD *and*
/// ignores Range answers 200 with the whole object, so buffering the response
/// would turn a `len()` call into a full download of the media -- gigabytes
/// into memory before playback, just to learn a number the headers already
/// carry.
fn probe_http_total_length(
    agent: &ureq::Agent,
    uri: &str,
    http_headers: &[(String, String)],
) -> Result<Option<u64>> {
    let probe = ByteRange {
        start: 0,
        length: Some(1),
    };
    let mut request = agent.get(uri).header("Range", &http_range_header(probe));
    for (name, value) in http_headers {
        request = request.header(name, value);
    }
    let response = request
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|error| {
            http_trace_log(format!(
                "{{\"event\":\"http_length_probe_error\",\"phase\":\"request\",\"error\":\"{}\"}}",
                json_escape(&error.to_string()),
            ));
            SourceError::Http(error.to_string())
        })?;
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    let total_length = match status {
        206 | 416 => header("content-range")
            .as_deref()
            .and_then(parse_content_range_total),
        // Range was ignored; Content-Length is the whole object, which is
        // exactly the total being probed for.
        200 => header("content-length").and_then(|value| value.trim().parse::<u64>().ok()),
        status if status >= 400 => {
            let error = SourceError::Http(format!("http status: {status}"));
            http_trace_log(format!(
                "{{\"event\":\"http_length_probe_error\",\"phase\":\"status\",\"status\":{status}}}"
            ));
            return Err(error);
        }
        _ => None,
    };
    http_trace_log(format!(
        "{{\"event\":\"http_length_probe\",\"status\":{},\"total\":{}}}",
        status,
        total_length.map_or_else(|| "null".to_string(), |total| total.to_string()),
    ));
    Ok(total_length)
}

fn http_range_header(range: ByteRange) -> String {
    match range.length {
        Some(length) if length > 0 => {
            let end = range.start.saturating_add(length).saturating_sub(1);
            format!("bytes={}-{}", range.start, end)
        }
        _ => format!("bytes={}-", range.start),
    }
}

fn fetch_http_range(
    agent: &ureq::Agent,
    uri: &str,
    http_headers: &[(String, String)],
    range: ByteRange,
    event: &str,
) -> Result<HttpRangeResponse> {
    let mut bytes = Vec::new();
    let mut total_length = None;
    // Entity validator from the first response. A resumed request replays it as
    // `If-Range` so an origin that re-encoded or load-balanced to a different
    // variant answers 200 (which the status check below rejects for a non-zero
    // start) instead of handing back bytes from a different object to be spliced
    // onto the prefix we already hold.
    let mut validator: Option<String> = None;
    let mut attempt = 0u32;
    let deadline_started = Instant::now();
    loop {
        attempt += 1;
        // Resume from what already arrived: earlier attempts keep their bytes
        // and the Range start advances past them.
        let received = bytes.len() as u64;
        if range.length.is_some_and(|length| received >= length) {
            // A body error surfaced after every requested byte arrived; the
            // payload is complete, so do not re-request an open-ended tail.
            return Ok(HttpRangeResponse {
                bytes,
                total_length,
            });
        }
        let resume_range = ByteRange {
            start: range.start.saturating_add(received),
            length: range.length.map(|length| length.saturating_sub(received)),
        };
        let header = http_range_header(resume_range);
        let started = Instant::now();
        let mut request = agent.get(uri).header("Range", &header);
        for (name, value) in http_headers {
            request = request.header(name, value);
        }
        if received > 0
            && let Some(validator) = validator.as_deref()
        {
            request = request.header("If-Range", validator);
        }
        // 请求"发出"即记录（此前只在完成/失败时打 http_range/..._error；若 CDN
        // 响应挂起，请求在途期间无任何日志——seek 后定格时无法区分"没发请求"与
        // "请求在途卡住"。start 日志让在途请求可见并可计算其等待时长）。
        http_trace_log(format!(
            "{{\"event\":\"{}_start\",\"attempt\":{},\"start\":{},\"length\":{},\"received\":{},\"status_pending\":true}}",
            event,
            attempt,
            resume_range.start,
            resume_range
                .length
                .map_or_else(|| "null".to_string(), |length| length.to_string()),
            received,
        ));
        let mut response = match request.call() {
            Ok(response) => response,
            Err(error) => {
                http_trace_log(format!(
                    "{{\"event\":\"{}_error\",\"phase\":\"request\",\"attempt\":{},\"start\":{},\"length\":{},\"elapsed_ms\":{:.3},\"error\":\"{}\"}}",
                    event,
                    attempt,
                    resume_range.start,
                    resume_range
                        .length
                        .map_or_else(|| "null".to_string(), |length| length.to_string()),
                    started.elapsed().as_secs_f64() * 1000.0,
                    json_escape(&error.to_string()),
                ));
                let backoff = http_retry_backoff(attempt);
                if attempt < HTTP_FETCH_MAX_ATTEMPTS
                    && http_error_is_retryable(&error)
                    && http_retry_fits_budget(deadline_started, backoff)
                {
                    http_trace_log(format!(
                        "{{\"event\":\"{}_retry\",\"phase\":\"request\",\"attempt\":{},\"start\":{},\"received\":{},\"backoff_ms\":{}}}",
                        event,
                        attempt,
                        resume_range.start,
                        received,
                        backoff.as_millis(),
                    ));
                    thread::sleep(backoff);
                    continue;
                }
                return Err(SourceError::Http(error.to_string()));
            }
        };
        let status = response.status().as_u16();
        match status {
            206 => {
                let content_range = response
                    .headers()
                    .get("content-range")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                // A resumed request must continue exactly where the prefix
                // ends. A server that answers 206 from a different offset --
                // or a changed entity that ignored If-Range -- would otherwise
                // be spliced onto the bytes already held and returned as
                // silently corrupt media.
                if let Some(start) = content_range.as_deref().and_then(parse_content_range_start)
                    && start != resume_range.start
                {
                    http_trace_log(format!(
                        "{{\"event\":\"{}_error\",\"phase\":\"content_range\",\"attempt\":{},\"start\":{},\"served_start\":{}}}",
                        event, attempt, resume_range.start, start,
                    ));
                    return Err(SourceError::Http(format!(
                        "server served range from {start}, expected {}",
                        resume_range.start
                    )));
                }
                if total_length.is_none() {
                    total_length = content_range.as_deref().and_then(parse_content_range_total);
                }
                if validator.is_none() {
                    validator = response_entity_validator(&response);
                }
            }
            200 => {
                if resume_range.start > 0 {
                    // The server sent the file from byte zero: treating that
                    // payload as `resume_range.start` data would silently
                    // corrupt the cache, so fail instead of retrying.
                    http_trace_log(format!(
                        "{{\"event\":\"{}_error\",\"phase\":\"status\",\"attempt\":{},\"start\":{},\"status\":200}}",
                        event, attempt, resume_range.start,
                    ));
                    return Err(SourceError::Http(
                        "server ignored Range request (status 200)".to_string(),
                    ));
                }
                if total_length.is_none() {
                    total_length = response
                        .headers()
                        .get("content-length")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok());
                }
                if validator.is_none() {
                    validator = response_entity_validator(&response);
                }
            }
            // ureq maps 4xx/5xx to Error::StatusCode before this point; any
            // other status (204, 304, ...) carries no usable range payload.
            _ => {
                http_trace_log(format!(
                    "{{\"event\":\"{}_error\",\"phase\":\"status\",\"attempt\":{},\"start\":{},\"status\":{}}}",
                    event, attempt, resume_range.start, status,
                ));
                return Err(SourceError::Http(format!(
                    "unexpected HTTP status {status} for Range request"
                )));
            }
        }
        if let Err(error) = response.body_mut().as_reader().read_to_end(&mut bytes) {
            http_trace_log(format!(
                "{{\"event\":\"{}_error\",\"phase\":\"body\",\"attempt\":{},\"start\":{},\"length\":{},\"status\":{},\"bytes\":{},\"elapsed_ms\":{:.3},\"error\":\"{}\"}}",
                event,
                attempt,
                resume_range.start,
                resume_range
                    .length
                    .map_or_else(|| "null".to_string(), |length| length.to_string()),
                status,
                bytes.len(),
                started.elapsed().as_secs_f64() * 1000.0,
                json_escape(&error.to_string()),
            ));
            let backoff = http_retry_backoff(attempt);
            if attempt < HTTP_FETCH_MAX_ATTEMPTS
                && http_retry_fits_budget(deadline_started, backoff)
            {
                http_trace_log(format!(
                    "{{\"event\":\"{}_retry\",\"phase\":\"body\",\"attempt\":{},\"start\":{},\"received\":{},\"backoff_ms\":{}}}",
                    event,
                    attempt,
                    range.start.saturating_add(bytes.len() as u64),
                    bytes.len(),
                    backoff.as_millis(),
                ));
                thread::sleep(backoff);
                continue;
            }
            return Err(SourceError::Http(error.to_string()));
        }
        http_trace_log(format!(
            "{{\"event\":\"{}\",\"attempt\":{},\"start\":{},\"length\":{},\"status\":{},\"bytes\":{},\"elapsed_ms\":{:.3}}}",
            event,
            attempt,
            range.start,
            range
                .length
                .map_or_else(|| "null".to_string(), |length| length.to_string()),
            status,
            bytes.len(),
            started.elapsed().as_secs_f64() * 1000.0,
        ));
        return Ok(HttpRangeResponse {
            bytes,
            total_length,
        });
    }
}

impl std::fmt::Debug for HttpRangeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRangeSource")
            .field("uri", &redacted_uri(&self.uri))
            .field("content_length", &self.content_length)
            .field("cache_start", &self.cache_start)
            .field("cache_bytes", &self.cache_bytes.len())
            .field("read_ahead_bytes", &self.read_ahead_bytes)
            .finish()
    }
}

impl MediaSource for HttpRangeSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        if self.content_length.is_some() {
            return Ok(self.content_length);
        }
        let started = Instant::now();
        http_trace_log(format!(
            "[erika-http-trace] stage=head_request uri={} cache_start={} cache_end={} read_ahead={}",
            redacted_uri(&self.uri),
            self.cache_start,
            self.cache_end(),
            self.read_ahead_bytes,
        ));
        // Keep metadata probing off the range-request pool. Some HTTP/1.0
        // servers close a HEAD connection without an explicit Connection
        // header; reusing that stale socket for the first GET otherwise
        // surfaces as `Peer disconnected`.
        // TODO(perf): cache a dedicated metadata agent on `HttpRangeSource` so
        // repeated `len()` probes without Content-Length do not rebuild the TLS
        // client, while still keeping HEAD sockets out of the range-request pool.
        let head_agent = http_agent();
        let mut attempt = 0u32;
        let head_error = loop {
            attempt += 1;
            let mut request = head_agent.head(&self.uri);
            for (name, value) in &self.http_headers {
                request = request.header(name, value);
            }
            match request.call() {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let length = response
                        .headers()
                        .get("content-length")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok());
                    // Some streaming servers synthesize an empty HEAD body and
                    // incorrectly report that body length as the media length.
                    // A zero-byte media resource is not useful to the demuxer,
                    // so verify it with a one-byte range request before caching
                    // the value. Content-Range carries the actual object size.
                    if length == Some(0) {
                        http_trace_log(format!(
                            "[erika-http-trace] stage=head_zero_length_fallback status={} elapsed_ms={:.3}",
                            status,
                            started.elapsed().as_secs_f64() * 1000.0,
                        ));
                        return match probe_http_total_length(
                            &self.agent,
                            &self.uri,
                            &self.http_headers,
                        ) {
                            Ok(total_length) => {
                                self.content_length = total_length;
                                Ok(self.content_length)
                            }
                            Err(error) => Err(SourceError::Http(format!(
                                "HEAD reported Content-Length: 0 and range probe failed: {error}"
                            ))),
                        };
                    }
                    self.content_length = length;
                    self.publish_diag();
                    http_trace_log(format!(
                        "[erika-http-trace] stage=head_response status={} length={} elapsed_ms={:.3}",
                        status,
                        length.map_or_else(|| "null".to_string(), |length| length.to_string()),
                        started.elapsed().as_secs_f64() * 1000.0,
                    ));
                    return Ok(length);
                }
                Err(error) => {
                    http_trace_log(format!(
                        "[erika-http-trace] stage=head_error attempt={} elapsed_ms={:.3} error={}",
                        attempt,
                        started.elapsed().as_secs_f64() * 1000.0,
                        json_escape(&error.to_string()),
                    ));
                    let backoff = http_retry_backoff(attempt);
                    if attempt < HTTP_FETCH_MAX_ATTEMPTS
                        && http_error_is_retryable(&error)
                        && http_retry_fits_budget(started, backoff)
                    {
                        http_trace_log(format!(
                            "[erika-http-trace] stage=head_retry attempt={} backoff_ms={}",
                            attempt,
                            backoff.as_millis(),
                        ));
                        thread::sleep(backoff);
                        continue;
                    }
                    break error;
                }
            }
        };
        // Some servers reject HEAD (e.g. 405) yet still serve ranges. Probe
        // with a one-byte GET and take the total from Content-Range.
        http_trace_log(format!(
            "[erika-http-trace] stage=head_fallback_range error={}",
            json_escape(&head_error.to_string()),
        ));
        match probe_http_total_length(&self.agent, &self.uri, &self.http_headers) {
            Ok(total_length) => {
                self.content_length = total_length;
                self.publish_diag();
                Ok(self.content_length)
            }
            Err(_) => Err(SourceError::Http(head_error.to_string())),
        }
    }

    fn prefetch_hint(&mut self, target: u64) {
        HttpRangeSource::prefetch_hint(self, target)
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        if let Some(bytes) = self.cached_slice(range) {
            self.maybe_start_prefetch(range);
            http_trace_log(format!(
                "{{\"event\":\"http_cache_hit\",\"start\":{},\"length\":{},\"bytes\":{}}}",
                range.start,
                range.length.unwrap_or_default(),
                bytes.len(),
            ));
            trace_read_return("http_read", range, &self, &bytes);
            return Ok(bytes);
        }

        let requested_length = range.length.unwrap_or(0);
        if let Some(prefix) = self.cached_prefix(range) {
            let prefix_length = prefix.len() as u64;
            let suffix_range = ByteRange {
                start: range.start.saturating_add(prefix_length),
                length: Some(requested_length.saturating_sub(prefix_length)),
            };
            if let Some(prefetch) = self.take_prefetch(suffix_range) {
                let (prefetch_start, prefetch_bytes) = prefetch?;
                let prefetch_range = ByteRange {
                    start: prefetch_start,
                    length: Some(prefetch_bytes.len() as u64),
                };
                if range_contains(prefetch_range, suffix_range) {
                    let mut cache_bytes = prefix;
                    cache_bytes.extend_from_slice(&prefetch_bytes);
                    self.reset_cache(range.start, cache_bytes);
                    self.maybe_start_prefetch(range);
                    let out = self.window_slice(range, requested_length).unwrap_or_default();
                    trace_read_return("http_read_prefix_prefetch", range, self, &out);
                    return Ok(out);
                }
            }

            let fetch_length = self.fetch_length(suffix_range)?;
            if fetch_length == Some(0) {
                return Ok(prefix);
            }
            let suffix = self.fetch_range(ByteRange {
                start: suffix_range.start,
                length: fetch_length,
            })?;
            let mut cache_bytes = prefix;
            cache_bytes.extend_from_slice(&suffix);
            self.reset_cache(range.start, cache_bytes);
            self.maybe_start_prefetch(range);
            let out = self.window_slice(range, requested_length).unwrap_or_default();
            trace_read_return("http_read_prefix_fetch", range, self, &out);
            return Ok(out);
        }
        let fetch_length = self.fetch_length(range)?;
        if fetch_length == Some(0) {
            return Ok(Vec::new());
        }
        let fetched = match self.take_prefetch(range) {
            Some(Ok((start, bytes))) => {
                self.reset_cache(start, bytes);
                if let Some(bytes) = self.cached_slice(range) {
                    self.maybe_start_prefetch(range);
                    trace_read_return("http_read_prefetch", range, self, &bytes);
                    return Ok(bytes);
                }
                // The prefetch returned fewer bytes than requested (short
                // read). An empty result here would be mistaken for EOF by
                // the AVIO layer, so fall back to a synchronous fetch.
                http_trace_log(format!(
                    "{{\"event\":\"http_prefetch_short_read\",\"start\":{},\"length\":{},\"cache_start\":{},\"cache_bytes\":{}}}",
                    range.start,
                    range
                        .length
                        .map_or_else(|| "null".to_string(), |length| length.to_string()),
                    self.cache_start,
                    self.cache_bytes.len(),
                ));
                self.fetch_range(ByteRange {
                    start: range.start,
                    length: fetch_length,
                })?
            }
            Some(Err(error)) => return Err(error),
            None => self.fetch_range(ByteRange {
                start: range.start,
                length: fetch_length,
            })?,
        };
        if range.length.is_none() {
            return Ok(fetched);
        }

        self.reset_cache(range.start, fetched);
        self.maybe_start_prefetch(range);
        let out = self.window_slice(range, requested_length).unwrap_or_default();
        trace_read_return("http_read_fetch", range, self, &out);
        Ok(out)
    }
}

/// 诊断：read_range 返回数据与窗口状态（错位定位用——比较返回字节 head
/// 与"预期文件字节"可区分窗口错位 vs 上层错位）。仅 ERIKA_HTTP_TRACE=1 时打。
fn trace_read_return(event: &str, range: ByteRange, src: &HttpRangeSource, bytes: &[u8]) {
    http_trace_log(format!(
        "{{\"event\":\"{}\",\"rs\":{},\"rl\":{},\"cs\":{},\"cb\":{},\"head\":\"{}\"}}",
        event,
        range.start,
        range.length.unwrap_or(0),
        src.cache_start,
        src.cache_bytes.len(),
        bytes
            .iter()
            .take(8)
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(""),
    ));
}

pub fn source_from_uri(uri: &str) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint(uri, MediaSourceHint::Auto)
}

/// Reads an entire URI through the same MediaSource abstraction used by FFmpeg.
///
/// This is intentionally synchronous for small sidecar assets such as danmaku or
/// subtitle files. On Android it also establishes and completes the ownership
/// transfer for `fd://` descriptors within the native call.
pub fn read_uri_to_end(uri: &str) -> Result<Vec<u8>> {
    let mut source = source_from_uri(uri)?;
    source.read_range(ByteRange::suffix_from(0))
}

pub fn source_from_uri_with_hint(
    uri: &str,
    source_hint: MediaSourceHint,
) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint_and_headers(uri, source_hint, Vec::new())
}

pub fn source_from_uri_with_hint_and_headers(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint_and_headers_slot(uri, source_hint, http_headers, 0)
}

/// 同 `source_from_uri_with_hint_and_headers`，但指定诊断槽位
/// （DASH 双流：video=0 / audio=1，各源独立发布/清空诊断互不覆盖）。
pub(crate) fn source_from_uri_with_hint_and_headers_slot(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
    diag_slot: usize,
) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint_and_headers_slot_limits(
        uri,
        source_hint,
        http_headers,
        diag_slot,
        HttpRangeSource::MAX_PREFETCH_IN_FLIGHT,
    )
}

/// 同 `source_from_uri_with_hint_and_headers_slot`，但可指定并行预取上限。
/// DASH 音频源传小值（如 2）：音频码率低，满额并发只与视频流抢带宽。
pub(crate) fn source_from_uri_with_hint_and_headers_slot_limits(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
    diag_slot: usize,
    max_prefetch_in_flight: usize,
) -> Result<Box<dyn MediaSource>> {
    match source_hint {
        MediaSourceHint::Auto => {
            source_from_auto_uri_with_slot(uri, http_headers, diag_slot, max_prefetch_in_flight)
        }
        MediaSourceHint::LocalFile => source_from_local_uri(uri),
        MediaSourceHint::Http => {
            if uri.starts_with("http://") || uri.starts_with("https://") {
                Ok(Box::new(
                    HttpRangeSource::with_http_headers_and_diag_slot_limits(
                        uri,
                        http_headers,
                        diag_slot,
                        max_prefetch_in_flight,
                    ),
                ))
            } else {
                Err(SourceError::Unsupported(uri.to_string()))
            }
        }
        MediaSourceHint::HttpStream => {
            if uri.starts_with("http://") || uri.starts_with("https://") {
                HttpLiveStreamSource::open(uri, http_headers)
                    .map(|source| Box::new(source) as Box<dyn MediaSource>)
            } else {
                Err(SourceError::Unsupported(uri.to_string()))
            }
        }
    }
}

/// Auto 提示的 URI 探测 + 源构造。**必须透传 diag_slot / 预取上限**：
/// 早期版本 HTTP 分支走 `with_http_headers`（固定槽 0），DASH 双流若都
/// 用 Auto hint（bili-rs 默认），视频与音频源会同时写诊断槽 0 互相覆盖
/// ——宿主读到"视频缓存/音频缓存"错乱（音频缓存恒 0、视频总长在两个
/// 流的长度间跳变）。
fn source_from_auto_uri_with_slot(
    uri: &str,
    http_headers: Vec<(String, String)>,
    diag_slot: usize,
    max_prefetch_in_flight: usize,
) -> Result<Box<dyn MediaSource>> {
    if uri.starts_with("fd://") {
        return source_from_local_uri(uri);
    }
    if let Some(path) = uri.strip_prefix("file://") {
        return Ok(Box::new(LocalFileSource::open(path)?));
    }
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return Ok(Box::new(
            HttpRangeSource::with_http_headers_and_diag_slot_limits(
                uri,
                http_headers,
                diag_slot,
                max_prefetch_in_flight,
            ),
        ));
    }
    let path = Path::new(uri);
    if path.exists() {
        return Ok(Box::new(LocalFileSource::open(path)?));
    }
    Err(SourceError::Unsupported(uri.to_string()))
}

fn source_from_local_uri(uri: &str) -> Result<Box<dyn MediaSource>> {
    if uri.starts_with("fd://") {
        #[cfg(target_os = "android")]
        {
            // SAFETY: accepting this URI is the ownership-transfer boundary.
            return Ok(Box::new(unsafe {
                OwnedFileDescriptorSource::open_uri(uri)?
            }));
        }
        #[cfg(not(target_os = "android"))]
        {
            return Err(SourceError::Unsupported(uri.to_string()));
        }
    }
    Ok(Box::new(LocalFileSource::open(local_path_from_uri(uri))?))
}

fn local_path_from_uri(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

fn range_contains(container: ByteRange, range: ByteRange) -> bool {
    let (Some(container_length), Some(range_length)) = (container.length, range.length) else {
        return false;
    };
    let Some(container_end) = container.start.checked_add(container_length) else {
        return false;
    };
    let Some(range_end) = range.start.checked_add(range_length) else {
        return false;
    };
    range.start >= container.start && range_end <= container_end
}

fn http_read_ahead_bytes() -> u64 {
    env::var("ERIKA_HTTP_READAHEAD_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(HttpRangeSource::DEFAULT_READ_AHEAD_BYTES)
}

fn http_back_bytes() -> u64 {
    env::var("ERIKA_HTTP_BACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(HttpRangeSource::DEFAULT_BACK_BUFFER_BYTES)
}

fn http_trace_log(line: impl AsRef<str>) {
    if !trace::env_flag("ERIKA_HTTP_TRACE") {
        return;
    }
    let path = env::var_os("ERIKA_HTTP_TRACE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/erika_http_trace.jsonl"));
    trace::append_line(line.as_ref(), path);
}

fn redacted_uri(uri: &str) -> String {
    let mut value = uri.to_string();
    for key in ["api_key=", "AccessToken="] {
        let mut search_from = 0;
        while let Some(relative) = value[search_from..].find(key) {
            let start = search_from + relative + key.len();
            let end = value[start..]
                .find('&')
                .map(|relative_end| start + relative_end)
                .unwrap_or(value.len());
            value.replace_range(start..end, "REDACTED");
            search_from = start + "REDACTED".len();
        }
    }
    value
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn http_retry_budget_stops_retrying_once_the_stall_ceiling_is_reached() {
        let fresh = Instant::now();
        assert!(http_retry_fits_budget(fresh, Duration::from_millis(200)));
        assert!(http_retry_fits_budget(fresh, Duration::from_secs(1)));

        // A fetch that already burned the budget must fail fast rather than
        // arm another attempt: read_range blocks the demuxer thread.
        let exhausted = Instant::now() - HTTP_FETCH_TOTAL_BUDGET;
        assert!(!http_retry_fits_budget(exhausted, Duration::ZERO));

        // The backoff counts against the budget, so a retry is refused when
        // only the sleep would still fit.
        let nearly_done = Instant::now() - (HTTP_FETCH_TOTAL_BUDGET - Duration::from_millis(500));
        assert!(http_retry_fits_budget(
            nearly_done,
            Duration::from_millis(200)
        ));
        assert!(!http_retry_fits_budget(nearly_done, Duration::from_secs(1)));
    }

    struct MockResponse {
        delay: Duration,
        raw: Vec<u8>,
    }

    impl MockResponse {
        fn immediate(raw: Vec<u8>) -> Self {
            Self {
                delay: Duration::ZERO,
                raw,
            }
        }

        fn delayed(delay: Duration, raw: Vec<u8>) -> Self {
            Self { delay, raw }
        }
    }

    /// Serves each response over one connection (in order) and reports every
    /// received request head through the returned channel.
    fn spawn_mock_http_server(responses: Vec<MockResponse>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let _ = sender.send(head);
                if !response.delay.is_zero() {
                    thread::sleep(response.delay);
                }
                let _ = stream.write_all(&response.raw);
                let _ = stream.flush();
            }
        });
        (uri, receiver)
    }

    fn http_206_response(start: u64, total: u64, body: &[u8]) -> Vec<u8> {
        let end = start + body.len() as u64 - 1;
        let mut raw = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len(),
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    /// A 206 head that promises `declared_length` bytes but sends fewer before
    /// the connection closes, producing a body-phase transport error.
    fn http_206_truncated_response(
        start: u64,
        total: u64,
        declared_length: usize,
        body: &[u8],
    ) -> Vec<u8> {
        let end = start + declared_length as u64 - 1;
        let mut raw = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n",
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    fn http_simple_response(status_line: &str, body: &[u8]) -> Vec<u8> {
        let mut raw = format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len(),
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    fn recv_request_head(requests: &mpsc::Receiver<String>) -> String {
        requests
            .recv_timeout(Duration::from_secs(5))
            .expect("mock server should have received a request")
            .to_lowercase()
    }

    #[test]
    fn local_file_source_reads_ranges() {
        let path = std::env::temp_dir().join(format!("erika-source-{}.bin", std::process::id()));
        {
            let mut file = File::create(&path).unwrap();
            file.write_all(b"abcdef").unwrap();
        }

        let mut source = LocalFileSource::open(&path).unwrap();
        assert_eq!(source.len().unwrap(), Some(6));
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 2,
                    length: Some(3)
                })
                .unwrap(),
            b"cde"
        );
        assert_eq!(
            read_uri_to_end(&format!("file://{}", path.display())).unwrap(),
            b"abcdef"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn source_from_uri_rejects_unknown_scheme() {
        match source_from_uri("smb://example/video.mkv") {
            Ok(_) => panic!("unexpectedly accepted unsupported source"),
            Err(error) => assert!(matches!(error, SourceError::Unsupported(_))),
        }
    }

    #[test]
    fn source_hint_controls_selection() {
        let source =
            source_from_uri_with_hint("https://example.invalid/video.mp4", MediaSourceHint::Http)
                .unwrap();
        assert_eq!(source.uri(), "https://example.invalid/video.mp4");

        assert!(matches!(
            source_from_uri_with_hint("file:///tmp/video.mp4", MediaSourceHint::Http),
            Err(SourceError::Unsupported(_))
        ));
    }

    #[test]
    fn owned_fd_uri_parses_asset_slice() {
        assert_eq!(
            parse_fd_uri("fd://42?offset=4096&length=8192").unwrap(),
            OwnedFdUri {
                fd: 42,
                offset: 4096,
                length: Some(8192),
            }
        );
        assert_eq!(
            parse_fd_uri("fd://7?length=-1").unwrap(),
            OwnedFdUri {
                fd: 7,
                offset: 0,
                length: None,
            }
        );
    }

    #[test]
    fn owned_fd_uri_rejects_invalid_or_ambiguous_values() {
        for uri in [
            "fd://-1",
            "fd://not-a-number",
            "fd://3?offset=x",
            "fd://3?offset=1&offset=2",
            "fd://3?unknown=1",
        ] {
            assert!(matches!(
                parse_fd_uri(uri),
                Err(SourceError::InvalidFileDescriptorUri(_))
            ));
        }
    }

    #[cfg(target_os = "android")]
    #[test]
    fn unregistered_owned_fd_uri_cannot_adopt_a_numeric_descriptor() {
        let error = unsafe { OwnedFileDescriptorSource::open_uri("fd://2147483647") }
            .expect_err("an unregistered descriptor must be rejected");
        assert!(matches!(
            error,
            SourceError::InvalidFileDescriptorUri(message)
                if message.contains("not explicitly transferred")
        ));
    }

    #[test]
    fn http_default_read_ahead_is_streaming_sized() {
        assert_eq!(HttpRangeSource::DEFAULT_READ_AHEAD_BYTES, 2 * 1024 * 1024);
    }

    // ---- 带宽自适应（弱网修复）：段大小 / join 超时 / 估算器 ----

    #[test]
    fn prefetch_segment_bytes_scales_with_estimated_bandwidth() {
        let mut source = HttpRangeSource::new("https://example.invalid/video.m4s");
        // 冷启动（带宽未知）：保持 256KB 原行为
        assert_eq!(
            source.prefetch_segment_bytes(),
            HttpRangeSource::SEEK_PREFETCH_SEGMENT_BYTES
        );
        // 高带宽（1MB/s，8 段并行）：1MB/s × 3s / 8 = 384KB → 封顶 256KB
        source.est_bps = 1024.0 * 1024.0;
        assert_eq!(source.prefetch_segment_bytes(), 256 * 1024);
        // 中带宽（300KB/s）：300KB/s × 3s / 8 ≈ 112KB——低于 256KB 自动缩段
        //（固定 256KB 在 <~400KB/s 时 8 段均摊会超 join 超时 → 退化同步 fetch）
        source.est_bps = 300.0 * 1024.0;
        assert!(source.prefetch_segment_bytes() < 256 * 1024);
        assert_eq!(
            source.prefetch_segment_bytes(),
            (300.0 * 1024.0 * 3.0 / 8.0) as u64
        );
        // 极低带宽（50KB/s）：缩到下限 32KB（再小 HTTP 开销占比失控）
        source.est_bps = 50.0 * 1024.0;
        assert_eq!(
            source.prefetch_segment_bytes(),
            HttpRangeSource::MIN_PREFETCH_SEGMENT_BYTES
        );
        // 音频源并发上限 2：同等带宽下段更大（每段带宽份额更高，更快完成）
        source.est_bps = 100.0 * 1024.0;
        source.max_prefetch_in_flight = 2;
        assert_eq!(
            source.prefetch_segment_bytes(),
            (100.0 * 1024.0 * 3.0 / 2.0) as u64
        );
    }

    #[test]
    fn prefetch_join_timeout_widens_when_bandwidth_is_low() {
        let mut source = HttpRangeSource::new("https://example.invalid/video.m4s");
        // 冷启动：5s 原行为
        assert_eq!(source.prefetch_join_timeout(), Duration::from_secs(5));
        // 300KB/s：整窗完成 ≈112KB×8/300KB/s ≈ 3s，×1.5 = 4.5s < 5s → 保持 5s
        source.est_bps = 300.0 * 1024.0;
        assert_eq!(source.prefetch_join_timeout(), Duration::from_secs(5));
        // 50KB/s：32KB 段整窗 32×8/50 = 5.12s，×1.5 = 7.7s → 放宽到 ~7.7s
        //（避免慢段被 5s 超时误杀后退化同步 fetch）
        source.est_bps = 50.0 * 1024.0;
        let widened = source.prefetch_join_timeout();
        assert!(widened > Duration::from_secs(5));
        assert_eq!(
            widened.as_secs_f64(),
            32.0 * 1024.0 * 8.0 / (50.0 * 1024.0) * 1.5
        );
    }

    #[test]
    fn bandwidth_estimator_accumulates_parallel_segments_and_emphasizes() {
        let mut source = HttpRangeSource::new("https://example.invalid/video.m4s");
        // 窗口内累计 8 段（并行段叠加吞吐计入同一差分窗口；elapsed<2s 只累计不更新）
        for _ in 0..8 {
            source.record_downloaded(32 * 1024);
        }
        // 拨回窗口起点 → 触发差分：约 256KB / 2s ≈ 128KB/s（冷启动直接采用）
        source.last_est_at = Instant::now() - Duration::from_secs(2);
        source.record_downloaded(1);
        assert!((source.est_bps - 128.0 * 1024.0).abs() < 2.0);
        // 第二次采样：8×64KB / 2s ≈ 256KB/s → EMA 0.7/0.3 平滑
        for _ in 0..8 {
            source.record_downloaded(64 * 1024);
        }
        source.last_est_at = Instant::now() - Duration::from_secs(2);
        source.record_downloaded(1);
        let expected = 128.0 * 1024.0 * 0.7 + 256.0 * 1024.0 * 0.3;
        assert!((source.est_bps - expected).abs() < 2.0);
        // 窗口未到期（<2s）不重算：追加字节只累计不更新
        let before = source.est_bps;
        source.record_downloaded(1024 * 1024);
        assert_eq!(source.est_bps, before);
    }

    #[test]
    fn http_source_constructor_preserves_custom_headers() {
        let source = HttpRangeSource::with_http_headers(
            "https://example.invalid/video.mp4",
            vec![
                ("Authorization".to_string(), "Bearer test".to_string()),
                ("X-Playback-Session".to_string(), "session-123".to_string()),
            ],
        );
        assert_eq!(
            source.http_headers,
            vec![
                ("Authorization".to_string(), "Bearer test".to_string()),
                ("X-Playback-Session".to_string(), "session-123".to_string()),
            ]
        );
    }

    #[test]
    fn http_source_new_starts_without_custom_headers() {
        let source = HttpRangeSource::new("https://example.invalid/video.mp4");

        assert!(source.http_headers.is_empty());
    }

    #[test]
    fn http_source_preserves_headers_without_normalizing_values() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer a+b/c==".to_string()),
            (
                "X-Client-Tag".to_string(),
                "  preserve whitespace  ".to_string(),
            ),
        ];
        let source = HttpRangeSource::with_http_headers(
            "https://example.invalid/video.mp4",
            headers.clone(),
        );

        assert_eq!(source.http_headers, headers);
    }

    #[test]
    fn range_contains_accepts_inner_byte_ranges() {
        assert!(range_contains(
            ByteRange {
                start: 100,
                length: Some(200),
            },
            ByteRange {
                start: 128,
                length: Some(64),
            },
        ));
        assert!(!range_contains(
            ByteRange {
                start: 100,
                length: Some(200),
            },
            ByteRange {
                start: 280,
                length: Some(64),
            },
        ));
    }

    #[test]
    fn content_range_total_parses_totals_and_rejects_unknown() {
        assert_eq!(parse_content_range_total("bytes 0-99/1234"), Some(1234));
        assert_eq!(parse_content_range_total("bytes 100-199/200"), Some(200));
        assert_eq!(parse_content_range_total("bytes */555"), Some(555));
        assert_eq!(parse_content_range_total("bytes 0-99/*"), None);
        assert_eq!(parse_content_range_total("items 0-99/1234"), None);
        assert_eq!(parse_content_range_total(""), None);
    }

    #[test]
    fn length_probe_keeps_non_range_http_statuses_as_errors() {
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )]);
        let error = probe_http_total_length(&http_agent(), &uri, &[]).unwrap_err();
        assert!(error.to_string().contains("404"));
    }

    #[test]
    fn http_range_rejects_status_200_for_nonzero_offset() {
        let body = vec![b'a'; 100];
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_simple_response("200 OK", &body),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(100);
        let error = source
            .read_range(ByteRange {
                start: 10,
                length: Some(10),
            })
            .expect_err("a 200 answer to a mid-file Range request must fail");
        assert!(matches!(
            error,
            SourceError::Http(message) if message.contains("ignored Range")
        ));
        assert!(recv_request_head(&requests).contains("range: bytes=10-99"));
    }

    #[test]
    fn http_range_accepts_status_200_for_whole_file_and_backfills_total() {
        let body = b"whole-file-payload".to_vec();
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_simple_response("200 OK", &body),
        )]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(
            source.read_range(ByteRange::suffix_from(0)).unwrap(),
            body.clone()
        );
        // Content-Length of the 200 response backfills the total without HEAD.
        assert_eq!(source.len().unwrap(), Some(body.len() as u64));
    }

    #[test]
    fn http_range_backfills_total_from_206_content_range() {
        let body = vec![b'x'; 16];
        let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(0, 4096, &body),
        )]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.read_range(ByteRange::suffix_from(0)).unwrap(), body);
        // The 206 Content-Range total satisfies len() without a HEAD request.
        assert_eq!(source.len().unwrap(), Some(4096));
    }

    #[test]
    fn http_range_retries_after_server_error() {
        let body: Vec<u8> = (0..64u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_simple_response("500 Internal Server Error", b"boom")),
            MockResponse::immediate(http_206_response(0, 64, &body)),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(64),
                })
                .unwrap(),
            body
        );
        assert!(recv_request_head(&requests).contains("range: bytes=0-63"));
        assert!(recv_request_head(&requests).contains("range: bytes=0-63"));
    }

    #[test]
    fn http_range_resumes_truncated_body_from_received_offset() {
        let body: Vec<u8> = (0..64u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            // Promises 64 bytes but closes after 32: a body-phase error.
            MockResponse::immediate(http_206_truncated_response(0, 64, 64, &body[..32])),
            MockResponse::immediate(http_206_response(32, 64, &body[32..])),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(64),
                })
                .unwrap(),
            body
        );
        assert!(recv_request_head(&requests).contains("range: bytes=0-63"));
        // The resumed request must start where the truncated body stopped.
        assert!(recv_request_head(&requests).contains("range: bytes=32-63"));
    }

    #[test]
    fn prefetch_fills_multiple_parallel_segments() {
        let body: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_response(1024 * 1024, 128 * 1024 * 1024, &body)),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 2 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 3 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 4 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 5 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 6 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 7 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(128 * 1024 * 1024);
        // 段大小固定 256KB（SEEK_PREFETCH_SEGMENT_BYTES），read_ahead 需大于段大小
        // 才能拆出多段并行。12MB 窗口 / 256KB 段 → 48 段（到 MAX_PREFETCH_IN_FLIGHT
        // 上限 8）。mock server 必须同配 8 个响应连接——每个 in-flight 段占一个。
        source.read_ahead_bytes = 12 * 1024 * 1024;
        source.cache_start = 0;
        source.cache_bytes = vec![0u8; 1024 * 1024];
        // 请求末端 == 缓存末尾（remaining=0）→ 立即触发多段并行预取
        let range = ByteRange {
            start: 0,
            length: Some(1024 * 1024),
        };
        source.maybe_start_prefetch(range);
        // 填满上限个并行段，且从 cache_end 起连续、覆盖 read_ahead 窗口
        assert_eq!(
            source.prefetch.len(),
            HttpRangeSource::MAX_PREFETCH_IN_FLIGHT
        );
        let mut start = source.cache_end();
        for p in &source.prefetch {
            assert_eq!(p.range.start, start);
            start = p.range_end();
        }
        assert!(start <= source.cache_end() + source.read_ahead_bytes);
        // 全部后台请求均已发出（互不阻塞、并行发起）
        for _ in 0..HttpRangeSource::MAX_PREFETCH_IN_FLIGHT {
            let _ = recv_request_head(&requests);
        }
    }

    #[test]
    fn reset_cache_keeps_segments_beyond_new_window() {
        // 顺序读取：消费并合并第一段后窗口前移，窗口末尾之后的在飞段是
        // 未来数据，reset_cache 必须保留（无脑清空会浪费在飞下载）。
        let body: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_response(1024 * 1024, 128 * 1024 * 1024, &body)),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 2 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 3 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 4 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 5 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 6 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 7 * 698 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(128 * 1024 * 1024);
        // 12MB read_ahead / 256KB 段 → 48 段（上限 8 并行，见 prefetch_fills 注释）；
        // mock server 同配 8 个响应连接。
        source.read_ahead_bytes = 12 * 1024 * 1024;
        // 本测试只验证预取段保留语义：禁用后向缓冲，窗口起点保持新数据起点
        source.back_bytes = 0;
        source.cache_start = 0;
        source.cache_bytes = vec![0u8; 1024 * 1024];
        let range = ByteRange {
            start: 0,
            length: Some(1024 * 1024),
        };
        source.maybe_start_prefetch(range);
        assert_eq!(
            source.prefetch.len(),
            HttpRangeSource::MAX_PREFETCH_IN_FLIGHT
        );
        let segments: Vec<u64> = source.prefetch.iter().map(|p| p.range.start).collect();
        // 模拟 read_range 合并了第一段（take_prefetch 已 remove 它）：
        // 新窗口 [0, 第一段末尾)，窗口末尾之后的段应保留
        let merged_end = segments[1]; // 段连续：第二段起点即第一段末尾
        source.reset_cache(0, vec![0u8; merged_end as usize]);
        assert_eq!(
            source.prefetch.len(),
            HttpRangeSource::MAX_PREFETCH_IN_FLIGHT - 1
        );
        for p in &source.prefetch {
            assert!(p.range.start >= source.cache_end());
        }
        for _ in 0..HttpRangeSource::MAX_PREFETCH_IN_FLIGHT {
            let _ = recv_request_head(&requests);
        }
    }

    #[test]
    fn rewind_seek_hits_back_buffer_without_redownload() {
        // 后向缓冲：顺序播放推进后 seek 回退到后向区（mpv demuxer-max-back-bytes
        // 等价），命中缓存不重新下载。两次顺序读后窗口含后向区，回退读无网络请求。
        let body: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_response(0, 512, &body)),
            MockResponse::immediate(http_206_response(256, 512, &body)),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(512);
        source.back_bytes = 512;
        // 首次下载 [0, 256)
        let first = source
            .read_range(ByteRange {
                start: 0,
                length: Some(256),
            })
            .unwrap();
        assert_eq!(first, body);
        // 顺序续读 [256, 512)：reset_cache 保留 [0, 256) 后向区，窗口 [0, 512)
        let second = source
            .read_range(ByteRange {
                start: 256,
                length: Some(256),
            })
            .unwrap();
        assert_eq!(second, body);
        assert_eq!(source.cache_start, 0);
        assert_eq!(source.cache_bytes.len(), 512);
        let _ = recv_request_head(&requests);
        let _ = recv_request_head(&requests);
        // seek 回 [0, 128)：命中后向缓冲 → 无新的网络请求
        let rewind = source
            .read_range(ByteRange {
                start: 0,
                length: Some(128),
            })
            .unwrap();
        assert_eq!(rewind, body[..128]);
        assert!(requests.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn reset_cache_drops_back_buffer_when_rewind_exceeds_cap() {
        // 后向缓冲上限：回退超过 back_bytes 时旧数据整体丢弃、从目标全新建窗
        //（与 mpv demuxer-max-back-bytes 一致，超范围 seek 重新拉流）。
        let body: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_response(0, 1024, &body)),
            MockResponse::immediate(http_206_response(256, 1024, &body)),
            MockResponse::immediate(http_206_response(0, 1024, &body)),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(1024);
        source.back_bytes = 128; // 后向上限小于回退距离
        let _ = source
            .read_range(ByteRange {
                start: 0,
                length: Some(256),
            })
            .unwrap();
        let _ = source
            .read_range(ByteRange {
                start: 256,
                length: Some(256),
            })
            .unwrap();
        // 后向上限 128：窗口起点回退到 256-128=128，保留 [128,256) 的 128B
        assert_eq!(source.cache_start, 128);
        assert_eq!(source.cache_bytes.len(), 128 + 256);
        let _ = recv_request_head(&requests);
        let _ = recv_request_head(&requests);
        // seek 回 [0, 64)：超出后向区（窗口起点在 128）→ 重新下载
        let _ = source
            .read_range(ByteRange {
                start: 0,
                length: Some(64),
            })
            .unwrap();
        assert_eq!(source.cache_start, 0);
        let _ = recv_request_head(&requests); // 第三次网络请求发生
        assert!(requests.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn take_prefetch_joins_inflight_thread_instead_of_refetching() {
        let body: Vec<u8> = (0..100u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::delayed(
            Duration::from_millis(250),
            http_206_response(0, 100, &body),
        )]);
        let mut source = HttpRangeSource::new(uri.clone());
        source.content_length = Some(100);
        let range = ByteRange {
            start: 0,
            length: Some(100),
        };
        source.prefetch = vec![PendingHttpFetch::spawn(uri, Vec::new(), range)];
        assert_eq!(source.read_range(range).unwrap(), body);
        let _ = recv_request_head(&requests);
        // Joining the pending prefetch must not issue a duplicate download.
        assert!(requests.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn short_prefetch_read_falls_back_to_synchronous_fetch() {
        let body: Vec<u8> = (0..100u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            // Prefetch answer is complete HTTP but shorter than the request.
            MockResponse::immediate(http_206_response(0, 100, &body[..50])),
            MockResponse::immediate(http_206_response(0, 100, &body)),
        ]);
        let mut source = HttpRangeSource::new(uri.clone());
        source.content_length = Some(100);
        let range = ByteRange {
            start: 0,
            length: Some(100),
        };
        source.prefetch = vec![PendingHttpFetch::spawn(uri, Vec::new(), range)];
        // A short prefetch must trigger the synchronous fallback, not an
        // empty (fake-EOF) read.
        assert_eq!(source.read_range(range).unwrap(), body);
        let _ = recv_request_head(&requests);
        assert!(recv_request_head(&requests).contains("range: bytes=0-99"));
    }

    #[test]
    fn len_retries_head_before_succeeding() {
        let head_response = b"HTTP/1.1 200 OK\r\nContent-Length: 4321\r\nConnection: close\r\n\r\n";
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_simple_response("500 Internal Server Error", b"")),
            MockResponse::immediate(head_response.to_vec()),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(4321));
        assert!(recv_request_head(&requests).starts_with("head"));
        assert!(recv_request_head(&requests).starts_with("head"));
    }

    #[test]
    fn len_probe_does_not_download_a_body_that_ignores_range() {
        // HEAD is rejected and the origin ignores Range, answering 200 with the
        // whole object. The probe must take the total from Content-Length and
        // leave the payload on the wire instead of buffering the media.
        let payload = vec![b'x'; 512 * 1024];
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        raw.extend_from_slice(&payload);
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
            MockResponse::immediate(raw),
        ]);

        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(payload.len() as u64));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"), "probe head: {probe}");
    }

    #[test]
    fn resumed_range_is_bound_to_the_first_response_entity() {
        let body: Vec<u8> = (0..64u8).collect();
        // Same truncated-then-resume shape as the resume test above, but the
        // first response carries a validator the retry has to replay.
        let mut truncated = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-63/64\r\nContent-Length: 64\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        truncated.extend_from_slice(&body[..32]);
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(truncated),
            MockResponse::immediate(http_206_response(32, 64, &body[32..])),
        ]);

        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        assert_eq!(
            source
                .read_range(ByteRange {
                    start: 0,
                    length: Some(64),
                })
                .unwrap(),
            body
        );

        let first = recv_request_head(&requests);
        assert!(!first.contains("if-range"), "first request: {first}");
        let resumed = recv_request_head(&requests);
        assert!(
            resumed.contains("if-range: \"v1\""),
            "resumed request must replay the validator: {resumed}"
        );
    }

    #[test]
    fn resumed_range_rejects_a_response_served_from_a_different_offset() {
        let body: Vec<u8> = (0..64u8).collect();
        let (uri, _requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_truncated_response(0, 64, 64, &body[..32])),
            // The resume asked for byte 32; this answers from 0 instead, which
            // would splice mismatched bytes onto the prefix already held.
            MockResponse::immediate(http_206_response(0, 64, &body[..32])),
        ]);

        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(64);
        let error = source
            .read_range(ByteRange {
                start: 0,
                length: Some(64),
            })
            .unwrap_err();
        assert!(
            error.to_string().contains("served range from 0"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn len_falls_back_to_range_probe_when_head_is_rejected() {
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
            MockResponse::immediate(http_206_response(0, 1234, b"z")),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(1234));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"));
    }

    #[test]
    fn len_falls_back_to_range_probe_when_head_reports_zero() {
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ),
            MockResponse::immediate(http_206_response(0, 911_198_509, b"z")),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(911_198_509));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"));
    }

    #[test]
    fn len_preserves_zero_when_range_probe_confirms_empty_resource() {
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ),
            MockResponse::immediate(
                b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
        ]);
        let mut source = HttpRangeSource::new(uri);
        assert_eq!(source.len().unwrap(), Some(0));
        assert!(recv_request_head(&requests).starts_with("head"));
        let probe = recv_request_head(&requests);
        assert!(probe.starts_with("get"));
        assert!(probe.contains("range: bytes=0-0"));
    }

    #[test]
    fn redacted_uri_hides_access_tokens() {
        assert_eq!(
            redacted_uri("https://example.invalid/video.mkv?api_key=secret&x=1"),
            "https://example.invalid/video.mkv?api_key=REDACTED&x=1"
        );
        assert_eq!(
            redacted_uri("https://example.invalid/video.mkv?AccessToken=secret"),
            "https://example.invalid/video.mkv?AccessToken=REDACTED"
        );
    }
}
