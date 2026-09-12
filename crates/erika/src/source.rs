#[cfg(target_os = "android")]
use std::collections::HashMap;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(target_os = "android")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

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
    /// **实时**累计已接收字节（字节在线路上到达即计入，不等待整段完成）。
    ///
    /// 语义变更（2026-09-01）：此前只在「整段下载完成」时累加，慢网下一个
    /// 预取段要数秒到十几秒才完成，传输期间本值不动 → 宿主差分恒为 0 →
    /// 把「正在缓慢下载」误判成「无供给」，触发饥饿换源 / ABR 连环降档
    /// （用户反馈的「无故换源」「自动降级画质」根因）。现由 `fetch_http_range`
    /// 在 body 分块读循环里逐块 `fetch_add`，涓流也如实反映。
    ///
    /// 含**被丢弃**的字节（seek 后作废的在飞段、整片预载被前移丢弃的段）——
    /// 带宽估算要的是「线路上真实到达的量」，丢不丢不影响吞吐判定。
    pub downloaded_bytes: u64,
    /// 源内带宽估算（字节/秒，2s 滑动 EMA）。
    ///
    /// **拉模型**（2026-09-01 改）：此前是推模型——只在「段完成」事件里结算，
    /// 慢网下 est_bps 会在整段传输期间**冻结**在旧值（不归零、也不更新），
    /// 与宿主差分同样失真。现在任何读取方（`publish_diag` 或宿主
    /// `source_diag_snapshot_slot`）调用即以「实时已收字节总量」做窗口差分，
    /// 与段完成事件解耦——即使 demux 线程正阻塞在长 join 上，宿主的 500ms
    /// poll 也能读到新鲜值。
    pub est_bps: f64,
    /// 当前活跃 host（多源无缝换 host 用）：显示"现在用的是哪家 CDN"，
    /// 换源时随之变化，便于直观确认切换生效。无候选时为空。
    pub active_host: String,
    /// 当前活跃候选下标（0 = candidates[0]）。
    pub active_idx: usize,
    /// 候选 host 总数（0 = 单源模式）。
    pub candidate_count: usize,
    /// demux 端缓存窗口终点（**未合并**整片预载桶）。宿主策略算「真实可播
    /// 缓冲秒」必须用它：`cache_end = max(demux, 桶 covered_until)` 会把桶
    /// 从 0 顺序预载的进度混进来，seek 后窗口被严重高估 → ABR「余粮充足」
    /// 误判不降档 / starve 判不出来（2026-09-02 无感化改造 K4）。
    pub demux_cache_end: u64,
    /// 源 host 代数（每次实际切换 +1）。宿主据此确认 request_host_switch 已生效。
    pub host_epoch: u64,
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
        active_host: String::new(),
        active_idx: 0,
        candidate_count: 0,
        demux_cache_end: 0,
        host_epoch: 0,
    },
    SourceDiagSnapshot {
        content_length: None,
        cache_start: 0,
        cache_end: 0,
        prefetch_in_flight: 0,
        downloaded_bytes: 0,
        est_bps: 0.0,
        active_host: String::new(),
        active_idx: 0,
        candidate_count: 0,
        demux_cache_end: 0,
        host_epoch: 0,
    },
]);

/// 每槽位的「实时已收字节」计数器集合（一个槽可能同时登记多个计数器：
/// 源自身的 `rx` + 整片预载桶的 `downloaded`）。
///
/// HTTP 段在 body 分块读循环里直接 `fetch_add` 到这里登记的计数器，因此
/// 慢网下「段在飞、未完成」期间总量也在增长——这是 `downloaded_bytes` /
/// `est_bps` 不再失真的基础。源构造时登记、`Drop` 时按 `Arc::ptr_eq` 注销。
static SOURCE_RX: Mutex<[Vec<Arc<AtomicU64>>; SOURCE_DIAG_SLOTS]> =
    Mutex::new([Vec::new(), Vec::new()]);

/// 带宽估算的窗口采样状态（拉模型用）。
struct BandwidthSample {
    at: Instant,
    total: u64,
    bps: f64,
}

/// 每槽位上一窗口的采样点。宿主 poll 与 `publish_diag` 共用——先到者触发结算，
/// 后到者读到缓存值（窗口未满时复用），保证「同一窗口内多次读取得到同一值」。
static SOURCE_BW: Mutex<[Option<BandwidthSample>; SOURCE_DIAG_SLOTS]> = Mutex::new([None, None]);

/// 音频优先缓冲协调器（2026-09-02）。
///
/// 播放 DASH 双流时，音频体积小（即便 192K 也仅数十 MB），应优先把它整条
/// 拉完，从源头杜绝「音频饿死」（4K 弱网一卡一顿、声音卡住拖垮视频时钟的根因）。
/// 音频源用**激进（不向 demux 让路）的整片预载桶**把音轨尽快拉完，并点亮本
/// 协调器；视频源据此在音频未达标前压低自身并发（预取降到 1 段 + 整片桶让路），
/// 把带宽完整让给音频。单流/直播不点亮协调器 → 无节流，行为不变。
///
/// 可用 `ERIKA_AUDIO_FIRST=0` 整体关闭（退回双段并行预取）；
/// `ERIKA_AUDIO_FIRST_THRESHOLD_PERMILLE`（默认 500 = 50%）调相位阈值——
/// 音频预载进度达到该千分比前视频保持节流，达到后视频恢复全速。
struct AudioFirstCoord {
    /// 音频优先是否激活（仅当音频源启用激进整片预载时点亮）。
    enabled: AtomicBool,
    /// 音频整片预载已覆盖进度（0..=1000，千分比）。
    audio_permille: AtomicU64,
    /// 音频总长是否已探测到（未探测到时视频同样节流，先让音频跑）。
    audio_total_known: AtomicBool,
    /// 点亮时刻（单调毫秒），用于防呆：点亮过久（如音频总长迟迟探测不到）
    /// 则放弃节流，避免视频被永久压低。
    enabled_at_ms: AtomicU64,
}
static AUDIO_FIRST: AudioFirstCoord = AudioFirstCoord {
    enabled: AtomicBool::new(false),
    audio_permille: AtomicU64::new(0),
    audio_total_known: AtomicBool::new(false),
    enabled_at_ms: AtomicU64::new(0),
};
static AUDIO_FIRST_START: OnceLock<Instant> = OnceLock::new();
/// 音频优先最长节流时长：超过则放弃节流（兜底，避免音频异常时视频被永久压低）。
const AUDIO_FIRST_MAX_THROTTLE_MS: u64 = 45_000;

fn audio_first_now_ms() -> u64 {
    AUDIO_FIRST_START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// 是否启用音频优先策略（默认开，`ERIKA_AUDIO_FIRST=0/false` 关闭）。
fn audio_first_env_enabled() -> bool {
    std::env::var("ERIKA_AUDIO_FIRST")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true)
}

/// 相位阈值：音频预载进度达到该千分比后视频恢复全速（默认 50%）。
fn audio_first_threshold_permille() -> u64 {
    std::env::var("ERIKA_AUDIO_FIRST_THRESHOLD_PERMILLE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(500)
}

/// 视频源是否应被节流（把带宽让给尚未达标的音频）。
fn audio_first_throttle_video() -> bool {
    if !AUDIO_FIRST.enabled.load(Ordering::Relaxed) {
        return false;
    }
    if !audio_first_env_enabled() {
        return false;
    }
    // 防呆：点亮过久（如音频总长迟迟探测不到）则放弃节流。
    let since = audio_first_now_ms().saturating_sub(AUDIO_FIRST.enabled_at_ms.load(Ordering::Relaxed));
    if since > AUDIO_FIRST_MAX_THROTTLE_MS {
        return false;
    }
    if !AUDIO_FIRST.audio_total_known.load(Ordering::Relaxed) {
        // 音频总长还没探测到 → 先让音频跑，视频节流。
        return true;
    }
    AUDIO_FIRST.audio_permille.load(Ordering::Relaxed) < audio_first_threshold_permille()
}

/// 带宽估算差分窗口：每 2s 用窗口内**实时到达**的字节更新一次 EMA。
/// 太短（<1s）会被单次请求的突发抖动带偏；太长响应迟缓。
const BANDWIDTH_ESTIMATE_WINDOW: Duration = Duration::from_secs(2);

/// EMA 权重：新瞬时值 0.3 / 历史 0.7（沿用推模型取值，行为连续）。
const BANDWIDTH_EMA_NEW: f64 = 0.3;

/// 槽位实时已收字节总量（该槽登记的所有计数器之和）。
fn live_rx_bytes(slot: usize) -> u64 {
    let Ok(counters) = SOURCE_RX.lock() else {
        return 0;
    };
    counters
        .get(slot)
        .map(|list| {
            list.iter()
                .map(|c| c.load(Ordering::Relaxed))
                .fold(0u64, u64::saturating_add)
        })
        .unwrap_or(0)
}

/// 带宽估算（**拉模型**）：以「实时已收字节总量」为基准做窗口差分并更新 EMA。
///
/// 关键性质：与「段完成事件」解耦。旧推模型在慢网下整段传输期间不结算，
/// est_bps 冻结在旧值；现在任何调用方（demux 的 `publish_diag` 或宿主的
/// `source_diag_snapshot_slot`）调用都会按真实窗口时间结算——宿主 500ms
/// 轮询即使撞上 demux 正阻塞在长 join 上，也能读到新鲜供给值。
///
/// 跨源（换源）时总量会回落（旧计数器注销、新计数器从 0 起），此处用
/// `saturating_sub` 兜底——该窗口瞬时值记 0，EMA 平滑吸收，不会出负速率。
fn refresh_bandwidth(slot: usize, total_rx: u64) -> f64 {
    let now = Instant::now();
    let Ok(mut slots) = SOURCE_BW.lock() else {
        return 0.0;
    };
    let Some(entry) = slots.get_mut(slot) else {
        return 0.0;
    };
    let Some(prev) = entry.as_ref() else {
        // 冷启动：只记基线，本窗口不出估算（避免把"从 0 起"算成瞬时速率）。
        *entry = Some(BandwidthSample {
            at: now,
            total: total_rx,
            bps: 0.0,
        });
        return 0.0;
    };
    let elapsed = now.duration_since(prev.at);
    if elapsed < BANDWIDTH_ESTIMATE_WINDOW {
        return prev.bps;
    }
    let inst_bps = total_rx.saturating_sub(prev.total) as f64 / elapsed.as_secs_f64();
    let mut bps = if prev.bps > 0.0 {
        prev.bps * (1.0 - BANDWIDTH_EMA_NEW) + inst_bps * BANDWIDTH_EMA_NEW
    } else {
        inst_bps
    };
    // 零供给地板：长时间无字节（暂停/挂起数分钟）时 EMA 指数衰减趋零但**永不
    // 等于 0**——denormal 级的 est_bps 仍满足 `> 0.0`，自适应路径
    // （`prefetch_join_timeout` 的 `seg / est_bps`）会拿它当真实带宽去除，
    // 得到天文数字秒数，`Duration::from_secs_f64` 对 "too big" 直接 panic。
    // 低于 1bps 归零，回到「est_bps <= 0 = 未知」语义（下游走默认值分支）。
    if bps < 1.0 {
        bps = 0.0;
    }
    *entry = Some(BandwidthSample {
        at: now,
        total: total_rx,
        bps,
    });
    bps
}

/// 读取当前 `HttpRangeSource` 诊断快照（宿主轮询用；无播放时返回零值/上次刷新）。
/// 兼容旧单槽语义：返回槽 0（主流/单流/直播）。
pub fn source_diag_snapshot() -> SourceDiagSnapshot {
    source_diag_snapshot_slot(0)
}

/// 读取指定槽位的诊断快照。DASH 双流约定 video=0、audio=1，
/// 各源独立发布/清空，宿主据此分别显示两条流的缓冲状态。
/// 槽位越界或无对应源时返回零值。
pub fn source_diag_snapshot_slot(slot: usize) -> SourceDiagSnapshot {
    let mut snap = SOURCE_DIAG
        .lock()
        .unwrap()
        .get(slot)
        .cloned()
        .unwrap_or_default();
    // **读取时刻刷新供给信号**（拉模型的核心）。快照其余字段（缓存窗口/在飞段
    // 数/总长）由 demux 线程写，demux 阻塞期间不动是合理的；但供给速度必须实时
    // ——宿主 500ms 一次的策略 poll 正是靠这里拿到真值。若 demux 正卡在长 join
    // 上（慢网常态），没有这一步 est_bps 与 downloaded_bytes 会一起冻结。
    let total = live_rx_bytes(slot);
    snap.downloaded_bytes = total;
    snap.est_bps = refresh_bandwidth(slot, total);
    snap
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
    uri: Arc<Mutex<String>>,
    /// 打开时登记的主 URI（首候选）。多源换 host 时只变 `uri`（活跃 fetch
    /// 地址），此字段保持不变，作为源的稳定标识供 `uri()` / 日志使用。
    primary_uri: String,
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
    /// 累计**已消费**的字节（同步 fetch 完成 + 预取段被 join 时累加）。
    /// 仅供内部记账，不参与任何取数逻辑；对外诊断一律走 `rx`（实时已收）。
    downloaded_bytes: u64,
    /// **实时已收字节**计数器：本源所有 HTTP 段（同步 fetch / 并行预取 /
    /// 整片预载）在 body 分块读循环里逐块 `fetch_add`。登记进全局
    /// `SOURCE_RX[diag_slot]`，宿主 poll 与 `publish_diag` 都读它。
    /// 与 `downloaded_bytes` 的区别：前者是「线路上到达即计」，后者是
    /// 「整段完成并被消费才计」——慢网下二者差距就是一个段的传输时长。
    rx: Arc<AtomicU64>,
    /// 后台整片预载（见 `FullPreloadState`）。None = 不启用（直播源等）。
    full_preload: Option<Arc<FullPreloadState>>,
    /// 后台整片预载线程句柄。**故意不在 `Drop` 里 join**——预载线程会一直
    /// 跑到文件尾（数 GB），若窗口关闭时 join 会把 UI 线程卡到下载完成，正是
    /// 用户最忌讳的"关窗卡死"。`Drop` 只置 `stop = true`，线程在下次循环看到
    /// 后即退出（退出前可能再发一个 256KB 段请求，几十毫秒级，可接受），不
    /// 阻塞任何调用方。句柄留在这里仅用于 drop 时显式 `drop(handle)` 让线程
    /// 在 stop 后自然结束、不被提前 detach 成 leaked（保持可观测）。
    preload_thread: Option<std::thread::JoinHandle<()>>,
    /// 多 host 候选（按快→慢排序；index 0 最优）。Option A 无缝换 host：
    /// 当前 host 慢/死时，无需重建 source、缓存零丢失地切到下一个候选，
    /// 循环回到起点。空 = 单源模式（沿用旧行为）。
    candidates: Vec<CandidateHost>,
    /// 当前活跃候选下标（0 = candidates[0]）。
    active_idx: usize,
    /// 最近一次收到字节的 epoch 毫秒（死连检测：长时间无字节且 demux 仍在
    /// 调用 read_range = 源已死，触发换 host）。
    last_byte_ms: u64,
    /// 最近一次被 read_range 调用的 epoch 毫秒（活跃检测：demux 在等数据
    /// 才算"在播"，暂停时不误换）。
    last_read_call_ms: u64,
    /// 换 host 冷却截止时刻：避免一轮内对同一死连反复横跳。
    switch_cooldown_until: Instant,
    /// 源 host 代数：每次 `maybe_switch_host` 实际切换时 +1。在飞预取段记录
    /// spawn 时刻的代数，代数不匹配 = 段落在旧 host 上 → 作废（K1）。
    host_epoch: u64,
    /// 宿主经 `request_host_switch(slot)` 下发的强制切换请求计数（本源上次
    /// 消费到的值）。构造/`set_candidates` 时同步为当前计数，防止一开播就被
    /// 旧请求误触发（K4）。
    last_switch_req: u64,
    /// 重入护栏：切换中重发预取段会经 `spawn_prefetch_window` →
    /// `publish_diag` → 回到 `maybe_switch_host`。护栏让重入调用直接返回
    /// （切后已有冷却兜底，护栏只是不让重入路径再做无谓判定）。
    switching_host: bool,
}

/// 多 host 候选（Option A 无缝换 host 用）。
struct CandidateHost {
    uri: String,
    /// 诊断/排序用得分（B站侧按历史失败分+测速综合得出，越大越好）。
    score: f64,
}

struct PendingHttpFetch {
    range: ByteRange,
    handle: JoinHandle<()>,
    receiver: mpsc::Receiver<Result<HttpRangeResponse>>,
    /// spawn 时刻的源 host epoch。宿主等待本段期间若发生 host 切换
    ///（`take_prefetch` 分片轮询里调用 `maybe_switch_host`），epoch 不匹配
    /// 即判定本段落在旧 host 上 → 作废并返回 None，让 read_range 走同步
    /// fetch（取**当前** uri = 新 host）——这是「卡在等段时也能换 host」
    /// 的关键一环（2026-09-02 无感化改造 K1/K3）。
    host_epoch: u64,
}

/// Bytes fetched for one HTTP range request plus the resource total reported
/// by the server (`Content-Range` on 206, `Content-Length` on a whole-file
/// 200). The total lets callers backfill `content_length` when HEAD is
/// unavailable (e.g. servers answering HEAD with 405).
struct HttpRangeResponse {
    bytes: Vec<u8>,
    total_length: Option<u64>,
}

/// 后台整片预载存储（用户定规 2026-08-30：打开播放页就应持续缓冲直到整片
/// 下载完毕，暂停也不例外；窗口关闭随源销毁）。独立线程顺序下载文件到内
/// 存桶，与 demux 队列/播放时钟完全解耦——队列满、暂停、seek 都不中断。
/// `read_range` 完整落在桶区段内时直接取用（零网络）；`spawn_prefetch_window`
/// 跳过桶已覆盖的区段（不重复下载）。诊断 `cache_end`/`downloaded_bytes`
/// 并入桶进度（缓冲前沿/下载速度如实反映整片下载）。
struct FullPreloadState {
    /// 本桶所属的诊断槽（0 = 视频/单流，1 = 音频）。只用于诊断上报——
    /// 落盘失败降级是必须被宿主看见的事实，而报告里得说清是哪条流。
    slot: usize,
    stop: AtomicBool,
    /// 桶已覆盖到的**绝对**文件偏移终点（= `inner.start` + 桶窗口长度）。
    /// 0 = 尚未开始。注意桶起点会前移，故区间未必从 0 起（见 `start`）。
    end: AtomicU64,
    /// 累计网络下载字节（诊断速度差分用，单调递增）。
    ///
    /// 用 `Arc` 包一层的原因：它是**实时**计数器，需要登记进全局
    /// `SOURCE_RX` 供宿主 poll 读取（与源自身的 `rx` 同口径）。`fetch_http_range`
    /// 在 body 分块读循环里直接 `fetch_add`，因此暂停后桶接管下载时诊断面板
    /// 也能看到非零速度。
    downloaded: Arc<AtomicU64>,
    /// demux 端已覆盖到的前沿（`cache_end`）。桶**跳过**该偏移之前的区间，
    /// 避免与 demux 并行预取重复下载同一段——实测重复下载把有效供给砍半
    /// （23KB/s 里一半是重复字节，缓冲永远建不起来 → 一路饥渴换源 → 换到
    /// 差源就表现为"拉流失败"）。由 `read_range` 每次同步，单调取 max。
    floor: AtomicU64,
    /// demux 最近一次 `read_range` 的墙上时钟（epoch 毫秒）。用于判定 demux
    /// 是否正在消费（播放中）——活跃时桶让路，见 `demux_active`。
    last_demux_read_ms: AtomicU64,
    inner: Mutex<FullPreloadInner>,
}

// ---------------------------------------------------------------------------
// 整片预载桶的存储后端（落盘改造）
// ---------------------------------------------------------------------------

/// 整片预载桶的存储后端。
///
/// 语义与原来的纯内存 `Vec<u8>` **完全一致**：持有文件 `[start, start + len)`
/// 的连续字节，读方（`copy_range`）不感知后端差异。改造动机是把这条桶的常驻
/// 内存降到 0——实测打开一个 1080P 视频桶覆盖 39.2MB、长视频 300~400MB、上限
/// 512MB，DASH 双流就是两份这样的常驻内存。落盘后这份数据由 OS page cache
/// 承担（**可回收**），RSS 不再被整条流钉死，而"打开就持续缓冲直到整片下载
/// 完毕"的弱网整片耐受一点不减。
///
/// `Memory` 分支保留：磁盘写失败（磁盘满 / 权限 / 杀软锁）必须能就地回退。
/// 落盘是省内存的优化，不是可用性依赖。
enum PreloadStore {
    /// 全内存桶：`Vec` 即 `[start, end)` 的字节，隐含起点是 `FullPreloadInner::start`。
    Memory(Vec<u8>),
    /// 落盘桶（布局见 `DiskStore`）。
    Disk(DiskStore),
}

/// 桶文件布局：
///
/// ```text
/// [0, 32)   头部：magic(8) + 版本(4) + 保留(4) + data_start(8) + data_len(8)
/// [32, ..)  数据区：文件 [data_start, data_start + data_len) 的连续字节
/// ```
///
/// 物理偏移 = `DISK_HEADER_BYTES + (逻辑偏移 - data_start)`。
///
/// `data_start` 只在**桶整体前移**时改变（demux 预取跑赢桶，见
/// `full_preload_step` 的前移分支）：那一步把逻辑窗口重置到 `floor`，同时把
/// 数据区截断成零长、`data_start` 换成新起点。为什么不"只挪逻辑偏移、保留
/// 旧物理布局"：前移的触发条件是 `floor > end`，新起点落在旧窗口终点**之后**，
/// 沿用旧布局就会在文件里留下 `[end, floor)` 的空洞——洞里的字节永远不会被
/// 读，却按文件长度真实占盘（NTFS 非稀疏文件照实分配），长片反复前移能把盘
/// 吃光。截断后文件长度恒等于"当前窗口长度 + 32"。
///
/// 读写都在 `FullPreloadState::inner` 锁内完成：前移会重置整个物理布局，
/// 锁外读会撞上被截断/被重写的区间而读到**错位字节**（静默数据损坏）。代价
/// 可接受——读命中的是尾部窗口或 page cache，写只是顺序 append。
struct DiskStore {
    file: PreloadFile,
    /// 数据区首字节对应的**逻辑**文件偏移（头部 `data_start` 字段）。恒等于
    /// `FullPreloadInner::start`（由 `reset_at` 同步维护）；单独存一份是为了
    /// 跨进程复用残留文件时能恢复出非零起点。
    data_start: u64,
    /// 数据区已写入字节数（头部 `data_len` 字段）。恒等于 `state.end - data_start`。
    data_len: u64,
    /// 尾部内存窗口：最近写入的 `PRELOAD_TAIL_WINDOW_BYTES` 字节，`tail[0]`
    /// 相对窗口起点的偏移是 `tail_offset`。
    ///
    /// 存在的理由不是"再留一份内存"，而是压掉 demux 前沿的小块读放大——桶
    /// 前沿恰恰是 demux 最常命中的位置（追赶缓冲、seek 到桶尾都落在那里），
    /// 一次 32KB 读若撞上 page cache 未命中就是一次真实磁盘 IO，落在播放线程
    /// 上就是几百微秒到毫秒级抖动。上限 1MiB，相对 512MB 的桶可忽略，不会
    /// 退化成"又一份整片内存"。
    tail: Vec<u8>,
    tail_offset: u64,
}

/// 桶文件句柄 + 生命周期收尾。
///
/// 单独包一层只为 `Drop` 的顺序：Windows 上文件还开着时 `remove_file` 会以
/// `Access is denied` 失败，必须先显式关掉句柄再删（Linux/macOS 无此约束，
/// 但统一走一条路径更省心）。删除失败不上报——残留文件由启动清理
/// （`sweep_stale_preload`）兜底，不在这里制造噪音。
struct PreloadFile {
    file: Option<File>,
    path: PathBuf,
}

impl PreloadFile {
    /// 打开一个**已存在**的桶文件，并取得独占锁。
    ///
    /// 锁必须在构造守卫**之前**就拿到：守卫的 `Drop` 会删文件，若先构造再发现
    /// 锁在别人手里，就等于替对方删掉了他正在写的桶。拿不到锁时局部 `File`
    /// 被丢弃（只关句柄），文件原封不动。
    fn open_existing(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock()
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self {
            file: Some(file),
            path: path.to_path_buf(),
        })
    }

    /// 新建一个**独占**的桶文件（`create_new`：同名的另一个会话已经在写时
    /// 直接失败，绝不共用）。
    fn create_new(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        // 刚建出来的文件不存在竞争；锁失败只可能是极端环境问题，按失败处理。
        file.try_lock()
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self {
            file: Some(file),
            path: path.to_path_buf(),
        })
    }

    /// 关句柄并删文件（启动清理用；`Drop` 也做同一件事，但那里的失败无法上报）。
    fn discard(mut self) -> io::Result<()> {
        self.file.take();
        std::fs::remove_file(&self.path)
    }

    fn file(&self) -> &File {
        self.file.as_ref().expect("preload file already closed")
    }

    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("preload file already closed")
    }
}

impl Drop for PreloadFile {
    fn drop(&mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 桶文件头部长度（见 `DiskStore` 布局注释）。
const DISK_HEADER_BYTES: u64 = 32;
/// 头部魔数：判定"这个 `.part` 是不是本模块写的"。
const DISK_MAGIC: [u8; 8] = *b"ERIKAPRE";
/// 布局版本：改布局必须 +1（版本不符的文件直接重建，绝不复用）。
const DISK_LAYOUT_VERSION: u32 = 1;
/// 尾部内存窗口上限（见 `DiskStore::tail`）。
const PRELOAD_TAIL_WINDOW_BYTES: usize = 1024 * 1024;
/// 陈旧桶文件的保留时长（`ERIKA_PRELOAD_RETENTION_DAYS` 覆盖，默认 3 天）。
const PRELOAD_RETENTION_DEFAULT: Duration = Duration::from_secs(3 * 24 * 60 * 60);
/// 桶目录总量上限：超出时按最久未动优先淘汰，直到不再超限。防的是"反复播放
/// 把系统盘塞满"——单文件上限就是一个 `full_preload_cap()`（默认 512MB），
/// 几十次播放就是几十 GB。
const PRELOAD_DIR_LIMIT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

impl DiskStore {
    /// 当前窗口长度（= `data_len`）。
    fn len(&self) -> u64 {
        self.data_len
    }

    /// 打开（或复用）`dir` 下属于 `key` 的桶文件。
    fn open(dir: &Path, key: u64, cap: u64) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        // 首选名：可能是上一次异常退出留下的残片（`Drop` 没跑到）。能拿到独占
        // 锁就说明没有存活的桶在用它，于是允许**复用**：把已落盘区间当作窗口
        // [data_start, data_start + data_len)，弱网下重开同一集不必重下。
        let primary = dir.join(format!("{key:016x}.part"));
        if let Ok(store) = Self::open_file(&primary, cap) {
            return Ok(store);
        }
        // 拿不到锁 / 建不出来：换一个进程内唯一的名字新建。"同一条流重开"只要求
        // **不冲突**——复用是加分项，两个会话共写一个文件是必须避免的事故。
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        for _ in 0..8 {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!(
                "{key:016x}-{}-{sequence}.part",
                std::process::id()
            ));
            if let Ok(store) = Self::open_file(&path, cap) {
                return Ok(store);
            }
        }
        Err(io::Error::other("preload: no usable bucket file name"))
    }

    fn open_file(path: &Path, cap: u64) -> io::Result<Self> {
        // 只在**确实不存在**时才新建。其余失败（文件被别的会话锁着 / 权限 /
        // 只读）一律原样返回、换下一个候选名——那条路径上的文件可能正被别人
        // 写着，任何"顺手重建"都是破坏。
        let mut file = match PreloadFile::open_existing(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                PreloadFile::create_new(path)?
            }
            Err(error) => return Err(error),
        };
        let (data_start, data_len) = Self::recover(&mut file, cap)?;
        let mut store = Self {
            file,
            data_start,
            data_len,
            tail: Vec::new(),
            tail_offset: 0,
        };
        store.write_header()?;
        Ok(store)
    }

    /// 校验头部、恢复可复用窗口，返回 `(data_start, data_len)`。
    ///
    /// 所有不确定都往"少复用"一侧倒：复用错字节 = 静默播放损坏数据，比少复用
    /// 严重得多。所以头部声明长度超过文件实际长度（写完数据还没更新头部就被
    /// 杀）时取下界，版本不符则整个丢掉重建。
    fn recover(file: &mut PreloadFile, cap: u64) -> io::Result<(u64, u64)> {
        let physical = file.file().metadata()?.len();
        let mut header = [0u8; DISK_HEADER_BYTES as usize];
        let valid = physical >= DISK_HEADER_BYTES
            && file.file_mut().seek(SeekFrom::Start(0)).is_ok()
            && file.file_mut().read_exact(&mut header).is_ok()
            && header[0..8] == DISK_MAGIC
            && u32::from_le_bytes(header[8..12].try_into().unwrap()) == DISK_LAYOUT_VERSION;
        if !valid {
            file.file_mut().set_len(DISK_HEADER_BYTES)?;
            return Ok((0, 0));
        }
        let data_start = u64::from_le_bytes(header[16..24].try_into().unwrap());
        let declared = u64::from_le_bytes(header[24..32].try_into().unwrap());
        // 只认"文件里真的躺着这么多字节"的部分，再按本次桶上限截断——cap 比
        // 上次小（宿主换了配置）时不能沿用更大的旧窗口。
        let len = declared
            .min(physical - DISK_HEADER_BYTES)
            .min(cap.saturating_sub(data_start));
        file.file_mut().set_len(DISK_HEADER_BYTES + len)?;
        Ok((data_start, len))
    }

    /// 写回头部。每次追加后都写一次：崩溃时最多丢一个段（256KB）的可复用区间，
    /// 换来复用判定永远不用猜"文件里这堆字节到底算不算数"。
    fn write_header(&mut self) -> io::Result<()> {
        let mut header = [0u8; DISK_HEADER_BYTES as usize];
        header[0..8].copy_from_slice(&DISK_MAGIC);
        header[8..12].copy_from_slice(&DISK_LAYOUT_VERSION.to_le_bytes());
        header[16..24].copy_from_slice(&self.data_start.to_le_bytes());
        header[24..32].copy_from_slice(&self.data_len.to_le_bytes());
        let file = self.file.file_mut();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)
    }

    /// 顺序追加一段与窗口尾部连续的字节，并同步尾部窗口。
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        let position = DISK_HEADER_BYTES + self.data_len;
        {
            let file = self.file.file_mut();
            file.seek(SeekFrom::Start(position))?;
            // 故意不 fsync：整片预载是"看完就丢"的缓存（源 Drop 时文件即删），
            // 落盘与否不影响正确性，而 fsync 会把 SSD 写放大的代价搬进播放线程
            // 的写路径。写进 page cache 已经足够，回刷交给 OS。
            file.write_all(bytes)?;
        }
        self.data_len += bytes.len() as u64;
        self.push_tail(bytes);
        self.write_header()
    }

    /// 把新写入的字节并进尾部窗口（超出上限就从最旧的一端丢）。
    fn push_tail(&mut self, bytes: &[u8]) {
        if self.tail.is_empty() {
            self.tail_offset = self.data_len.saturating_sub(bytes.len() as u64);
        }
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > PRELOAD_TAIL_WINDOW_BYTES {
            let excess = self.tail.len() - PRELOAD_TAIL_WINDOW_BYTES;
            self.tail.drain(..excess);
            self.tail_offset += excess as u64;
        }
    }

    /// 读窗口内 `[offset, offset + length)`（`offset` 相对窗口起点）。
    fn read(&mut self, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        if let Some(hit) = self.tail_read(offset, length) {
            return Ok(hit);
        }
        if offset.saturating_add(length as u64) > self.data_len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "preload read beyond bucket end",
            ));
        }
        let mut out = vec![0u8; length];
        let file = self.file.file_mut();
        file.seek(SeekFrom::Start(DISK_HEADER_BYTES + offset))?;
        file.read_exact(&mut out)?;
        Ok(out)
    }

    /// 尾部窗口命中判定（纯内存，不走文件）。
    fn tail_read(&self, offset: u64, length: usize) -> Option<Vec<u8>> {
        let start = usize::try_from(offset.checked_sub(self.tail_offset)?).ok()?;
        let end = start.checked_add(length)?;
        if end > self.tail.len() {
            return None;
        }
        Some(self.tail[start..end].to_vec())
    }

    /// 窗口整体前移到 `new_start`：逻辑窗口清空 + 数据区截断（见布局注释）。
    fn reset_at(&mut self, new_start: u64) -> io::Result<()> {
        self.file.file_mut().set_len(DISK_HEADER_BYTES)?;
        self.data_start = new_start;
        self.data_len = 0;
        self.tail.clear();
        self.tail_offset = 0;
        self.write_header()
    }

    /// 就地降级到内存：把已落盘窗口读回内存。`None` = 连读都失败（文件被外部
    /// 删掉 / 句柄失效），调用方只能把窗口缩到当前位置。
    fn into_memory(mut self) -> Option<Vec<u8>> {
        let length = usize::try_from(self.data_len).ok()?;
        let mut recovered = vec![0u8; length];
        let file = self.file.file_mut();
        let ok = file.seek(SeekFrom::Start(DISK_HEADER_BYTES)).is_ok()
            && (length == 0 || file.read_exact(&mut recovered).is_ok());
        ok.then_some(recovered)
    }
}

impl PreloadStore {
    /// 当前窗口长度（= `state.end - inner.start`）。
    fn len(&self) -> u64 {
        match self {
            Self::Memory(bytes) => bytes.len() as u64,
            Self::Disk(disk) => disk.len(),
        }
    }

    /// 后端自带的窗口起点（逻辑文件偏移）。只有落盘后端知道自己的起点——它
    /// 记录在文件头部，跨进程复用残留文件时可能不是 0；内存桶的起点恒由
    /// `FullPreloadInner::start` 决定，这里返回 `None`。
    ///
    /// 存在理由：复用一份"前移过"的残留文件时，`inner.start` 必须跟着文件头
    /// 里的 `data_start` 走。两边一旦不一致，`copy_range` 算出来的窗口内偏移
    /// 就会和 `DiskStore::read` 期待的偏移错开一个 `data_start`——读出来的是
    /// **错位字节**，正是这条桶最不能出的错。
    fn window_start(&self) -> Option<u64> {
        match self {
            Self::Memory(_) => None,
            Self::Disk(disk) => Some(disk.data_start),
        }
    }

    /// 顺序追加一段与窗口尾部连续的字节。
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Memory(buffer) => {
                buffer.extend_from_slice(bytes);
                Ok(())
            }
            Self::Disk(disk) => disk.append(bytes),
        }
    }

    /// 读窗口内 `[offset, offset + length)`，`offset` 相对**窗口起点**。
    ///
    /// 越界或 IO 失败一律 `Err`，调用方回退到 demux 自己的取数路径
    /// （`copy_range` 返回 None）——**绝不能返回错位字节**。
    fn read(&mut self, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        match self {
            Self::Memory(buffer) => {
                let start = usize::try_from(offset).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "preload offset overflow")
                })?;
                let end = start
                    .checked_add(length)
                    .filter(|end| *end <= buffer.len())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "preload read beyond bucket end",
                        )
                    })?;
                Ok(buffer[start..end].to_vec())
            }
            Self::Disk(disk) => disk.read(offset, length),
        }
    }

    /// 窗口整体前移到 `new_start`（`[start, new_start)` 的字节作废）。
    fn reset_at(&mut self, new_start: u64) -> io::Result<()> {
        match self {
            Self::Memory(buffer) => {
                buffer.clear();
                Ok(())
            }
            Self::Disk(disk) => disk.reset_at(new_start),
        }
    }
}

/// 桶文件目录。
///
/// Windows 默认 `%LOCALAPPDATA%\bili-rs\preload`：与宿主的应用数据目录约定
/// 对齐（`storage::data_dir` / `image_cache` 同款，LOCALAPPDATA 不可用时同样
/// 退到 `%TEMP%\bili-rs`）。不选 `%TEMP%` 作默认是因为它会被"磁盘清理"随手
/// 删掉——被删掉的后果是整片预载白下一遍，正是这条桶要对抗的场景。
/// `ERIKA_PRELOAD_DIR` 可整体改道（用户想把缓存放到别的盘时用）。
fn preload_dir() -> PathBuf {
    if let Some(dir) = env::var_os("ERIKA_PRELOAD_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "windows") {
        let base = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(env::temp_dir);
        return base.join("bili-rs").join("preload");
    }
    if let Some(base) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(base).join("erika").join("preload");
    }
    env::temp_dir().join("erika-preload")
}

/// 桶文件名摘要：源 URI 的**稳定部分** + 流槽位。
///
/// 不能直接哈希整条 URI：B 站 CDN URL 带 `deadline`/`upsig` 等一次性签名，
/// 每次播放都变，整条哈希等于每次都换新名字，"同一条流重开"永远命中不到同一个
/// 文件。URI 里真正标识内容的是 path（含 cid 与码率 id，形如
/// `/<...>/<cid>-1-30280.m4s`），去掉 query/fragment 后跨会话稳定。
///
/// 用自写的 FNV-1a 而不是 `DefaultHasher`：后者只承诺"同一进程内稳定"，
/// 而文件名要跨进程（乃至跨 std 版本）复用。
fn preload_key(uri: &str, slot: usize) -> u64 {
    let stable = uri.split(['?', '#']).next().unwrap_or(uri);
    let slot_bytes = slot.to_le_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in stable.as_bytes().iter().chain(slot_bytes.iter()) {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 是否把整片预载桶落到磁盘。默认**开**，`ERIKA_PRELOAD_TO_DISK=0` 关闭
/// （退回全内存桶，即落盘改造前的行为）。
fn preload_to_disk_enabled() -> bool {
    // 与 `trace::env_flag` 同一套字面量，但默认相反（这条开关默认开）。
    !matches!(
        env::var("ERIKA_PRELOAD_TO_DISK").ok().as_deref(),
        Some("0" | "false" | "FALSE" | "off" | "OFF" | "")
    )
}

/// 陈旧桶文件的保留时长（`ERIKA_PRELOAD_RETENTION_DAYS` 覆盖，默认 3 天）。
fn preload_retention() -> Duration {
    env::var("ERIKA_PRELOAD_RETENTION_DAYS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|days| Duration::from_secs(days.saturating_mul(24 * 60 * 60)))
        .unwrap_or(PRELOAD_RETENTION_DEFAULT)
}

/// 启动清理：删掉桶目录下**陈旧**的文件。
///
/// 必要性：(1) 进程被强杀（任务管理器结束进程 / 崩溃）时桶文件会留在盘上，
/// `Drop` 跑不到，不清理就是几十~几百 MB 的永久垃圾；(2) 反复重开同一集会
/// 因为"不冲突"策略而生成带序号的新文件。判定用 mtime：正在用的桶每写一段
/// 就刷新一次 mtime，"很久没动过"只可能是死文件。
///
/// 两条淘汰路径：超龄（默认 3 天）必删；目录总量超过 `PRELOAD_DIR_LIMIT_BYTES`
/// 时按最久未动优先删，删到不再超限为止（不做"批量清空"——多留一份没超限的
/// 桶，就可能多省一次重下）。
///
/// 全程 best-effort：清理是卫生工作，任何一步失败都不影响播放。
fn sweep_stale_preload(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    let mut candidates: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        // 只碰自己的 `.part`：目录可能被 `ERIKA_PRELOAD_DIR` 指到用户自己的
        // 目录上，绝不能顺手删别人的文件。
        if path.extension().and_then(|value| value.to_str()) != Some("part") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(now);
        total = total.saturating_add(metadata.len());
        candidates.push((path, modified, metadata.len()));
    }
    if candidates.is_empty() {
        return;
    }
    // 旧 → 新：超限时先牺牲最久未动的。
    candidates.sort_by_key(|(_, modified, _)| *modified);
    let retention = preload_retention();
    let mut remaining = total;
    let mut removed = 0usize;
    let mut freed = 0u64;
    for (path, modified, size) in &candidates {
        let expired = now.duration_since(*modified).unwrap_or_default() > retention;
        if !expired && remaining <= PRELOAD_DIR_LIMIT_BYTES {
            break; // 后面的文件都更新，也不会再超限
        }
        // 活跃文件绝不删：拿不到独占锁说明另一个播放会话正用着它（超龄判定
        // 已经足够保守，这是第二道闸）。`open_existing` 自带这个判定。
        let Ok(file) = PreloadFile::open_existing(path) else {
            continue;
        };
        if file.discard().is_ok() {
            removed += 1;
            freed = freed.saturating_add(*size);
            remaining = remaining.saturating_sub(*size);
        }
    }
    if removed > 0 {
        trace::diagnostic(format!(
            "{{\"event\":\"erika_preload_sweep\",\"dir\":\"{}\",\"removed\":{removed},\"freed\":{freed}}}",
            json_escape(&dir.to_string_lossy()),
        ));
    }
}

/// 启动清理只跑一次：本进程第一次要用磁盘桶时。放在这里而不是某个 `main`——
/// Erika 是被宿主嵌入的库，没有自己的启动点，而"第一次要用磁盘桶"就是磁盘
/// 路径真正的启动时刻。
static PRELOAD_SWEEP: LazyLock<()> = LazyLock::new(|| sweep_stale_preload(&preload_dir()));

struct FullPreloadInner {
    /// 文件总长（由首个成功响应的 total 回填；None = 未探测到，线程等待）。
    total: Option<u64>,
    /// 桶存储区间的**绝对起点**，初始 0。demux 预取跑赢桶时整体前移到
    /// `floor` 并丢弃旧字节——那些字节 demux cache 里已有一份，桶里再留
    /// 就是重复下载（纯浪费带宽）。前移后桶表示 [start, end)。
    start: u64,
    /// 存储桶：文件 [start, end) 的连续字节。落盘改造前这里是 `Vec<u8>`，
    /// 现在默认是文件；`PreloadStore::Memory` 只在关闭落盘或落盘失败时启用。
    store: PreloadStore,
}

impl FullPreloadInner {
    /// 把一段与窗口尾部连续的字节追加进桶。
    ///
    /// 落盘写失败时就地**降级到内存桶**并把这批字节补进内存——磁盘满 / 权限 /
    /// 杀软锁都不该让播放失败。返回 `Err` 表示刚刚发生过降级（只可能发生一次），
    /// 调用方负责把这件事报出去。
    fn append_chunk(&mut self, bytes: &[u8]) -> std::result::Result<(), String> {
        let error = match self.store.append(bytes) {
            Ok(()) => return Ok(()),
            Err(error) => error.to_string(),
        };
        // 降级尽量完整：已落盘区间读回内存，窗口原样保留，读路径完全无感；
        // 读不回来（文件被外部删掉）就把窗口缩到当前位置重新起步——`start = end`
        // 时没有可服务区间，但桶继续往内存里长，后续字节照样能命中。
        let covered_end = self.start + self.store.len();
        let previous = std::mem::replace(&mut self.store, PreloadStore::Memory(Vec::new()));
        self.store = match previous {
            PreloadStore::Disk(disk) => match disk.into_memory() {
                Some(recovered) => PreloadStore::Memory(recovered),
                None => {
                    self.start = covered_end;
                    PreloadStore::Memory(Vec::new())
                }
            },
            memory => memory,
        };
        let _ = self.store.append(bytes);
        Err(error)
    }

    /// 桶整体前移到 `new_start`：逻辑窗口清空、起点换到 `new_start`，落盘后端
    /// 同时截断数据区。失败不致命——调用方换成空的内存桶即可（前移本来就是要
    /// 丢掉全部旧字节，这条路径没有任何额外数据损失）。
    fn reset_at(&mut self, new_start: u64) -> io::Result<()> {
        self.start = new_start;
        self.store.reset_at(new_start)
    }

    /// 前移失败时的兜底：换成空的内存桶（见 `reset_at` 注释）。
    fn fallback_to_memory_at(&mut self, new_start: u64) {
        self.start = new_start;
        self.store = PreloadStore::Memory(Vec::new());
    }
}

/// 整片预载单步：从当前 `end` 下载一段（≤256KB）追加进桶。返回 true 表示
/// 已完成（总量已知且到顶/达上限）或被停止；false 表示还有后续（调用方循
/// 环）。失败时不前进、由调用方退避重试——瞬时断流在这里被消化，不升级。
fn full_preload_step(
    state: &FullPreloadState,
    agent: &ureq::Agent,
    uri: &str,
    http_headers: &[(String, String)],
    cap: u64,
) -> bool {
    if state.stop.load(Ordering::Relaxed) {
        return true;
    }
    let floor = state.floor.load(Ordering::Relaxed);
    let (start, end, total) = {
        let g = state.inner.lock().expect("full preload poisoned");
        (g.start, state.end.load(Ordering::Relaxed), g.total)
    };
    let Some(total) = total else {
        return false; // 总长未探测到：等待（调用方退避后重试）
    };
    // demux 并行预取已跑赢桶：整体前移到 floor 并丢弃旧字节，下一步从 floor
    // 起下载。旧字节 demux cache 里已持有一份，桶再从 0 追就是与 demux 重复
    // 下载同一区间——实测这会把有效供给砍半（23KB/s 中一半是重复字节），缓冲
    // 永远建不起来 → 一路饥渴换源。前移 = 放弃追赶、只补 demux 前方，是带宽
    // 受限下的正确取舍（暂停后 demux 停止推进，桶随即接管并持续补到文件尾）。
    if floor > end {
        let mut g = state.inner.lock().expect("full preload poisoned");
        // 前移要改写物理布局（落盘后端会截断数据区）。重置失败也不致命：旧
        // 字节本来就要丢，换成空的内存桶即可，不把磁盘问题升级成播放失败。
        if let Err(error) = g.reset_at(floor) {
            trace::diagnostic(format!(
                "{{\"event\":\"erika_preload_disk_reset_failed\",\"slot\":{},\"start\":{},\"error\":\"{}\"}}",
                state.slot,
                floor,
                json_escape(&error.to_string()),
            ));
            g.fallback_to_memory_at(floor);
        }
        drop(g);
        state.end.store(floor, Ordering::Relaxed);
        return false; // 无网络动作，下一步直接从 floor 下载
    }
    if end >= total || end >= cap {
        return true;
    }
    let want = (total.min(cap) - end).min(256 * 1024);
    if want == 0 {
        return true;
    }
    let range = ByteRange {
        start: end,
        length: Some(want),
    };
    // 把桶的计数器交给分块读循环实时累加（整片预载是暂停期间唯一的数据通路，
    // 没有实时进度时"暂停后缓冲"在诊断里表现为速度 0）。收尾不再单独
    // `fetch_add`——那会重复计数（分块循环已按实际到达字节累加过一遍）。
    match fetch_http_range(
        agent,
        uri,
        http_headers,
        range,
        "http_full_preload",
        Some(state.downloaded.as_ref()),
    ) {
        Ok(response) => {
            let n = response.bytes.len() as u64;
            if n == 0 {
                return true; // 服务器判定到尾（短于 Content-Range 声明）
            }
            let mut g = state.inner.lock().expect("full preload poisoned");
            if g.total.is_none() {
                g.total = response.total_length;
            }
            // 顺序完整性守卫：写入偏移与桶起点都必须与快照一致——其间若发生
            // 前移（start 变化）或并发写入，丢弃本次并退避重试，绝不产生空洞
            // 或把新字节追加到已前移的桶上造成错位。
            if state.end.load(Ordering::Relaxed) != end || g.start != start {
                return false;
            }
            // 落盘写失败（磁盘满/权限/杀软锁）已就地降级到内存桶：慢一拍，
            // 但内存桶照样把这段字节收下，播放不受影响。
            let degraded = g.append_chunk(&response.bytes).err();
            drop(g);
            if let Some(reason) = degraded {
                // diag 是无条件通道（不受 trace 开关影响）：宿主必须知道这条流
                // 已经退回"整片常驻内存"了——这正是本次改造要消灭的形态。
                trace::diagnostic(format!(
                    "{{\"event\":\"erika_preload_disk_fallback\",\"slot\":{},\"start\":{},\"length\":{},\"error\":\"{}\"}}",
                    state.slot,
                    start,
                    n,
                    json_escape(&reason),
                ));
            }
            state.end.fetch_add(n, Ordering::Relaxed);
            // 注意：`state.downloaded` 已由 `fetch_http_range` 的分块读循环实时
            // 累加（含本段被完整性守卫丢弃的情形——字节确实到过线路上，计入
            // 带宽估算是正确的，丢弃只影响桶内容不影响吞吐判定）。
            false
        }
        Err(_) => false, // 调用方退避后重试（同一段续传）
    }
}

/// 后台整片预载线程循环。独立 `OS` 线程，与 demux/runtime/UI 都无关联——
/// `core::time` 自然带，`AtomicBool::load(Relaxed)` 唤醒，单调追加直到
/// `step` 返回 true（end ≥ total 或 ≥ cap 或被 stop）。失败退避 200ms 后重试
/// 同一段续传（HTTP range 服务器天然支持），不会破坏桶的连续性。退出条件：
/// 1) stop 信号；2) 完成（end ≥ total/cap）；3) 致命错误（连续失败默认让循环
/// 继续；Drop 时由 `HttpRangeSource::drop` 兜底终止）。
fn full_preload_loop(
    agent: &ureq::Agent,
    uri: Arc<Mutex<String>>,
    headers: &[(String, String)],
    state: &FullPreloadState,
    cap: u64,
    // 激进模式（仅音频优先桶）：不向 demux 让路，必须抢在 demux 之前把整条
    // 音轨拉完，从源头杜绝「音频饿死」。
    eager: bool,
    // 是否把预载进度上报到音频优先协调器（仅音频桶）。
    report_audio_first: bool,
    // 是否为视频桶：音频优先激活期间让路，把带宽让给音频。
    video_bucket: bool,
) {
    const RETRY_BACKOFF: Duration = Duration::from_millis(200);
    let mut consecutive_failures: u32 = 0;
    loop {
        if state.stop.load(Ordering::Relaxed) {
            break;
        }
        // 音频优先：视频桶在音频未达标前让路，带宽完整留给音频整片预载。
        if video_bucket && audio_first_throttle_video() {
            std::thread::sleep(PRELOAD_YIELD_SLEEP);
            continue;
        }
        // demux 正在消费（播放中）→ 让路：本轮不下载，把带宽完整留给 demux 的
        // 多段并行预取。两路同时下载同一区间会把有效供给砍半（实测 23KB/s 里
        // 一半是重复字节），而多段并行正是聚合被 CDN 限速的单连接带宽的手段
        // ——桶只有单段顺序，既抢不过也不该抢。暂停/缓冲充足后 demux 停止
        // read，桶自动接管并持续补到文件尾（用户定规：打开就应一直缓冲）。
        // 激进音频桶（eager）除外：音频是优先下载目标，必须抢在 demux 之前拉完。
        if !eager && state.demux_active() {
            std::thread::sleep(PRELOAD_YIELD_SLEEP);
            continue;
        }
        let end_before = state.covered_until();
        // 每步取最新 host（换 host 时自动跟随，字节偏移缓存不变 → 无缝续下）。
        let uri_str = uri.lock().expect("uri poisoned").clone();
        let done = full_preload_step(state, agent, &uri_str, headers, cap);
        // 音频桶把预载进度上报到协调器，供视频源判定相位。
        if report_audio_first {
            if let Some(total) = state.len() {
                AUDIO_FIRST.audio_total_known.store(true, Ordering::Relaxed);
                let covered = state.covered_until();
                let permille = covered.saturating_mul(1000) / total.max(1);
                AUDIO_FIRST
                    .audio_permille
                    .store(permille.min(1000), Ordering::Relaxed);
            }
        }
        if done {
            break;
        }
        // step 未结束：若 end 没推进 → 失败退避；否则立即下一段。
        // 注意：与 demux 并行的良性 race 下，可能把"成功刚推进一毫米"读
        // 成相等，代价只是偶尔多睡 200ms——不会反向，所以对桶正确性安全。
        if state.covered_until() == end_before {
            consecutive_failures = consecutive_failures.saturating_add(1).min(60);
            let sleep = RETRY_BACKOFF * consecutive_failures.min(5);
            std::thread::sleep(sleep);
        } else {
            consecutive_failures = 0;
        }
    }
}

fn full_preload_cap() -> u64 {
    std::env::var("ERIKA_HTTP_FULLPRELOAD_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(512 * 1024 * 1024)
}

/// demux 活跃判定窗口：最近该时长内有 read 即视为"正在播放"，桶让路。
/// 500ms 覆盖播放中的常规读间隔与缓冲充足时的短暂空档，又足够短——暂停后
/// 约 0.5s 桶即接管（用户感知不到）。
const DEMUX_ACTIVE_WINDOW_MS: u64 = 500;

/// 让路时的休眠步长，同时决定 stop 信号的响应粒度（关窗时最多多等 100ms）。
const PRELOAD_YIELD_SLEEP: Duration = Duration::from_millis(100);

/// 当前墙上时钟（epoch 毫秒）。仅用于"最近是否有 demux 读"这类粗粒度判定，
/// 不需要单调性——系统时钟回拨最坏导致一次误判让路，无正确性影响。
fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl FullPreloadState {
    /// 构造一份新的**全内存**桶预载状态（桶空、总长未知）。测试与
    /// `ERIKA_PRELOAD_TO_DISK=0` 的对照路径用它；生产路径走 `open`（默认落盘）。
    #[cfg(test)]
    fn new() -> Arc<Self> {
        Self::build(
            0,
            FullPreloadInner {
                total: None,
                start: 0,
                store: PreloadStore::Memory(Vec::new()),
            },
        )
    }

    /// 构造预载状态：默认把整片预载桶落到磁盘（`ERIKA_PRELOAD_TO_DISK=0` 关闭）。
    ///
    /// 落盘的任何失败（目录建不出来 / 权限 / 磁盘满）都只是**退回内存桶**并
    /// 上报一条 diag——整片预载本身照常工作，宿主行为与落盘改造前完全一致。
    /// 落盘是省内存的优化，不该有能力把播放变成"开不了"。
    ///
    /// `dir` 由调用方传入（而不是内部读 `preload_dir()`）：既让生产路径只有
    /// 一处目录来源，也让测试能指到自己的临时目录、走完整的"复用残留文件"
    /// 链路——这条链路决定了桶里每个字节的**逻辑偏移**，必须端到端测。
    fn open(uri: &str, slot: usize, cap: u64, dir: &Path) -> Arc<Self> {
        let store = if preload_to_disk_enabled() {
            // 启动清理随第一次落盘建桶一起跑（只跑一次）。
            LazyLock::force(&PRELOAD_SWEEP);
            match DiskStore::open(dir, preload_key(uri, slot), cap) {
                Ok(disk) => PreloadStore::Disk(disk),
                Err(error) => {
                    trace::diagnostic(format!(
                        "{{\"event\":\"erika_preload_disk_unavailable\",\"slot\":{slot},\"dir\":\"{}\",\"error\":\"{}\"}}",
                        json_escape(&dir.to_string_lossy()),
                        json_escape(&error.to_string()),
                    ));
                    PreloadStore::Memory(Vec::new())
                }
            }
        } else {
            PreloadStore::Memory(Vec::new())
        };
        Self::build(
            slot,
            FullPreloadInner {
                total: None,
                // 复用残留文件时窗口起点可能非 0（那份文件上次前移过），必须
                // 跟着文件头走，见 `PreloadStore::window_start`。
                start: store.window_start().unwrap_or(0),
                store,
            },
        )
    }

    fn build(slot: usize, inner: FullPreloadInner) -> Arc<Self> {
        // 复用残留文件时窗口可能非空：`end` 必须从窗口终点起步，否则
        // `covered_until` 会谎报 0，桶会把已经躺在盘上的字节重下一遍。
        let covered = inner.start + inner.store.len();
        Arc::new(Self {
            slot,
            stop: AtomicBool::new(false),
            end: AtomicU64::new(covered),
            downloaded: Arc::new(AtomicU64::new(0)),
            floor: AtomicU64::new(0),
            last_demux_read_ms: AtomicU64::new(0),
            inner: Mutex::new(inner),
        })
    }

    /// 全文件总长（首个成功响应回填）。None 表示尚未探测完成。
    fn len(&self) -> Option<u64> {
        self.inner.lock().ok().and_then(|g| g.total)
    }

    /// 已覆盖到的最大文件偏移。0 表示尚未下载任何字节。
    fn covered_until(&self) -> u64 {
        self.end.load(Ordering::Acquire)
    }

    /// 桶是否完整覆盖整片文件（end ≥ total 且 total 已知）。
    fn is_complete(&self) -> bool {
        match self.len() {
            Some(total) => self.covered_until() >= total,
            None => false,
        }
    }

    /// 桶当前是否正有一段下载在飞。整片预载线程单段顺序下载，故 1/0。
    /// 诊断面板「下载速度 > 0」「缓冲前进」直接由该字段反映出来——不再依赖
    /// demux 主动 read 触发的 prefetch（暂停了就归 0）。
    ///
    /// **demux 活跃（播放中）时返回 0**（2026-09-02 K4）：播放期桶在让路、
    /// 实际没有下载在飞，此前恒返回 1 导致宿主「prefetch_in_flight == 0」
    /// 判据永不成立（源空闲检测退化为单一字节停滞条件）。
    fn in_flight_segments(&self) -> u32 {
        if self.is_complete() || self.demux_active() {
            0
        } else if self.stop.load(Ordering::Relaxed) {
            0
        } else {
            1
        }
    }

    /// 请求线程退出。Drop 时由 HttpRangeSource 调用。Relaxed 序即可——线程在
    /// 每次 `full_preload_step` 开头都会检查，且 0/1 二值不需要与状态量同步。
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// 记录 demux 读取时刻（由 `read_range` 每次调用）。
    fn mark_demux_read(&self) {
        self.last_demux_read_ms
            .store(now_epoch_ms(), Ordering::Relaxed);
    }

    /// demux 是否在最近 `DEMUX_ACTIVE_WINDOW_MS` 内读过数据。活跃 → 桶让路：
    /// 播放期的带宽必须完整留给 demux 的多段并行预取。两路同下同一区间会把
    /// 有效供给砍半（实测 23KB/s 里一半是重复字节），而多段并行是聚合被 CDN
    /// 限速的单连接带宽的关键——桶单段抢不过。暂停/缓冲充足后 demux 停止
    /// read，桶随即接管。
    fn demux_active(&self) -> bool {
        let last = self.last_demux_read_ms.load(Ordering::Relaxed);
        if last == 0 {
            // 尚无任何 read（open 探测阶段）：桶可先跑，demux 后续读直接命中桶。
            return false;
        }
        now_epoch_ms().saturating_sub(last) < DEMUX_ACTIVE_WINDOW_MS
    }

    /// 同步 demux 端已覆盖前沿（`cache_end`）。桶据此跳过该偏移之前的区间，
    /// 避免与 demux 并行预取重复下载同一段。单调取 max——demux 回退读（seek
    /// 后窗口收缩）不应让 floor 倒退，否则桶会回头重下已覆盖区间。
    fn set_floor(&self, floor: u64) {
        self.floor.fetch_max(floor, Ordering::Relaxed);
    }

    /// 桶是否完整覆盖给定 [offset, offset+length) 范围。满足时调用方复制返回，
    /// 走零网络直返路径。桶表示 [start, end)：起点会随 demux 前沿前移，故必须
    /// 同时判定下界——只判 `end >= offset+length` 会在桶已前移时把低地址请求
    /// 误判为覆盖（那些字节已随前移丢弃），`copy_range` 会 copy 出错位数据。
    /// 未覆盖的走原 demux 链路（互不干扰）。
    fn covers(&self, offset: u64, length: u64) -> bool {
        // 一次加锁同时取 total 与 start：covers 在 read 热路径上每次都调，
        // 两次 lock 无谓（无竞争但仍是两次原子操作）。
        let (total, start) = match self.inner.lock() {
            Ok(g) => (g.total, g.start),
            Err(_) => return false,
        };
        let Some(total) = total else {
            return false;
        };
        if offset >= total {
            return false;
        }
        // 范围终点 = offset + length (u64 wrap 保护)
        let end = match offset.checked_add(length) {
            Some(e) => e,
            None => return false,
        };
        offset >= start && self.covered_until() >= end
    }

    /// 复制 [offset, offset+length) 字节出桶。调用方必须先 `covers()` 返回 true。
    /// 绝对偏移需减去桶起点 `start` 才是桶内下标。
    fn copy_range(&self, offset: u64, length: u64) -> Option<Vec<u8>> {
        // 读全程持锁（**含磁盘读**）：前移会重置落盘后端的物理布局，锁外读会
        // 撞上被截断/被重写的区间而读到错位字节——静默数据损坏比"这次回退到
        // demux 自己的路径"严重得多。锁内代价可忽略：命中尾部窗口是纯内存拷贝，
        // 其余也基本落在 page cache 上。
        let mut g = self.inner.lock().ok()?;
        if g.total.is_none() {
            return None;
        }
        let end = offset.checked_add(length)?;
        // 绝对 → 桶内相对偏移（checked_sub 覆盖 offset < start 的情况）
        let rel_start = offset.checked_sub(g.start)?;
        let rel_end = end.checked_sub(g.start)?;
        if rel_end > g.store.len() {
            return None;
        }
        // 读失败（越界 / IO 错）一律 None：调用方会回退到 demux 自己的缓存与
        // 网络路径，绝不把半截或错位的字节交出去。
        g.store.read(rel_start, (rel_end - rel_start) as usize).ok()
    }
}

/// 多源无缝换 host（Option A）参数。
/// 死连判定：超过 `MULTI_HOST_STALL_MS` 没收到字节、且 demux 仍在调用
/// read_range（播放活跃）→ 源已死，切到下一个候选。
const MULTI_HOST_STALL_MS: u64 = 6_000;
/// 活跃窗口：read_range 最近被调用的时间须落在此窗口内，才判定"在播"
/// （暂停时不误换）。
const MULTI_HOST_ACTIVE_MS: u64 = 6_000;
/// 慢速提早换阈值：带宽持续低于此值（字节/秒）即切，不等完全死。
const MULTI_HOST_LOW_BPS: f64 = 25_000.0;
/// 换 host 后冷却：避免一轮内对同一死连反复横跳。
const MULTI_HOST_COOLDOWN: Duration = Duration::from_secs(4);
/// 宿主强制切换（`request_host_switch`）的冷却。比自发切换短——宿主只在
/// 「视频流停滞而音频流在涨」这类强证据下才请求，策略层另有 10s 冷却，
/// 这里只防同一 host 被瞬时重复请求（2026-09-02 无感化改造 K4）。
const MULTI_HOST_FORCED_COOLDOWN: Duration = Duration::from_secs(2);
/// 宿主强制切换请求计数（每槽位）。`request_host_switch` 只 +1 计数，
/// 实际切换由该源在下一个判定点（read_range / publish_diag /
/// take_prefetch 等待分片）消费——不触碰 source 所有权，demux 线程安全。
static HOST_SWITCH_REQ: [AtomicU64; SOURCE_DIAG_SLOTS] = [AtomicU64::new(0), AtomicU64::new(0)];

/// 宿主请求指定槽位立即换到下一个候选 host（返回 false = 槽位越界）。
/// DASH 双流约定 video=0、audio=1。只是置位请求计数，实际切换在该源的
/// `maybe_switch_host` 下一次被调用时执行（read_range 入口 / publish_diag /
/// take_prefetch 等待分片），强制切换冷却 2s。
pub fn request_host_switch(slot: usize) -> bool {
    match HOST_SWITCH_REQ.get(slot) {
        Some(counter) => {
            counter.fetch_add(1, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// 当前 epoch 毫秒（与 `FullPreloadState.last_demux_read_ms` 同口径）。
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 从 URL 提取 host（仅用于诊断日志，不触及签名）。
fn host_of(uri: &str) -> &str {
    uri.split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or(uri)
}

impl HttpRangeSource {
    /// 视频/单流默认 read-ahead 窗口。**2MB → 4MB（2026-09-03）**：对齐
    /// PiliPlus 的 mpv `demuxer-max-bytes` 默认 4 MiB
    /// （`lib/utils/storage_pref.dart:825-841`，`Pref.bufferSize = 4.0`）。
    /// 2MB 对 1080P（约 3 Mbps）只有约 5.3 秒缓冲，PiliPlus 有约 11 秒
    /// ——**缓冲深度差一倍是它比我们能扛抖动的直接原因**（对照研究见
    /// bili-rs `research_report_cdn_switch_vs_piliplus.md`）。4MB 让策略层
    /// 的 2 秒饥饿线不再被常规 CDN 抖动顶到；`spawn_prefetch_window` 的
    /// `window_cap` 会真正执行该上限，窗口不会无界增长。代价仅常驻 +2MB。
    /// 可用 `ERIKA_HTTP_READAHEAD_BYTES` 覆盖。
    const DEFAULT_READ_AHEAD_BYTES: u64 = 4 * 1024 * 1024;
    /// 后向缓冲默认大小（对齐 mpv `--demuxer-max-back-bytes` 默认 4MB，
    /// 与 PiliPlus `Pref.bufferSize` 默认一致）。可用环境变量
    /// `ERIKA_HTTP_BACK_BYTES` 覆盖。
    const DEFAULT_BACK_BUFFER_BYTES: u64 = 4 * 1024 * 1024;
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
    /// 自适应放宽的硬上界（K2，2026-09-02）：join 超时区间固定 `[5s, 8s]`。
    /// 原实现可随 est_bps 反向放大到 30s——链路越慢挂死代价越大，与
    /// 「慢网要更快放弃」的直觉相反。8s 内新 host 轮换（强制冷却 2s）+
    /// 桶接管补片通常已能恢复，超过 8s 的等待换不来数据只换冻结。
    const PREFETCH_JOIN_TIMEOUT_MAX: Duration = Duration::from_secs(8);
    /// 自适应段大小下限：带宽极低（<~100KB/s）时 32KB 段（8 段整窗 256KB）
    /// 约 2.5s 完成，仍在 join 超时内；再小 HTTP 请求开销占比失控。
    const MIN_PREFETCH_SEGMENT_BYTES: u64 = 32 * 1024;

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
        Self::with_http_headers_and_options(uri, http_headers, diag_slot, None)
    }

    /// `read_ahead`: explicit read-ahead window in bytes; `None` (or `Some(0)`)
    /// falls back to the `ERIKA_HTTP_READAHEAD_BYTES` env override, then the
    /// engine default (video slot 4 MiB, see `DEFAULT_READ_AHEAD_BYTES`).
    pub fn with_http_headers_and_read_ahead(
        uri: impl Into<String>,
        http_headers: Vec<(String, String)>,
        read_ahead: Option<u64>,
    ) -> Self {
        Self::with_http_headers_and_options(uri, http_headers, 0, read_ahead)
    }

    /// 诊断槽位 + per-request read-ahead 的完整构造器，其余入口均委托至此。
    fn with_http_headers_and_options(
        uri: impl Into<String>,
        http_headers: Vec<(String, String)>,
        diag_slot: usize,
        read_ahead: Option<u64>,
    ) -> Self {
        let agent = http_agent();
        let rx = Arc::new(AtomicU64::new(0));
        // 登记到本槽的实时计数器集合：宿主 poll（`source_diag_snapshot_slot`）
        // 与 `publish_diag` 都从这里取「线路上已到达」的字节总量。
        if let Ok(mut counters) = SOURCE_RX.lock() {
            if let Some(list) = counters.get_mut(diag_slot) {
                list.push(rx.clone());
            }
        }
        let uri: String = uri.into();
        let primary = uri.clone();
        Self {
            uri: Arc::new(Mutex::new(uri)),
            primary_uri: primary,
            agent,
            http_headers,
            content_length: None,
            diag_slot,
            cache_start: 0,
            cache_bytes: Vec::new(),
            back_bytes: http_back_bytes(diag_slot),
            read_ahead_bytes: read_ahead
                .filter(|bytes| *bytes > 0)
                .unwrap_or_else(|| http_read_ahead_bytes(diag_slot)),
            est_bps: 0.0,
            last_est_at: Instant::now(),
            est_window_bytes: 0,
            prefetch: Vec::new(),
            prefetch_target: None,
            downloaded_bytes: 0,
            rx,
            // 整片预载默认关闭（直播源 / 单流场景不需要）：None 即不启用，
            // 由上层在需要时通过 `set_full_preload` 注入 `FullPreloadState`。
            full_preload: None,
            preload_thread: None,
            candidates: Vec::new(),
            active_idx: 0,
            last_byte_ms: now_ms(),
            last_read_call_ms: now_ms(),
            switch_cooldown_until: Instant::now(),
            // 新源不消费旧请求：同步为当前计数，宿主的 stale 请求不触发切换
            host_epoch: 0,
            last_switch_req: HOST_SWITCH_REQ
                .get(diag_slot)
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(0),
            switching_host: false,
        }
    }

    /// 注入多 host 候选（按快→慢排序的 `(uri, score)`）。必须在
    /// `enable_full_preload` 之前调用，使桶线程捕获到含候选的共享 uri 引用。
    /// 空列表 = 单源模式（沿用旧行为）。注入后立即把活跃 uri 切到最优候选
    /// （index 0），保证 open 即从最快源起。
    pub fn set_candidates(&mut self, list: Vec<(String, f64)>) {
        if list.is_empty() {
            self.candidates.clear();
            return;
        }
        if let Some((best, _)) = list.first() {
            *self.uri.lock().expect("uri poisoned") = best.clone();
        }
        self.candidates = list
            .into_iter()
            .map(|(uri, score)| CandidateHost { uri, score })
            .collect();
        self.active_idx = 0;
        self.last_byte_ms = now_ms();
        self.switch_cooldown_until = Instant::now();
        // 候选链重建 = 换了新的一轮注入，宿主的旧强制请求不再适用
        self.last_switch_req = HOST_SWITCH_REQ
            .get(self.diag_slot)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0);
    }

    /// 死连/慢速无缝换 host（Option A）。在 read_range / publish_diag /
    /// `take_prefetch` 等待分片入口调用。
    /// 切 host 只替换 `uri`（字节偏移缓存、预载桶、prefetch 队列全部保留），
    /// 因此零缓存丢失、零重建 source——正解决"CDN 源死 → 周期性卡顿"。
    ///
    /// 切换时**作废落在旧 host 上的在飞预取段并按新 host 重发**（K1，
    /// 2026-09-02 无感化改造）：此前在飞段 spawn 时已 clone 旧 uri，换 host
    /// 后它们会一直挂到 join 超时才被放弃，把一次本应零成本的切换放大成
    /// 5~30s 的 demux 冻结。现复用 `prefetch_hint` 的"clear + respawn"语义。
    fn maybe_switch_host(&mut self) {
        if self.candidates.len() < 2 {
            return; // 需 ≥2 个候选才有得换
        }
        // 重入护栏：respawn 路径 publish_diag → 回到这里，直接让路
        if self.switching_host {
            return;
        }
        let now = Instant::now();
        // 宿主强制请求（request_host_switch）：消费计数并放宽自发条件，
        // 但仍尊重更短的强制冷却（2s），防止同一 host 被瞬时重复请求。
        let req = HOST_SWITCH_REQ
            .get(self.diag_slot)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(self.last_switch_req);
        let forced = req != self.last_switch_req;
        if forced {
            self.last_switch_req = req;
        }
        if now < self.switch_cooldown_until {
            return;
        }
        let t = now_ms();
        // 死连：长时间无字节，但 demux 仍在调用 read_range（播放活跃）。
        let stalled = t.saturating_sub(self.last_byte_ms) > MULTI_HOST_STALL_MS
            && t.saturating_sub(self.last_read_call_ms) < MULTI_HOST_ACTIVE_MS;
        // 慢速：带宽过低（提早换，不等完全死）。
        let slow = self.est_bps > 1_000.0 && self.est_bps < MULTI_HOST_LOW_BPS;
        if !forced && !stalled && !slow {
            return;
        }
        let next = (self.active_idx + 1) % self.candidates.len();
        if next == self.active_idx {
            return;
        }
        let new_uri = self.candidates[next].uri.clone();
        *self.uri.lock().expect("uri poisoned") = new_uri.clone();
        http_trace_log(format!(
            "{{\"event\":\"mhost_switch\",\"from\":{},\"to\":{},\"reason\":\"{}\",\"est_bps\":{:.0},\"next_score\":{:.2}}}",
            self.active_idx,
            next,
            if forced {
                "forced"
            } else if stalled {
                "stalled"
            } else {
                "slow"
            },
            self.est_bps,
            self.candidates[next].score
        ));
        self.active_idx = next;
        self.last_byte_ms = t;
        self.host_epoch = self.host_epoch.wrapping_add(1);
        self.switch_cooldown_until =
            now + if forced { MULTI_HOST_FORCED_COOLDOWN } else { MULTI_HOST_COOLDOWN };
        // 作废旧 host 上的在飞段，按新 host 从最早未覆盖处重发（K1）。
        // clear 只丢 receiver：后台线程跑完自退，结果无人接收即丢弃。
        if !self.prefetch.is_empty() {
            let from = self
                .prefetch
                .iter()
                .map(|p| p.range.start)
                .min()
                .unwrap_or(0);
            let discarded = self.prefetch.len();
            self.prefetch.clear();
            http_trace_log(format!(
                "{{\"event\":\"http_prefetch_abandoned_after_host_switch\",\"discarded\":{},\"respawn_from\":{},\"epoch\":{}}}",
                discarded, from, self.host_epoch
            ));
            self.switching_host = true;
            self.spawn_prefetch_window(from, self.prefetch_segment_bytes());
            self.switching_host = false;
        }
    }

    /// 启用后台整片预载。新建 `FullPreloadState`、记录预载上限（环境变量
    /// **`ERIKA_HTTP_FULLPRELOAD_MAX_BYTES`**，默认 512MB），把状态 `Arc` 注入
    /// 自身，再用 `std::thread::spawn` 启动独立下载线程。线程是 `Fn` 循环（不在
    /// demux 与 runtime 上、也不在 UI 线程上），顺序按 `full_preload_step` 推进
    /// 桶：自然被 `core::time` 与 OS 调度器带，独立于 demux 队列——demux 暂停
    /// 或队列已满都不影响下载继续。失败由线程自身退避重试（瞬时网络抖动
    /// 在此消化）。返回 `self` 便于链式调用。
    ///
    /// 桶的存储后端默认**落盘**（`%LOCALAPPDATA%\bili-rs\preload\<摘要>.part`，
    /// 见 `DiskStore`），使这条"整片预载"不再把 300~512MB 常驻内存；落盘不可用
    /// 或写失败时自动退回内存桶。`ERIKA_PRELOAD_TO_DISK=0` 可整体关掉落盘。
    ///
    /// 注意：历史注释里出现过的 `ERIKA_FULL_PRELOAD_CAP` **从来不存在**——代码
    /// 只读 `ERIKA_HTTP_FULLPRELOAD_MAX_BYTES`，设前者唯一的效果是"设了个寂寞"
    /// （这条订正于落盘改造，原文的错误注释误导过一次排查）。要调上限请用后者。
    ///
    /// ```ignore
    /// HttpRangeSource::with_http_headers_and_diag_slot(uri, hdr, slot)
    ///     .enable_full_preload()
    /// ```
    ///
    /// 主调用方 `source_from_uri_with_hint_and_headers` / `source_from_auto_uri_with_slot`
    /// 在 VOD 路径自动启用；直播与本地文件不调本方法。Drop 时由
    /// `Drop for HttpRangeSource` 调 `stop.store(true, Relaxed)` 并 detach 线程
    /// （线程在 stop 后自退，桶文件随最后一个 `Arc` 释放而删除）。
    pub fn enable_full_preload(mut self) -> Self {
        // 已经启用过则幂等跳过——避免上游多次构造/换源场景下重复启动。
        if self.full_preload.is_some() || self.preload_thread.is_some() {
            return self;
        }
        // 音频优先策略：仅对音频流（diag_slot 1）且在环境变量允许时启用。
        let audio_first = self.diag_slot == 1 && audio_first_env_enabled();
        // 音频流但关闭音频优先：保持旧行为（不启用整片桶，退回双段并行预取，
        // 见下方双段并行的说明），不点亮协调器 → 视频不被节流。
        if self.diag_slot == 1 && !audio_first {
            return self;
        }
        let cap = full_preload_cap();
        // 文件名摘要在 `set_candidates` 之后算：那时 `uri` 已经切到最优候选，
        // 桶线程捕获的也是同一份。摘要只取 URI 的稳定部分，换 CDN 不影响复用。
        let uri = self.uri.lock().expect("uri poisoned").clone();
        let state = FullPreloadState::open(&uri, self.diag_slot, cap, &preload_dir());
        // 桶的计数器和源共用同一套「实时已收」语义：桶下载期间（尤其暂停后
        // 桶接管全速补片的阶段）宿主读到非零供给速度，而不是"暂停=0 速度"。
        if let Ok(mut counters) = SOURCE_RX.lock() {
            if let Some(list) = counters.get_mut(self.diag_slot) {
                list.push(state.downloaded.clone());
            }
        }
        self.full_preload = Some(state.clone());
        // 音频优先点亮协调器：音频源用激进整片预载把整条音轨拉完，视频源据此
        // 在音频达标前压低并发。每次启用都复位进度（上一场播放的残留值归零）。
        if audio_first {
            AUDIO_FIRST.enabled.store(true, Ordering::Relaxed);
            AUDIO_FIRST.audio_permille.store(0, Ordering::Relaxed);
            AUDIO_FIRST.audio_total_known.store(false, Ordering::Relaxed);
            AUDIO_FIRST.enabled_at_ms.store(audio_first_now_ms(), Ordering::Relaxed);
        }
        let (eager, report, video_bucket) = if audio_first {
            // 激进音频桶：不向 demux 让路，优先把整条音轨拉完；进度上报协调器。
            (true, true, false)
        } else {
            // 普通视频桶：播放中向 demux 让路、不报音频优先。
            (false, false, true)
        };
        let shared_uri = self.uri.clone(); // Arc<Mutex<String>>：换 host 时桶线程自动跟随
        let headers = self.http_headers.clone();
        let agent = self.agent.clone();
        let handle = std::thread::Builder::new()
            .name(format!("erika-full-preload-{}", self.diag_slot))
            .spawn(move || {
                full_preload_loop(
                    &agent,
                    shared_uri,
                    &headers,
                    &state,
                    cap,
                    eager,
                    report,
                    video_bucket,
                );
            })
            .expect("failed to spawn full-preload thread");
        self.preload_thread = Some(handle);
        self
    }

    /// 累计已下载字节进带宽估算窗口；跨 2s 差分做 EMA 更新。
    /// 同步 fetch 与预取段消费两处都调用——窗口内并行段全部计入，
    /// 估算反映多连接叠加后的真实总吞吐。
    fn record_downloaded(&mut self, bytes: u64) {
        self.last_byte_ms = now_ms(); // 收到字节 = 源仍活着（死连检测）
        self.est_window_bytes = self.est_window_bytes.saturating_add(bytes);
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_est_at);
        if elapsed >= BANDWIDTH_ESTIMATE_WINDOW {
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

    /// 自适应 join 超时：`max(5s, min(单段完成时间 × 1.5, 8s))`，结果有界
    /// `[5s, 8s]`。段大小已按带宽缩放，放宽只对「带宽估算偏低、段比预期
    /// 慢一点」兜底；**不随 est_bps 反向放大**（2026-09-02 无感化改造 K2）：
    /// 原上限 30s 在极慢链路（est_bps≈2KB/s）下把一次 host 死连放大成
    /// 半分钟 demux 冻结——链路越慢越该早点放弃慢段、走同步小段 fetch
    /// （只取当前读取所需，与预取段同量级），比干等廉价得多。
    fn prefetch_join_timeout(&self) -> Duration {
        // 守卫必须先于除法：est_bps 为 NaN/inf（EMA 混入坏值）、趋零 denormal
        // （长期暂停后 EMA 衰减，`refresh_bandwidth` 地板兜底前的存量值）或
        // 低于 1bps 的"亚正常"值时，`seg / est_bps` 会得到 NaN 或天文数字，
        // `Duration::from_secs_f64` 对 "too big or NaN" 直接 panic。而本函数
        // 运行在 demux 线程 `extern "C"` 的 AVIO 读回调内，panic 不可 unwind
        // → 进程 fail-fast abort（实测 0xc0000409 闪退：2026-09-01 16:55，
        // 暂停 10 分钟后点播放即崩）。地板取 1bps 与 `refresh_bandwidth`
        // 的零供给地板一致：这种量级必然是估算失真而非真实带宽。
        if !(self.est_bps.is_finite() && self.est_bps >= 1.0) {
            return Self::PREFETCH_JOIN_TIMEOUT;
        }
        let seg = self.prefetch_segment_bytes() as f64;
        let needed = (seg / self.est_bps * 1.5).min(Self::PREFETCH_JOIN_TIMEOUT_MAX.as_secs_f64());
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
        let n = usize::try_from(requested_length)
            .unwrap_or(0)
            .min(self.cache_bytes.len() - offset);
        Some(self.cache_bytes[offset..offset + n].to_vec())
    }

    fn cache_end(&self) -> u64 {
        self.cache_start
            .saturating_add(self.cache_bytes.len() as u64)
    }

    /// 把当前网络/缓冲状态发布到自己的诊断槽（宿主 UI 轮询读取）。
    /// DASH 双流（video=0/audio=1）各发布各的，互不覆盖。
    fn publish_diag(&mut self) {
        // 多源无缝换 host：诊断刷新时顺带判定死连/慢速并切到下一候选。
        self.maybe_switch_host();
        // 合并桶数据：缓存窗口前沿 = max(demux 端 cache_end, 桶 covered_until)。
        // 用户定规的核心表征——「缓冲百分比」「下载速度」—都通过这两个字段
        // 反映：桶独立于 demux 推进，pause 后桶仍覆盖到更前方，诊断面板如实
        // 显示「缓冲 80% 仍在涨」。
        let cache_end = match self.full_preload.as_ref() {
            Some(p) => self.cache_end().max(p.covered_until()),
            None => self.cache_end(),
        };
        let prefetch_in_flight = match self.full_preload.as_ref() {
            Some(p) => self.prefetch.len() + p.in_flight_segments() as usize,
            None => self.prefetch.len(),
        };
        // 供给信号一律走**实时**口径：取本槽登记的全部计数器之和（源自身 rx +
        // 整片预载桶），而不是「段完成才累加」的 `self.downloaded_bytes`。
        let downloaded_bytes = live_rx_bytes(self.diag_slot);
        // 拉模型结算（见 `refresh_bandwidth`）：此处刷新后写入 `self.est_bps`，
        // 段大小/join 超时的自适应也跟着用上实时口径，慢网下不再冻结。
        self.est_bps = refresh_bandwidth(self.diag_slot, downloaded_bytes);
        let uri_guard = self.uri.lock().expect("uri poisoned");
        let active_host = host_of(&uri_guard);
        if let Ok(mut slots) = SOURCE_DIAG.lock() {
            if let Some(slot) = slots.get_mut(self.diag_slot) {
                *slot = SourceDiagSnapshot {
                    content_length: self.content_length,
                    cache_start: self.cache_start,
                    cache_end,
                    prefetch_in_flight,
                    downloaded_bytes,
                    est_bps: self.est_bps,
                    active_host: active_host.to_string(),
                    active_idx: self.active_idx,
                    candidate_count: self.candidates.len(),
                    // demux 自身窗口终点（不含桶）：宿主策略算真实缓冲秒用
                    demux_cache_end: self.cache_end(),
                    host_epoch: self.host_epoch,
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
        // uri 是共享 Arc<Mutex<String>>：克隆出当前 host 字符串（不持锁过网络调用），
        // 换 host 时桶线程/本函数各自取到最新值。
        let uri = self.uri.lock().expect("uri poisoned").clone();
        let response = fetch_http_range(
            &self.agent,
            &uri,
            &self.http_headers,
            range,
            "http_range",
            // 同步 fetch 同样实时计数：demux 正等这段数据时，宿主 poll 能看出
            // 它到底是在下（慢）还是根本没在下（挂死）。
            Some(self.rx.as_ref()),
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
        // 超时按实测带宽自适应（有界 [5s, 8s]，见 `prefetch_join_timeout`）。
        //
        // **分片轮询**（K3，2026-09-02 无感化改造）：此前是单次 recv_timeout
        // 一次性睡满整个超时——期间 demux 线程完全阻塞，`maybe_switch_host`
        // 的两个既有调用点（read_range 入口 / publish_diag）都不可达，等于
        // 「卡住的时候恰好换不了 host」。改为每 250ms 醒一次并判定一次
        // host 切换；若等待期间 epoch 已变（宿主强制请求或死连判定生效），
        // 本段落在了旧 host 上 → 作废并返回 None，read_range 的同步 fetch
        // 取当前 uri（= 新 host）立即重取，把一次 host 死连的可见代价从
        // 「join 超时全值」压到「下一个 250ms 分片」。
        let deadline = Instant::now() + self.prefetch_join_timeout();
        let slice = Duration::from_millis(250);
        let result = loop {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
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
            match pending.receiver.recv_timeout(slice.min(remain)) {
                Ok(result) => break result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // 等待期间允许换 host：死连判定 / 宿主强制请求都从这里生效
                    self.maybe_switch_host();
                    if pending.host_epoch != self.host_epoch {
                        http_trace_log(format!(
                            "{{\"event\":\"http_prefetch_abandoned_in_join\",\"start\":{},\"waited_ms\":{:.3},\"epoch\":{}}}",
                            start,
                            join_started.elapsed().as_secs_f64() * 1000.0,
                            self.host_epoch
                        ));
                        return None; // 同步 fetch 将用新 host 重取该区间
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Some(Err(SourceError::Http(
                        "http prefetch thread panicked".to_string(),
                    )));
                }
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
                let keep_end = usize::try_from(start - old_start).unwrap_or(0).min(old_len);
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
        // 整片预载桶已覆盖到 demux 窗口前沿：demux 端并行预取（默认 2 段在飞）
        // 会变成与后台线程重复下载同一区间。桶线程独立于 demux 与 runtime、
        // 暂停也持续向前，让桶独占后续字节下载更省带宽——这里短路掉 demux
        // 端的预取填充，避免双份流量。
        if let Some(state) = self.full_preload.as_ref() {
            if state.covered_until() >= cache_end {
                return;
            }
        }
        let remaining = cache_end.saturating_sub(end);
        // 激进预缓冲：只要还有存量数据即发起并行预取（spawn_prefetch_window
        // 按 ERIKA_HTTP_PREFETCH_PARALLEL 默认 2 段在飞），持续把 cache_end
        // 后方的 read_ahead 窗口向前推进（对齐 PiliPlus：播放中持续大步向前
        // 缓冲）。不设"剩余 > 半窗才停"门限——慢网下半窗未到预取段无法启动，
        // 窗口被消费后才补段，预取永远落后。段间按字节偏移连续切分、join 严格
        // 按覆盖关系（读哪段 join 哪段），乱序完成段在队列中等待前序段——窗口
        // 按偏移有序增长，无拼接错位（历史坏包根因是 window_slice 偏移与
        // reset_cache 重复保留，均已修复，见各自注释）。
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

    /// 从 `from` 起连续切分**并行预取段**，填满并发上限（默认 2 段在飞；
    /// `ERIKA_HTTP_PREFETCH_PARALLEL` 覆盖，钳 1..=8）。顺序预取
    /// （`maybe_start_prefetch`）与窗口外 seek 预取（`prefetch_hint`）共用。
    ///
    /// 历史"坏包"根因（avcodec Invalid data 的 30 坏包/时序相关）**不是并行
    /// 本身**，而是 `window_slice` 偏移与 `reset_cache` 重复保留旧窗口数据——
    /// 这两处已修复，并行安全。段间按字节偏移紧邻切分、join 严格按覆盖关系
    /// （读哪段 join 哪段），乱序完成的段在队列中等待前序段，窗口按偏移有序
    /// 增长，无拼接错位。多段并发把高 RTT/单连接限速链路的吞吐叠加 ×N：
    /// 弱源 150-250KB/s 多为服务端单连接限速，多连接是唯一杠杆。
    /// 从 `from` 起**连续切分**并行预取段，填满并发上限（默认 2 段在飞；
    /// `ERIKA_HTTP_PREFETCH_PARALLEL` 覆盖）。多段并发把高 RTT/限速链路的
    /// 单连接吞吐叠加 ×N——弱源 150-250KB/s 多为服务端单连接限速，单段
    /// 串行无论怎么调参都突破不了，多连接是唯一杠杆。
    /// 段间按字节偏移紧邻切分；take_prefetch 仅 join 覆盖当前读取的段，
    /// 乱序完成的段在队列中等待前序段——窗口按偏移有序增长，无拼接错位。
    /// 同时补上 read_ahead 窗口硬约束：在飞段+新段不得越过
    /// `cache_end + read_ahead_bytes`（此前该上限仅在日志中打印、未真正
    /// 执行，快网下窗口可无界涨到整个文件）。
    fn spawn_prefetch_window(&mut self, from: u64, seg_len: u64) {
        let Some(total) = self.content_length else {
            return;
        };
        if from >= total {
            return;
        }
        let window_cap = self.cache_end().saturating_add(self.read_ahead_bytes);
        // 音频流（diag_slot 1）也 2 段在途：此前强制单段——音频码率低、单段
        // 吞吐理论上够。但弱网下视频流（2 并行 + 整片预载桶）抢光带宽，音频
        // 单段（est_bps 低 → 缩到 32KB 下限）下载间歇，demux 读包频繁 join
        // 阻塞（实测平均 348ms）→ 平均供给 ~18KB/s < 音频码率 ~23KB/s →
        // 音频下溢 + 时钟跳过（underflow 832、stage=stale），声音卡住拖垮视频
        // 时钟（4K 弱网"一卡一顿"根因）。2 段在途让下载更持续，供给提到 ~23KB/s
        // 持平码率。总并发 5（v2+桶1+a2）仍低于触发限流的 8，风险可控。
        // 注意：开启「音频优先」后，音频由激进整片预载桶整条拉完，此处的
        // 2 段并行预取会在桶覆盖 cache_end 后被 `maybe_start_prefetch` 短路，
        // 实际只剩桶在跑，不会重复下载。
        let parallel = if self.diag_slot == 1 {
            2
        } else if audio_first_throttle_video() {
            // 音频优先相位：视频把并发压到 1 段（仅维持 live demux 不被饿死），
            // 把带宽让给正在整片拉取的音频，从源头杜绝音频饿死。
            1
        } else {
            http_prefetch_parallel()
        };
        let mut cursor = from;
        while self.prefetch.len() < parallel {
            if cursor >= total || cursor >= window_cap {
                break;
            }
            // 已有在飞段覆盖 cursor（顺序预取与 seek 预取可能重叠）→
            // 跳到该段末尾继续切下一段
            if let Some(covered_end) = self
                .prefetch
                .iter()
                .find(|p| p.range.start <= cursor && cursor < p.range_end())
                .map(|p| p.range_end())
            {
                cursor = covered_end;
                continue;
            }
            let len = seg_len
                .min(total.saturating_sub(cursor))
                .min(window_cap.saturating_sub(cursor));
            if len == 0 {
                break;
            }
            let prefetch_range = ByteRange {
                start: cursor,
                length: Some(len),
            };
            http_trace_log(format!(
                "{{\"event\":\"http_prefetch_start\",\"start\":{},\"length\":{},\"in_flight\":{}}}",
                prefetch_range.start,
                len,
                self.prefetch.len(),
            ));
            self.prefetch.push(PendingHttpFetch::spawn(
                self.uri.lock().expect("uri poisoned").clone(),
                self.http_headers.clone(),
                prefetch_range,
                self.rx.clone(),
                self.host_epoch,
            ));
            self.publish_diag();
            cursor = cursor.saturating_add(len);
        }
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
    /// `progress`：与源共享的实时计数器。预取段是慢网下的主要数据通路，
    /// **在飞期间**就要能看出字节在涨——否则宿主只能等整段完成才知道有供给。
    fn spawn(
        uri: String,
        http_headers: Vec<(String, String)>,
        range: ByteRange,
        progress: Arc<AtomicU64>,
        host_epoch: u64,
    ) -> Self {
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
            let result = fetch_http_range(
                &agent,
                &uri,
                &http_headers,
                range,
                "http_prefetch_range",
                Some(progress.as_ref()),
            );
            let _ = sender.send(result);
        });
        Self {
            range,
            handle,
            receiver,
            host_epoch,
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
        // 后台整片预载线程：**只置 stop，不 join**（见 `preload_thread` 字段
        // 注释）。线程下次循环看到 `stop` 即退出——关闭播放窗后网络连接在
        // 一个段请求（≤256KB，几十毫秒）内归零，绝不阻塞 UI 线程。把句柄
        // take 出来再 drop，让线程在 stop 后自然结束而非 leaked（保持可观测）。
        // 注销实时计数器：按 `Arc::ptr_eq` 精确摘掉自己登记的那几个（换源时
        // 新源已先登记，误删会让它"永远 0 字节"→ 换源后立刻被判饥饿）。
        let unregister = |counter: &Arc<AtomicU64>| {
            if let Ok(mut counters) = SOURCE_RX.lock() {
                if let Some(list) = counters.get_mut(self.diag_slot) {
                    list.retain(|c| !Arc::ptr_eq(c, counter));
                }
            }
        };
        unregister(&self.rx);
        if let Some(state) = self.full_preload.take() {
            unregister(&state.downloaded);
            state.stop();
        }
        // 音频源销毁（播放停止/换源）→ 关闭音频优先：复位协调器，视频源随即
        // 解除节流、恢复全速（双流同生命周期，通常一起销毁）。
        if self.diag_slot == 1 {
            AUDIO_FIRST.enabled.store(false, Ordering::Relaxed);
            AUDIO_FIRST.audio_permille.store(0, Ordering::Relaxed);
            AUDIO_FIRST.audio_total_known.store(false, Ordering::Relaxed);
        }
        if let Some(handle) = self.preload_thread.take() {
            drop(handle); // 线程已在 stop 后自退，drop 句柄不阻塞（detached）。
        }
        // source 销毁（播放停止/换源）→ 清空**自己的**诊断槽，宿主读到
        // 零值即"该流无播放"。只清本槽：DASH 双流另一条流的诊断不受影响。
        if let Ok(mut slots) = SOURCE_DIAG.lock() {
            if let Some(slot) = slots.get_mut(self.diag_slot) {
                *slot = SourceDiagSnapshot::default();
            }
        }
        // 带宽采样点一并作废：新源从 0 起累计，若沿用旧基线会算出
        // `saturating_sub = 0` 的瞬时速率并拖低 EMA（新源起步期被低估）。
        // 清成 None → 下一个窗口冷启动，直接取真实瞬时值。
        if let Ok(mut samples) = SOURCE_BW.lock() {
            if let Some(entry) = samples.get_mut(self.diag_slot) {
                *entry = None;
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
                    active_host: String::new(),
                    active_idx: 0,
                    candidate_count: 0,
                    demux_cache_end: self.offset,
                    host_epoch: 0,
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

/// body 分块读取的块大小。64KB 兼顾两件事：块够大 → 单次 `fetch_add` 的
/// 原子操作开销可忽略（1MB/s 下每秒仅 16 次）；块够小 → 慢网（如 50KB/s）
/// 下每 1.3s 就有一次进度可见，诊断不会"长时间不动"。
const HTTP_RECV_CHUNK_BYTES: usize = 64 * 1024;

/// `progress`：实时进度计数器（可选）。**不为 None 时**，body 每读到一块就
/// `fetch_add` 一次，使 `downloaded_bytes` 在段传输期间持续增长——慢网下
/// 「正在缓慢下载」不再被宿主差分读成 0（换源/降档误判的根因）。
fn fetch_http_range(
    agent: &ureq::Agent,
    uri: &str,
    http_headers: &[(String, String)],
    range: ByteRange,
    event: &str,
    progress: Option<&AtomicU64>,
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
        // 分块读替代 `read_to_end`：每块到达即累加实时进度（见 `progress` 参数
        // 注释）。读失败时保留已到达的字节——下面的重试按 `bytes.len()` 续传，
        // 已计入的字节不会被重复计数。
        let mut reader = response.body_mut().as_reader();
        let mut chunk = [0u8; HTTP_RECV_CHUNK_BYTES];
        let mut read_error: Option<std::io::Error> = None;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(counter) = progress {
                        counter.fetch_add(n as u64, Ordering::Relaxed);
                    }
                }
                Err(error) => {
                    read_error = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = read_error {
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
        let u = self.uri.lock().expect("uri poisoned");
        f.debug_struct("HttpRangeSource")
            .field("uri", &redacted_uri(&u))
            .field("content_length", &self.content_length)
            .field("cache_start", &self.cache_start)
            .field("cache_bytes", &self.cache_bytes.len())
            .field("read_ahead_bytes", &self.read_ahead_bytes)
            .finish()
    }
}

impl MediaSource for HttpRangeSource {
    fn uri(&self) -> &str {
        &self.primary_uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        if self.content_length.is_some() {
            return Ok(self.content_length);
        }
        let started = Instant::now();
        http_trace_log(format!(
            "[erika-http-trace] stage=head_request uri={} cache_start={} cache_end={} read_ahead={}",
            redacted_uri(&self.uri.lock().expect("uri poisoned")),
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
        let uri = self.uri.lock().expect("uri poisoned").clone();
        let mut attempt = 0u32;
        let head_error = loop {
            attempt += 1;
            let mut request = head_agent.head(&uri);
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
                        let uri = self.uri.lock().expect("uri poisoned").clone();
                        return match probe_http_total_length(
                            &self.agent,
                            &uri,
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
        let uri = self.uri.lock().expect("uri poisoned").clone();
        match probe_http_total_length(&self.agent, &uri, &self.http_headers) {
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
        self.last_read_call_ms = now_ms();
        self.maybe_switch_host();
        // 与整片预载桶的两个协同信号（都是无锁原子写，每次读刷新，成本可忽略）：
        // ① mark_demux_read：告诉桶"demux 正在消费"，播放中桶让路，把带宽完整
        //    留给 demux 的多段并行预取（聚合被 CDN 限速的单连接带宽）；
        // ② set_floor：把 demux 已覆盖前沿交给桶，桶跳过该偏移之前的区间，
        //    绝不重复下载 demux 已下过的字节（重复下载会让有效供给砍半）。
        if let Some(state) = self.full_preload.as_ref() {
            state.mark_demux_read();
            state.set_floor(self.cache_end());
        }
        // 整片预载桶优先：桶 [start, covered_until) 是独立线程下载、跨 demux 与
        // 暂停依然持续推进的"档案式"内存镜像。被桶完整覆盖的请求直接复制
        // 返回（零网络，绕开 demux prefetch 队列的全部路径），让 demux 哪怕
        // 暂停也能从桶里取到字节流而不退化。
        if let Some(state) = self.full_preload.as_ref() {
            if let Some(length) = range.length {
                if state.covers(range.start, length) {
                    if let Some(bytes) = state.copy_range(range.start, length) {
                        self.maybe_start_prefetch(range);
                        self.publish_diag();
                        http_trace_log(format!(
                            "{{\"event\":\"http_full_preload_hit\",\"start\":{},\"length\":{}}}",
                            range.start,
                            length,
                        ));
                        trace_read_return("http_read_full_preload", range, self, &bytes);
                        return Ok(bytes);
                    }
                }
            }
        }
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
                    let out = self
                        .window_slice(range, requested_length)
                        .unwrap_or_default();
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
            let out = self
                .window_slice(range, requested_length)
                .unwrap_or_default();
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
        let out = self
            .window_slice(range, requested_length)
            .unwrap_or_default();
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
    source_from_uri_with_hint_and_headers(uri, source_hint, Vec::new(), Vec::new())
}

pub fn source_from_uri_with_hint_and_headers(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
    candidates: Vec<(String, f64)>,
) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint_and_headers_slot(uri, source_hint, http_headers, 0, candidates)
}

/// 同 `source_from_uri_with_hint_and_headers`，但指定诊断槽位
/// （DASH 双流：video=0 / audio=1，各源独立发布/清空诊断互不覆盖）。
pub(crate) fn source_from_uri_with_hint_and_headers_slot(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
    diag_slot: usize,
    candidates: Vec<(String, f64)>,
) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint_and_headers_slot_and_read_ahead(
        uri,
        source_hint,
        http_headers,
        diag_slot,
        candidates,
        None,
    )
}

/// `http_read_ahead_bytes` only applies to HTTP(S) sources and overrides the
/// per-request read-ahead window; `None` keeps the default resolution
/// (env override, then the engine default).
pub fn source_from_uri_with_options(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
    http_read_ahead_bytes: Option<u64>,
) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint_and_headers_slot_and_read_ahead(
        uri,
        source_hint,
        http_headers,
        0,
        Vec::new(),
        http_read_ahead_bytes,
    )
}

/// 诊断槽位 + 多 host 候选 + per-request read-ahead 的完整工厂，其余入口均委托至此。
pub(crate) fn source_from_uri_with_hint_and_headers_slot_and_read_ahead(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
    diag_slot: usize,
    candidates: Vec<(String, f64)>,
    http_read_ahead_bytes: Option<u64>,
) -> Result<Box<dyn MediaSource>> {
    match source_hint {
        MediaSourceHint::Auto => source_from_auto_uri_with_slot(
            uri,
            http_headers,
            diag_slot,
            candidates,
            http_read_ahead_bytes,
        ),
        MediaSourceHint::LocalFile => source_from_local_uri(uri),
        MediaSourceHint::Http => {
            if uri.starts_with("http://") || uri.starts_with("https://") {
                let mut s = HttpRangeSource::with_http_headers_and_options(
                    uri,
                    http_headers,
                    diag_slot,
                    http_read_ahead_bytes,
                );
                s.set_candidates(candidates);
                Ok(Box::new(s.enable_full_preload()))
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

/// Auto 提示的 URI 探测 + 源构造。**必须透传 diag_slot**：
/// 早期版本 HTTP 分支走 `with_http_headers`（固定槽 0），DASH 双流若都
/// 用 Auto hint（bili-rs 默认），视频与音频源会同时写诊断槽 0 互相覆盖
/// ——宿主读到"视频缓存/音频缓存"错乱（音频缓存恒 0、视频总长在两个
/// 流的长度间跳变）。
fn source_from_auto_uri_with_slot(
    uri: &str,
    http_headers: Vec<(String, String)>,
    diag_slot: usize,
    candidates: Vec<(String, f64)>,
    http_read_ahead_bytes: Option<u64>,
) -> Result<Box<dyn MediaSource>> {
    if uri.starts_with("fd://") {
        return source_from_local_uri(uri);
    }
    if let Some(path) = uri.strip_prefix("file://") {
        return Ok(Box::new(LocalFileSource::open(path)?));
    }
    if uri.starts_with("http://") || uri.starts_with("https://") {
        let mut s = HttpRangeSource::with_http_headers_and_options(
            uri,
            http_headers,
            diag_slot,
            http_read_ahead_bytes,
        );
        s.set_candidates(candidates);
        return Ok(Box::new(s.enable_full_preload()));
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

/// DASH 音频流（diag_slot > 0）的默认 read-ahead。音频码率与视频差两个
/// 数量级：B 站音频 128-320Kbps（杜比 ~768Kbps）≈ 40-96KB/s，而视频
/// 1080P ≈ 2-4MB/s。**之前音频与视频共用同一 read-ahead（宿主 24MB）**
/// ——24MB 音频 = 4-10 分钟的缓冲，纯浪费（音频消费端 audio_lead_time
/// 仅 1.5s）。2MB 对 40KB/s 也有 50s 余量，弱网 + 自适应段大小（32KB
/// 下限）下预取链路依然成立。整体省 ~22MB 播放常驻。
const AUDIO_DEFAULT_READ_AHEAD_BYTES: u64 = 2 * 1024 * 1024;
/// DASH 音频流默认 back buffer（对齐上面音频 read-ahead 的量级逻辑）：
/// 1MB ≈ 10-25s 的回退缓冲，seek 回退命中缓存足够；再大性价比低。
const AUDIO_DEFAULT_BACK_BUFFER_BYTES: u64 = 1024 * 1024;

/// diag_slot == 0：视频/单流（跟随 `ERIKA_HTTP_READAHEAD_BYTES`，默认
/// `DEFAULT_READ_AHEAD_BYTES`）。diag_slot > 0：DASH 音频流——**用音频
/// 默认值而非视频值**（见 `AUDIO_DEFAULT_READ_AHEAD_BYTES` 注释），可用
/// `ERIKA_HTTP_READAHEAD_BYTES_AUDIO` 覆盖（设 0 关闭音频预取）。
/// 顺序预取并行在途段数（`ERIKA_HTTP_PREFETCH_PARALLEL` 覆盖，默认 2，
/// 钳制 1..=8；音频流的 2 段并行仅在「音频优先」关闭时生效，开启后音频由
/// 整片预载桶整条拉完、并行预取被短路，见 `spawn_prefetch_window`）。
/// 多段并发把
/// 高 RTT/单连接限速链路的吞吐叠加 ×N；段间按字节偏移连续切分、join
/// 严格按覆盖关系（读哪段 join 哪段），乱序完成的段在队列中等待前序段
/// ——窗口按偏移有序增长，无拼接错位。历史"坏包"根因是 window_slice
/// 偏移与 reset_cache 重复保留（均已修复），并行安全。
///
/// **默认 3→2（2026-09-02）**：DASH 双流各按上限拉段，视频 3 + 桶 1 +
/// 音频 3 + 桶 1 = 8 个并发 Range 连接。B 站 CDN 对单 IP 并发连接数
/// 有限制，实测 8 并发触发限流惩罚：在飞段集体挂死（0 字节数十秒）、
/// 新建连接被主动断开（Peer disconnected / TLS unexpected EOF）→ open
/// 失败循环 + 播放约 10s 追上缓冲边界后断流。2 并行 + 桶 + 音频单段
/// 把稳态并发压到 4，仍是单连接限速下 2-4 倍吞吐聚合。
fn http_prefetch_parallel() -> usize {
    std::env::var("ERIKA_HTTP_PREFETCH_PARALLEL")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.clamp(1, 8))
        .unwrap_or(2)
}

fn http_read_ahead_bytes(diag_slot: usize) -> u64 {
    if diag_slot > 0 {
        return env::var("ERIKA_HTTP_READAHEAD_BYTES_AUDIO")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(AUDIO_DEFAULT_READ_AHEAD_BYTES);
    }
    env::var("ERIKA_HTTP_READAHEAD_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(HttpRangeSource::DEFAULT_READ_AHEAD_BYTES)
}

/// 同 [`http_read_ahead_bytes`]：视频/单流走 `ERIKA_HTTP_BACK_BYTES`，
/// 音频流走 `ERIKA_HTTP_BACK_BYTES_AUDIO`（默认 `AUDIO_DEFAULT_BACK_BUFFER_BYTES`）。
fn http_back_bytes(diag_slot: usize) -> u64 {
    if diag_slot > 0 {
        return env::var("ERIKA_HTTP_BACK_BYTES_AUDIO")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(AUDIO_DEFAULT_BACK_BUFFER_BYTES);
    }
    env::var("ERIKA_HTTP_BACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(HttpRangeSource::DEFAULT_BACK_BUFFER_BYTES)
}

/// ERIKA_HTTP_TRACE 门控缓存（进程内 env 只在启动阶段写入，值不变）。
/// read_range 每个 cache hit 都会走 http_trace_log（FFmpeg AVIO 32KB 块读
/// → 视频流 ~100 次/秒）——此前每次都做 env::var 查询：Windows 上拿进程
/// 环境锁 + 线性查找，在 demux 热路径上是实打实的锁竞争开销。LazyLock
/// 首次求值后零开销（宿主在 main 早期 set_var，先于任何源创建，时序安全）。
static HTTP_TRACE_ENABLED: LazyLock<bool> =
    LazyLock::new(|| trace::env_flag("ERIKA_HTTP_TRACE"));

fn http_trace_log(line: impl AsRef<str>) {
    if !*HTTP_TRACE_ENABLED {
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

    #[test]
    fn full_preload_skips_ranges_already_covered_by_demux() {
        // demux 并行预取跑赢桶时，桶必须整体前移到 demux 前沿再继续下载——
        // 否则就是与 demux 重复下载同一区间，有效供给被自己砍半（实测
        // 23KB/s 里一半是重复字节，缓冲永远建不起来 → 一路饥渴换源 → 换到
        // 差源就表现为"拉流失败"）。
        let body = vec![7u8; 256 * 1024];
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(512 * 1024, 1024 * 1024, &body),
        )]);
        let state = FullPreloadState::new();
        state.inner.lock().unwrap().total = Some(1024 * 1024);
        // demux 已覆盖到 512KB
        state.set_floor(512 * 1024);

        // 第一步：只前移（丢弃 demux 已持有的旧字节），不发任何网络请求
        assert!(!full_preload_step(&state, &http_agent(), &uri, &[], 1024 * 1024));
        assert_eq!(state.covered_until(), 512 * 1024);
        assert!(
            requests.recv_timeout(Duration::from_millis(100)).is_err(),
            "前移步不应发起请求"
        );

        // 第二步：从 demux 前沿（512KB）起下载，绝不回头重下 [0, 512KB)
        assert!(!full_preload_step(&state, &http_agent(), &uri, &[], 1024 * 1024));
        let head = recv_request_head(&requests);
        assert!(
            head.contains("range: bytes=524288-"),
            "应从 demux 前沿 512KB 起下载，实际请求头: {head}"
        );
        assert_eq!(state.covered_until(), 512 * 1024 + 256 * 1024);
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
        // 4MB = PiliPlus mpv `demuxer-max-bytes` 默认（见常量处注释）。
        // 与后向缓冲同量级，1080P 下约 11 秒前向余量。
        assert_eq!(HttpRangeSource::DEFAULT_READ_AHEAD_BYTES, 4 * 1024 * 1024);
    }

    // ---- 带宽自适应（弱网修复）：段大小 / join 超时 / 估算器 ----

    #[test]
    fn dash_audio_source_uses_small_buffers_by_default() {
        // 音频流（diag_slot=1）：read-ahead/back 走音频独立配置（码率比
        // 视频低两个数量级，视频级窗口是纯浪费——见 AUDIO_DEFAULT_* 注释）。
        // 断言按"构造值 == 同槽位配置函数值"路由一致性写，宿主设置
        // ERIKA_HTTP_*_BYTES 环境变量时测试依然成立（env 变更在 edition
        // 2024 是 unsafe，测试不做增删）。
        let audio = HttpRangeSource::with_http_headers_and_diag_slot(
            "https://example.invalid/audio.m4s",
            Vec::new(),
            1,
        );
        assert_eq!(audio.read_ahead_bytes, http_read_ahead_bytes(1));
        assert_eq!(audio.back_bytes, http_back_bytes(1));
        // 音频槽位不继承视频槽位的 env 覆盖（宿主只调大 ERIKA_HTTP_
        // READAHEAD_BYTES 时，音频保持 2MB 小窗而非跟着变大）
        unsafe {
            std::env::set_var("ERIKA_HTTP_READAHEAD_BYTES", "8388608");
        }
        let audio_after = HttpRangeSource::with_http_headers_and_diag_slot(
            "https://example.invalid/audio.m4s",
            Vec::new(),
            1,
        );
        let video_after = HttpRangeSource::with_http_headers_and_diag_slot(
            "https://example.invalid/video.m4s",
            Vec::new(),
            0,
        );
        assert_eq!(audio_after.read_ahead_bytes, AUDIO_DEFAULT_READ_AHEAD_BYTES);
        assert_eq!(video_after.read_ahead_bytes, 8 * 1024 * 1024);
        // 常规测试环境（两个 env 均未设置）：视频窗口（4MB，对齐 PiliPlus
        // mpv `demuxer-max-bytes`）现在**大于**音频窗口（2MB）。此前两者同为
        // 2MB，视频侧缓冲只有 PiliPlus 的一半——这是两者抗抖动能力差异的
        // 直接来源（2026-09-03 对照研究，见常量处注释）。
        assert!(
            HttpRangeSource::DEFAULT_READ_AHEAD_BYTES > AUDIO_DEFAULT_READ_AHEAD_BYTES,
            "视频 read-ahead 应大于音频：音频码率低两个数量级，等大窗口是浪费"
        );
    }

    #[test]
    fn prefetch_segment_bytes_scales_with_estimated_bandwidth() {
        let mut source = HttpRangeSource::new("https://example.invalid/video.m4s");
        // 冷启动（带宽未知）：保持 256KB 原行为
        assert_eq!(
            source.prefetch_segment_bytes(),
            HttpRangeSource::SEEK_PREFETCH_SEGMENT_BYTES
        );
        // 高带宽（1MB/s 顺序单段）：1MB/s × 3s = 3MB → 封顶 256KB
        source.est_bps = 1024.0 * 1024.0;
        assert_eq!(source.prefetch_segment_bytes(), 256 * 1024);
        // 中带宽（100KB/s 顺序单段）：100KB/s × 3s = 300KB → 封顶 256KB
        //（顺序预取只有 1 段在途，段大小即整窗供给速度）
        source.est_bps = 100.0 * 1024.0;
        assert_eq!(source.prefetch_segment_bytes(), 256 * 1024);
        // 更低带宽（80KB/s）：80KB/s × 3s = 240KB → 自动缩到 240KB
        source.est_bps = 80.0 * 1024.0;
        assert_eq!(
            source.prefetch_segment_bytes(),
            (80.0 * 1024.0 * 3.0) as u64
        );
        // 极低带宽（50KB/s）：50KB/s × 3s = 150KB → 继续缩段；
        // 30KB/s 触及下限 32KB（再小 HTTP 开销占比失控）
        source.est_bps = 50.0 * 1024.0;
        assert_eq!(
            source.prefetch_segment_bytes(),
            (50.0 * 1024.0 * 3.0) as u64
        );
        // 极低带宽（8KB/s）：8KB/s × 3s = 24KB < 下限 → 缩到下限 32KB
        //（再小 HTTP 开销占比失控）
        source.est_bps = 8.0 * 1024.0;
        assert_eq!(
            source.prefetch_segment_bytes(),
            HttpRangeSource::MIN_PREFETCH_SEGMENT_BYTES
        );
    }

    /// 预构造一个已完成/延迟完成的预取段（不经真实 HTTP：sender 由测试
    /// 控制发送时机，可模拟乱序完成）。
    fn pending_from_bytes(range: ByteRange, bytes: Vec<u8>, delay_ms: u64) -> PendingHttpFetch {
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            if delay_ms > 0 {
                thread::sleep(Duration::from_millis(delay_ms));
            }
            let _ = sender.send(Ok(HttpRangeResponse {
                bytes,
                total_length: None,
            }));
        });
        PendingHttpFetch {
            range,
            handle,
            receiver,
            host_epoch: 0,
        }
    }

    /// 每个文件偏移一个确定的字节值——任何拼接错位都会让断言立刻失败。
    fn patterned(range: ByteRange) -> Vec<u8> {
        (0..range.length.unwrap_or(0))
            .map(|i| ((range.start + i) % 251) as u8)
            .collect()
    }

    #[test]
    fn parallel_prefetch_joins_out_of_order_completions_in_offset_order() {
        let mut source = HttpRangeSource::new("https://example.invalid/v.m4s");
        source.content_length = Some(1024 * 1024);
        source.cache_start = 0;
        source.cache_bytes = patterned(ByteRange {
            start: 0,
            length: Some(256 * 1024),
        });
        let seg1 = ByteRange {
            start: 256 * 1024,
            length: Some(256 * 1024),
        };
        let seg2 = ByteRange {
            start: 512 * 1024,
            length: Some(256 * 1024),
        };
        // seg2 先完成、seg1 延迟 200ms —— 并行下载的乱序完成场景
        source.prefetch.push(pending_from_bytes(seg2, patterned(seg2), 0));
        source.prefetch.push(pending_from_bytes(seg1, patterned(seg1), 200));

        // 读 seg1 起点：join seg1（等它完成），必须返回 seg1 的字节
        let out = source
            .read_range(ByteRange {
                start: 256 * 1024,
                length: Some(64 * 1024),
            })
            .unwrap();
        assert_eq!(out, patterned(ByteRange { start: 256 * 1024, length: Some(64 * 1024) }));
        // 窗口连续延伸到 seg1 末尾；队列 = seg2（等待）+ join 后自动回填的
        // seg3（流水线常满：消费一段即补一段）
        assert_eq!(source.cache_end(), 512 * 1024);
        assert_eq!(source.prefetch.len(), 2);

        // 读 seg2：join 后字节正确（绝不允许 seg1/seg2 内容错位）
        let out = source
            .read_range(ByteRange {
                start: 512 * 1024,
                length: Some(64 * 1024),
            })
            .unwrap();
        assert_eq!(out, patterned(ByteRange { start: 512 * 1024, length: Some(64 * 1024) }));
        assert_eq!(source.cache_end(), 768 * 1024);
        // join seg2 后 refill 又回填 [768K,1MB)（该段打 example.invalid 不可达
        // 而 Err，窗口推进到它时走同步兜底——队列非空属正常）
        assert_eq!(source.prefetch.len(), 1);
    }

    #[test]
    fn spawn_prefetch_fills_parallel_segments() {
        let mut source = HttpRangeSource::new("https://example.invalid/v.m4s");
        source.content_length = Some(8 * 1024 * 1024);
        source.cache_start = 0;
        source.cache_bytes = vec![0u8; 256 * 1024];
        source.read_ahead_bytes = 1024 * 1024;
        source.spawn_prefetch_window(256 * 1024, 256 * 1024);
        // 并发上限 2（默认，2026-09-02 由 3 下调——见 `http_prefetch_parallel`）：
        // 从 cache_end 起连续切 2 段
        assert_eq!(source.prefetch.len(), 2);
        for (i, p) in source.prefetch.iter().enumerate() {
            assert_eq!(p.range.start, (256 * 1024 + i * 256 * 1024) as u64);
            assert_eq!(p.range.length, Some(256 * 1024));
        }
    }

    #[test]
    fn prefetch_respects_read_ahead_window_cap() {
        let mut source = HttpRangeSource::new("https://example.invalid/v.m4s");
        source.content_length = Some(8 * 1024 * 1024);
        source.cache_start = 0;
        source.cache_bytes = vec![0u8; 256 * 1024];
        // 窗口上限 = cache_end + 384KB = 640KB：第二段被截到 128KB
        source.read_ahead_bytes = 384 * 1024;
        source.spawn_prefetch_window(256 * 1024, 256 * 1024);
        assert_eq!(source.prefetch.len(), 2);
        assert_eq!(source.prefetch[0].range.length, Some(256 * 1024));
        assert_eq!(source.prefetch[1].range.length, Some(128 * 1024));
        assert_eq!(source.prefetch[1].range_end(), 640 * 1024);
    }

    #[test]
    fn prefetch_join_timeout_widens_when_bandwidth_is_low() {
        let mut source = HttpRangeSource::new("https://example.invalid/video.m4s");
        // 冷启动：5s 原行为
        assert_eq!(source.prefetch_join_timeout(), Duration::from_secs(5));
        // 高带宽：256KB 段 / 1MB/s = 0.25s，×1.5 = 0.375s < 5s → 保持 5s
        source.est_bps = 1024.0 * 1024.0;
        assert_eq!(source.prefetch_join_timeout(), Duration::from_secs(5));
        // 只有带宽估算低于实际（段比预期更慢）时才放宽兜底：
        // 8KB/s：32KB 下限段 / 8KB/s = 4s，×1.5 = 6s → 放宽到 ~6s
        //（避免慢段被 5s 超时误杀后退化同步 fetch）
        source.est_bps = 8.0 * 1024.0;
        let widened = source.prefetch_join_timeout();
        assert!(widened > Duration::from_secs(5));
        assert_eq!(widened.as_secs_f64(), 32.0 * 1024.0 / (8.0 * 1024.0) * 1.5);
    }

    #[test]
    fn prefetch_join_timeout_survives_degenerate_bandwidth_estimates() {
        // 回归（2026-09-01 16:55 闪退）：暂停 10 分钟后 EMA 衰减到 denormal 级
        // est_bps，`seg / est_bps` 得到 ~1e44 秒，`Duration::from_secs_f64`
        // panic（"too big"），且 panic 落在 `extern "C"` AVIO 回调内不可
        // unwind → 进程 fail-fast（0xc0000409）。此处逐一验证退化输入
        // 都回落默认 5s 超时而不是 panic。
        let mut source = HttpRangeSource::new("https://example.invalid/video.m4s");
        // denormal 趋零（EMA 衰减 ~300 个 2s 窗口的产物）
        source.est_bps = 1e-40;
        assert_eq!(source.prefetch_join_timeout(), Duration::from_secs(5));
        // NaN / inf（坏值混入 EMA 的假想情形）
        source.est_bps = f64::NAN;
        assert_eq!(source.prefetch_join_timeout(), Duration::from_secs(5));
        source.est_bps = f64::INFINITY;
        assert_eq!(source.prefetch_join_timeout(), Duration::from_secs(5));
        // 低于 1bps 地板（与 refresh_bandwidth 零供给地板一致）：视为未知
        source.est_bps = 0.001;
        assert_eq!(source.prefetch_join_timeout(), Duration::from_secs(5));
        // 有限但荒谬小的带宽（10bps）：needed ~4915s → 钳到 8s 上界
        // （2026-09-02 K2 收紧：原 30s 上界把 host 死连放大成半分钟 demux
        // 冻结；慢网要更快放弃慢段、走同步小段 fetch，见
        // prefetch_join_timeout 注释）
        source.est_bps = 10.0;
        assert_eq!(source.prefetch_join_timeout(), Duration::from_secs(8));
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
    fn http_source_constructor_preserves_explicit_read_ahead() {
        let source = HttpRangeSource::with_http_headers_and_read_ahead(
            "https://example.invalid/video.mp4",
            Vec::new(),
            Some(16 * 1024 * 1024),
        );

        assert_eq!(source.read_ahead_bytes, 16 * 1024 * 1024);
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
    fn prefetch_spawns_parallel_segments() {
        let body: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_response(1024 * 1024, 128 * 1024 * 1024, &body)),
            MockResponse::immediate(http_206_response(1024 * 1024 + 256 * 1024, 128 * 1024 * 1024, &body)),
            MockResponse::immediate(http_206_response(1024 * 1024 + 512 * 1024, 128 * 1024 * 1024, &body)),
        ]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(128 * 1024 * 1024);
        source.read_ahead_bytes = 12 * 1024 * 1024;
        source.cache_start = 0;
        source.cache_bytes = vec![0u8; 1024 * 1024];
        // 请求末端 == 缓存末尾（remaining=0）→ 立即触发顺序预取
        let range = ByteRange {
            start: 0,
            length: Some(1024 * 1024),
        };
        source.maybe_start_prefetch(range);
        // 并行预取（默认 2 段在飞）：从 cache_end 起按偏移连续切 2 段，
        // 段间紧邻；join 按覆盖关系保序，乱序完成不产生拼接错位
        //（错位回归由 parallel_prefetch_joins_out_of_order 测试盯防）。
        assert_eq!(source.prefetch.len(), 2);
        for (i, p) in source.prefetch.iter().enumerate() {
            assert_eq!(p.range.start, source.cache_end() + (i * 256 * 1024) as u64);
            assert_eq!(
                p.range.length,
                Some(HttpRangeSource::SEEK_PREFETCH_SEGMENT_BYTES)
            );
        }
        // 后台请求确已发出
        let _ = recv_request_head(&requests);
    }

    #[test]
    fn reset_cache_keeps_segments_beyond_new_window() {
        // 窗口重设（如回退读触发窗口收缩）时，起点在新窗口末尾之后的在飞
        // 预取段是未来数据，reset_cache 必须保留（无脑清空会浪费在飞下载）。
        let body: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(1024 * 1024, 128 * 1024 * 1024, &body),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(128 * 1024 * 1024);
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
        assert_eq!(source.prefetch.len(), 2);
        // 模拟回退读后窗口收缩：新窗口 [512KB, 768KB)，在飞段（1MB 起）
        // 在窗口末尾之外 → 必须保留（全部 2 段）
        source.reset_cache(512 * 1024, vec![0u8; 256 * 1024]);
        assert_eq!(source.prefetch.len(), 2);
        assert_eq!(source.prefetch[0].range.start, 1024 * 1024);
        for p in &source.prefetch {
            assert!(p.range.start >= source.cache_end());
        }
        let _ = recv_request_head(&requests);
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
        source.prefetch = vec![PendingHttpFetch::spawn(
            uri,
            Vec::new(),
            range,
            Arc::new(AtomicU64::new(0)),
            0,
        )];
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
        source.prefetch = vec![PendingHttpFetch::spawn(
            uri,
            Vec::new(),
            range,
            Arc::new(AtomicU64::new(0)),
            0,
        )];
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

    // ============ 多 host 无缝换源（2026-09-02 无感化改造 K1~K4） ============

    #[test]
    fn join_timeout_is_bounded_regardless_of_bandwidth() {
        let (uri, _requests) = spawn_mock_http_server(vec![]);
        let mut source = HttpRangeSource::new(uri);
        // 冷启动（est_bps 未知）→ 基础 5s
        assert_eq!(
            source.prefetch_join_timeout(),
            HttpRangeSource::PREFETCH_JOIN_TIMEOUT
        );
        // 极慢链路（原实现会随 est_bps 反向放大到 ~24-30s）→ 钳在 8s 上界
        source.est_bps = 2.0 * 1024.0;
        let slow = source.prefetch_join_timeout();
        assert!(slow >= HttpRangeSource::PREFETCH_JOIN_TIMEOUT);
        assert!(slow <= HttpRangeSource::PREFETCH_JOIN_TIMEOUT_MAX);
        // 正常带宽 → 同样在界内
        source.est_bps = 1024.0 * 1024.0;
        let fast = source.prefetch_join_timeout();
        assert!(fast >= HttpRangeSource::PREFETCH_JOIN_TIMEOUT);
        assert!(fast <= HttpRangeSource::PREFETCH_JOIN_TIMEOUT_MAX);
    }

    #[test]
    fn host_switch_abandons_inflight_segments_and_respawns_on_new_host() {
        // A：响应极慢（模拟死连，在飞段挂在它上面）；B：立即响应。
        // mock 服务器串行 accept：A 的延迟响应会阻塞后续连接上报，
        // 因此 A 只放一个响应、只消费一个请求头（第二个在飞段照样连接 A，
        // 只是其头部在延迟期内不会出现在通道里）。
        let (uri_a, requests_a) = spawn_mock_http_server(vec![MockResponse::delayed(
            Duration::from_secs(60),
            http_206_response(0, 1024 * 1024, &vec![0u8; 256 * 1024]),
        )]);
        let (uri_b, requests_b) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_response(
                0,
                1024 * 1024,
                &vec![0u8; 256 * 1024],
            )),
            MockResponse::immediate(http_206_response(
                0,
                1024 * 1024,
                &vec![0u8; 256 * 1024],
            )),
        ]);
        let mut source =
            HttpRangeSource::with_http_headers_and_diag_slot(uri_a.clone(), Vec::new(), 0);
        source.content_length = Some(1024 * 1024);
        source.set_candidates(vec![(uri_a.clone(), 1.0), (uri_b.clone(), 0.5)]);
        assert_eq!(source.active_idx, 0);
        // 在 A 上铺在飞预取段（并行度 2，线程真实连接 A，60s 内不会完成）
        source.spawn_prefetch_window(0, 256 * 1024);
        assert_eq!(source.prefetch.len(), 2);
        let _ = recv_request_head(&requests_a);
        // 模拟死连：久无字节但 demux 仍在读
        source.last_byte_ms = now_ms().saturating_sub(10_000);
        source.last_read_call_ms = now_ms();
        source.maybe_switch_host();
        assert_eq!(source.active_idx, 1);
        assert_eq!(source.host_epoch, 1);
        // 旧段被作废、按新 host 重发：B 收到请求，且新段携带新 epoch
        assert!(!source.prefetch.is_empty());
        assert!(source.prefetch.iter().all(|p| p.host_epoch == 1));
        // 重发是并行的两段，B 收到两条请求的先后顺序不确定（原断言假定起点段
        // 先到，实测约 1/3 概率失败 → flaky）。断言改为"窗口起点必须出现在这
        // 两条请求之一"，语义不变、不依赖到达顺序。
        let mut heads_b = vec![recv_request_head(&requests_b)];
        if let Ok(second) = requests_b.recv_timeout(Duration::from_millis(500)) {
            heads_b.push(second.to_lowercase());
        }
        assert!(
            heads_b.iter().any(|head| head.contains("range: bytes=0-")),
            "respawn must request the window from its start: {heads_b:?}"
        );
        // A 不再收到任何新请求（被作废的段不会重试）
        assert!(requests_a.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn take_prefetch_abandons_segment_after_host_switch() {
        // 段落在旧 host（epoch 0），源已切到新代（epoch 1）→ 等待首个分片
        //（250ms）后立即作废返回 None，read_range 将走同步 fetch 打新 host。
        let (uri_slow, _requests) = spawn_mock_http_server(vec![MockResponse::delayed(
            Duration::from_secs(60),
            http_206_response(0, 65536, &vec![0u8; 65536]),
        )]);
        let mut source = HttpRangeSource::new("http://new-host.invalid/video.mkv");
        source.host_epoch = 1;
        let range = ByteRange {
            start: 0,
            length: Some(65536),
        };
        source.prefetch.push(PendingHttpFetch::spawn(
            uri_slow,
            Vec::new(),
            range,
            Arc::new(AtomicU64::new(0)),
            0, // spawn 于旧 epoch
        ));
        let started = Instant::now();
        let result = source.take_prefetch(range);
        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn request_host_switch_forces_switch_and_respects_cooldown() {
        let (uri_a, _requests_a) = spawn_mock_http_server(vec![]);
        let (uri_b, _requests_b) = spawn_mock_http_server(vec![]);
        // 用槽位 1（音频槽），避免与其它并行测试对槽 0 的全局请求互相干扰
        let mut source =
            HttpRangeSource::with_http_headers_and_diag_slot(uri_a.clone(), Vec::new(), 1);
        source.set_candidates(vec![(uri_a.clone(), 1.0), (uri_b.clone(), 0.5)]);
        assert!(request_host_switch(1));
        source.maybe_switch_host();
        assert_eq!(source.active_idx, 1);
        // 强制冷却（2s）内不切
        source.maybe_switch_host();
        assert_eq!(source.active_idx, 1);
        // 冷却过期后，非强制且非死连/慢速 → 仍不切
        source.switch_cooldown_until = Instant::now();
        source.last_byte_ms = now_ms();
        source.maybe_switch_host();
        assert_eq!(source.active_idx, 1);
    }

    #[test]
    fn full_preload_in_flight_is_zero_while_demux_active() {
        let state = FullPreloadState::new();
        // 尚无 demux 读（last=0 → 不活跃）→ 桶在飞记 1
        assert_eq!(state.in_flight_segments(), 1);
        state.mark_demux_read();
        // demux 活跃（播放中）→ 桶让路，无在飞
        assert_eq!(state.in_flight_segments(), 0);
    }

    /// 落盘桶测试用的独立目录（按名字分，跑完清掉）。不引 `tempfile`：仓库
    /// 没有这个依赖，为几条测试拉一个新 crate 不值当。
    fn preload_test_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "erika-preload-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn preload_disk_bucket_round_trips_and_shifts_offsets() {
        let dir = preload_test_dir("round-trip");
        let mut store = DiskStore::open(&dir, 0xabc, 64 * 1024 * 1024).unwrap();
        store.append(&[1u8; 256 * 1024]).unwrap();
        store.append(&[2u8; 256 * 1024]).unwrap();
        assert_eq!(store.len(), 512 * 1024);
        // 尾部窗口内（第二段）纯内存命中，窗口外（第一段）走文件读：同一份
        // 绝对偏移从两条路径读出来必须完全一致，否则 demux 会拿到错位字节。
        assert_eq!(store.read(0, 8).unwrap(), vec![1u8; 8]);
        assert_eq!(store.read(256 * 1024, 8).unwrap(), vec![2u8; 8]);
        assert_eq!(store.read(512 * 1024 - 4, 4).unwrap(), vec![2u8; 4]);

        // 前移：窗口清空、起点换到 1MB，且文件里**不能**留下 [512KB, 1MB) 的
        // 空洞——非稀疏文件会照实占盘，长片反复前移能把盘吃光。
        store.reset_at(1024 * 1024).unwrap();
        assert_eq!(store.len(), 0);
        assert_eq!(store.data_start, 1024 * 1024);
        assert_eq!(
            std::fs::metadata(&store.file.path).unwrap().len(),
            DISK_HEADER_BYTES
        );
        store.append(&[3u8; 64]).unwrap();
        assert_eq!(store.read(0, 64).unwrap(), vec![3u8; 64]);
        // 越界读必须报错（调用方回退 demux 路径），绝不能返回错位的字节。
        assert!(store.read(64, 1).is_err());

        let path = store.file.path.clone();
        drop(store);
        // Drop 负责删文件：桶是"看完就丢"的缓存，不该把用户盘当垃圾场。
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 造一份"上次异常退出留下的桶文件"：合法头部 + 数据区。
    fn write_preload_fixture(path: &Path, data_start: u64, data: &[u8]) {
        let mut header = [0u8; DISK_HEADER_BYTES as usize];
        header[0..8].copy_from_slice(&DISK_MAGIC);
        header[8..12].copy_from_slice(&DISK_LAYOUT_VERSION.to_le_bytes());
        header[16..24].copy_from_slice(&data_start.to_le_bytes());
        header[24..32].copy_from_slice(&(data.len() as u64).to_le_bytes());
        let mut file = File::create(path).unwrap();
        file.write_all(&header).unwrap();
        file.write_all(data).unwrap();
    }

    #[test]
    fn preload_disk_bucket_recovers_leftover_file() {
        let dir = preload_test_dir("recover");
        // 上次异常退出（Drop 没跑到）留下的桶：合法头部 + 数据区。
        let path = dir.join(format!("{:016x}.part", 0x1234));
        write_preload_fixture(&path, 0, &[9u8; 4096]);
        let mut store = DiskStore::open(&dir, 0x1234, 64 * 1024 * 1024).unwrap();
        assert_eq!(store.len(), 4096, "残留文件的窗口必须被恢复（弱网重开省流量）");
        assert_eq!(store.data_start, 0);
        assert_eq!(store.read(0, 4).unwrap(), vec![9u8; 4]);

        // 头部声明超过文件实际长度（写完数据、还没更新头部就被杀）→ 只认文件
        // 里真的躺着的那部分：宁可少复用，也不能复用出错位数据。
        let truncated = dir.join(format!("{:016x}.part", 0x4321));
        {
            let mut header = [0u8; DISK_HEADER_BYTES as usize];
            header[0..8].copy_from_slice(&DISK_MAGIC);
            header[8..12].copy_from_slice(&DISK_LAYOUT_VERSION.to_le_bytes());
            header[24..32].copy_from_slice(&(1024u64 * 1024).to_le_bytes());
            let mut file = File::create(&truncated).unwrap();
            file.write_all(&header).unwrap();
            file.write_all(&[5u8; 128]).unwrap();
        }
        let store = DiskStore::open(&dir, 0x4321, 64 * 1024 * 1024).unwrap();
        assert_eq!(store.len(), 128);

        // 不是本模块写的文件（版本不符 / 垃圾）→ 整个丢掉重建，绝不猜。
        let junk = dir.join(format!("{:016x}.part", 0x77));
        std::fs::write(&junk, b"not a preload bucket at all, just noise").unwrap();
        let store = DiskStore::open(&dir, 0x77, 64 * 1024 * 1024).unwrap();
        assert_eq!(store.len(), 0);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preload_state_adopts_reused_window_start() {
        let dir = preload_test_dir("adopt-start");
        let uri = "https://example.invalid/upgcxcode/1/2/3/4000000000002-1-30077.m4s?deadline=1";
        // 造一份"上次前移过"的残留桶：数据区对应逻辑 [512KB, 512KB + 4096)。
        let path = dir.join(format!("{:016x}.part", preload_key(uri, 0)));
        write_preload_fixture(&path, 512 * 1024, &[4u8; 4096]);

        let state = FullPreloadState::open(uri, 0, 64 * 1024 * 1024, &dir);
        assert_eq!(
            state.covered_until(),
            512 * 1024 + 4096,
            "复用窗口必须计入已覆盖前沿（否则桶会把盘上已有的字节重下一遍）"
        );
        assert_eq!(state.inner.lock().unwrap().start, 512 * 1024);
        // 总长要等第一个响应回填；那边之前桶只用于读，未探测到 total 时不可命中。
        state.inner.lock().unwrap().total = Some(2 * 1024 * 1024);
        assert_eq!(
            state.copy_range(512 * 1024, 4096).unwrap(),
            vec![4u8; 4096],
            "复用的字节必须按它自己的绝对偏移读出来，错开一个 data_start 就是错位数据"
        );
        assert!(!state.covers(0, 16), "窗口起点之前的地址不在桶覆盖范围内");
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preload_disk_bucket_never_touches_a_locked_bucket() {
        let dir = preload_test_dir("locked");
        let path = dir.join(format!("{:016x}.part", 0x888));
        let mut held = DiskStore::open(&dir, 0x888, 64 * 1024 * 1024).unwrap();
        held.append(&[1u8; 64]).unwrap();

        // 同一摘要的第二次打开：拿不到独占锁，必须换名字新建。**绝不能**把
        // 第一个会话正在写的文件删掉——那是别人的播放数据。
        let second = DiskStore::open(&dir, 0x888, 64 * 1024 * 1024).unwrap();
        assert_ne!(second.file.path, held.file.path);
        assert_eq!(second.len(), 0);
        assert_eq!(held.len(), 64);
        assert!(path.exists(), "正在使用的桶文件不能被动到");
        drop(second);
        assert!(path.exists(), "关掉第二个会话也不能带走第一个的文件");
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preload_write_failure_degrades_to_memory_bucket() {
        let dir = preload_test_dir("degrade");
        let path = dir.join("readonly.part");
        std::fs::write(&path, [0u8; DISK_HEADER_BYTES as usize]).unwrap();
        // 用**只读**句柄构造落盘后端：之后任何 append 都必然拿到 IO 错误，
        // 与磁盘满 / 权限不足 / 杀软锁同一类，且不依赖测试机的磁盘状态。
        let disk = DiskStore {
            file: PreloadFile {
                file: Some(File::open(&path).unwrap()),
                path: path.clone(),
            },
            data_start: 0,
            data_len: 0,
            tail: Vec::new(),
            tail_offset: 0,
        };
        let mut inner = FullPreloadInner {
            total: None,
            start: 0,
            store: PreloadStore::Disk(disk),
        };
        let error = inner.append_chunk(b"first-chunk").unwrap_err();
        assert!(!error.is_empty());
        // 硬要求：落盘写失败只能降级成内存桶，绝不能把播放变成"失败"。
        assert!(
            matches!(inner.store, PreloadStore::Memory(_)),
            "写失败必须退回内存桶"
        );
        // 降级后窗口仍然连续：这批字节必须完整落进内存桶，一个都不能丢。
        assert_eq!(inner.store.len(), 11);
        assert_eq!(inner.store.read(0, 11).unwrap(), b"first-chunk".to_vec());
        // 后续追加照常（内存桶不会再失败），桶继续往前长。
        assert!(inner.append_chunk(b"-more").is_ok());
        assert_eq!(inner.store.read(11, 5).unwrap(), b"-more".to_vec());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preload_sweep_keeps_fresh_and_unrelated_files() {
        let dir = preload_test_dir("sweep");
        let live = dir.join(format!("{:016x}.part", 0xaaa));
        write_preload_fixture(&live, 0, &[1u8; 64]);
        std::fs::write(dir.join("keep-me.txt"), b"not ours").unwrap();
        let held = DiskStore::open(&dir, 0xaaa, 64 * 1024 * 1024).unwrap();
        assert_eq!(held.len(), 64);

        // 清理是"卫生工作"，最要紧的不是删得干净，而是**绝不误伤**：正在用的
        // 桶（拿不到独占锁）和非本模块的文件都必须原封不动。
        sweep_stale_preload(&dir);
        assert!(live.exists(), "正在使用的桶不能被清理");
        assert!(
            dir.join("keep-me.txt").exists(),
            "`.part` 之外的任何文件都不能碰"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preload_key_ignores_one_time_url_signature() {
        // 同一条流的两次播放签名不同（B 站 URL 的 deadline/upsig 每次都变），
        // 摘要必须一致——否则"重开同一集"永远命不中自己上次的文件。
        let first = "https://cn-gotcha.bilivideo.com/upgcxcode/1/2/3/4000000000000-1-30280.m4s?deadline=111&upsig=aaa";
        let second = "https://cn-gotcha.bilivideo.com/upgcxcode/1/2/3/4000000000000-1-30280.m4s?deadline=999&upsig=zzz";
        assert_eq!(preload_key(first, 1), preload_key(second, 1));
        // 不同流绝不能撞名——撞名就是两个会话共写一个文件，字节必然错位。
        assert_ne!(preload_key(first, 1), preload_key(second, 0));
        let other = "https://cn-gotcha.bilivideo.com/upgcxcode/1/2/3/4000000000001-1-30077.m4s?deadline=1";
        assert_ne!(preload_key(first, 1), preload_key(other, 1));
    }

    #[test]
    fn full_preload_step_keeps_disk_bucket_offsets() {
        // 与 `full_preload_skips_ranges_already_covered_by_demux` 同构，但后端
        // 换成文件：下载 + 按**绝对**偏移读回必须与内存桶完全一致（落盘是纯
        // 存储替换，对上层不可见）。
        let dir = preload_test_dir("step");
        let body = vec![7u8; 256 * 1024];
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(512 * 1024, 1024 * 1024, &body),
        )]);
        let disk = DiskStore::open(&dir, 0x99, 1024 * 1024).unwrap();
        let state = FullPreloadState::new();
        *state.inner.lock().unwrap() = FullPreloadInner {
            total: Some(1024 * 1024),
            // 与 `set_floor` 一致：模拟 demux 已跑赢、桶刚刚前移过
            start: 512 * 1024,
            store: PreloadStore::Disk(disk),
        };
        state.end.store(512 * 1024, Ordering::Relaxed);

        assert!(!full_preload_step(
            &state,
            &http_agent(),
            &uri,
            &[],
            1024 * 1024
        ));
        let head = recv_request_head(&requests);
        assert!(
            head.contains("range: bytes=524288-"),
            "应从桶当前前沿 512KB 起下载，实际请求头: {head}"
        );
        assert_eq!(state.covered_until(), 512 * 1024 + 256 * 1024);
        assert_eq!(
            state.copy_range(512 * 1024, 256 * 1024).unwrap(),
            body,
            "落盘桶按绝对偏移读回的字节必须与刚下载的一致"
        );
        // 桶起点之前的地址不在窗口内，绝不能"命中"（前移丢掉的字节没有第二份）。
        assert!(!state.covers(0, 1024));
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
