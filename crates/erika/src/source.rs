#[cfg(target_os = "android")]
use std::collections::HashMap;
use std::collections::VecDeque;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(target_os = "android")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use thiserror::Error;

mod http;

use http::BodyReader;
pub use http::SourceCancellation;

use crate::core::MediaSourceHint;
use crate::trace;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("io error: {0}")]
    Io(String),
    #[error("http error: {0}")]
    Http(String),
    /// **暂时性**读取失败：传输层断连/超时/body 读错误/5xx —— 源可能马上又能用。
    ///
    /// 与 [`SourceError::Http`] 的区别就是"该不该等"：调用方（
    /// [`HttpRangeSource::read_range_with_transient_wait`]）会吸收本变体，
    /// 退避重试并等待字节回来（mpv `paused-for-cache` 语义）；
    /// 而 `Http` 是**终局**（4xx/签名过期、Range 被忽略、Content-Range 违约、
    /// 帧内数据错位）——那些重试多少次都不会好，必须立刻让宿主换源。
    ///
    /// 分类由 `fetch_http_range` 产生：重试耗尽时按 `http_error_is_retryable`
    /// （传输错误与 5xx = 可重试 → 本变体；4xx = 不可重试 → `Http`）判定。
    #[error("transient http error: {0}")]
    Transient(String),
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
    /// 正在下载的**段/连接数**（in-flight）。
    ///
    /// 语义分两条通路（2026-09-03）：
    ///   - 顺序长读（生产默认）：**0 或 1**——只有一条持续读的长连接，
    ///     0 可能是"正在背压等待"（缓冲已满）或"读完收工"；
    ///   - 旧分段路径：并行预取段数 + 整片桶（历史上稳态 5~6）。
    /// 宿主判"源是否空闲"必须结合 `demux_cache_end`（0 且窗口为空才是真空闲）：
    /// 长读路径下 0 只表示"此刻没在读 socket"，不代表没数据。
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
    /// **本片是否已完整缓冲到服务端声明的文件末尾**（"不需要再下载了"）。
    ///
    /// 判据：长读窗口的网络填充前沿 `write_pos` 已经到达服务端声明的总长
    /// `total`（首个 206 的 `Content-Range: bytes x-y/total` 回填，或 HEAD）。
    /// 旧分段路径下退化为 `cache_end >= content_length`。
    ///
    /// **为什么必须独立成字段**（真机事故 2026-09-15）：整片下载完之后
    /// `write_pos` 不再前进 → 「距上次收到字节」的时间戳一路涨到几百秒，
    /// 与"链路死了"在**任何只看到达字节的指标上完全同形**。宿主因此打出
    /// `vstall=596s` / `采用供给0.00Mbps`，读起来像"没网速"，实际是
    /// "已经下完了"。本字段是唯一能把这两种现实分开的**正面事实**：
    /// 它为真时，`stalled_sec` 大、`est_bps` 为 0 都是**预期**，不是故障。
    ///
    /// ⚠️ **不要**用 [`Self::cache_end`]（`max(demux, 整片预载桶)`）判：桶的
    /// `covered_until` 只增不减，且从 0 顺序预载，seek 后仍停在旧前沿。这里
    /// 用**长读窗口自己的** `write_pos`——它就是"这条流真的读到哪了"。
    pub whole_file_cached: bool,
    /// 源 host 代数（每次实际切换 +1）。宿主据此确认 request_host_switch 已生效。
    pub host_epoch: u64,
    /// 该槽的读路径此刻是否正处在「暂时读不到数据、退避重试中」的等待窗口
    /// （见 [`HttpRangeSource::read_range_with_transient_wait`]）。
    ///
    /// 语义与 `prefetch_in_flight` **不同**：后者只说明"有段在飞"，段挂死时同样
    /// 为真；本字段只在"读请求已经失败、源正在等待字节回来"时为真。
    /// 内核 worker 读它对外发 `BufferingChanged`，宿主据此显示"缓冲中"而不是
    /// 把这段静止当成"源已死"——这是 mpv `paused-for-cache` 的对等语义：
    /// **等数据，不判整条流失败**。
    ///
    /// 置位时机：等待窗口打开；清除时机：读恢复、等待窗口判定为真死、或源析构。
    pub transient_wait: bool,
    /// **累计已发出的 HTTP Range 请求数**（进程级单调，**含重试与断点续传**）。
    ///
    /// 口径：每一次真正打到网络的 GET 记 1。计数点在**唯一发请求的那一处**
    /// ——[`fetch_http_range`] 的重试循环里、`request.call()` 之前；因此一次
    /// 逻辑 fetch 因失败重试或断点续传发出的**每一个**请求都各记 1（三次
    /// 尝试 = +3）。**不在调用点计数**：调用点记数会把重试合并成 1 次，
    /// "请求放大"这一现象（重试/续传把请求数推高、每请求平均字节被拖低）
    /// 就看不出来了，而这恰恰是判定"是不是被 CDN 限速"的关键证据。
    ///
    /// **进程级单调、永不清零**：故意不在源构造/析构时复位。宿主同进程里
    /// 存在短命的"探针源"（`probe_http_total_length` 只为探长度而建），把
    /// 计数挂在源实例上、或构造时清零，探针一构造就会把正在播放那条流的
    /// 计数洗掉，宿主两次轮询的差分随即失真。单调总量 + 宿主差分是唯一稳的
    /// 口径（与 `downloaded_bytes` 的差分同一套做法）。换源/重开播都不复位：
    /// 宿主读到的（差分得到的）是"当前这一刻本槽的请求速率"，与源实例无关。
    pub http_requests: u64,
    /// 与 [`Self::http_requests`] **同口径**的累计已收字节（进程级单调）。
    ///
    /// 用途：算"平均每个请求收回多少字节"（`http_request_bytes /
    /// http_requests`），即"请求画像"的均值一侧。**不能拿 `downloaded_bytes`
    /// 去除**：后者是**源级**计数（`Drop` 时注销、换源即从 0 起），与进程级
    /// 单调的 `http_requests` 生命周期不同，换过源的会话里这个商会把分母
    /// 留在历史值上 → 得到远小于真值的伪均值。两个进程级单调量相除才自洽。
    ///
    /// 口径含失败请求：连接失败/5xx 的请求只进分母（收到 0 字节），均值因此
    /// 天然反映"重试浪费"——正常顺序预取时它应接近**段大小**（段上限
    /// 256KB）；显著偏低说明请求在空转（在飞段没填满 / 段被反复作废）。
    pub http_request_bytes: u64,
    // ---- 滑动窗口分层缓存（2026-09-04）----
    /// **内存层（热区）常驻字节数**：顺序长读窗口里实际躺着的字节
    /// （前向 read-ahead + 后向回看）。单位：字节。上限见 `memory_budget_bytes`。
    pub cache_memory_bytes: u64,
    /// **磁盘层（冷区）当前字节数**：热区淘汰下来、按段落盘的区间总量
    /// （含仍在内存缓冲写、尚未落盘的尾部）。单位：字节；上限由冷区预算
    /// （`ERIKA_HTTP_COLD_BUDGET_BYTES`，默认 256MiB）钉住，超限丢最久远的段。
    pub cache_disk_bytes: u64,
    /// **冷区（磁盘层）命中次数**（累计单调，含跨冷/热边界的前缀命中）。
    ///
    /// 口径：每一次"因为冷区里有字节而**没有**发网络请求"的读记 1。它是
    /// "回拖/重播不重下"的现场证据：用户在已缓冲区间里拖动时，这个数在涨，
    /// 而 `http_requests` 不涨。
    pub cold_hits: u64,
    /// 冷区命中字节数（累计单调，与 `cold_hits` 同口径配对）。
    pub cold_hit_bytes: u64,
    /// **内存层硬上限**（字节预算）：`前后向缓存字节数之和`永不超过它。
    /// 默认视频/单流 24MiB、音频 6MiB（`ERIKA_HTTP_WINDOW_MEMORY_BYTES` 覆盖），
    /// 且永不低于宿主配置的 read-ahead + back（不许比改造前更小）。
    pub memory_budget_bytes: u64,
    /// **前向预算**（字节）：当前"往前读到哪"的目标——由时间预算
    /// `N`（秒）× 实测码率算出，弱网下 N 变大（180s 上限），因此这个值也变大。
    /// 达到它读线程就停在那里（背压），消费推进后再往前补。
    pub forward_budget_bytes: u64,
    /// 当前**前向可播秒数** = 前向缓冲字节 ÷ 实测码率（无码率估计时为 0）。
    /// 这是"弱网更激进"最直观的验收数字：吞吐越低，它应当越大。
    pub forward_playable_secs: f64,
    /// **窗口未满期间实测到的链路吞吐**（字节/秒，500ms 窗口 + 0.3/0.7 EMA）。
    ///
    /// 口径与为什么可信：只有窗口**没满**（读线程真的在读 socket）的时间片才
    /// 采样；窗口满（背压）时不采样、不衰减，所以本值反映"链路能推多快"，
    /// 而**不会**被"客户端故意不读"钳成消费速率。宿主判"能不能升画质/换源"
    /// 要的是这个口径。读数路径：顺序长读线程直接写全局槽，宿主 500ms poll
    /// 直接读，**不经过 demux 线程**——demux 阻塞在别处也照样新鲜。
    /// 0.0 = 尚未测得（如全程都在背压/还未开始读）。
    ///
    /// **它同时是滑动窗口预算里"实测吞吐"的唯一来源**（内核 `supply_estimate_bps`）：
    /// 预算与宿主读的是同一个数，不存在两套互相打架的网速判断。
    pub fill_throughput_bps: f64,
    /// 顺序长读的**连接重建总次数**（= 除首个请求外多出来的请求数）。
    ///
    /// 恒等式：**某条流的请求数 = 1 + `stream_reconnects_total`**（首个请求不算
    /// 重建）。宿主可用它把自己的 `http_requests` 差分拆开。
    ///
    /// 现场判据：**正常顺序播放期间它应当为 0**（一条连接读到底）。它不是 0 时，
    /// 用下面四个分桶字段看是谁的锅——用户的真机实测正是靠这几个数分清
    /// "CDN 提前收线"与"我们的 reset/换源"。
    pub stream_reconnects_total: u64,
    /// 分桶：**服务端提前收线 / body 传输错误**（本次连接收到过字节）。CDN 行为。
    ///
    /// 用户真机实测的形态：CDN 声明了完整 `Content-Range` 却在约 1MB 处收线，
    /// 内核静默续传——每 ~1MB 一个请求，稳定 ~0.5 次/秒。这**不是**内核的 bug，
    /// 但也无法消除：请求数下界 = 实际下载字节 ÷ CDN 每次连接愿给的字节。
    pub stream_reconnects_early_close: u64,
    /// 分桶：连接建立后**一个字节都没收到**（限流/瞬时错误，退避重试）。
    pub stream_reconnects_empty: u64,
    /// 分桶：本地 `reset()`（seek 出窗 / 回拖超出后向窗口 / 乱序读）。
    /// 顺序播放期间应当为 0；它涨说明 demux 在窗口外跳读。
    pub stream_reconnects_reset: u64,
    /// 分桶：换 host（`host_epoch` 变化）导致的重建。
    pub stream_reconnects_host_switch: u64,
    /// **分片式 CDN 的正常续传次数**：服务端用有界 `Content-Range`
    /// （`end + 1 < total`）明确表示"这次只给这一段"，内核在分片边界续传。
    /// 计入 `stream_reconnects_total`，但语义是"按服务端节奏走"，不是故障。
    pub stream_chunk_resumes: u64,
    /// **判决实验（假说 D）**：请求起点与"上一次连接结束时的前沿"一致的次数
    /// ——即**断点续传**。单调递增的请求序列会全部落进这个桶。
    pub stream_range_contiguous: u64,
    /// 判决实验：起点**大于**前沿的次数（前向跳读）。
    pub stream_range_forward_jumps: u64,
    /// 判决实验：起点**小于**前沿的次数（**回拖 = demux 在 seek**）。
    /// 它明显非零时，请求放大的根因是 demux 的读模式，**与 CDN 无关**。
    pub stream_range_backward_jumps: u64,
    /// 最近一次 Range 请求的起点偏移（字节）。
    pub stream_range_last_start: u64,
    /// 最近一条连接实际收到的字节数。"每条连接给多少字节"的分布是判
    /// "CDN 有固定配额 / 还是停读导致"（调研疑问 1）的一手数据。
    pub stream_last_connection_bytes: u64,
    /// 所有连接里收到字节最多的那一条（看有没有长连接真跑出过大块）。
    pub stream_max_connection_bytes: u64,
    /// **最近一次换源的可读原因**（如
    /// `upos-a.bilivideo.com → upos-b.bilivideo.com：starving(前向0.8s<2.0s 持续3.2s)`）。
    /// 空串 = 本轮还没换过。宿主直接把它显示在诊断面板上：用户看到"换源 N 次"
    /// 时必须能同时看到"为什么换"（"看得见才算数"）。
    pub last_host_switch_reason: String,
    // ---- seek 出窗的**跨节点并行回填**（2026-09-15；形态约束见文件上方整段）----
    /// **累计跨节点并行回填批次数**：每次"seek 到未缓冲位置"尝试并行 +1。
    ///
    /// **正常顺播期间恒不涨**——这就是把三种形态分开的那个数：
    ///   - 纯顺播（一条 open-ended Range）：`seek_fill_batches` 不动；
    ///   - seek 走了跨节点 N 段：`seek_fill_batches` 与 `seek_fill_connections` 同涨；
    ///   - **seek 退化为单连接**（门禁否决/分段失败）：`seek_fill_batches` 涨、
    ///     `seek_fill_gate_denied` 或 `seek_fill_fallbacks` 涨、而
    ///     `seek_fill_connections` 几乎不涨。
    /// 有了这三个数，`http_requests` 上涨才不会被误读成"退化成 downloader"。
    pub seek_fill_batches: u64,
    /// 累计建立的并行分段连接数（每个候选节点一条；含只做了下载前预检的段）。
    pub seek_fill_connections: u64,
    /// 累计**被采用**的跨节点并行字节数（回退时被丢弃的段不计）。
    pub seek_fill_bytes: u64,
    /// **下载前门禁否决**次数（ETag/文件总长缺失或不一致 → 退回单节点）。
    /// 它涨说明"这次没并行是因为证据不足"，**不是并行没生效**。
    pub seek_fill_gate_denied: u64,
    /// 回退到单连接的批次次数（任一段失败/超时/比对不过/期间又 seek 了）。
    pub seek_fill_fallbacks: u64,
    /// 边界重叠逐字节比对**通过**的批次数（跨节点数据一致性的直接证据）。
    pub seek_fill_overlap_ok: u64,
    /// 边界重叠逐字节比对**失败**的批次数。`> 0` 时请立刻看日志那一行：
    /// 该批已被整批丢弃并回退，**没有任何可疑字节进过播放流**。
    pub seek_fill_overlap_mismatch: u64,
    /// 最近一次批次的段数（0 = 本次会话还没走过跨节点并行）。
    pub seek_fill_last_segments: u64,
    /// **当前在飞的分段连接数**（0 = 没有并行批次在跑）。用户要的"用了几条连接"
    /// 当场可读；它同时是硬上限 [`SEEK_PARALLEL_MAX_CONNECTIONS`] 的执行依据
    /// （超过上限的新批次会被整批放弃，见 `run_seek_fill`）。
    pub seek_fill_in_flight: u64,
    /// 最近一次批次被采用的字节数。
    pub seek_fill_last_bytes: u64,
    /// 最近一次批次墙钟（毫秒）。
    pub seek_fill_last_ms: u64,
    /// 最近一次批次"同一批数据用单连接拉"的预计墙钟（毫秒）。实测 ÷ 它
    /// 就是**可复核的提速幅度**（日志那一行同时给出两个数）。
    pub seek_fill_last_expected_ms: u64,
    /// 最近一次批次是否最终回退（true = 这次 seek 走了单连接串行）。
    pub seek_fill_last_fallback: bool,
    /// 最近一次**下载前门禁裁定的可读理由**（内核原文，宿主原样展示：
    /// 用户看到"没加速"时必须能同时看到"为什么"）。
    pub seek_fill_last_gate_reason: String,
    /// 自适应关闭：连续实测到"并行没有叠加"（例如 CDN 按出口 IP 限速）已停用
    /// 本会话的并行。此时 seek 仍走单连接——**不是功能坏了，是测出来没用**。
    pub seek_fill_disabled: bool,
}

/// 诊断槽数。DASH 双流（video + audio）各占一槽独立发布/清空，互不覆盖；
/// 单流/直播只用槽 0。见 `HttpRangeSource::with_http_headers_and_diag_slot`。
/// 内核 worker 遍历全部槽判定"源是否在等字节"（`core::source_transient_wait_active`）。
pub(crate) const SOURCE_DIAG_SLOTS: usize = 2;

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
        whole_file_cached: false,
        transient_wait: false,
        http_requests: 0,
        http_request_bytes: 0,
        cache_memory_bytes: 0,
        cache_disk_bytes: 0,
        cold_hits: 0,
        cold_hit_bytes: 0,
        memory_budget_bytes: 0,
        forward_budget_bytes: 0,
        forward_playable_secs: 0.0,
        fill_throughput_bps: 0.0,
        stream_reconnects_total: 0,
        stream_reconnects_early_close: 0,
        stream_reconnects_empty: 0,
        stream_reconnects_reset: 0,
        stream_reconnects_host_switch: 0,
        stream_chunk_resumes: 0,
        stream_range_contiguous: 0,
        stream_range_forward_jumps: 0,
        stream_range_backward_jumps: 0,
        stream_range_last_start: 0,
        stream_last_connection_bytes: 0,
        stream_max_connection_bytes: 0,
        last_host_switch_reason: String::new(),
        seek_fill_batches: 0,
        seek_fill_connections: 0,
        seek_fill_bytes: 0,
        seek_fill_gate_denied: 0,
        seek_fill_fallbacks: 0,
        seek_fill_overlap_ok: 0,
        seek_fill_overlap_mismatch: 0,
        seek_fill_last_segments: 0,
        seek_fill_in_flight: 0,
        seek_fill_last_bytes: 0,
        seek_fill_last_ms: 0,
        seek_fill_last_expected_ms: 0,
        seek_fill_last_fallback: false,
        seek_fill_last_gate_reason: String::new(),
        seek_fill_disabled: false,
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
        whole_file_cached: false,
        transient_wait: false,
        http_requests: 0,
        http_request_bytes: 0,
        cache_memory_bytes: 0,
        cache_disk_bytes: 0,
        cold_hits: 0,
        cold_hit_bytes: 0,
        memory_budget_bytes: 0,
        forward_budget_bytes: 0,
        forward_playable_secs: 0.0,
        fill_throughput_bps: 0.0,
        stream_reconnects_total: 0,
        stream_reconnects_early_close: 0,
        stream_reconnects_empty: 0,
        stream_reconnects_reset: 0,
        stream_reconnects_host_switch: 0,
        stream_chunk_resumes: 0,
        stream_range_contiguous: 0,
        stream_range_forward_jumps: 0,
        stream_range_backward_jumps: 0,
        stream_range_last_start: 0,
        stream_last_connection_bytes: 0,
        stream_max_connection_bytes: 0,
        last_host_switch_reason: String::new(),
        seek_fill_batches: 0,
        seek_fill_connections: 0,
        seek_fill_bytes: 0,
        seek_fill_gate_denied: 0,
        seek_fill_fallbacks: 0,
        seek_fill_overlap_ok: 0,
        seek_fill_overlap_mismatch: 0,
        seek_fill_last_segments: 0,
        seek_fill_in_flight: 0,
        seek_fill_last_bytes: 0,
        seek_fill_last_ms: 0,
        seek_fill_last_expected_ms: 0,
        seek_fill_last_fallback: false,
        seek_fill_last_gate_reason: String::new(),
        seek_fill_disabled: false,
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

/// **HTTP 请求画像**的进程级单调计数器（每槽一份）：累计已发出的 HTTP Range
/// 请求数（含重试与断点续传）。计数点在 [`fetch_http_range`] 里唯一发请求的
/// 那一处，口径见 [`SourceDiagSnapshot::http_requests`]。
///
/// 为什么是"进程级 + 单调 + 永不清零"：宿主同进程里存在短命的"探针源"
/// （`probe_http_total_length` 只为探总长而建），若计数挂在源实例上、或源
/// 构造时清零，探针一构造就会把正在播放那条流的计数洗掉，宿主两次轮询的
/// 差分随即失真。单调总量 + 宿主差分是唯一稳的口径（与 `downloaded_bytes`
/// 同一套做法）；换源、重开播都不复位。
static SOURCE_HTTP_REQUESTS: [AtomicU64; SOURCE_DIAG_SLOTS] =
    [AtomicU64::new(0), AtomicU64::new(0)];

/// 与 `SOURCE_HTTP_REQUESTS` 配对的"累计已收字节"（同口径、同生命周期、
/// 同一处累加）。只用于算"平均每请求多少字节"——两个原子量必须同源同寿命，
/// 相除才自洽（拿**源级**的 `downloaded_bytes` 去除，换过源就会得到把分母
/// 留在历史值上的伪均值）。
static SOURCE_HTTP_REQUEST_BYTES: [AtomicU64; SOURCE_DIAG_SLOTS] =
    [AtomicU64::new(0), AtomicU64::new(0)];

/// **填窗期实测吞吐**（每槽一份，`f64` 位模式装在 `AtomicU64` 里）。
///
/// 口径见 [`SourceDiagSnapshot::fill_throughput_bps`]：只在窗口未满（读线程
/// 真的在读 socket）的时间片上采样，因此不会被"客户端故意不读"的背压钳成
/// 消费速率。由顺序长读线程直接写、宿主的 500ms poll 直接读——不经过 demux
/// 线程，所以 demux 阻塞在长 join 上时宿主照样能读到新鲜值。
static SOURCE_FILL_BPS: [AtomicU64; SOURCE_DIAG_SLOTS] = [AtomicU64::new(0), AtomicU64::new(0)];

/// 发布填窗期吞吐到全局槽（宿主 poll 读它）。
fn store_fill_bps(slot: usize, bps: f64) {
    if let Some(entry) = SOURCE_FILL_BPS.get(slot) {
        entry.store(bps.to_bits(), Ordering::Relaxed);
    }
}

/// 读取填窗期吞吐（未测得为 0.0）。
fn load_fill_bps(slot: usize) -> f64 {
    SOURCE_FILL_BPS
        .get(slot)
        .map(|entry| f64::from_bits(entry.load(Ordering::Relaxed)))
        .unwrap_or(0.0)
}

/// **滑动窗口预算使用的"实测吞吐"口径（唯一入口）。**
///
/// 为什么集中到这一个函数：网速判断必须只有一套。内核侧曾有三套来源
/// （`est_bps` 的 2s EMA、宿主 `policy::LinkThroughput` 的填窗期 EMA、以及
/// 自适应测量层）。本函数明确：**预算只用"填窗期实测吞吐"**，它的唯一事实源
/// 就是内核顺序长读线程自己采样并发布的 [`SourceDiagSnapshot::fill_throughput_bps`]
/// （全局槽 `SOURCE_FILL_BPS[slot]`，宿主 500ms 轮询读的也是同一个值）。
/// 宿主若要做统一供给估计（如 `supply_estimate_bps()`），**直接读这个字段即可**
/// ——不要再另起一套 EMA，也不要再喂 `est_bps`（`est_bps` 是"全部到达字节速率"，
/// 窗口满时会退化成消费速率，口径不同，只服务旧分段路径的自适应）。
///
/// 返回 `None` = **未知**（不是 0、更不是"充裕"）。调用方必须把"未知"当成
/// "我们不知道链路有多快"来处理：回退到保守的固定秒数/字节，见 `window_budget`。
///
/// 新鲜度：读线程在窗口未满时每 ≥500ms 发布一次（`FillMeter`），进入背压
/// （窗口满）时把该填窗期结算掉然后**冻结**读数——窗口满期间读到的就是"上一次
/// 真实填窗期测到的链路吞吐"，不会被"我们故意不读"稀释成 0。
fn supply_estimate_bps(slot: usize) -> Option<f64> {
    let bps = load_fill_bps(slot);
    (bps.is_finite() && bps > 0.0).then_some(bps)
}

/// 「这次 HTTP 请求画像记到哪两个原子量上」。
///
/// 为什么不直接传槽位下标：槽位是进程级共享数组的索引，而**单测需要一个
/// 不受并行用例干扰的计数**——静态数组是全进程共享的，同进程并行跑的两个
/// 用例会互相污染计数，"一次成功请求 = 1"这类断言就不可靠了。把"记到哪里"
/// 显式作为参数传入：生产路径指向 `SOURCE_HTTP_REQUESTS[diag_slot]`，用例
/// 指向自己的静态计数器，两者互不影响。
#[derive(Debug, Clone, Copy)]
struct HttpRequestProfile {
    requests: &'static AtomicU64,
    bytes: &'static AtomicU64,
}

/// 取某槽位的请求画像计数目标；槽位越界返回 None（不计数，但不影响取数）。
fn http_request_profile(slot: usize) -> Option<HttpRequestProfile> {
    Some(HttpRequestProfile {
        requests: SOURCE_HTTP_REQUESTS.get(slot)?,
        bytes: SOURCE_HTTP_REQUEST_BYTES.get(slot)?,
    })
}

impl HttpRequestProfile {
    /// 记录一次**真正发出**的 GET（重试与断点续传各记 1）。
    fn record_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录本次请求在 body 分块读循环里实际到达的字节。
    fn record_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }
}

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
    let since =
        audio_first_now_ms().saturating_sub(AUDIO_FIRST.enabled_at_ms.load(Ordering::Relaxed));
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

// ---------------------------------------------------------------------------
// 滑动窗口预算（2026-09-04 架构改造：时间预算 + 内存硬上限 + 冷区磁盘预算）
// ---------------------------------------------------------------------------
//
// ## 一、为什么"按字节卡预算"在弱网下是反的
//
// 缓冲的本职是**吸收抖动**，而抖动的绝对量纲是**时间**，不是字节。同一条
// KB 级弱网上，"再攒 20MB"可能意味着好几分钟的播放；而在 1080P 高速链路上
// 攒 20MB 只是几十秒。若只按固定字节预算推进前向窗口：
//
//     链路越差 → 单位时间能拉到的字节越少 → 按固定字节折算的"可播秒数"越短
//     → 越卡的场景缓冲越薄 → 抖动一来就饿死
//
// 这正是要修的反向激励。本改造把预算拆成**两个互不冲突的口径**：
//
//   - **时间预算**（保体验）：目标是"前方始终有 `N` 秒可播"。`N` 随实测吞吐
//     相对码率的下降而**上升**（弱网更激进地去缓冲），见 `window_forward_secs`。
//   - **字节硬上限**（保内存）：无论 `N` 多大，**内存常驻不超过硬上限**；超出
//     的部分走**冷区分层落盘**（`FullPreloadState` 的段存储），不是停止读取。
//
// 两者能同时成立，是因为弱网下 ABR 早已把码率降到很低：秒数要得激进，字节
// 压力却很小（360P ≈ 100KB/s，攒 120 秒也只有约 12MB）。也就是说
// "按秒激进 + 按字节守内存"不是折中，而是**在弱网上同时拿满两个好处**。
//
// ## 一·五、对照标准：官方网页端的激进程度（取值不是凭空拍的）
//
// 用户对照实测：**同一视频、同一清晰度，网页端（B 站 web 播放器）在弱网下
// 不卡，我们的客户端卡**；网页端的形态被描述为"会直接缓冲后面好大一段"
// （同类客户端 issue 亦记录同一对照：`cotaku/Bibi-Release#344`「希望增强
// 缓冲能力」——"在网速较差的环境会频繁卡顿……使用网页端观看同清晰度的同一
// 视频就不卡，而且可以观察到网页端的缓冲策略比较激进，会直接缓冲后面好大一段"）。
//
// 因此 `WINDOW_FORWARD_SECS_COMFORT = 30s` / `WINDOW_FORWARD_SECS_MAX = 180s`
// 这两个数字的目标就是**对齐并超过**那个"好大一段"的量级：
//   - 充裕链路：30s 前向（1080P ≈ 12MB）——与 mpv 默认 `demuxer-max-bytes` 4MiB
//     同向但更宽松，且不依赖宿主调参；
//   - KB 级弱网：180s 前向 + 60s 后向。低码率（360P ≈ 100KB/s）下 180s ≈ 18MB，
//     **字节压力只有 1080P 的一半都不到**，但弱网抖动的容忍窗口是它的 6 倍。
// 现象层面的验收标准就是网页端那条：**同片同清晰度下，本客户端的可播秒数
// 不得低于网页端的激进缓冲**（弱网档位尤其如此），而内存占用仍被硬上限钉住。
//
// ## 二、冲突时的优先级（明确，别改回去）
//
//   1. **"往前读到哪"由时间预算决定**（`forward_bytes`）。冷区/内存预算再紧
//      也不阻止读线程往前读——多出来的字节落到冷区（磁盘）。
//   2. **"这些字节放内存还是放磁盘"由字节预算决定**：前向窗口（热区）不超过
//      `memory_bytes` 里给前向的份额，其余在淘汰时交由冷区段存储（默认落盘）。
//   3. **冷区预算真的不够时**：丢**最久远**的冷区段（`evict_cold_to_budget`），
//      保留最近回看窗口；仍然不停读。丢的永远是最旧的一段，**不会**让
//      "前方的可播秒数"变小——所以它不会退化成"弱网缓冲变薄"。
//
// ## 三、单位与取值来源
//
//   - `bitrate_bps`：**实测消费速率**（`HttpRangeSource::consume_bps`，demux
//     稳态消费的字节/秒）。稳态下它就是当前 ABR 档位的有效码率（demux 只能
//     按播放速度消费，缓冲满时被背压钳住），因此不需要宿主把码率告诉我们。
//     可用 `ERIKA_HTTP_STREAM_BITRATE_BPS` 覆盖（排障 / 离线对照）。
//   - `throughput_bps`：**实测链路吞吐**，唯一入口是 [`supply_estimate_bps`]
//     —— 也就是内核自己上报的**填窗期实测吞吐**（诊断字段
//     `SourceDiagSnapshot::fill_throughput_bps`，宿主 500ms 轮询读的是同一个值）。
//     采样条件：只在窗口未满、读线程真的在读 socket 时采样；新鲜度：≥500ms
//     一更；窗口满（背压）时冻结读数（不衰减）。`None` = **未知**。
//     **预算不再使用 `est_bps`**（那是"全部到达字节的速率"，窗口满时会退化成
//     消费速率，口径不同，只服务旧分段路径的段大小/join 超时自适应）。
//   - 时间预算：`N = clamp(max(N_COMFORT, N_MIN * WEAK_GAIN / ratio), N_MIN, N_MAX)`，
//     `ratio = clamp(吞吐/码率, 0.10, 4.0)`：
//       ratio ≥ 4（链路 4 倍于码率）→ N = 30s
//       ratio = 1（刚好够播）        → N = 30s
//       ratio = 0.5（半速）          → N = 40s
//       ratio = 0.25（四分之一速）   → N = 80s
//       ratio ≤ 0.10（KB 级弱网）    → N = 180s（上限）
//   - **吞吐未知时**（`supply_estimate_bps` 返回 `None`）：`N` 回退到**保守的
//     固定秒数** `WINDOW_FORWARD_SECS_CONSERVATIVE = 20s`——既不因为"不知道"就
//     多缓（那等于拍脑袋吃内存/磁盘），也不因为"不知道"就少缓（那等于裸奔）。
//     20s 的理由：介于充裕基准（30s）与下限（10s）之间；1080P 下约 8MB，落在
//     24MiB 内存硬上限内、且高于改造前的 4MB read-ahead（不构成回归）；对常见
//     的 1~2s CDN 抖动仍有 10 倍以上余量。
//     码率也未知（开播最初 ~2s，消费速率还没测出来）时，"秒数 × 码率"也无从算起
//     → 退回**配置值**（`ERIKA_HTTP_READAHEAD_BYTES` / `BACK_BYTES`），与改造前
//     行为完全一致（这条同时保证既有单测的确定性）。
//   - 字节：`forward_bytes = clamp(N × 码率, read_ahead, memory_share)`，
//     `back_bytes = clamp(N_back × 码率, back, memory - forward)`，
//     `N_back = clamp(N/2, 8s, 60s)`（回拖命中窗口）。
//   - 内存硬上限：视频/单流 24MiB、音频 6MiB（`ERIKA_HTTP_WINDOW_MEMORY_BYTES`
//     覆盖），且**永不低于配置的 read-ahead + back**（不许比改造前更小）。
//   - 冷区预算：默认 256MiB（`ERIKA_HTTP_COLD_BUDGET_BYTES` 覆盖）；关掉落盘
//     （纯内存冷区）时钳到 32MiB，避免"落盘开关一关就把内存吃回去"。

/// 前向时间预算的"充裕链路"基准秒数。
const WINDOW_FORWARD_SECS_COMFORT: f64 = 30.0;
/// **吞吐未知时的保守回退秒数**（见文件上方说明）：不猜"充裕"、也不裸奔。
const WINDOW_FORWARD_SECS_CONSERVATIVE: f64 = 20.0;
/// 前向时间预算下限：任何场景前方至少攒这么多秒（保证不卡）。
const WINDOW_FORWARD_SECS_MIN: f64 = 10.0;
/// 前向时间预算上限：弱网堆垫上限（180s = 3 分钟）。
///
/// 为什么不更高：超过 3 分钟后，"多攒的秒数"边际收益极低（用户从开播到
/// 关窗的时间尺度就是分钟级），而弱网下它已经把低码率流堆到十几 MB；再高
/// 只会把内存/磁盘预算挤爆，反而触发冷区淘汰。
const WINDOW_FORWARD_SECS_MAX: f64 = 180.0;
/// 弱网增益：吞吐/码率 每降一半，前向时间预算按此系数放大（见公式）。
const WINDOW_WEAK_GAIN: f64 = 2.0;
/// 吞吐/码率比的下限：链路慢到只有码率的 1/10 时按 1/10 计（再低也不再加码，
/// 交给 180s 上限兜底）。
const WINDOW_SPEED_RATIO_MIN: f64 = 0.10;
/// 吞吐/码率比的上限：链路远快于码率时不再放宽时间预算（省内存）。
const WINDOW_SPEED_RATIO_MAX: f64 = 4.0;
/// 回看（后向）时间预算 = 前向 / 此系数，并钳在 [8s, 60s]。
const WINDOW_BACK_SECS_MIN: f64 = 8.0;
const WINDOW_BACK_SECS_MAX: f64 = 60.0;
/// 内存层硬上限默认值（视频/单流）。
const WINDOW_MEMORY_BUDGET_DEFAULT: u64 = 24 * 1024 * 1024;
/// 内存层硬上限默认值（DASH 音频流：码率低两个数量级）。
const WINDOW_MEMORY_BUDGET_AUDIO: u64 = 6 * 1024 * 1024;
/// 内存层里前向窗口可占的最大比例（其余留给后向回看）。
const WINDOW_FORWARD_MEMORY_SHARE: f64 = 0.6;
/// 冷区（磁盘层）默认预算。理由：256MiB ≈ 1080P 约 10 分钟回看窗口，
/// 覆盖"往回拖一点/回到刚才的播放点"这类真实用法；同时它只是宿主缓存目录
/// 上限（`PRELOAD_DIR_LIMIT_BYTES` = 4GiB）的 1/16，多开几个会话也不会撑爆盘。
const WINDOW_COLD_BUDGET_DEFAULT: u64 = 256 * 1024 * 1024;
/// 纯内存冷区（`ERIKA_PRELOAD_DISK=0` / `ERIKA_PRELOAD_TO_DISK=0`）时的冷区
/// 预算上限：32MiB。落盘开关一关就该退回"小内存缓存"，而不是把 256MiB 搬进 RAM。
const WINDOW_COLD_MEMORY_ONLY_MAX: u64 = 32 * 1024 * 1024;
/// 冷区单段大小。段是冷区淘汰的最小粒度：8MiB 让"丢最久远的一段"既不过粗
/// （浪费盘）也不过细（段头/文件句柄开销）。
const COLD_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;
/// 冷区交接通道容量（块）与单块大小：32 × 64KiB = 2MiB 缓冲。
/// 通道满 = 交接方（demux 线程）**直接放弃**这一段（冷区出现空洞 → 后续
/// 字节开新段），绝不阻塞播放线程。
const COLD_CHANNEL_CHUNKS: usize = 32;
const COLD_CHUNK_BYTES: usize = 64 * 1024;
/// 拖拽时的冷区**预读**长度：seek 提示命中冷区时，由冷区线程（独立线程）
/// 提前把目标位置这一段读进页缓存，demux 随后真正读时命中页缓存而非裸盘。
const COLD_WARM_BYTES: u64 = 2 * 1024 * 1024;
/// 消费速率（码率代理）的采样窗口下限：两次采样间隔超过它（长时间暂停 /
/// 长段冷区命中，demux 没在新字节上推进）时不更新 EMA，只重置窗口——
/// 否则一次跨越数分钟的"累计消费量"会被当成瞬时码率。
const CONSUME_SAMPLE_MAX_GAP: Duration = Duration::from_secs(4);

/// 前向时间预算（秒）：`N = clamp(max(N_COMFORT, N_MIN × GAIN / ratio), MIN, MAX)`。
///
/// 单调性质（本次改造的核心，勿改反）：**吞吐/码率 越低，N 越大**。
fn window_forward_secs(bitrate_bps: f64, throughput_bps: f64) -> f64 {
    if !(bitrate_bps.is_finite() && bitrate_bps > 0.0)
        || !(throughput_bps.is_finite() && throughput_bps > 0.0)
    {
        return WINDOW_FORWARD_SECS_COMFORT;
    }
    let ratio =
        (throughput_bps / bitrate_bps).clamp(WINDOW_SPEED_RATIO_MIN, WINDOW_SPEED_RATIO_MAX);
    let weak = WINDOW_FORWARD_SECS_MIN * WINDOW_WEAK_GAIN / ratio;
    weak.max(WINDOW_FORWARD_SECS_COMFORT)
        .clamp(WINDOW_FORWARD_SECS_MIN, WINDOW_FORWARD_SECS_MAX)
}

/// 一次预算结算的结果（字节 + 秒，供实现与诊断共用）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct WindowBudget {
    /// 前向窗口目标（字节）：读线程允许领先 demux 消费前沿这么多字节。
    forward_bytes: u64,
    /// 后向保留目标（字节）：demux 消费推进后仍留在内存里的已播字节。
    back_bytes: u64,
    /// 内存层硬上限（字节）：forward + back 永不超过它。
    memory_bytes: u64,
    /// 前向时间预算（秒）。
    forward_secs: f64,
    /// 结算用的码率估计（字节/秒）。
    bitrate_bps: f64,
    /// 结算用的链路吞吐（`None` = 未知 → 走了保守固定秒数回退）。
    throughput_bps: Option<f64>,
}

/// 结算滑动窗口预算。输入口径见文件上方"滑动窗口预算"整段注释。
///
/// `read_ahead` / `back` 是**配置值**（`ERIKA_HTTP_READAHEAD_BYTES` 等）：
/// 它们是预算的**下限**——改造后窗口只可能更宽，绝不比改造前更窄。
fn window_budget(
    slot: usize,
    read_ahead: u64,
    back: u64,
    bitrate_bps: f64,
    throughput_bps: Option<f64>,
) -> WindowBudget {
    let configured_floor = read_ahead.saturating_add(back);
    // 滴灌余量（`STREAM_DRIP_HEADROOM_BYTES`）**计在内存硬上限之内**：它也要占
    // 内存，不能躲在预算外——所以上限先把它算进来，前向/后向份额再从中扣掉。
    let drip = if stream_drip_enabled() {
        STREAM_DRIP_HEADROOM_BYTES
    } else {
        0
    };
    let memory_bytes = window_memory_budget(slot)
        .max(configured_floor.saturating_add(drip))
        .min(u64::MAX);
    let bitrate_known = bitrate_bps.is_finite() && bitrate_bps > 0.0;
    // 码率未知（开播最初 ~2s，消费速率还没测出来）→ 完全退回配置值：不放大也
    // 不缩小，**也不拿"典型 1080P 码率"之类的猜测值去代**（猜测一旦偏低就会把
    // 窗口算窄；配置值本来就是宿主给的下限）。
    if !bitrate_known {
        return WindowBudget {
            forward_bytes: read_ahead,
            back_bytes: back,
            memory_bytes,
            forward_secs: 0.0,
            bitrate_bps,
            throughput_bps: None,
        };
    }
    // 吞吐未知 → **保守的固定秒数**（不是"不知道就多缓"、也不是"少了缓"），
    // 见文件上方"吞吐未知时"整段。字节口径与已知时完全一致（同一条 clamp 链）。
    let forward_secs = match throughput_bps {
        Some(throughput) if throughput.is_finite() && throughput > 0.0 => {
            window_forward_secs(bitrate_bps, throughput)
        }
        _ => WINDOW_FORWARD_SECS_CONSERVATIVE,
    };
    let back_secs = (forward_secs / 2.0).clamp(WINDOW_BACK_SECS_MIN, WINDOW_BACK_SECS_MAX);
    // 前向目标：时间预算换算成字节，下限 = 配置 read-ahead，上限 = 内存层
    // 允许前向占的份额（其余留给后向回看 + 滴灌余量）。
    let forward_share = (memory_bytes as f64 * WINDOW_FORWARD_MEMORY_SHARE) as u64;
    let forward_ceiling = forward_share
        .min(memory_bytes.saturating_sub(back).saturating_sub(drip))
        .max(read_ahead.max(1));
    let forward_bytes =
        ((forward_secs * bitrate_bps) as u64).clamp(read_ahead.max(1), forward_ceiling);
    // 后向保留：时间预算的一半，下限 = 配置 back，上限 = 内存层剩余额度
    // （再扣掉滴灌余量，保证 `前向 + 后向 + 滴灌 ≤ 硬上限`）。
    let back_ceiling = memory_bytes
        .saturating_sub(forward_bytes)
        .saturating_sub(drip)
        .max(back);
    let back_bytes = ((back_secs * bitrate_bps) as u64).clamp(back, back_ceiling);
    WindowBudget {
        forward_bytes,
        back_bytes,
        memory_bytes,
        forward_secs,
        bitrate_bps,
        throughput_bps: throughput_bps.filter(|bps| bps.is_finite() && *bps > 0.0),
    }
}

/// 内存层硬上限（字节）。`ERIKA_HTTP_WINDOW_MEMORY_BYTES` 覆盖（0 = 用默认）。
fn window_memory_budget(slot: usize) -> u64 {
    env::var("ERIKA_HTTP_WINDOW_MEMORY_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or(if slot > 0 {
            WINDOW_MEMORY_BUDGET_AUDIO
        } else {
            WINDOW_MEMORY_BUDGET_DEFAULT
        })
}

/// 冷区（磁盘层）预算（字节）。`ERIKA_HTTP_COLD_BUDGET_BYTES` 覆盖。
/// 落盘关闭时钳到 `WINDOW_COLD_MEMORY_ONLY_MAX`（见常量注释）。
fn window_cold_budget(disk: bool) -> u64 {
    let budget = env::var("ERIKA_HTTP_COLD_BUDGET_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or(WINDOW_COLD_BUDGET_DEFAULT);
    if disk {
        budget
    } else {
        budget.min(WINDOW_COLD_MEMORY_ONLY_MAX)
    }
}

/// 码率的显式覆盖（`ERIKA_HTTP_STREAM_BITRATE_BPS`，字节/秒）。排障/离线对照用：
/// 生产上码率由 `HttpRangeSource::consume_bps` 实测（见预算整段注释）。
fn window_bitrate_override() -> Option<f64> {
    env::var("ERIKA_HTTP_STREAM_BITRATE_BPS")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|bps| bps.is_finite() && *bps > 0.0)
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
    // 填窗期吞吐：由顺序长读线程直接写全局槽（不经过 demux 线程），这里取最新值
    // ——宿主 500ms 轮询必须在 demux 卡住时也能读到新鲜链路吞吐（判升画质用）。
    snap.fill_throughput_bps = load_fill_bps(slot);
    // HTTP 请求画像同理由**读取时刻直接读**（不依赖 demux 线程发布）：这两个
    // 量是进程级单调计数，demux 卡在长 join 上时它照样在涨（预取线程/整片
    // 预载桶是独立线程），宿主 1s 差分拿到的就是真值。
    if let Some(profile) = http_request_profile(slot) {
        snap.http_requests = profile.requests.load(Ordering::Relaxed);
        snap.http_request_bytes = profile.bytes.load(Ordering::Relaxed);
    }
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

    /// 返回一个线程安全的取消句柄，用于中断阻塞中的源读取。
    ///
    /// 存在的理由（宿主拆除路径）：`ureq` 时代从外部中断在途请求是做不到的，关窗
    /// 只能靠"2s 超时后 detach demux 线程"兜底，而 detach 出去的线程仍在读网络。
    /// reqwest/Tokio 的原生能力是丢弃 future 即可中断 connect/TLS/headers/body，
    /// 这条缝把它暴露给上层，让 join 不必依赖超时。
    ///
    /// 默认 `None`：本地文件、直播等源没有可取消的在途请求。
    fn cancellation(&self) -> Option<SourceCancellation> {
        None
    }
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

// ===========================================================================
// 顺序长读（sequential stream）：把「每 256KB 一个 Range 请求」换成
// 「一条持续读的连接」——对齐 mpv / PiliPlus / 官方客户端的访问模型
// ===========================================================================
//
// ## 一、改造前的现状（实测口径，不是估算）
//
// 改造前 `HttpRangeSource` 的**唯一**数据通路是「一个 HTTP Range 请求下载
// 一段」，段大小 `prefetch_segment_bytes() = clamp(est_bps × 3s, 32KB, 256KB)`，
// 上限是**编译期常量** `SEEK_PREFETCH_SEGMENT_BYTES = 256KB`：
//
//   - demux 每读 32~64KB 就消费一段、消费完立刻补一段 → 段大小即"请求粒度"；
//   - 1080P 需求约 347KB/s ⇒ 256KB 段 ≈ 0.74s ⇒ **≈1.35 次/秒**，
//     一小时 ≈ 4700~6000 个 Range 请求（用户实测"上来就 6000 次"就是它）；
//   - 稳态并发连接 5~6 条（视频顺序预取 2 + 整片桶 1 + 音频 2 + 音频桶 1）。
//
// 对照：mpv / PiliPlus 对当前播放位置只发**一个** `Range: bytes=<start>-`
// （开放结尾）的长连接，然后**持续从 socket 读**进环形缓冲；缓冲满了就
// **停止读**（不读 ⇒ 内核接收缓冲填满 ⇒ TCP 窗口收缩 ⇒ 服务端自然减速，
// 这就是背压），有空位再继续读。所以顺序播放整段只有 1~3 个请求。
//
// ## 二、改造目标（可验收，见文件末尾的单测）
//
//   1. 一次连续播放（8 分钟 1080P ≈ 166MB）的请求数 = **1**（每条流），
//      与时长、与文件大小无关；只有 seek 出窗 / 换 host / 连接被断 /
//      瞬时错误重试才 +1。
//   2. 稳态并发连接 = 2（video 1 + audio 1），不再是 5~6。
//   3. 既有能力**一个不丢**：4MB read-ahead + 4MB 后向缓冲、`est_bps` EMA、
//      多 host 自愈切换、`transient_wait` 语义、整片预载桶（已同步改成流式，
//      见 `full_preload_stream_loop`）、诊断快照全部字段语义。
//
// ## 三、与历史结论「为什么用小段 256KB」的关系（重要，别再踩回去）
//
// `SEEK_PREFETCH_SEGMENT_BYTES` 处留着一条历史结论：慢网下大段（2MB+）
// "join 必超时 → read 放弃预取段 → 退回同步小步拉取 → 一顿一顿"。
// 那条结论成立的前提是**旧访问模型**：段是"一个请求换一段字节"，必须先把
// 整段收齐（join）才能交给 demux，段越大越容易在 join 超时窗口内收不齐。
//
// 顺序长读**没有"join 一个段"这个概念**：字节是**边到边交付**的——socket
// 读到一个 64KB 块就立刻 push 进窗口并唤醒 demux，demux 立刻能拿到。
// "段大小"这个变量在新模型里根本不存在，因此那条历史结论对本路径不适用
// （旧路径作为兜底保留，仍按它自己的约束用小段）。
//
// ## 四、旧路径为什么保留（而不是删掉）
//
// 旧的分段路径没有删，而是降级为兜底，由 `ERIKA_HTTP_SEQUENTIAL=0` 或
// "不经 VOD 工厂直接 `HttpRangeSource::new`" 触发：
//   - 它是"服务端不支持 open-ended Range / 忽略 Range 返回 200 全片"等异常
//     情形的对照实现，保留零成本；
//   - 它承载着一批既有单测（段队列、join 语义、后向缓冲、host 切换作废段…），
//     保留它 = 保留这些回归防线。
// 生产路径（VOD 工厂 `source_from_uri_with_hint_and_headers_slot_and_read_ahead`
// 的 HTTP 分支）默认启用新路径。

/// 冷区（磁盘分层）交接通道的发送端：热区淘汰的字节经它交给冷区。
///
/// 语义：**被热区淘汰的字节**（`consume_to` 裁掉的历史 / `reset` 丢弃的旧窗口）
/// 按绝对偏移交给冷区段存储（默认落盘）。`try_send` 非阻塞——通道满就直接放弃
/// 这一段（冷区出现空洞 → 后续字节另开新段），**绝不把 demux 线程堵在后面**。
///
/// 这是"已经下好的内容不丢"的载体：内存只留热区（前向 read-ahead + 后向回看），
/// 更早的区间落到磁盘；用户回拖 / 重播命中冷区时**零网络**从盘读回。
#[derive(Clone)]
struct ColdSink {
    tx: mpsc::SyncSender<ColdMessage>,
}

/// 热区 → 冷区的一条消息。
enum ColdMessage {
    /// 热区淘汰下来的字节（`offset` = 文件里的绝对偏移）。
    Evicted(ColdChunk),
    /// 拖拽预读请求（目标绝对偏移）：只预热页缓存，不产出数据。
    Warm(u64),
}

// ============================================================================
// **seek 出窗的跨 CDN 节点并行回填**（2026-09-15）
//
// 场景（本机制唯一的目标）：用户把进度条拖到还没下载过的位置（出窗）→ 松手 →
// 现在要等约 5 秒画面才跳过去。
//
// 归因已由前序实测钉死：`video_seek_preroll=5103ms ≈ 4980ms「等回填字节」+
// 120ms「解码+丢帧+调度」`，与 open 那 6 秒**同源**（都是"字节数 ÷ 单流链路
// 速率"）。回填量恒为「`AVSEEK_FLAG_BACKWARD` 落点到目标」的那**一个 GOP
// 窗口**（样本 GOP=5.000s/150 帧，实测 1.2~1.6MB），限速实验逐位吻合
// （`986857B ÷ 320KB/s = 3.0118s`，实测 3.00s）。**瓶颈在"单条连接"上，不在
// 链路带宽**：单条流实测 ≈320KB/s，而 video+audio 两条流同时下载合计 ≈1.1MB/s。
//
// 形态**严格限定**（用户批准的边界；改这段之前先读完它）：
//   1. **只在 seek 出窗时触发**。唯一入口是 `stream_seek_hint` 的重设分支；
//      **正常顺播路径一行都不碰**——"328MB 一个请求"的长连接优点原样保住。
//   2. **跨 CDN 节点**：N 段来自 N 个**不同的候选 host**，每个节点只开**一条**
//      正常的单次 Range 请求。**不对同一个 host 多开连接**。
//   3. **2~4 条**，条数由实测单流通量算出（见 `seek_parallel_connections`），
//      不是恒定开满。
//   4. **总量 ≤ 一个回填窗口**（`SEEK_PARALLEL_FILL_BYTES`，且不超过 `front_cap`）：
//      同一批数据换 N 个节点拉，**不是额外多拉**。
//   5. **用完即关**：每段一条有界 Range（`bytes=<s>-<e>`），读完即 drop，
//      不留常驻连接。
//   6. **失败即回退**：任一段失败/超时/实体校验不过/代数变化 → 丢弃剩余分段，
//      读线程照旧从 `write_pos` 起一条 open-ended Range 串行续传；**已 push 的
//      前缀字节保留**（它们本来就是正确前缀，丢了反而要重下）。
//   7. **候选只有一个 host 时不并行**（宁可慢，也不用可疑形态）：见
//      `HttpRangeSource::seek_fill_plan` 的 `sources.len() < 2` 分支。
//
// **为什么"跨节点"不是绕过告警，而是另一种形态**：上游
// `bilibili-downloader-gui` 的告警针对的是**对同一个源/节点开多条连接**——那在
// 单个节点上确实表现为滥用。跨节点时每个节点只看到**一条与普通用户无异的单次
// Range 请求**，总吞吐是"各节点速率之和"。而 B 站本身就是**多镜像 CDN 架构**：
// 宿主 `playback::build_cdn_candidates` 的做法就是**保留 path+签名、只换 host**
// （同一对象的不同镜像），官方客户端本身就在节点间调度。
// **但结论不靠推断**：本仓库既有的一手证据（见 `http_prefetch_parallel` 注释）
// 记录了 8 并发触发 CDN 限流惩罚的真机现象，因此这里把并发压在 2~4 且**分散在
// 不同节点**；跨节点这一形态本身的风控结论见报告（含找不到依据的部分）。
// ============================================================================

/// **seek 出窗并行回填**的回填窗口字节数。
///
/// 依据 = 上面的实测：`AVSEEK_FLAG_BACKWARD` 落点到目标之间那一个 GOP 窗口就是
/// demux 必须读回的字节量（1.2~1.6MB）。取 **1.5MiB** 覆盖该区间并留少量余量：
/// 它**就是回填窗口本身**，不是额外多拉。
const SEEK_PARALLEL_FILL_BYTES: u64 = 1536 * 1024;
/// 单段下限：再小 HTTP 头开销占比失控、且分段本身失去意义（照旧单连接）。
const SEEK_PARALLEL_MIN_SEGMENT_BYTES: u64 = 256 * 1024;
/// **段间重叠校验字节**（跨节点字节一致性的**直接证据**）。
///
/// 做法：段 k（k >= 1）的请求范围**比它分配的区间往前多要这么多字节**，收到后
/// 先把它与段 k-1 的**末尾同样多字节**逐字节比对，一致才 push 自己那份；不一致
/// → 整批作废、回退单连接，**绝不把不一致的字节拼进播放流**。
///
/// 为什么是 4KiB：在 TCP 段量级之上留了充分余量，足以在"两个节点给的其实是不同
/// 实体"时立刻暴露（那种情况下从边界起的字节几乎必然不同）；同时
/// `(N-1) × 4KiB ≤ 12KiB`，相对 1.5MiB 的回填窗口只有 ~0.8% 的额外流量——**校验
/// 字节不计入回填窗口配额**，但量级可忽略。
const SEEK_PARALLEL_OVERLAP_BYTES: u64 = 4 * 1024;
/// 并发上限（用户批准区间 2~4 的上界）。**不是默认条数**：实际条数由
/// `seek_parallel_connections` 按实测单流通量算出来，常见落点 2~3。
/// 上界的依据是既有真机教训（见 `http_prefetch_parallel` 注释：8 并发触发
/// B 站 CDN 限流惩罚——在飞段集体挂死、新建连接被主动断开），4 是安全区上沿。
const SEEK_PARALLEL_MAX_CONNECTIONS: usize = 4;
/// 单流通量估不出来时的默认条数：前序实测 320KB/s 单流 × 3 ≈ 960KB/s，正好把
/// 1.5MiB 压在 ~1.6s 目标内。
const SEEK_PARALLEL_DEFAULT_CONNECTIONS: usize = 3;
/// 目标墙钟（秒）：达到「回填窗口 ÷ 本秒数」的总吞吐就够了。
const SEEK_PARALLEL_TARGET_SECS: f64 = 1.6;
/// 单段等待上限：超过即放弃并行、回退单连接（与既有 `PREFETCH_JOIN_TIMEOUT`
/// 同量级——那条超时也是"慢段不拖死 demux"的同一套判据）。
const SEEK_PARALLEL_SEGMENT_TIMEOUT: Duration = Duration::from_secs(5);
/// **下载前预检**的等待上限：预检只是 1 字节 Range，正常耗时 ≈ 1 个 RTT + TTFB。
/// 取 2s——比正常值宽得多，但**短到不惩罚用户**：万一某个节点连头部都给不出，
/// 门禁会在这 2s 内判"证据不足"，随即回退单连接（而不是死等 5s 再回退）。
const SEEK_PARALLEL_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// 并行"没有叠加"的判定比：批次实测总吞吐低于「单流估算 × 它」就算无效。
/// 低于这个比值说明瓶颈不在单节点速率上（例如 CDN 按 IP 限速），并发只是白开
/// 连接、白涨请求数——此时**测出来就自动关掉**（见 `SeekFillDiag::note_gain`）。
const SEEK_PARALLEL_NO_GAIN_RATIO: f64 = 1.3;
/// 连续多少次"实测无叠加"就关掉本次会话的并行（进程内，不回滚）。
const SEEK_PARALLEL_NO_GAIN_LIMIT: u64 = 2;

/// 一个并行分段及其**来源节点**（跨节点并行：每段一个 host、每 host 一条连接）。
#[derive(Debug, Clone)]
struct SeekFillSegment {
    /// 本段在**文件里的分配区间** `[start, start + length)`（不含前向重叠）。
    start: u64,
    /// 分配区间长度。各段之和 = 回填窗口字节数。
    length: u64,
    /// 前向重叠字节数（**0 = 首段**：首段直接续在窗口前沿上，无可比对对象）。
    overlap: u64,
    /// 承载本段的候选 host URI（`candidates[active_idx + k]` 轮转、按 host 去重）。
    uri: String,
    /// host 标签（日志/诊断用；不含 path 与签名）。
    host: String,
}

impl SeekFillSegment {
    /// 实际请求的起点 = 分配起点 - 重叠（重叠只用于与上一段比对）。
    fn request_start(&self) -> u64 {
        self.start.saturating_sub(self.overlap)
    }

    /// 实际请求的字节数 = 分配长度 + 重叠。
    fn request_length(&self) -> u64 {
        self.length.saturating_add(self.overlap)
    }
}

/// **seek 出窗后的一次性跨节点并行回填计划**（demux 线程写、读线程取走执行一次）。
///
/// 形态约束见文件上方整段；这里是它的数据形态。放进 `StreamBuffer`（与
/// `generation` 同锁）而不是另开一把锁，是为了让"reset + 装计划"与读线程的
/// "取计划 + 复核代数"天然互斥，不需要第二套同步论证。
#[derive(Debug, Clone)]
struct SeekFillPlan {
    /// 计划对应的窗口代数：读线程执行前复核，不一致（又 seek 了）= 作废。
    generation: u64,
    /// 回填窗口起点（= `reset(target)` 的 target）。
    start: u64,
    /// 回填窗口字节数 = 各段 `length` 之和（**不含**重叠校验字节）。
    bytes: u64,
    /// 分段表（`len() >= 2`；1 段表示不并行，计划根本不会被建出来）。
    segments: Vec<SeekFillSegment>,
    /// 定条数时用的**单流通量估算**（字节/秒；0 = 未测得 → 走了默认条数）。
    /// 存进计划是为了让"为什么是 3 条"以及"实测 vs 期望"在日志/诊断里可复核。
    per_conn_bps: f64,
}

/// 一次冷区交接：`offset` = 这段字节在**文件里的绝对偏移**（不是段内偏移）。
struct ColdChunk {
    offset: u64,
    bytes: Vec<u8>,
}

/// 顺序流字节窗口（mpv demuxer-cache 的最小等价物）。
///
/// 语义：`[base, write_pos())` 是一段**连续**的文件字节。
///   - `consumed`：demux 已消费到的绝对偏移，由 [`StreamBuffer::consume_to`] 推进；
///   - `write_pos()`：网络已经填到的绝对偏移；
///   - `front_len() = write_pos - consumed`：**前向缓冲深度**，也是读线程唯一的
///     节流依据（≥ `front_cap` 就停止从 socket 读 → TCP 背压）。
///
/// 单写者（读线程）/ 单读者（demux 线程）共享 `Arc<StreamShared>`，用
/// `Mutex<StreamBuffer>` + `Condvar` 同步——4MB 窗口上无锁实现换不来可测收益，
/// 却会引入一整套内存序论证，不划算。
///
/// 用 `VecDeque` 而不是 `Vec`：`Vec::drain(..n)` 在 4MB 窗口上是 O(n) memmove，
/// demux 每消费 64KB 就搬 4MB 会直接吃掉一个核；VecDeque 的环形语义让头部丢弃
/// 与尾部追加都是 O(追加量)，`as_slices()` 还能让命中拷贝保持两段 memcpy。
struct StreamBuffer {
    /// `bytes[0]` 对应的文件绝对偏移。
    base: u64,
    bytes: VecDeque<u8>,
    /// demux 已消费前沿（绝对偏移）。**必须由读路径推进**（`consume_to`）：
    /// 它是 `front_len` 的减数，忘记推进会令 `front_len` 永不下降 → 读线程
    /// 认为缓冲永远是满的 → 播放直接停摆。
    consumed: u64,
    /// 前向缓冲上限（预算结算给出：默认对齐配置 read-ahead，弱网按时间预算放宽，
    /// 见文件上方"滑动窗口预算"整段）。
    front_cap: usize,
    /// 后向保留上限（预算结算给出：默认配置 back_bytes）：消费推进后仍保留这么多
    /// 已读字节，seek 回退命中窗口内零网络（mpv `--demuxer-max-back-bytes` 等价）。
    back_cap: usize,
    /// **滴灌余量**（字节）：窗口满时允许再多读这么一点点（见
    /// `STREAM_DRIP_HEADROOM_BYTES`），让 CDN 看到"连接还在被读"而不是写空闲。
    /// 它**计在内存硬上限之内**（预算里已扣掉），不会让常驻内存越预算。
    drip_headroom: u64,
    /// 冷区交接出口（`None` = 不分层，淘汰即丢——旧行为）。
    cold: Option<ColdSink>,
    /// 源已读到文件尾（服务端声明终点已到，或重复空读判定为尾）。
    eof: bool,
    /// 连接级**终局**错误（4xx / Range 被忽略 / Content-Range 违约）。
    /// 读方取走（`take`）即上报，让宿主换源；瞬时错误不会写这里。
    error: Option<String>,
    /// 重设代数：`reset`（seek 出窗）时 +1。读线程每条连接记下建立时的代数，
    /// 发现不一致即放弃该连接、从新 `write_pos` 重连——这是"seek 后只 +1
    /// 请求"的实现方式。
    generation: u64,
    /// 服务端声明的文件总长（首个 206 的 Content-Range total 回填）。
    /// 回填后 `len()` 不必再发 HEAD（旧路径的首个 206 也做同样的事）。
    total: Option<u64>,
    /// **seek 出窗的跨节点并行回填计划**（`None` = 没有待执行计划；
    /// **正常顺播恒为 `None`**——计划只在 `stream_seek_hint` 的重设分支里装）。
    ///
    /// 与 `generation` 同锁：`reset()` 清掉它（又 seek 了 → 旧计划作废），
    /// 调用点在**同一把锁内**"算计划 + reset + 装计划"，因此读线程看到的要么是
    /// 与新代数配套的计划、要么什么都没有，不会出现"旧代数 + 新计划"的错配。
    /// 读线程 `take_seek_fill_plan()` 取走即清空 → **天然一次性**：失败后不会
    /// 自动重试（避免请求放大）。
    fill_plan: Option<SeekFillPlan>,
}

impl StreamBuffer {
    fn new(base: u64, front_cap: usize, back_cap: usize) -> Self {
        Self {
            base,
            bytes: VecDeque::new(),
            consumed: base,
            front_cap,
            back_cap,
            drip_headroom: if stream_drip_enabled() {
                STREAM_DRIP_HEADROOM_BYTES
            } else {
                0
            },
            cold: None,
            eof: false,
            error: None,
            generation: 0,
            fill_plan: None,
            total: None,
        }
    }

    /// 挂上冷区交接出口（`ensure_stream` 时按 `full_preload` 是否存在决定）。
    fn set_cold_sink(&mut self, sink: Option<ColdSink>) {
        self.cold = sink;
    }

    /// 预算结算后更新窗口容量。只影响后续节流判定；若窗口已超过新上限，立即
    /// 按同一套"先交接冷区、再丢"的规则裁掉超出部分（否则"内存硬上限"要等到
    /// 下次消费推进才生效，诊断里会看到常驻字节短暂越界）。
    fn set_caps(&mut self, front_cap: usize, back_cap: usize) {
        self.front_cap = front_cap;
        self.back_cap = back_cap;
        let keep_from =
            self.consumed
                .saturating_sub(self.back_cap as u64)
                .max(self.write_pos().saturating_sub(
                    self.front_cap as u64 + self.back_cap as u64 + self.drip_headroom,
                ))
                .max(self.base);
        self.drop_below(keep_from);
    }

    /// 窗口常驻字节数（内存层热区实际占用的字节）。
    fn resident_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// 网络已填充到的绝对偏移（窗口尾部）。
    fn write_pos(&self) -> u64 {
        self.base.saturating_add(self.bytes.len() as u64)
    }

    /// 前向缓冲深度（demux 消费前沿到网络填充前沿的距离）。
    fn front_len(&self) -> u64 {
        self.write_pos().saturating_sub(self.consumed)
    }

    /// 读线程是否应当继续从 socket 读。false = 背压（**旧行为：完全不读**；
    /// 现在滴灌开启时窗口上限额外加 `drip_headroom`，见那里的注释）。
    fn has_space(&self) -> bool {
        !self.eof && self.front_len() < self.front_cap as u64 + self.drip_headroom
    }

    /// 前向预算是否已满（超过它之后只剩滴灌余量可用，见 `drip_headroom`）。
    fn front_reached_budget(&self) -> bool {
        self.front_len() >= self.front_cap as u64
    }

    /// 窗口**彻底饱和**（连滴灌余量都用尽）→ 读线程必须完全停读。
    fn is_saturated(&self) -> bool {
        !self.eof && self.front_len() >= self.front_cap as u64 + self.drip_headroom
    }

    /// 当前前向窗口容量（预算 + 滴灌余量），诊断里核"窗口停在预算附近"用。
    ///
    /// 当前无调用点，保留待用。
    #[cfg(test)]
    #[allow(dead_code)]
    fn front_cap_total(&self) -> u64 {
        self.front_cap as u64 + self.drip_headroom
    }

    /// 追加一段与窗口尾部连续的字节（唯一写入口，单写者）。
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk.iter().copied());
    }

    /// 完整命中才返回 Some（`[start, start+length)` 全在窗口内）。
    /// 半覆盖返回 None，由调用方决定"等"还是"重设窗口"。
    fn read(&self, start: u64, length: u64) -> Option<Vec<u8>> {
        let end = start.checked_add(length)?;
        if start < self.base || end > self.write_pos() {
            return None;
        }
        let offset = usize::try_from(start - self.base).ok()?;
        let want = usize::try_from(length).ok()?;
        let mut out = Vec::with_capacity(want);
        let (first, second) = self.bytes.as_slices();
        if offset < first.len() {
            let take = want.min(first.len() - offset);
            out.extend_from_slice(&first[offset..offset + take]);
        }
        if out.len() < want {
            let second_offset = offset.saturating_sub(first.len());
            let take = (want - out.len()).min(second.len().saturating_sub(second_offset));
            out.extend_from_slice(&second[second_offset..second_offset + take]);
        }
        if out.len() != want {
            return None;
        }
        Some(out)
    }

    /// `start` 处**当前可得**的字节（可以少于请求量；`start` 未覆盖时返回
    /// `None`，窗口内但暂时无字节时返回 `Some(空)`）。文件尾与 `length == None`
    /// 的"读到尾"路径用它。
    fn read_available(&self, start: u64, max: usize) -> Option<Vec<u8>> {
        if start < self.base {
            return None;
        }
        let offset = usize::try_from(start - self.base).ok()?;
        let write_pos = self.write_pos();
        if start > write_pos {
            return None;
        }
        let available = usize::try_from(write_pos - start).unwrap_or(usize::MAX);
        let want = available.min(max);
        if want == 0 {
            return Some(Vec::new());
        }
        let (first, second) = self.bytes.as_slices();
        let mut out = Vec::with_capacity(want);
        if offset < first.len() {
            let take = want.min(first.len() - offset);
            out.extend_from_slice(&first[offset..offset + take]);
        }
        if out.len() < want {
            let second_offset = offset.saturating_sub(first.len());
            let take = (want - out.len()).min(second.len().saturating_sub(second_offset));
            out.extend_from_slice(&second[second_offset..second_offset + take]);
        }
        Some(out)
    }

    /// 推进消费前沿并裁掉后向缓冲之外的历史字节。
    ///
    /// 裁到 `consumed - back_cap` 为止：再往前的字节已经超出回退窗口，留着
    /// 只会让常驻内存无界（旧路径的 `cache_bytes` 正是靠窗口重设定界，这里
    /// 改成消费驱动的前移，语义等价且不会漏）。
    ///
    /// **2026-09-04 改造**：裁掉之前先把这段字节交接给冷区（磁盘分层），
    /// 于是"内存里更早的区间"不再消失，而是换了个更便宜的载体继续存在。
    fn consume_to(&mut self, pos: u64) {
        if pos > self.consumed {
            self.consumed = pos;
        }
        let keep_from = self
            .consumed
            .saturating_sub(self.back_cap as u64)
            .max(self.base);
        self.drop_below(keep_from);
    }

    /// 丢弃 `[base, keep_from)`：先尽力交接冷区，再真丢。唯一收口点
    /// （`consume_to` / `set_caps` 都走它），避免两套淘汰规则分叉。
    fn drop_below(&mut self, keep_from: u64) {
        if keep_from <= self.base {
            return;
        }
        // 交接是尽力而为：通道背压时只交接了连续前缀，剩下的随窗口一起丢
        // （那段区间之后回拖需要重新下载——诚实的代价，不阻塞播放线程）。
        self.hand_off_to_cold(keep_from);
        let drop = usize::try_from(keep_from - self.base)
            .unwrap_or(usize::MAX)
            .min(self.bytes.len());
        if drop > 0 {
            self.bytes.drain(..drop);
            self.base = self.base.saturating_add(drop as u64);
        }
    }

    /// 把 `[base, end)` 的字节按 [`COLD_CHUNK_BYTES`] 分块交给冷区（尽力而为）。
    ///
    /// 返回交接成功的终点。**遇到背压立即停止**：冷区收到的永远是一段连续
    /// 前缀（不会"中间丢了、后面又收到"），因此冷区的覆盖判定永远是段内连续，
    /// 不存在需要额外记录的洞。
    fn hand_off_to_cold(&mut self, end: u64) -> u64 {
        let Some(sink) = self.cold.clone() else {
            return self.base;
        };
        let mut sent = self.base;
        while sent < end {
            let take = (end - sent).min(COLD_CHUNK_BYTES as u64) as usize;
            let Some(relative) = usize::try_from(sent - self.base).ok() else {
                break;
            };
            let Some(chunk) = self.slice(relative, take) else {
                break;
            };
            if sink
                .tx
                .try_send(ColdMessage::Evicted(ColdChunk {
                    offset: sent,
                    bytes: chunk,
                }))
                .is_err()
            {
                break;
            }
            sent = sent.saturating_add(take as u64);
        }
        sent
    }

    /// 窗口内相对偏移处取 `length` 字节（冷区交接用的纯拷贝，不改窗口）。
    fn slice(&self, offset: usize, length: usize) -> Option<Vec<u8>> {
        if offset.checked_add(length)? > self.bytes.len() {
            return None;
        }
        let (first, second) = self.bytes.as_slices();
        let mut out = Vec::with_capacity(length);
        if offset < first.len() {
            let take = length.min(first.len() - offset);
            out.extend_from_slice(&first[offset..offset + take]);
        }
        if out.len() < length {
            let second_offset = offset.saturating_sub(first.len());
            let take = (length - out.len()).min(second.len().saturating_sub(second_offset));
            out.extend_from_slice(&second[second_offset..second_offset + take]);
        }
        (out.len() == length).then_some(out)
    }

    /// seek 出窗：丢弃全部已缓冲字节，从 `base` 重建窗口（代数 +1 让读线程
    /// 放弃在飞连接）。旧位置的字节一律不可复用（错位读比多一个请求危险得多）。
    ///
    /// **2026-09-04 改造**：清空前先尽力把窗口内容交接给冷区——"seek 走了"不等于
    /// "这些已下好的字节没用了"：用户拖回这段区间时应当零网络命中。交接同样是
    /// 尽力而为（通道满即停），代价有界（≤ 通道容量），不阻塞 demux 线程。
    fn reset(&mut self, base: u64) {
        self.hand_off_to_cold(self.write_pos());
        self.bytes.clear();
        self.base = base;
        self.consumed = base;
        self.eof = false;
        self.error = None;
        self.generation = self.generation.wrapping_add(1);
        // 又 seek 了：旧位置的并行回填计划作废（调用点在同一把锁内装新计划）。
        self.fill_plan = None;
    }

    /// 取走待执行的并行回填计划（**一次性**：取走即清空；正常顺播恒返回 `None`）。
    ///
    /// 两道守卫（任一不满足都作废，退回既有单连接路径）：
    ///   - **代数一致**：不一致说明取计划之前又发生了 `reset`（用户连拖）；
    ///   - **窗口前沿正好落在计划起点**：不相等说明这段字节已经由别的路径
    ///     拿到了（或窗口已被消费推进），计划没有意义。
    fn take_seek_fill_plan(&mut self) -> Option<SeekFillPlan> {
        let plan = self.fill_plan.take()?;
        if plan.generation != self.generation || self.write_pos() != plan.start {
            return None;
        }
        Some(plan)
    }
}

/// 窗口 + 唤醒条件变量（读线程 push 后 notify，demux 侧分片等待）。
struct StreamShared {
    buf: Mutex<StreamBuffer>,
    data_cv: Condvar,
}

/// 取窗口锁，**poison 不 panic**。
///
/// 调用点落在 demux 线程的 `extern "C"` AVIO 读回调内，panic 不可 unwind →
/// 进程 fail-fast abort（实测 0xc0000409 闪退形态）。写线程已经消失时，
/// 窗口里的字节依然是自洽的，取回内部数据继续读即可。
fn lock_stream(shared: &StreamShared) -> std::sync::MutexGuard<'_, StreamBuffer> {
    match shared.buf.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// demux 侧等待新字节的分片长度：与旧 `take_prefetch` 的分片轮询同构，
/// 保证等待期间仍能推进换 host / 诊断（不退回"卡住时恰好换不了 host"的旧缺口）。
const STREAM_WAIT_SLICE: Duration = Duration::from_millis(250);

/// 读线程空闲轮询步长（背压期间 / 等写入窗口时的睡眠）。
/// 50ms：够短（消费后最迟 50ms 恢复读），够长（不烧 CPU）。
const STREAM_IDLE_POLL: Duration = Duration::from_millis(50);

/// 允许"等一等"的最大前跳距离：demux 读到的位置领先网络填充前沿不超过它时，
/// 不重发请求，等读线程把字节填上来（≤64KB ≈ 0.2s @1080P）。
/// 超过它（= 真正的 seek / 乱序跳读）就重设窗口、重发一条连接——这才是
/// mpv 的行为：seek 到缓冲外必须重新请求，而不是把几 MB 顺着读完。
const STREAM_FORWARD_SLACK_BYTES: u64 = 64 * 1024;

/// 重连退避上限。200ms 起翻倍到 2s（与 `full_preload_loop` 的既有节奏一致）：
/// 服务端被并发限制时的退避不应超过一次握手成本太多，否则恢复慢半拍。
const STREAM_RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(2);

/// 窗口最小前向容量：即便上层把 read-ahead 配得很小，也要容得下一个 AVIO
/// 读块（64KB）+ 一点余量，否则"等一下"的判定会反复触到边。
const STREAM_MIN_FRONT_CAP: usize = 256 * 1024;

/// 源析构时等待读线程退出的上限。超过即 detach（见 `Drop for HttpRangeSource`）。
const STREAM_STOP_JOIN_TIMEOUT: Duration = Duration::from_millis(500);

/// 长读路径的**诊断发布节流**间隔（毫秒）：宿主 500ms 轮询一次，发布更勤只是
/// 把进程级全局锁拉进 demux 热路径。见 `publish_diag_throttled`。
const STREAM_DIAG_PUBLISH_MS: u64 = 500;

/// 一条已建立的顺序读连接（header 已校验、body reader 已就绪）。
struct SequentialResponse {
    reader: Box<dyn Read + Send + Sync>,
    /// 本连接声明的**终点**（Content-Range 的 end，含）。到它就说明文件读完，
    /// 不必再重连（这是"1 个请求播完 8 分钟"的出口条件）。
    end: Option<u64>,
    /// 服务端声明的总长（Content-Range total / 200 的 Content-Length）。
    total: Option<u64>,
}

/// 顺序长读的重连退避推进：0 → 200ms → 400ms → … → 上限
/// [`STREAM_RECONNECT_BACKOFF_MAX`]（与旧 `RETRY_BACKOFF` 节奏一致）。
///
/// 注意"退避"只挂在**失败**上：连接建立成功、或连接被服务端提前收线但已经
/// 收到整块数据时，重连必须是**立即**的——那两种情形下字节仍在正常流动，
/// 白等 200ms 只会把 demux 拖出一次不必要的冻结。
fn stream_retry_backoff(current: Duration) -> Duration {
    if current.is_zero() {
        Duration::from_millis(200)
    } else {
        (current * 2).min(STREAM_RECONNECT_BACKOFF_MAX)
    }
}

/// 解析 `Content-Range: bytes start-end/total` 的 `end`。
fn parse_content_range_end(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, _) = rest.rsplit_once('/')?;
    let (_, end) = range.trim().split_once('-')?;
    end.trim().parse::<u64>().ok()
}

/// 一次 HTTP 交换要用的客户端与逐请求 deadline。
///
/// 拆成两件是因为生效范围不同：**连接超时是 client 级的**（本进程只有一个 data
/// client，4s），而**整次请求（发出 → body 读完）的总时长是逐请求的**——顺序长读
/// 的 body 是"到文件尾"的无限长响应，必须不限时；有界的段请求则必须限时，否则
/// 挂死的分段会永久占着在飞配额。
#[derive(Clone, Copy)]
struct HttpFetch {
    client: &'static reqwest::Client,
    /// 整次请求（发出 → body 读完）的总时长上限；`None` = 不限。
    timeout: Option<Duration>,
}

impl HttpFetch {
    fn get(&self, uri: &str) -> reqwest::RequestBuilder {
        self.with_timeout(self.client.get(uri))
    }

    /// HEAD 探测专用（`len()` 走这条路，拿不到 Content-Length 时才回退到
    /// 一字节 Range 请求，见 `probe_http_total_length`）。
    fn head(&self, uri: &str) -> reqwest::RequestBuilder {
        self.with_timeout(self.client.head(uri))
    }

    fn with_timeout(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.timeout {
            Some(timeout) => request.timeout(timeout),
            None => request,
        }
    }
}

/// 同步发出一次请求，并把失败归类成 [`HttpExchangeFailure`]。
///
/// 同步桥接见 [`http::block_on`]：读路径刻意是同步的（跑在 demux 线程上、自己
/// 持有同步字节窗口），所以异步调用在这里被驱动到完成，而不是把调用方变异步。
///
/// **注意这里只覆盖发送阶段**：reqwest 不把 4xx/5xx 当错误，状态码由调用点
/// 自己的 status 分支处理（并在 `>= 400` 时构造 [`HttpExchangeFailure::Status`]），
/// 以保持与旧实现一致的重试口径——5xx 可重试、4xx 终局。
fn send_http_request(
    request: reqwest::RequestBuilder,
) -> std::result::Result<reqwest::Response, HttpExchangeFailure> {
    match http::block_on(async move {
        request
            .send()
            .await
            .map_err(|error| SourceError::Transient(error.to_string()))
    }) {
        Ok(response) => Ok(response),
        Err(SourceError::Transient(message)) => Err(HttpExchangeFailure::Transport(message)),
        Err(other) => Err(HttpExchangeFailure::Transport(other.to_string())),
    }
}

/// 顺序长读专用：**只保留连接超时，无 body 总时长限制**。
///
/// 与 [`http_live_agent`] 是同一个理由（那里的注释记录了根因）：任何"整次请求"
/// 的总时长上限，对"从当前位置到文件尾"的无限长响应都会变成"播满 N 秒必断流"。
/// 短请求（长度探测 / 候选测速）用 [`http_agent`]。
fn http_stream_agent() -> HttpFetch {
    HttpFetch {
        client: http::data_client(),
        timeout: None,
    }
}

// ----------------------------------------------------------------------------
// seek 出窗的跨节点并行回填：**执行侧**（形态约束见文件上方整段）。
//
// 读线程在一次 seek 出窗后执行一批**分段**（每段一个候选节点、一条有界 Range），
// 然后**按序**把它们 push 进窗口；任一段出问题就整批放弃、回退既有的单连接串行
// 续传。所有分段都不写窗口，写窗口的只有读线程一个（既有单写者语义不变）。
// ----------------------------------------------------------------------------

/// 并行分段专用：**有界段**（几百 KB），因此可以安全地设总时长上限——段跑不完
/// 就报错退出，而不是像长读那样必须不限时（长读的 body 是"到文件尾"的无限长
/// 响应，见 [`http_stream_agent`]）。
///
/// 上限取 [`SEEK_PARALLEL_SEGMENT_TIMEOUT`] + 2s：比读线程的等待窗口略长，
/// 保证**先超时的是读线程**——回退决策统一由读线程做，worker 只负责如实报错。
/// 没有这道上限，一个挂死的分段线程会永久占着在飞配额。
///
/// **已知差异**：旧实现另有 `timeout_recv_response(8s)` 单独约束"发出到收到响应
/// 头"。reqwest 只有整次请求一个粒度的 `timeout`，没有等价项，因此响应头挂死从
/// 8s 失败变成 22s 失败。取舍同上：读线程仍会先超时并回退，结论不变。
fn http_seek_fill_agent() -> HttpFetch {
    HttpFetch {
        client: http::data_client(),
        timeout: Some(SEEK_PARALLEL_SEGMENT_TIMEOUT + Duration::from_secs(2)),
    }
}

/// **并行回填的累计计数 + 最近一次批次明细**（诊断与日志的唯一数据源）。
///
/// 存在的理由（用户要的"可验证反馈"）：并行分段会让 `http_requests` 上涨，
/// 必须能区分三种形态——**顺播一条长连接** / **seek 时的跨节点 N 段** /
/// **seek 退化为单连接**。这组计数只在 seek 出窗时动，**顺播期间恒为 0**，
/// 这就是判据本身（见 `SourceDiagSnapshot::seek_fill_batches`）。
#[derive(Default)]
struct SeekFillDiag {
    /// 累计批次数（每次 seek 出窗尝试并行 +1；回退的批次也计）。
    batches: AtomicU64,
    /// 累计建立的并行分段连接数（每个节点一条）。
    connections: AtomicU64,
    /// 累计**真正被采用**的并行字节数（回退时丢弃的段不计）。
    bytes: AtomicU64,
    /// 回退到单连接的批次次数（任一段失败/超时/校验不过/代数变化）。
    fallbacks: AtomicU64,
    /// 最近一次批次的段数（0 = 本次会话还没并行过 → "顺播"判据）。
    last_segments: AtomicU64,
    /// 最近一次批次被采用的字节数。
    last_bytes: AtomicU64,
    /// 最近一次批次墙钟（毫秒）。
    last_ms: AtomicU64,
    /// 最近一次批次的**期望墙钟**（毫秒）= 采用字节 ÷ 单流通量估算，即"同一批
    /// 数据用单连接拉要多久"。用户拿"实测 vs 期望"就能复核提速幅度，不需要信
    /// 任何人的口头结论。
    last_expected_ms: AtomicU64,
    /// 定条数时用的单流通量估算（字节/秒，f64 位模式）。
    last_per_conn_bps: AtomicU64,
    /// 最近一次批次是否回退（0/1）。
    last_fallback: AtomicU64,
    /// **自适应闸门**：连续测得"并行没有叠加"的次数（见 `note_gain`）。
    no_gain: AtomicU64,
    /// 自适应关闭发生的次数。关掉之后 env 开关仍是"开"的状态，但本会话不再
    /// 并发——现场要能从诊断里看出"是测出来没用才不并发，不是没生效"。
    auto_disabled: AtomicU64,
    /// **下载前门禁否决**的次数（凭据不足/不一致 → 退回单节点）。
    ///
    /// 这是"**这次没并行是因为没过校验**"的直接证据：现场看到 seek 变慢时，
    /// 先看这个数——它涨说明门禁在工作，而不是并行没生效。
    gate_denied: AtomicU64,
    /// 最近一次门禁裁定的可读理由（原样展示，不重写）。
    last_gate_reason: Mutex<String>,
    /// **边界重叠逐字节比对失败**的批次数：**核心安全计数**。它非 0 说明确实有
    /// 节点给的字节与邻居对不上——那一批已经被整批丢弃并回退。
    overlap_mismatch: AtomicU64,
    /// 边界重叠逐字节比对**全部通过**的批次数（与 `overlap_mismatch` 成对：
    /// 两者之和 + 未走到比对就被中止的批次 = `batches`）。
    overlap_ok: AtomicU64,
}

impl SeekFillDiag {
    /// 本会话是否已因"实测无叠加"关掉并行。
    fn disabled(&self) -> bool {
        self.no_gain.load(Ordering::Relaxed) >= SEEK_PARALLEL_NO_GAIN_LIMIT
    }

    /// 把本批计数写进诊断快照（`HttpRangeSource::diag_snapshot` 调用）。
    ///
    /// 口径：累计量都是**进程内单调**（与 `http_requests` 同寿命，宿主差分读）；
    /// `*_last_*` 是最近一次批次的明细，供"这次 seek 到底发生了什么"当场判读。
    fn fill_snapshot(&self, snapshot: &mut SourceDiagSnapshot) {
        snapshot.seek_fill_batches = self.batches.load(Ordering::Relaxed);
        snapshot.seek_fill_connections = self.connections.load(Ordering::Relaxed);
        snapshot.seek_fill_bytes = self.bytes.load(Ordering::Relaxed);
        snapshot.seek_fill_gate_denied = self.gate_denied.load(Ordering::Relaxed);
        snapshot.seek_fill_fallbacks = self.fallbacks.load(Ordering::Relaxed);
        snapshot.seek_fill_overlap_ok = self.overlap_ok.load(Ordering::Relaxed);
        snapshot.seek_fill_overlap_mismatch = self.overlap_mismatch.load(Ordering::Relaxed);
        snapshot.seek_fill_last_segments = self.last_segments.load(Ordering::Relaxed);
        snapshot.seek_fill_last_bytes = self.last_bytes.load(Ordering::Relaxed);
        snapshot.seek_fill_last_ms = self.last_ms.load(Ordering::Relaxed);
        snapshot.seek_fill_last_expected_ms = self.last_expected_ms.load(Ordering::Relaxed);
        snapshot.seek_fill_last_fallback = self.last_fallback.load(Ordering::Relaxed) != 0;
        snapshot.seek_fill_disabled =
            self.disabled() || self.auto_disabled.load(Ordering::Relaxed) > 0;
        if let Ok(reason) = self.last_gate_reason.lock() {
            snapshot.seek_fill_last_gate_reason = reason.clone();
        }
    }

    /// 最近一次批次用的单流通量估算（**测试断言用**；生产路径只写不读）。
    ///
    /// 当前无调用点，保留待用。
    #[cfg(test)]
    #[allow(dead_code)]
    fn last_per_conn_bps(&self) -> f64 {
        f64::from_bits(self.last_per_conn_bps.load(Ordering::Relaxed))
    }

    /// 记录一次批次的实测吞吐，判定"并行到底有没有叠加"。
    ///
    /// 判据：`实测总吞吐 < 单流估算 × SEEK_PARALLEL_NO_GAIN_RATIO` ⇒ 通了但**没
    /// 叠加**（例如 CDN 按出口 IP 限速、或几个候选其实落在同一出口）。连续
    /// [`SEEK_PARALLEL_NO_GAIN_LIMIT`] 次就关掉本会话的并行——**宁可慢，也不做
    /// 只涨请求数、不涨吞吐的事**。任何一次测到有叠加就归零。
    fn note_gain(&self, achieved_bps: f64, per_conn_bps: f64) {
        if !(achieved_bps > 0.0) || !(per_conn_bps > 0.0) {
            return;
        }
        if achieved_bps >= per_conn_bps * SEEK_PARALLEL_NO_GAIN_RATIO {
            self.no_gain.store(0, Ordering::Relaxed);
            return;
        }
        let previous = self.no_gain.fetch_add(1, Ordering::Relaxed);
        if previous + 1 == SEEK_PARALLEL_NO_GAIN_LIMIT {
            self.auto_disabled.fetch_add(1, Ordering::Relaxed);
            trace::diagnostic(format!(
                "[erika seek-parallel] 实测并行无叠加（{:.0}KB/s vs 单流 {:.0}KB/s）：本会话不再并发，seek 回到单连接串行",
                achieved_bps / 1024.0,
                per_conn_bps / 1024.0,
            ));
        }
    }
}

/// 一个节点的**预检结果**：下载前能拿到的全部凭据（1 字节 Range 的响应头）。
///
/// 为什么必须有这一步：用户的原话是"你真的能在下载、也就是在流下来之前，就确定
/// 那个视频是一致的吗？"——预检就是回答它的那一枪。**没有通过门禁的节点，一个
/// body 字节都不会被读**（见 [`SeekFillGate`]）。
#[derive(Debug, Clone, Default)]
struct SeekFillProbe {
    /// 服务端声明的**文件总长**（`Content-Range` total / `Content-Length`）。
    /// 跨节点一致性的**必要条件**。
    total: Option<u64>,
    /// `ETag` 原文（**强证据**：实测 B 站官方 upos 镜像节点跨节点同值）。
    etag: Option<String>,
    /// `Last-Modified` 原文（**弱证据，门禁不作依据**）。
    ///
    /// 实测反例（2026-09-15，同一 cid 的三个节点）：MCDN 节点给
    /// `Mon, 14 Sep 2026 15:15:20 GMT`，两个官方镜像给
    /// `Sun, 31 Mar 2024 20:54:03 GMT`——**同一份内容、不同 Last-Modified**。
    /// 所以它只能进日志/诊断供人看，不能当一致性判据。
    last_modified: Option<String>,
    /// 预检本身耗时（诊断：这一步的代价有多大）。
    elapsed: Duration,
}

/// 一个并行分段交回的结果。
struct SeekFillResponse {
    /// 收到的完整字节（= 请求长度；长度不符的段在 worker 里就被判失败）。
    bytes: Vec<u8>,
    /// 本段自身耗时（worker 侧计时；日志/诊断用）。
    elapsed: Duration,
}

/// 门禁裁定结果（读线程裁定、各 worker 读取）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeekFillVerdict {
    /// 放行：跨节点凭据一致（下载后仍有重叠比对兜底）。
    Allow,
    /// 作废：凭据不足/不一致 → **退回单节点**（宁可慢，不冒数据损坏的险）。
    Deny,
}

/// slot 的两阶段状态机：`Pending → Probed(预检) → Done(数据/失败)`。
///
/// 之所以两阶段而不是"一个结果位"：门禁必须在**任何 body 被读之前**裁定，而裁定
/// 需要各节点的预检头部——所以 worker 把"头部"与"数据"分两次交回。
enum SeekFillSlotState {
    Pending,
    Probed(SeekFillProbe),
    Done(std::result::Result<SeekFillResponse, String>),
}

/// 一个分段槽：worker 分两次写入，读线程**按序**取走（取走即释放内存）。
struct SeekFillSlot {
    segment: SeekFillSegment,
    state: Mutex<SeekFillSlotState>,
    ready: Condvar,
}

impl SeekFillSlot {
    fn new(segment: SeekFillSegment) -> Self {
        Self {
            segment,
            state: Mutex::new(SeekFillSlotState::Pending),
            ready: Condvar::new(),
        }
    }

    /// 取槽锁，**poison 不 panic**（与 `lock_stream` 同一套理由：调用点落在 demux
    /// 线程的等待循环里，panic 越过 `extern "C"` 边界会 fail-fast abort）。
    fn lock(&self) -> std::sync::MutexGuard<'_, SeekFillSlotState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn publish_probe(&self, probe: SeekFillProbe) {
        let mut guard = self.lock();
        if matches!(*guard, SeekFillSlotState::Pending) {
            *guard = SeekFillSlotState::Probed(probe);
        }
        drop(guard);
        self.ready.notify_all();
    }

    fn publish_done(&self, value: std::result::Result<SeekFillResponse, String>) {
        let mut guard = self.lock();
        *guard = SeekFillSlotState::Done(value);
        drop(guard);
        self.ready.notify_all();
    }
}

/// **下载前门禁**：各节点的预检头部齐了之后由读线程裁定"放行 / 作废"，各 worker
/// 在这里等结果；作废时 worker 直接退出——**一个 body 字节都不读**。
struct SeekFillGate {
    decision: Mutex<Option<SeekFillVerdict>>,
    ready: Condvar,
}

impl SeekFillGate {
    fn new() -> Self {
        Self {
            decision: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn release(&self, verdict: SeekFillVerdict) {
        let mut guard = self
            .decision
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(verdict);
        drop(guard);
        self.ready.notify_all();
    }

    /// 等裁定；超时或源已停止一律按**作废**处理（宁可慢，不冒风险）。
    fn wait(&self, timeout: Duration, stop: &AtomicBool) -> SeekFillVerdict {
        let deadline = Instant::now() + timeout;
        let mut guard = self
            .decision
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(verdict) = *guard {
                return verdict;
            }
            if stop.load(Ordering::Relaxed) {
                return SeekFillVerdict::Deny;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return SeekFillVerdict::Deny;
            }
            let (next, _) = self
                .ready
                .wait_timeout(guard, remaining.min(Duration::from_millis(250)))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = next;
        }
    }
}

/// 在飞配额的 RAII 归还：worker 无论从哪条路径退出（预检失败 / 门禁拒绝 /
/// 超时 / 正常完成）都会归还，不会漏。
struct SeekFillQuota(Arc<AtomicUsize>);

impl Drop for SeekFillQuota {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 是否允许"凭据不足也跨节点"（用户显式接受风险）。默认**否**。
///
/// 默认行为是"证据不足就退回单节点"——用户的原话是"静默的数据损坏比慢 5 秒糟
/// 得多"。打开这个开关意味着：只剩**下载后的段间重叠逐字节比对**兜底，而它
/// **覆盖不到段内部**（段内部只有单一节点的数据，没有对照物）。
fn seek_parallel_trust_nodes() -> bool {
    env::var("ERIKA_SEEK_PARALLEL_TRUST_NODES")
        .map(|value| {
            let v = value.trim();
            v != "0" && !v.eq_ignore_ascii_case("false") && !v.eq_ignore_ascii_case("off")
        })
        .unwrap_or(false)
}

/// **跨节点一致性门禁**（纯函数，可单测）。
///
/// 判据（用户定的硬要求：宁慢不脏；每一条都对应一次可复核的实测）：
///   1. 每个节点都必须给出**文件总长**，且**全部相同** —— 否则多半不是同一个对象；
///   2. **`ETag` 全部存在且逐字相同**才放行。这是唯一被实测支持为"强证据"的
///      凭据。**强在哪**：ETag 与内容的关系是**实测钉死的**——完整下载某 cid 的
///      视频后算出的 MD5 为 `c3925abf0154d98095b824a712641d23`，与 upos 节点
///      返回的 ETag **逐字相同**（另一代理实证；同一 cid 的
///      `upos-sz-mirrorcoso1` 与 `upos-sz-mirrorcos` 也给出同一个
///      `"c3925abf0154d98095b824a712641d23"`，且重复请求稳定）。因此对 B 站
///      CDN 而言 **ETag 一致 ≈ 内容一致**：它在"下载之前"就把"几个节点是不是
///      同一份数据"从推断升级成了可比的字节指纹；
///   3. `ETag` 缺失或各节点不同 → **默认作废、退回单节点**。只有用户显式设
///      `ERIKA_SEEK_PARALLEL_TRUST_NODES=1`（接受风险）才放行；
///   4. `Last-Modified`**不作为依据**（实测已证伪：同一内容在不同节点上不同）；
///   5. 接口侧（`playurl`）**没有内容哈希可用**——真实响应里 `dash.video[]` /
///      `dash.audio[]` 只有 `id/baseUrl/backupUrl/bandwidth/mimeType/codecs/
///      width/height/frameRate/sar/startWithSap/SegmentBase/codecid`，
///      **没有 md5/hash/checksum/size**。能用的凭据只有 HTTP 头部。
///
/// 注意这里的依据**止步于实测**：ETag==MD5 是 B 站 CDN 当前行为，不是 HTTP
/// 规范承诺——所以"一致才并行"是硬门禁（宁可慢），而第 2 条之后的**下载后**
/// 还有段间重叠逐字节比对兜底（`run_seek_fill` 的 `overlap_mismatch`），
/// 两道闸都过了才有字节进播放流。
///
/// 返回 `(裁定, 可读理由)`——理由原样进日志与诊断，现场据此核对"为什么没并行"。
fn seek_fill_gate(probes: &[SeekFillProbe], trust_nodes: bool) -> (SeekFillVerdict, String) {
    let Some(first) = probes.first() else {
        return (SeekFillVerdict::Deny, "无预检结果".to_string());
    };
    if probes.iter().any(|probe| probe.total.is_none()) {
        return (
            SeekFillVerdict::Deny,
            "有节点未给出文件总长（Content-Length/Content-Range）".to_string(),
        );
    }
    if probes.iter().any(|probe| probe.total != first.total) {
        return (
            SeekFillVerdict::Deny,
            "各节点声明的文件总长不一致".to_string(),
        );
    }
    let total = first.total.unwrap_or(0);
    let etags: Vec<&str> = probes
        .iter()
        .filter_map(|probe| probe.etag.as_deref())
        .collect();
    if etags.len() == probes.len() && etags.iter().all(|etag| *etag == etags[0]) {
        return (
            SeekFillVerdict::Allow,
            format!(
                "{} 个节点 ETag 一致（{}）、总长一致 {total}B",
                probes.len(),
                etags[0]
            ),
        );
    }
    let detail = if etags.len() < probes.len() {
        format!("{}/{} 个节点给出了 ETag", etags.len(), probes.len())
    } else {
        format!("{} 个节点 ETag 各不相同", etags.len())
    };
    if trust_nodes {
        return (
            SeekFillVerdict::Allow,
            format!(
                "{detail}；用户显式接受风险（ERIKA_SEEK_PARALLEL_TRUST_NODES=1），仅重叠比对兜底"
            ),
        );
    }
    (
        SeekFillVerdict::Deny,
        format!(
            "{detail}，证据不足 → 默认退回单节点（可设 ERIKA_SEEK_PARALLEL_TRUST_NODES=1 显式接受风险）"
        ),
    )
}

/// 并行回填的共享上下文（读线程持有；worker 线程 clone 一份）。
///
/// 存在的理由：worker 是独立 `std::thread`，拿不到源实例与读线程局部量——与旧
/// 路径的 `PendingHttpFetch::spawn` 同一处境（那里也是把 uri/headers/progress/
/// diag_slot 逐个传进去）。这里把同一组量打包，避免七八个参数逐个透传。
#[derive(Clone)]
struct SeekFillContext {
    shared: Arc<StreamShared>,
    http_headers: Vec<(String, String)>,
    /// 实时到达字节计数器（`fetch_http_range` 的 `progress`）：分段的字节在
    /// **到达时**就计入宿主的下载量/带宽估算，与其它路径同口径。
    arrived: Arc<AtomicU64>,
    /// 「最近一次收到字节」时间戳：分段 push 后刷新，让换源判据看到的仍是
    /// "链路在给字节"（**不会被误判成多连接异常**）。
    last_arrival_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    diag_slot: usize,
    /// 读线程的 `in_flight` 标志（诊断 `prefetch_in_flight`：并行期间为 1）。
    reader_in_flight: Arc<AtomicU64>,
    /// **在飞分段配额**（硬上限）：连续拖动时靠它保证并发连接数不被推高——
    /// 新一批看到配额已被占满就整批放弃、回退单连接（那是既有的安全形态）。
    /// 配额在 worker **真正结束时**归还（超时被放弃的段仍占着一条连接）。
    quota: Arc<AtomicUsize>,
    /// 累计计数与最近一次明细。
    diag: Arc<SeekFillDiag>,
}

/// 等到本段就绪（或超时 / 源停止）。`Err(原因码)` = 本批应当回退到单连接。
///
/// 分片等待（250ms）而不是一次等到超时：期间仍能响应 `stop`（源析构不拖 5s）。
/// 等本段的**预检**（下载前凭据）。`None` = 失败/超时/源已停止 → 本批作废。
///
/// 分片等待（250ms）而不是一次等到超时：期间仍能响应 `stop`（源析构不拖满）。
fn wait_seek_fill_probe(ctx: &SeekFillContext, slot: &SeekFillSlot) -> Option<SeekFillProbe> {
    let deadline = Instant::now() + SEEK_PARALLEL_PROBE_TIMEOUT;
    let mut guard = slot.lock();
    loop {
        match &*guard {
            SeekFillSlotState::Probed(probe) => return Some(probe.clone()),
            SeekFillSlotState::Done(_) => return None,
            SeekFillSlotState::Pending => {}
        }
        if ctx.stop.load(Ordering::Relaxed) {
            return None;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let (next, _) = slot
            .ready
            .wait_timeout(guard, remaining.min(Duration::from_millis(250)))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard = next;
    }
}

/// 等本段**数据**就绪（门禁放行之后才有意义）。`Err(原因码)` = 本批应当回退。
fn wait_seek_fill_data(
    ctx: &SeekFillContext,
    slot: &SeekFillSlot,
) -> std::result::Result<SeekFillResponse, &'static str> {
    let deadline = Instant::now() + SEEK_PARALLEL_SEGMENT_TIMEOUT;
    let mut guard = slot.lock();
    loop {
        if matches!(*guard, SeekFillSlotState::Done(_)) {
            // 取走即释放内存（一段可能几百 KB）。
            let previous = std::mem::replace(&mut *guard, SeekFillSlotState::Pending);
            return match previous {
                SeekFillSlotState::Done(Ok(response)) => Ok(response),
                _ => Err("segment_error"),
            };
        }
        if ctx.stop.load(Ordering::Relaxed) {
            return Err("stopped");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("segment_timeout");
        }
        let (next, _) = slot
            .ready
            .wait_timeout(guard, remaining.min(Duration::from_millis(250)))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard = next;
    }
}

/// 起一个分段 worker：**一条连接、一个节点、读完即关**。
///
/// 三个阶段的顺序是**安全性的载体**，不能重排：
///   1. **预检**（1 字节 Range）→ 把响应头交回槽里，供读线程做下载前门禁；
///   2. **等门禁**：没通过就**一个 body 字节都不读**地退出；
///   3. **下载本段**（含前向重叠）→ 交回数据，由读线程按序 push。
///
/// worker 只负责"把这一段字节正确取回并交回槽里"；**不碰窗口**——push 由读线程
/// 按序做（见 `run_seek_fill`），所以既有"窗口只有一个写者"的语义没有被打破，
/// 也就不存在两个写者乱序写入的竞态。
///
/// 预检与下载用的是**同一个 HTTP 客户端**（同一个连接池）→ 同一节点上实际只有
/// **一条 TCP 连接上的两个单次 Range 请求**（用完即断），形态与普通用户无异。
fn spawn_seek_fill_worker(
    context: &SeekFillContext,
    slot: Arc<SeekFillSlot>,
    gate: Arc<SeekFillGate>,
) {
    let ctx = context.clone();
    let worker_slot = Arc::clone(&slot);
    let segment = slot.segment.clone();
    let spawned = thread::Builder::new()
        .name(format!("erika-seek-fill-{}", ctx.diag_slot))
        .spawn(move || {
            // 配额由 RAII guard 归还：预检失败 / 门禁拒绝 / 超时 / 完成，都跑不掉。
            let _quota = SeekFillQuota(Arc::clone(&ctx.quota));
            let agent = http_seek_fill_agent();
            // ---- 阶段 1：下载前预检（1 字节）----
            let probe_started = Instant::now();
            let probe = match fetch_http_range(
                &agent,
                &segment.uri,
                &ctx.http_headers,
                ByteRange {
                    start: segment.start,
                    length: Some(1),
                },
                "http_seek_parallel_probe",
                None,
                http_request_profile(ctx.diag_slot),
            ) {
                Ok(response) => SeekFillProbe {
                    total: response.total_length,
                    etag: response.etag,
                    last_modified: response.last_modified,
                    elapsed: probe_started.elapsed(),
                },
                Err(error) => {
                    worker_slot.publish_done(Err(format!("probe failed: {error}")));
                    return;
                }
            };
            worker_slot.publish_probe(probe);
            // ---- 阶段 2：等下载前门禁裁定 ----
            if gate.wait(SEEK_PARALLEL_PROBE_TIMEOUT, &ctx.stop) != SeekFillVerdict::Allow {
                worker_slot.publish_done(Err("gate denied".to_string()));
                return;
            }
            if ctx.stop.load(Ordering::Relaxed) {
                worker_slot.publish_done(Err("stopped".to_string()));
                return;
            }
            // ---- 阶段 3：下载本段（含前向重叠）----
            let started = Instant::now();
            let request = ByteRange {
                start: segment.request_start(),
                length: Some(segment.request_length()),
            };
            // 复用既有的 `fetch_http_range`（**不另造一套取数逻辑**）：它自带
            // 重试/断点续传、`Content-Range` 起点校验、`If-Range` 实体校验，
            // 并把到达字节实时计入 `arrived`/请求画像。
            let result = fetch_http_range(
                &agent,
                &segment.uri,
                &ctx.http_headers,
                request,
                "http_seek_parallel_range",
                Some(ctx.arrived.as_ref()),
                http_request_profile(ctx.diag_slot),
            );
            // **长度校验是字节正确性的第一道闸**（第二道是读线程 push 前的
            // 代数/位置复核，第三道是段间重叠的逐字节比对）。长度不符一律判失败
            // ——宁可回退单连接，也不拼接来路不明的字节。
            let value = match result {
                Ok(response) if response.bytes.len() as u64 == segment.request_length() => {
                    Ok(SeekFillResponse {
                        bytes: response.bytes,
                        elapsed: started.elapsed(),
                    })
                }
                Ok(response) => Err(format!(
                    "short segment: {} of {} bytes",
                    response.bytes.len(),
                    segment.request_length(),
                )),
                Err(error) => Err(error.to_string()),
            };
            worker_slot.publish_done(value);
        });
    if let Err(error) = spawned {
        context.quota.fetch_sub(1, Ordering::Relaxed);
        slot.publish_done(Err(format!("worker spawn failed: {error}")));
    }
}

/// 回退原因码 → 可读文本（日志用）。
fn seek_fill_reason_text(code: &str) -> String {
    match code {
        "none" => "全部按序采用".to_string(),
        "segment_timeout" => "分段超时".to_string(),
        "segment_error" => "分段失败".to_string(),
        "stopped" => "源已停止".to_string(),
        "generation_changed" => "期间又 seek 了".to_string(),
        "window_moved" => "窗口位置已变".to_string(),
        "window_full" => "窗口已满".to_string(),
        "entity_total_mismatch" => "跨节点文件总长不一致".to_string(),
        "overlap_mismatch" => "跨节点边界字节比对失败".to_string(),
        other => other.to_string(),
    }
}

/// **执行一次 seek 出窗的跨节点并行回填**（阻塞读线程，直到结束或回退）。
///
/// 控制流只有两种出口：
///   - 全部段**按序** push 成功 → 返回，读线程回到正常循环，从新前沿续一条
///     open-ended Range（"用完即关"：N 条分段连接在这一刻全部结束）；
///   - 任一段失败/超时/校验不过/代数变化 → 中止，读线程照旧从 `write_pos` 起
///     open-ended 串行续传；**已 push 的前缀保留**（正确前缀，丢了只会重下）。
///
/// 正常顺播永不进入本函数（`take_seek_fill_plan` 恒为 `None`）。
fn run_seek_fill(ctx: &SeekFillContext, plan: &SeekFillPlan) {
    let started = Instant::now();
    let segment_count = plan.segments.len();
    // ---- 硬上限：本批要不要开？----
    let already = ctx.quota.fetch_add(segment_count, Ordering::Relaxed);
    if already + segment_count > SEEK_PARALLEL_MAX_CONNECTIONS {
        ctx.quota.fetch_sub(segment_count, Ordering::Relaxed);
        ctx.diag.batches.fetch_add(1, Ordering::Relaxed);
        ctx.diag.fallbacks.fetch_add(1, Ordering::Relaxed);
        ctx.diag.last_fallback.store(1, Ordering::Relaxed);
        trace::diagnostic(format!(
            "[erika seek-parallel] slot={} target={} 放弃并行：上批仍有 {} 段在飞（上限 {}）→ 回退单连接串行",
            ctx.diag_slot, plan.start, already, SEEK_PARALLEL_MAX_CONNECTIONS,
        ));
        return;
    }
    ctx.reader_in_flight.store(1, Ordering::Relaxed);
    let slots: Vec<Arc<SeekFillSlot>> = plan
        .segments
        .iter()
        .cloned()
        .map(|segment| Arc::new(SeekFillSlot::new(segment)))
        .collect();
    let gate = Arc::new(SeekFillGate::new());
    for slot in &slots {
        spawn_seek_fill_worker(ctx, Arc::clone(slot), Arc::clone(&gate));
    }
    // ---- 下载前门禁：收齐各节点预检头部 → 裁定 ----
    //
    // **这一步是"流下来之前就确定一致"的实现**：预检只发 1 字节 Range，拿到
    // Content-Length/Content-Range total + ETag；任一个节点没通过就放它走人，
    // 它的 body 一个字节都不会被读（worker 在 gate 上等到 Deny 直接退出）。
    let probes: Vec<Option<SeekFillProbe>> = slots
        .iter()
        .map(|slot| wait_seek_fill_probe(ctx, slot))
        .collect();
    let (verdict, gate_reason) = if probes.iter().any(|probe| probe.is_none()) {
        (
            SeekFillVerdict::Deny,
            "有节点的预检失败/超时（1 字节 Range 都拿不到）".to_string(),
        )
    } else {
        let ready: Vec<SeekFillProbe> = probes.into_iter().flatten().collect();
        seek_fill_gate(&ready, seek_parallel_trust_nodes())
    };
    gate.release(verdict);
    let gate_log: String = slots
        .iter()
        .map(|slot| {
            let state = slot.lock();
            match &*state {
                SeekFillSlotState::Probed(probe) => format!(
                    "{}:total={} etag={} LM={} 预检={}ms",
                    slot.segment.host,
                    probe
                        .total
                        .map_or_else(|| "?".to_string(), |value| value.to_string()),
                    probe.etag.as_deref().unwrap_or("无"),
                    probe.last_modified.as_deref().unwrap_or("-"),
                    probe.elapsed.as_millis(),
                ),
                _ => format!("{}:未交回预检", slot.segment.host),
            }
        })
        .collect::<Vec<_>>()
        .join(" | ");
    trace::diagnostic(format!(
        "[erika seek-parallel] 下载前门禁 slot={} target={}：{} → {}｜{}",
        ctx.diag_slot,
        plan.start,
        gate_reason,
        if verdict == SeekFillVerdict::Allow {
            "放行跨节点并行"
        } else {
            "作废，退回单节点串行"
        },
        gate_log,
    ));
    if verdict == SeekFillVerdict::Deny {
        ctx.reader_in_flight.store(0, Ordering::Relaxed);
        ctx.diag.batches.fetch_add(1, Ordering::Relaxed);
        ctx.diag.gate_denied.fetch_add(1, Ordering::Relaxed);
        ctx.diag.fallbacks.fetch_add(1, Ordering::Relaxed);
        ctx.diag.last_fallback.store(1, Ordering::Relaxed);
        ctx.diag
            .last_segments
            .store(segment_count as u64, Ordering::Relaxed);
        ctx.diag
            .last_gate_reason
            .lock()
            .map(|mut slot| *slot = gate_reason)
            .ok();
        return;
    }
    // ---- 按序采用 ----
    let mut pushed_bytes = 0u64;
    let mut pushed_segments = 0usize;
    let mut fallback = "none";
    let mut reports: Vec<String> = Vec::with_capacity(segment_count);
    let mut tail: Vec<u8> = Vec::new();
    // 注：`ETag`/文件总长的一致性判定已经在**下载前**做过（见上面的 `gate`），
    // 这里只剩"下载后"的两道校验：边界重叠逐字节比对 + 按序 push 前的窗口复核。
    let mut overlap_mismatch = false;
    for (index, slot) in slots.iter().enumerate() {
        let response = match wait_seek_fill_data(ctx, slot) {
            Ok(response) => response,
            Err(reason) => {
                fallback = reason;
                break;
            }
        };
        let segment = &slot.segment;
        // ① **边界重叠逐字节比对**（跨节点字节一致性的**直接证据**，也是
        //    "下载后再验一次"的那道闸）。不一致 → 整批作废：
        //    **绝不把对不上的字节拼进播放流**。
        let overlap = usize::try_from(segment.overlap).unwrap_or(0);
        if overlap > 0
            && (response.bytes.len() < overlap
                || tail.len() < overlap
                || response.bytes[..overlap] != tail[..overlap])
        {
            overlap_mismatch = true;
            fallback = "overlap_mismatch";
            break;
        }
        // ② **按序 push**：只 push 自己那一份（重叠字节已经随前一段进窗口了）。
        //    三道守卫全部在**同一把窗口锁内**判定——与既有读循环的"读取—判定—
        //    写入必须原子"是同一条教训（旧实现踩过静默错位）。
        let rejected = {
            let mut g = lock_stream(&ctx.shared);
            if g.generation != plan.generation {
                Some("generation_changed")
            } else if g.write_pos() != segment.start {
                Some("window_moved")
            } else if !g.has_space() {
                Some("window_full")
            } else {
                g.push(&response.bytes[overlap..]);
                None
            }
        };
        if let Some(reason) = rejected {
            fallback = reason;
            break;
        }
        ctx.shared.data_cv.notify_all();
        ctx.last_arrival_ms.store(now_ms(), Ordering::Relaxed);
        pushed_bytes = pushed_bytes.saturating_add((response.bytes.len() - overlap) as u64);
        pushed_segments += 1;
        reports.push(format!(
            "#{} host={} start={} len={}B 耗时={}ms",
            index,
            segment.host,
            segment.start,
            response.bytes.len() - overlap,
            response.elapsed.as_millis(),
        ));
        // 保留本段末尾（长度 = 下一段的前向重叠量），供下一段比对。
        let next_overlap = plan
            .segments
            .get(index + 1)
            .and_then(|next| usize::try_from(next.overlap).ok())
            .unwrap_or(0);
        tail = if next_overlap > 0 {
            let take = response.bytes.len().min(next_overlap);
            response.bytes[response.bytes.len() - take..].to_vec()
        } else {
            Vec::new()
        };
    }
    ctx.reader_in_flight.store(0, Ordering::Relaxed);

    // ---- 结算：计数 + 最近一次明细 ----
    let elapsed = started.elapsed();
    let diag = ctx.diag.as_ref();
    let expected_ms = if plan.per_conn_bps > 0.0 {
        (pushed_bytes as f64 / plan.per_conn_bps * 1000.0) as u64
    } else {
        0
    };
    diag.batches.fetch_add(1, Ordering::Relaxed);
    diag.connections
        .fetch_add(segment_count as u64, Ordering::Relaxed);
    diag.bytes.fetch_add(pushed_bytes, Ordering::Relaxed);
    diag.last_segments
        .store(segment_count as u64, Ordering::Relaxed);
    diag.last_bytes.store(pushed_bytes, Ordering::Relaxed);
    diag.last_ms
        .store(elapsed.as_millis() as u64, Ordering::Relaxed);
    diag.last_expected_ms.store(expected_ms, Ordering::Relaxed);
    diag.last_per_conn_bps
        .store(plan.per_conn_bps.to_bits(), Ordering::Relaxed);
    diag.last_fallback
        .store(u64::from(fallback != "none"), Ordering::Relaxed);
    if fallback != "none" {
        diag.fallbacks.fetch_add(1, Ordering::Relaxed);
    }
    if overlap_mismatch {
        diag.overlap_mismatch.fetch_add(1, Ordering::Relaxed);
    }
    diag.overlap_ok.fetch_add(
        u64::from(overlap_mismatch == false && pushed_segments == segment_count),
        Ordering::Relaxed,
    );
    if fallback == "none" && pushed_bytes > 0 {
        diag.note_gain(
            pushed_bytes as f64 / elapsed.as_secs_f64(),
            plan.per_conn_bps,
        );
    }

    // ---- 日志：**无条件输出**的运营诊断通道（`trace::diagnostic`），用户拖一次
    //      进度条就能在日志里看到这一行，不需要开任何 trace 开关 ----
    let hosts = plan
        .segments
        .iter()
        .map(|segment| segment.host.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let single_conn_ms = if plan.per_conn_bps > 0.0 {
        (plan.bytes as f64 / plan.per_conn_bps * 1000.0) as u64
    } else {
        0
    };
    trace::diagnostic(format!(
        "[erika seek-parallel] slot={} target={} 跨节点 {} 段（{}）共采用 {}B/{}B 用时 {}ms；同一批数据单连接预计 {}ms（期望 {}ms @ 单流 {:.0}KB/s）；结果={}",
        ctx.diag_slot,
        plan.start,
        segment_count,
        hosts,
        pushed_bytes,
        plan.bytes,
        elapsed.as_millis(),
        single_conn_ms,
        expected_ms,
        plan.per_conn_bps / 1024.0,
        seek_fill_reason_text(fallback),
    ));
    if fallback != "none" {
        trace::diagnostic(format!(
            "[erika seek-parallel] 已回退单连接串行续传（已采用 {}/{} 段）；段明细（按序）: {}",
            pushed_segments,
            segment_count,
            if reports.is_empty() {
                "（无）".to_string()
            } else {
                reports.join(" | ")
            },
        ));
    } else {
        trace::diagnostic(format!(
            "[erika seek-parallel] 段明细（按序）: {}",
            reports.join(" | ")
        ));
    }
}

/// 发一条 `Range: bytes=<start>-`（开放结尾）并校验响应头。
///
/// **请求计数点与 `fetch_http_range` 同口径**：在唯一真正发请求的那一处
/// （`call()` 之前）记 1，因此每次重连/续传都如实各记 1（见
/// [`SourceDiagSnapshot::http_requests`]）。
///
/// 头校验（与 `fetch_http_range` 的守卫语义一致，防止静默错位）：
///   - 206：`Content-Range` 起点必须等于请求起点（换实体/换分片会违约）；
///   - 200：只在 `start == 0` 时接受（服务端忽略 Range 返回全片），
///     `start > 0` 直接判终局错误——把整片从头当续传字节拼接是静默数据损坏。
fn open_sequential_response(
    agent: &HttpFetch,
    uri: &str,
    http_headers: &[(String, String)],
    start: u64,
    diag_slot: usize,
    cancellation: SourceCancellation,
) -> Result<SequentialResponse> {
    let header = http_range_header(ByteRange {
        start,
        length: None,
    });
    let mut request = agent.get(uri).header("Range", &header);
    for (name, value) in http_headers {
        request = request.header(name, value);
    }
    http_trace_log(format!(
        "{{\"event\":\"http_stream_open\",\"start\":{},\"range\":\"{}\"}}",
        start,
        json_escape(&header),
    ));
    if let Some(profile) = http_request_profile(diag_slot) {
        profile.record_request();
    }
    let response = send_http_request(request).map_err(|failure| {
        http_trace_log(format!(
            "{{\"event\":\"http_stream_open_error\",\"phase\":\"request\",\"start\":{},\"error\":\"{}\"}}",
            start,
            json_escape(&failure.message()),
        ));
        http_failure_after_retries(&failure)
    })?;
    let status = response.status().as_u16();
    let content_range = response
        .headers()
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let content_length = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let mut total = content_range.as_deref().and_then(parse_content_range_total);
    let mut end = content_range.as_deref().and_then(parse_content_range_end);
    let mut reader = BodyReader::new(cancellation, response);
    match status {
        206 => {
            if let Some(served) = content_range.as_deref().and_then(parse_content_range_start)
                && served != start
            {
                return Err(SourceError::Http(format!(
                    "sequential stream served range from {served}, expected {start}"
                )));
            }
            if total.is_none() {
                // 没有 Content-Range 的 206：用 Content-Length 反推总长。
                total = content_length.map(|length| start.saturating_add(length));
            }
            if end.is_none() {
                end = total.map(|total| total.saturating_sub(1));
            }
        }
        200 => {
            if total.is_none() {
                total = content_length;
            }
            end = total.map(|total| total.saturating_sub(1));
            if start > 0 {
                // 服务端忽略 Range：从字节 0 发来整片。丢弃 `start` 之前的
                // 前缀，让调用方拿到的字节流与 `write_pos` 对齐——把整片
                // 直接当"从 start 开始的续传字节"写窗就是静默错位。
                // 前缀可能很大（seek 到文件深处），这是服务端不支持 Range
                // 的固有代价；丢到 EOF 说明起点已在文件尾。
                let mut skipped = 0u64;
                let mut discard = [0u8; HTTP_RECV_CHUNK_BYTES];
                while skipped < start {
                    // 只读到 `start` 为止：按整块读会多丢最多一块，续传字节流
                    // 就从错误的位置开始（静默错位）。
                    let remaining = (start - skipped).min(discard.len() as u64) as usize;
                    match reader.read(&mut discard[..remaining]) {
                        Ok(0) => break,
                        Ok(n) => skipped = skipped.saturating_add(n as u64),
                        Err(error) => {
                            http_trace_log(format!(
                                "{{\"event\":\"http_stream_open_error\",\"phase\":\"discard\",\"start\":{},\"skipped\":{},\"error\":\"{}\"}}",
                                start,
                                skipped,
                                json_escape(&error.to_string()),
                            ));
                            return Err(SourceError::Transient(error.to_string()));
                        }
                    }
                }
                if skipped < start && total.is_none() {
                    // 没有 Content-Length，丢了不到 start 就到 EOF：丢出去
                    // 多少就是文件全长，据此标出终点。
                    total = Some(skipped);
                    end = total.map(|total| total.saturating_sub(1));
                }
                http_trace_log(format!(
                    "{{\"event\":\"http_stream_open_200_skipped\",\"start\":{},\"skipped\":{},\"total\":{}}}",
                    start,
                    skipped,
                    total.map_or_else(|| "null".to_string(), |value| value.to_string()),
                ));
            }
        }
        _ => {
            return Err(SourceError::Http(format!(
                "unexpected HTTP status {status} for Range request"
            )));
        }
    }
    http_trace_log(format!(
        "{{\"event\":\"http_stream_open_ok\",\"start\":{},\"status\":{},\"end\":{},\"total\":{}}}",
        start,
        status,
        end.map_or_else(|| "null".to_string(), |value| value.to_string()),
        total.map_or_else(|| "null".to_string(), |value| value.to_string()),
    ));
    Ok(SequentialResponse {
        reader: Box::new(reader),
        end,
        total,
    })
}

/// 顺序长读线程（**一条持续读的连接**）。
///
/// 与旧模型的分界就在"缓冲满时做什么"：
///   - 旧模型：关掉连接，等下次需要时再发一个新的 256KB Range
///     → 请求数 ∝ 播放时长（一小时几千次）；
///   - 本模型：**只是不再调 `read()`** → 内核接收缓冲填满 → TCP 窗口收缩 →
///     服务端自然降速。连接不动、请求不发。mpv / 浏览器 / 官方客户端都这样。
///
/// **会重发请求的情形（穷举；正常顺序播放期间一条都不该命中）**：
///   1. **seek 到窗口外**：读路径 `reset()` 令 `generation` +1，线程在下一个
///      64KB 块边界发现代数变了 → 放弃当前连接，从新 `write_pos` 重发；
///   2. **换 host**：`maybe_switch_host` 改写共享 uri 并把 `host_epoch` +1，
///      线程发现 epoch 变了 → 断开、用新 host 从 `write_pos` 续（已收字节全在
///      窗口里，不丢）；
///   3. **服务端断开 / 读到 EOF**：连接级 `Ok(0)`。若已到服务端声明的终点
///      （或 `total`）→ 标 `eof` 收工（**不再重连**）；否则视为提前收线，
///      从 `write_pos` 静默续传（+1 请求，字节不丢）；
///   4. **瞬时错误重试**：`Err(_)` / 连接失败。分类沿用
///      `http_error_is_retryable`（5xx 与传输错误 = 瞬时）与
///      `http_failure_after_retries`（4xx = 终局，写进窗口让宿主换源）；
///      退避 200ms 起翻倍、上限 2s（既有 `RETRY_BACKOFF` 节奏）。
///
/// 另外两个"不再无限打请求"的守卫：
///   - **连续两次连接都 0 字节** → 认定文件到尾（服务端对 open-ended Range
///     立刻收线时，无限重连会退化成 200ms 一个请求的**请求风暴**）；
///   - 起点已 ≥ `total` → 直接标 `eof`，连都不连。
fn sequential_reader_loop(
    shared: Arc<StreamShared>,
    uri: Arc<Mutex<String>>,
    http_headers: Vec<(String, String)>,
    host_epoch: Arc<AtomicU64>,
    arrived: Arc<AtomicU64>,
    in_flight: Arc<AtomicU64>,
    reconnects: Arc<StreamReconnects>,
    stop: Arc<AtomicBool>,
    diag_slot: usize,
    fill: Arc<AtomicU64>,
    journal: Arc<RangeJournal>,
    last_arrival_ms: Arc<AtomicU64>,
    done: mpsc::Sender<()>,
    seek_fill: Arc<SeekFillDiag>,
    seek_fill_quota: Arc<AtomicUsize>,
    cancellation: SourceCancellation,
) {
    let agent = http_stream_agent();
    let mut backoff = Duration::from_millis(200);
    let mut empty_connects = 0u32;
    // 填窗期吞吐计量（口径见 `FillMeter` / `fill_throughput_bps` 注释）。
    let mut meter = FillMeter::new(diag_slot, fill);
    // **判决实验**用：请求起点序列 + 与上一次前沿的关系（见 `RangeJournal`）。
    let mut requests_sent = 0u64;
    let mut last_front = 0u64;
    // 滴灌节奏（窗口满时的小口读）。
    let mut last_drip = Instant::now();
    // **seek 出窗的跨节点并行回填**上下文（每批一个，见 `run_seek_fill`）。
    // 只在 `take_seek_fill_plan()` 拿到计划时才会被用到——正常顺播全程零开销。
    let seek_fill_ctx = SeekFillContext {
        shared: Arc::clone(&shared),
        http_headers: http_headers.clone(),
        arrived: Arc::clone(&arrived),
        last_arrival_ms: Arc::clone(&last_arrival_ms),
        stop: Arc::clone(&stop),
        diag_slot,
        reader_in_flight: Arc::clone(&in_flight),
        quota: Arc::clone(&seek_fill_quota),
        diag: Arc::clone(&seek_fill),
    };
    while !stop.load(Ordering::Relaxed) {
        // ---- 0. seek 出窗的跨节点并行回填（**一次性**：顺播恒取不到计划）----
        //
        // 计划由 demux 线程在 `stream_seek_hint` 的重设分支里装上（同一把锁内
        // 完成 reset + 装计划）。取不到 = 正常顺播 → 下面一行代码都不受影响。
        let plan = lock_stream(&shared).take_seek_fill_plan();
        if let Some(plan) = plan.as_ref() {
            // 预检 + 门禁 + 下载 + 按序 push 全在这一个调用里（阻塞到结束或回退）。
            run_seek_fill(&seek_fill_ctx, plan);
            // 结算链路吞吐基线：并行刚结束，别让"上批的旧速率"污染随后的填窗采样。
            meter.pause();
            continue;
        }
        // ---- 1. 背压：没有空位就不连（更不发请求）----
        let (start, generation, total) = {
            let g = lock_stream(&shared);
            if !g.has_space() {
                // 窗口满 → 不采样链路吞吐（否则会被"故意不读"钳成消费速率），
                // 只重置基线（见 `FillMeter::pause`）。
                meter.pause();
                drop(g);
                thread::sleep(STREAM_IDLE_POLL);
                continue;
            }
            (g.write_pos(), g.generation, g.total)
        };
        if let Some(total) = total
            && start >= total
        {
            let mut g = lock_stream(&shared);
            g.eof = true;
            drop(g);
            shared.data_cv.notify_all();
            continue;
        }
        let uri_now = match uri.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let epoch_now = host_epoch.load(Ordering::Relaxed);
        if !backoff.is_zero() {
            thread::sleep(backoff);
            // 睡过即清：退避是"这一次重连要等多久"，不是常驻状态——正常续传
            // 必须立刻重连（见 `stream_retry_backoff`）。
            backoff = Duration::ZERO;
        }
        // ---- 2. 一条 open-ended Range 连接 ----
        //
        // **判决实验的取证点**：每次真正发请求前，把"起点 vs 上一次连接结束时的
        // 前沿"记进 `RangeJournal`（限频日志 + 诊断分桶）。单调递增 = 断点续传
        // （服务端/传输侧断我们）；前后跳跃 = demux 在 seek（与 CDN 无关）。
        // 首个请求没有前沿，不分类。
        let front_before = (requests_sent > 0).then_some(last_front);
        journal.note_request(start, front_before, diag_slot);
        requests_sent = requests_sent.saturating_add(1);
        let opened = match open_sequential_response(
            &agent,
            &uri_now,
            &http_headers,
            start,
            diag_slot,
            cancellation.child(),
        ) {
            Ok(opened) => {
                backoff = Duration::ZERO; // 连接建立成功：后续若断，立即续传
                opened
            }
            Err(error) => {
                if let SourceError::Http(message) = error {
                    // 终局（4xx / Range 被忽略 / Content-Range 违约）：写进窗口，
                    // 读方取走即上报宿主换源。本线程继续退避重试——换 host 后
                    // epoch 变化会让我们用新 uri 重来。
                    lock_stream(&shared).error = Some(message);
                    shared.data_cv.notify_all();
                }
                reconnects.bump(ReconnectCause::Empty);
                backoff = stream_retry_backoff(backoff);
                continue;
            }
        };
        {
            let mut g = lock_stream(&shared);
            if g.total.is_none() {
                g.total = opened.total;
            }
        }
        shared.data_cv.notify_all();
        // ---- 3. 持续读：每块之前重新判背压 / 代数 / host ----
        in_flight.store(1, Ordering::Relaxed);
        let mut body = opened.reader;
        let mut chunk = [0u8; HTTP_RECV_CHUNK_BYTES];
        let mut received_here = 0u64;
        let mut restart = false;
        let mut cause = ReconnectCause::EarlyClose;
        loop {
            if stop.load(Ordering::Relaxed) {
                in_flight.store(0, Ordering::Relaxed);
                let _ = done.send(());
                return;
            }
            // ---- 背压 / 滴灌判定（一次加锁）----
            let budget_full;
            {
                let g = lock_stream(&shared);
                if g.generation != generation {
                    cause = ReconnectCause::Reset;
                    restart = true;
                    break;
                }
                if g.eof {
                    break;
                }
                if g.is_saturated() {
                    // **彻底饱和**（预算 + 滴灌余量都用尽）→ 完全不读。
                    // 这时连接会进入"写空闲"（若用户长时间暂停/消费停滞），
                    // 但内存硬上限优先；恢复消费后立刻继续读。
                    drop(g);
                    meter.pause();
                    thread::sleep(STREAM_IDLE_POLL);
                    continue;
                }
                // 前向预算已满、但还有滴灌余量 → 只按滴灌节奏小口读
                // （见 `STREAM_DRIP_*`：不让 CDN 看到写空闲而重置连接）。
                budget_full = g.front_reached_budget();
            }
            if budget_full {
                let elapsed = last_drip.elapsed();
                if !stream_drip_enabled() || elapsed < STREAM_DRIP_INTERVAL {
                    // 滴灌关闭（`ERIKA_HTTP_DRIP=0`）→ 回到旧的"完全不读"行为。
                    thread::sleep(STREAM_IDLE_POLL);
                    continue;
                }
                last_drip = Instant::now();
                // 滴灌期间不采链路吞吐（刻意的小口慢读不代表链路能力）。
                meter.pause();
            }
            if host_epoch.load(Ordering::Relaxed) != epoch_now {
                cause = ReconnectCause::HostSwitch;
                restart = true;
                break;
            }
            // 滴灌只读一小块；正常填窗读满块。
            let limit = if budget_full {
                STREAM_DRIP_BYTES
            } else {
                HTTP_RECV_CHUNK_BYTES
            };
            match body.read(&mut chunk[..limit]) {
                Ok(0) => break,
                Ok(n) => {
                    // **推送前必须在同一把锁内复核代数**：seek 可能恰好落在
                    // `read()` 在途期间（`reset` 把窗口挪到了新起点），此时这块
                    // 字节属于**旧位置**——直接 push 会被当成新起点的数据，造成
                    // 静默错位（本文件历史上"坏包"的同类陷阱）。同锁复核把
                    // "读取—判定—写入"变成原子步骤，彻底堵死这条路径。
                    let write_pos = {
                        let mut g = lock_stream(&shared);
                        if g.generation != generation || g.eof {
                            None
                        } else {
                            g.push(&chunk[..n]);
                            Some(g.write_pos())
                        }
                    };
                    let Some(write_pos) = write_pos else {
                        cause = ReconnectCause::Reset;
                        restart = true;
                        break;
                    };
                    arrived.fetch_add(n as u64, Ordering::Relaxed);
                    // 换源判据的"有字节到达"时间戳（顺序长读路径的唯一维护点）。
                    last_arrival_ms.store(now_ms(), Ordering::Relaxed);
                    // 填窗期样本：**滴灌的小口慢读不算**（那是刻意的，不代表
                    // 链路能力；记进去会把宿主的升画质依据拖低）。
                    if !budget_full {
                        meter.record(n as u64);
                    }
                    if let Some(profile) = http_request_profile(diag_slot) {
                        profile.record_bytes(n as u64);
                    }
                    shared.data_cv.notify_all();
                    received_here = received_here.saturating_add(n as u64);
                    empty_connects = 0;
                    let reached_total = lock_stream(&shared)
                        .total
                        .is_some_and(|total| write_pos >= total);
                    let reached_end = opened.end.is_some_and(|end| write_pos > end);
                    if reached_total || reached_end {
                        break;
                    }
                }
                // 瞬时：已收字节留在窗口里，退避后从 write_pos 续传。
                Err(_) => break,
            }
        }
        in_flight.store(0, Ordering::Relaxed);
        if stop.load(Ordering::Relaxed) {
            let _ = done.send(());
            return;
        }
        let (write_pos, total, already_eof) = {
            let g = lock_stream(&shared);
            (g.write_pos(), g.total, g.eof)
        };
        // 判决实验的基线：**本次连接自己**把前沿推到了哪里（`请求起点 + 本次收到
        // 字节数`）。不能用窗口的 `write_pos`：seek 可能在连接间隙里把窗口
        // `reset` 到更低的偏移，那样"回拖"会被误判成 contiguous（本测试第一版
        // 就踩了这个坑）。用连接自身的账才对得上"上一次看到的下载前沿"。
        last_front = start.saturating_add(received_here);
        // 每次连接给了多少字节：这是"断点位置分布"（疑问 1）最直接的一手数据。
        reconnects.note_connection(received_here);
        if already_eof {
            continue;
        }
        if restart {
            // seek / 换 host：立即重连（不退避）——这正是"一次 seek 只 +1 请求"。
            reconnects.bump(cause);
            continue;
        }
        let reached_end = opened.end.is_some_and(|end| write_pos > end);
        let reached_total = total.is_some_and(|total| write_pos >= total);
        // **有界分片判定**（2026-09-04）：服务端用 `Content-Range: bytes <start>-<end>/<total>`
        // 明确表示"这次只给这一段"（`end + 1 < total`）。这不是"到文件尾"，而是
        // 分片式 CDN 的正常形态：必须从 `write_pos` 续传，**绝不能把流判成 EOF**
        // （旧实现会在分片末尾直接 `eof = true`，音频就这样停在 2.7MB/8MB 再也回不来）。
        // `total` 未知时（`Content-Range: bytes x-y/*`）也按分片处理：宁可多要一次，
        // 也不能凭空把流截断。
        let chunked_span = match (opened.end, total) {
            (Some(end), Some(total)) => end.saturating_add(1) < total,
            (Some(_), None) => true,
            _ => false,
        };
        if reached_total || (reached_end && !chunked_span) {
            // 服务端声明的**文件终点**已到：**不再重连**。
            let mut g = lock_stream(&shared);
            g.eof = true;
            drop(g);
            shared.data_cv.notify_all();
            continue;
        }
        if chunked_span && reached_end {
            // **服务端给的片到期**（我们发的是开放结尾，片是服务端切的）→ 立即
            // 从 `write_pos` 续下一片：+1 请求，字节不丢、窗口不动、demux 无感。
            reconnects.bump(ReconnectCause::PlannedChunk);
            backoff = Duration::ZERO;
            continue;
        }
        if received_here == 0 {
            empty_connects = empty_connects.saturating_add(1);
            // 连续两次连接一个字节都没收到 + **总长未知**：判定服务端对 open-ended
            // Range 立刻收线（或已到尾），标 eof 收工——否则就是 200ms 一个请求的
            // **请求风暴**（这条守卫保留）。
            //
            // 但**总长已知且还没读到尾**时绝不这样收工（2026-09-04 订正）：那多半是
            // 限流/瞬时错误，标 eof 会让这条流永远停在半途（真机实测的音频
            // 2.7MB/8MB 停滞），宿主只能靠整源重建自愈。改为退避重试，由上层的
            // 等待窗口（`transient_wait`）与宿主换源策略兜底。
            if empty_connects >= 2 && total.is_none() {
                let mut g = lock_stream(&shared);
                g.eof = true;
                drop(g);
                shared.data_cv.notify_all();
                continue;
            }
            // 一个字节都没收到：退避后再试（限流/瞬时错误）。
            reconnects.bump(ReconnectCause::Empty);
            backoff = stream_retry_backoff(backoff);
            continue;
        }
        empty_connects = 0;
        // 服务端提前收线：从 `write_pos` 静默续传（+1 请求，字节不丢）。
        // 只在"收到的字节连一个块都不到"时才退避——那种形态下无限立即重连
        // 就是请求风暴；正常跑到一半被掐断则立即续上，demux 侧无感。
        reconnects.bump(ReconnectCause::EarlyClose);
        if received_here < HTTP_RECV_CHUNK_BYTES as u64 {
            backoff = stream_retry_backoff(backoff);
        } else {
            backoff = Duration::ZERO;
        }
    }
    let _ = done.send(());
}

/// 顺序长读**连接重建的原因**（诊断分桶用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconnectCause {
    /// 服务端/传输侧提前收线（**没到本次请求声明的片尾**）：CDN 或中间链路。
    EarlyClose,
    /// 连接建立了但一个字节都没收到（限流/瞬时错误）。
    Empty,
    /// 本地 `reset()`（`generation` 变化）：seek 出窗 / 回拖超出后向窗口。
    Reset,
    /// 换 host（`host_epoch` 变化）。
    HostSwitch,
    /// **按计划到的片尾**（有界 Range 的这一片读完了）→ 正常续下一片。
    PlannedChunk,
}

/// 顺序长读的连接重建计数（**按原因分桶**，单调递增）。
///
/// 存在的理由（用户真机实测暴露）：一条长连接被反复重建时，光看"请求数"分不清
/// 是谁的锅——我们的 `reset()`（seek/回拖/乱序读）、换 host、**服务端自己提前
/// 收线**，还是**我们自己按计划切片**。五个桶把原因分开，宿主与现场一眼可判，
/// 也是"顺序播放期间不该重建"这条验收的判据（见诊断 `stream_reconnects_*`）。
#[derive(Default)]
struct StreamReconnects {
    early_close: AtomicU64,
    empty: AtomicU64,
    reset: AtomicU64,
    host_switch: AtomicU64,
    planned_chunk: AtomicU64,
    /// 最近一次连接实际收到的字节数（"断点位置分布"的一手数据）。
    last_connection_bytes: AtomicU64,
    /// 连接收到字节数的最大值（看有没有长连接成功跑出过大块）。
    max_connection_bytes: AtomicU64,
}

impl StreamReconnects {
    /// 每条连接结束时记一次"这次给了多少字节"。
    fn note_connection(&self, bytes: u64) {
        self.last_connection_bytes.store(bytes, Ordering::Relaxed);
        let mut current = self.max_connection_bytes.load(Ordering::Relaxed);
        while bytes > current {
            match self.max_connection_bytes.compare_exchange_weak(
                current,
                bytes,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    fn last_connection_bytes(&self) -> u64 {
        self.last_connection_bytes.load(Ordering::Relaxed)
    }

    fn max_connection_bytes(&self) -> u64 {
        self.max_connection_bytes.load(Ordering::Relaxed)
    }

    fn bump(&self, cause: ReconnectCause) {
        let counter = match cause {
            ReconnectCause::EarlyClose => &self.early_close,
            ReconnectCause::Empty => &self.empty,
            ReconnectCause::Reset => &self.reset,
            ReconnectCause::HostSwitch => &self.host_switch,
            ReconnectCause::PlannedChunk => &self.planned_chunk,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn get(&self, cause: ReconnectCause) -> u64 {
        let counter = match cause {
            ReconnectCause::EarlyClose => &self.early_close,
            ReconnectCause::Empty => &self.empty,
            ReconnectCause::Reset => &self.reset,
            ReconnectCause::HostSwitch => &self.host_switch,
            ReconnectCause::PlannedChunk => &self.planned_chunk,
        };
        counter.load(Ordering::Relaxed)
    }

    /// 全部原因之和 = 这条流的连接重建总次数（= 除首个请求外多出来的请求数）。
    fn total(&self) -> u64 {
        self.early_close
            .load(Ordering::Relaxed)
            .saturating_add(self.empty.load(Ordering::Relaxed))
            .saturating_add(self.reset.load(Ordering::Relaxed))
            .saturating_add(self.host_switch.load(Ordering::Relaxed))
            .saturating_add(self.planned_chunk.load(Ordering::Relaxed))
    }
}

/// 一次 Range 请求相对**上一次连接结束时的前沿**的形态分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeKind {
    /// 起点 == 前沿：**断点续传**（服务端/传输侧结束了连接，我们从断点接上）。
    Contiguous,
    /// 起点 > 前沿：前向跳读（demuxer 往前 seek / 我们自己跳窗）。
    ForwardJump,
    /// 起点 < 前沿：**回拖/后退读**（demux 在 seek）——这类请求与 CDN 无关。
    BackwardJump,
}

impl RangeKind {
    fn label(self) -> &'static str {
        match self {
            RangeKind::Contiguous => "contiguous",
            RangeKind::ForwardJump => "forward_jump",
            RangeKind::BackwardJump => "backward_jump",
        }
    }
}

/// **请求起点序列日志**（"每 ~1MB 重建一次"的判决实验载体）。
///
/// 每次**真正发出** Range 请求时记一条，按"与上一次前沿的关系"分三类：
///   - `Contiguous`：起点 == 前沿 → **断点续传**（服务端/传输侧结束了连接）；
///   - `ForwardJump`：起点 > 前沿 → 前向跳读；
///   - `BackwardJump`：起点 < 前沿 → **回拖/后退读 = demux 在 seek**。
///
/// **判据**（对外调研的一手案例：`blog.dreamfever.me` 那篇 22MB mp4 → 1585 个
/// 开放结尾请求，根因是 demux 前后跳读、起点序列 `1562133 → 1999391 → 1577362`
/// 来回跳）：
///   - 只要 `BackwardJump`/`ForwardJump` 明显 → 请求放大来自 **demux 的前后跳读**，
///     与 CDN 无关，**先不要动请求模型**（改分片只会让请求更多）；
///   - 只有 `Contiguous` 占绝对多数 → 才是"连接被断、我们从断点续"。
///
/// 日志限频：最多 1 秒一条，不刷爆现场日志。
#[derive(Default)]
struct RangeJournal {
    contiguous: AtomicU64,
    forward_jump: AtomicU64,
    backward_jump: AtomicU64,
    /// 最近一次请求的起点（0 = 还没发过请求）。
    last_start: AtomicU64,
    /// 最近若干次请求起点（限频日志里打出来，便于事后看形态）。
    recent: Mutex<VecDeque<u64>>,
    /// 限频用：上次打日志的 epoch 毫秒。
    last_log_ms: AtomicU64,
}

/// 最近请求起点的保留个数（日志里一次打出这么多，够看出"单调/跳跃"）。
const RANGE_JOURNAL_RECENT: usize = 8;
/// 请求起点日志的限频间隔（毫秒）。
const RANGE_JOURNAL_LOG_MS: u64 = 1000;

impl RangeJournal {
    /// 记一次即将发出的 Range 请求。`front` = 上一次连接结束时的已收前沿
    /// （`None` = 首个请求，不分类）。返回分类结果供 trace 使用。
    fn note_request(&self, start: u64, front: Option<u64>, slot: usize) -> Option<RangeKind> {
        let kind = front.map(|front| {
            if start == front {
                self.contiguous.fetch_add(1, Ordering::Relaxed);
                RangeKind::Contiguous
            } else if start > front {
                self.forward_jump.fetch_add(1, Ordering::Relaxed);
                RangeKind::ForwardJump
            } else {
                self.backward_jump.fetch_add(1, Ordering::Relaxed);
                RangeKind::BackwardJump
            }
        });
        self.last_start.store(start, Ordering::Relaxed);
        if let Ok(mut recent) = self.recent.lock() {
            if recent.len() == RANGE_JOURNAL_RECENT {
                recent.pop_front();
            }
            recent.push_back(start);
        }
        // 限频日志：≥1s 一条，把最近起点序列与三个分桶一起打出来——这是
        // "判决假说 D（demux 跳读 vs 断点续传）"最直接的现场证据。
        let now = now_ms();
        let last = self.last_log_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= RANGE_JOURNAL_LOG_MS
            && self
                .last_log_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let recent = self
                .recent
                .lock()
                .map(|recent| {
                    recent
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            http_trace_log(format!(
                "{{\"event\":\"stream_range_journal\",\"slot\":{slot},\"kind\":\"{}\",\"start\":{start},\"front\":{},\"recent_starts\":\"[{recent}]\",\"contiguous\":{},\"forward_jump\":{},\"backward_jump\":{}}}",
                kind.map(RangeKind::label).unwrap_or("first"),
                front.map_or_else(|| "null".to_string(), |front| front.to_string()),
                self.contiguous.load(Ordering::Relaxed),
                self.forward_jump.load(Ordering::Relaxed),
                self.backward_jump.load(Ordering::Relaxed),
            ));
        }
        kind
    }
}

/// **填窗期吞吐计量器**（读线程私有）。
///
/// 口径（与诊断字段 [`SourceDiagSnapshot::fill_throughput_bps`] 的注释严格一致）：
/// 只在窗口**未满**、读线程真的在读 socket 的时间片上累计字节与时长，按
/// 500ms 结算成瞬时速率并做 0.3/0.7 EMA；窗口满（背压）时**不采样、不衰减**。
///
/// 为什么可信：读循环里能走到 `body.read()` 就说明刚刚判过 `has_space()`
/// （窗口没满），所以计进来的字节不受"客户端故意不读"的钳制——它反映的是
/// **链路能推多快**，而不是消费速率。宿主要判"能不能升画质"要的正是这个口径
/// （`est_bps` 在窗口满时会被消费速率带偏，是另一个口径：全部到达字节的速率）。
struct FillMeter {
    /// 本采样窗口起点（`pause` 会重置它，见下）。
    at: Instant,
    /// 本采样窗口内累计的字节。
    bytes: u64,
    /// EMA 结果（字节/秒）。
    ema: f64,
    /// 对外发布出口（源实例持有的原子量，测试可精确断言；同时写全局槽，
    /// 宿主 500ms 轮询读的就是全局槽）。
    out: Arc<AtomicU64>,
    slot: usize,
}

/// 填窗期采样窗口：与宿主 500ms 轮询同拍，保证每次 poll 都能读到新鲜值。
const FILL_METER_WINDOW: Duration = Duration::from_millis(500);
/// 结束一次填窗期（进入背压）时，短于它的样本**不作数**：几十微秒级的窗口
/// 只会量到进程内 memcpy 速度，不是链路速度。20ms 是"至少跨了几个 TCP 往返"
/// 的量级，同时让"快链路上几 MB 的填充"也能留下一个真实样本。
const FILL_METER_MIN_WINDOW: Duration = Duration::from_millis(20);

impl FillMeter {
    fn new(slot: usize, out: Arc<AtomicU64>) -> Self {
        Self {
            at: Instant::now(),
            bytes: 0,
            ema: 0.0,
            out,
            slot,
        }
    }

    /// 进入背压（窗口满）：把本次填窗期**结算掉**（否则短促但真实的填窗样本
    /// 会被直接丢掉、读数永远是 0），然后重置基线——这样恢复读取后的第一个
    /// 样本才不会被"整段背压时长"稀释成假低速。
    fn pause(&mut self) {
        if self.bytes > 0 && self.at.elapsed() >= FILL_METER_MIN_WINDOW {
            self.settle();
        }
        self.bytes = 0;
        self.at = Instant::now();
    }

    /// 记下"窗口未满时"真正从 socket 收到的字节。
    fn record(&mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.at.elapsed() >= FILL_METER_WINDOW {
            self.settle();
        }
    }

    /// 结算本采样窗口：瞬时速率 → 0.3/0.7 EMA → 发布（实例原子 + 全局槽）。
    fn settle(&mut self) {
        let elapsed = self.at.elapsed();
        if elapsed.is_zero() {
            return;
        }
        let inst = self.bytes as f64 / elapsed.as_secs_f64();
        let mut ema = if self.ema > 0.0 {
            self.ema * (1.0 - BANDWIDTH_EMA_NEW) + inst * BANDWIDTH_EMA_NEW
        } else {
            inst
        };
        if !ema.is_finite() || ema < 1.0 {
            ema = 0.0; // 与 est_bps 同一套"零供给地板"语义（0 = 未测得）
        }
        self.ema = ema;
        self.bytes = 0;
        self.at = Instant::now();
        self.out.store(ema.to_bits(), Ordering::Relaxed);
        store_fill_bps(self.slot, ema);
    }
}

/// **窗口满时的"滴灌"**：不把 socket 读彻底停掉，而是每
/// [`STREAM_DRIP_INTERVAL`] 读一小块（[`STREAM_DRIP_BYTES`]），把连接保持"活着"。
///
/// **依据（对外调研，一手）**：`bilibili-downloader-gui` 的限速器注释明确记录了
/// **"CDN write-idle resets"** —— 客户端长时间不读（TCP 窗口压到 0）会被 CDN
/// 因"写空闲"重置连接；该项目的对策是"保证每次睡眠都是亚秒级、缓冲保持浅"。
/// 我们旧实现是"窗口满 → 完全不读 socket 直到被消费"，**恰好在招致这一行为**。
///
/// **参数选择**：16KiB / 200ms = **80KB/s** 的滴灌速率——典型 1080P 码率（≈400KB/s）
/// 的 1/5，不会明显拖慢缓冲；睡眠 200ms 满足"亚秒级"；16KiB 是 64KiB 读块的四分之一，
/// 对 TCP/HTTP 层足够"看起来在读"。
///
/// **为什么不越内存预算**：滴灌的字节仍然进热窗口，但预算里给了它一块**独立余量**
/// （[`STREAM_DRIP_HEADROOM_BYTES`] = 1MiB，且这 1MiB **计在内存硬上限之内**，
/// 见 `window_budget`）；余量用尽（窗口满且已久无消费）就退回"完全不读"，
/// 因此常驻内存仍被硬上限钉住。滴灌字节**不计入填窗期吞吐**（见读线程注释），
/// 否则会把"我们刻意的小口慢读"误当成链路能力，拖低宿主判升画质的依据。
/// `ERIKA_HTTP_DRIP=0` 可关回旧的"完全停读"行为（现场 A/B 对照用）。
const STREAM_DRIP_BYTES: usize = 16 * 1024;
const STREAM_DRIP_INTERVAL: Duration = Duration::from_millis(200);
const STREAM_DRIP_HEADROOM_BYTES: u64 = 1024 * 1024;

/// 是否启用窗口满时的滴灌（默认开；`ERIKA_HTTP_DRIP=0` 关闭）。
fn stream_drip_enabled() -> bool {
    env::var("ERIKA_HTTP_DRIP")
        .map(|value| value != "0" && value != "false")
        .unwrap_or(true)
}

/// 是否启用 **seek 出窗的跨节点并行回填**（默认开）。
///
/// `ERIKA_SEEK_PARALLEL=0/false/off` 一行回退到改造前的行为：seek 出窗 = 重设
/// 窗口 + **一条** open-ended Range 串行拉（逐字节等价的旧路径，现场 A/B 用）。
fn seek_parallel_enabled() -> bool {
    env::var("ERIKA_SEEK_PARALLEL")
        .map(|value| {
            let v = value.trim();
            v != "0" && !v.eq_ignore_ascii_case("false") && !v.eq_ignore_ascii_case("off")
        })
        .unwrap_or(true)
}

/// 现场覆盖并行条数（`ERIKA_SEEK_PARALLEL_CONNS`，钳制 1..=4）。
/// `1` = 不并行（等价于关闭），`2`/`3`/`4` = 固定条数（跳过自适应估算）。
fn seek_parallel_connections_override() -> Option<usize> {
    env::var("ERIKA_SEEK_PARALLEL_CONNS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|value| value.clamp(1, SEEK_PARALLEL_MAX_CONNECTIONS))
}

/// 算出并行条数（`1` = 不并行）。
///
/// **依据**（可复核，不是拍脑袋）：真机实测单条长连接 ≈320KB/s
/// （`tests/open_stages_diag.rs` 的限速对照），而 video+audio 两条流同时下载
/// 合计 ≈1.1MB/s ⇒ **瓶颈在"单条连接"上，不在链路带宽**。把回填窗口拆到 N 个
/// **不同节点**上，期望总吞吐 ≈ N × 单节点速率；达到目标墙钟
/// （[`SEEK_PARALLEL_TARGET_SECS`]）所需的总吞吐 = 回填窗口 ÷ 目标秒数，于是
/// `N = ceil(所需总吞吐 ÷ 实测单流速率)`：
///   - 320KB/s 单流 + 1.5MiB 窗口 + 1.6s 目标 → **N = 3**（与实测吻合的默认档）；
///   - 快链路（单流已 ≥ 所需总吞吐）→ N = 1 → **不并行**（本来就够快，白开连接
///     只会多涨请求数）；
///   - 极慢链路 → 顶到上限 [`SEEK_PARALLEL_MAX_CONNECTIONS`]。
///
/// 上限 4 的依据是既有真机教训（见 `http_prefetch_parallel` 注释）：B 站 CDN 对
/// 单 IP 并发连接数有限制，8 并发触发限流惩罚（在飞段集体挂死、新建连接被主动
/// 断开）；2~4 是安全区。
fn seek_parallel_connections(fill_bytes: u64, per_conn_bps: f64) -> usize {
    // 窗口本身就得放得下 ≥2 个达标段，否则分段没有意义（照旧单连接）。
    let max_by_size = (fill_bytes / SEEK_PARALLEL_MIN_SEGMENT_BYTES) as usize;
    if max_by_size < 2 {
        return 1;
    }
    let wanted = match seek_parallel_connections_override() {
        Some(overridden) => overridden,
        // 单流通量未知（刚开播、尚未完成一次填窗）→ 用实测标定的默认档。
        None if !(per_conn_bps > 0.0) => SEEK_PARALLEL_DEFAULT_CONNECTIONS,
        None => {
            let need_bps = fill_bytes as f64 / SEEK_PARALLEL_TARGET_SECS;
            let raw = (need_bps / per_conn_bps).ceil();
            if !raw.is_finite() || raw <= 0.0 {
                SEEK_PARALLEL_DEFAULT_CONNECTIONS
            } else {
                raw as usize
            }
        }
    };
    wanted
        .clamp(1, SEEK_PARALLEL_MAX_CONNECTIONS)
        .min(max_by_size)
}

/// 顺序长读的运行时句柄（源持有；线程持有它的一份 Arc 副本）。
struct SequentialReader {
    shared: Arc<StreamShared>,
    /// 实时已收字节计数器：登记进全局 `SOURCE_RX`，供 `downloaded_bytes` /
    /// `est_bps` 使用（与旧路径的 `rx` 同口径：线路上到达即计）。
    arrived: Arc<AtomicU64>,
    /// **填窗期实测吞吐**（字节/秒，f64 位模式）：读线程写，诊断/测试读。
    /// 口径见 [`SourceDiagSnapshot::fill_throughput_bps`]。
    fill_bps: Arc<AtomicU64>,
    /// 0/1：当前是否有一条连接在飞（诊断 `prefetch_in_flight`）。
    in_flight: Arc<AtomicU64>,
    /// 累计连接重建次数（按原因分桶；`total()` 即"多出来的请求数"）。
    reconnects: Arc<StreamReconnects>,
    /// 请求起点序列日志（判决实验载体：断点续传 vs demux 跳读）。
    journal: Arc<RangeJournal>,
    /// **最近一次收到字节的 epoch 毫秒**（顺序长读自己维护）。
    ///
    /// 这是一处真 bug 的修复：旧实现里"最后收到字节"的时间戳只由旧分段路径
    /// （`record_downloaded`）维护，顺序长读从不更新 → 换源的死连判据在开播
    /// 6 秒后恒真 → 每 ~6 秒换一次 host（用户实测 87 秒 14 次）。
    last_arrival_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    /// 线程退出信号（bounded join 用，见 `Drop`）。
    done: mpsc::Receiver<()>,
    handle: Option<JoinHandle<()>>,
    /// **seek 出窗的跨节点并行回填**：累计计数 + 最近一次批次明细。
    /// `stream_seek_hint` 侧（demux 线程）读它的自适应闸门，`diag_snapshot` 读它
    /// 发布诊断，执行侧（读线程 + worker）写它。
    seek_fill: Arc<SeekFillDiag>,
    /// 在飞分段配额（硬上限 [`SEEK_PARALLEL_MAX_CONNECTIONS`]；连续拖动时靠它
    /// 保证并发连接数不会被推高）。
    seek_fill_quota: Arc<AtomicUsize>,
}

pub struct HttpRangeSource {
    uri: Arc<Mutex<String>>,
    /// 打开时登记的主 URI（首候选）。多源换 host 时只变 `uri`（活跃 fetch
    /// 地址），此字段保持不变，作为源的稳定标识供 `uri()` / 日志使用。
    primary_uri: String,
    agent: HttpFetch,
    /// 本源的取消域：宿主拆除/关窗时置位，让卡在 socket 上的读立刻返回，而不是
    /// 等 30s 的段超时（见 `http::BodyReader` 与 `SourceCancellation`）。
    /// 每条流的 body 泵用它的**子 token**，所以取消一条流不会误伤同进程里另一个源
    /// （DASH 双流的 video/audio 就是两个源）。
    cancellation: SourceCancellation,
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
    /// **实测消费速率**（字节/秒，2s 窗口 EMA）：demux 消费前沿的推进速度。
    /// 稳态播放（缓冲被背压钳住）时它就是当前档位的有效码率，是滑动窗口
    /// 预算公式里"码率"这一项的输入（见文件上方"滑动窗口预算"整段）。
    /// 0 = 未测得（用配置值兜底，行为与改造前一致）。
    consume_bps: f64,
    /// 消费速率窗口累计字节与窗口起点（与 `note_consumed` 配对）。
    consume_window_bytes: u64,
    last_consume_at: Instant,
    /// 最近一次记录的消费前沿（绝对偏移），用于算推进量、识别回拖。
    last_consumed_pos: u64,
    /// 最近一次结算出的滑动窗口预算（时间/字节），诊断与读窗口同步共用。
    window: WindowBudget,
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
    /// 桶/冷区目录覆写。生产恒为 `None`（目录由 `preload_dir()` 决定）；
    /// 单测用它指向自己的临时目录——单测改 `ERIKA_PRELOAD_DIR` 会与并行用例
    /// 互相污染（桶文件被别的用例清掉 / 复用）。
    preload_dir_override: Option<PathBuf>,
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
    /// **自发换源的时间戳环**（epoch 毫秒，最多 [`MULTI_HOST_SWITCH_WINDOW_MS`]
    /// 内的滑动限流）：窗口内超过 [`MULTI_HOST_MAX_SWITCHES`] 次就不再自发换，
    /// 停在当前 host 让宿主自愈链接手。
    switch_times: VecDeque<u64>,
    /// 当前这一次自发换源的冷却时长（复发退避：每次翻倍，上限
    /// [`MULTI_HOST_COOLDOWN_MAX`]）。成功读到字节（供给恢复）时归位。
    switch_backoff: Duration,
    /// 饥饿起始时刻（epoch 毫秒；0 = 当前不饥饿）。用于"持续
    /// [`MULTI_HOST_STARVE_PERSIST_MS`] 才算真饥饿"的迟滞。
    starve_since_ms: u64,
    /// **最近一次换源的可读原因**（诊断浮窗/日志用：宿主直接显示这一行，
    /// 用户就不用猜"为什么又换了"）。空串 = 本轮还没换过。
    last_switch_reason: String,
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
    /// 本源的读路径是否正处在「暂时读不到数据、退避重试」的等待窗口。
    /// 由 [`HttpRangeSource::read_range_with_transient_wait`] 维护，
    /// 经 `publish_diag` 发布到诊断槽的 `transient_wait` 字段。
    transient_wait: bool,
    /// **此刻是否有一次 `read_range` 正在等待数据**（demux 正卡在读上）。
    ///
    /// 换源判据要区分三态：**在播且饿着**（应该恢复）、**暂停**（什么都不该做）、
    /// **在播且供给充足**。只看"最近有没有调用 read_range"不够：一次读调用可能
    /// 在等待循环里挂 20 秒，期间时间戳早就过期 → 真饥饿反而被判成"暂停"而
    /// 按住不动（真机现象：`vstall=16s→21s` 却一直 `Hold`）。这个标志由读路径
    /// 显式维护：读调用进入时置位、返回时清除。
    read_in_flight: bool,
    /// **顺序长读**（见文件上方的大段说明）：true 时正常前进播放全部由一条
    /// 持续读的长连接覆盖，不再按 256KB 分段发请求。
    ///
    /// 由 VOD 工厂 `enable_sequential_stream()` 打开（生产默认路径），
    /// `ERIKA_HTTP_SEQUENTIAL=0` 可整体关回旧分段路径。直接
    /// `HttpRangeSource::new(...)` 构造的源保持旧路径（既有单测的对照基线）。
    sequential_enabled: bool,
    /// 顺序长读的运行时句柄。`None` = 尚未启动（懒启动：第一次读/第一条 seek
    /// 提示时才建，起点取当时的位置，避免"从 0 起一条连接"被立刻 seek 掉）。
    stream: Option<SequentialReader>,
    /// 与读线程共享的 host 代数（`maybe_switch_host` 每次切换 +1）。读线程据此
    /// 在块边界上放弃旧 host 的连接、用新 host 从 `write_pos` 续传。
    stream_epoch: Arc<AtomicU64>,
    /// 上一次发布诊断时读到的长读重连次数（用于把"每次重连"打成一条 trace，
    /// 这是现场核对"一次播放到底发了几个请求"的直接证据）。
    stream_reconnects_seen: u64,
    /// 长读路径上一次发布诊断的 epoch 毫秒（发布节流，见
    /// [`HttpRangeSource::publish_diag_throttled`]）。
    last_stream_publish_ms: u64,
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
/// 200), or measured at EOF of a whole-file 200. The total lets callers
/// backfill `content_length` when HEAD or length headers are unavailable.
struct HttpRangeResponse {
    bytes: Vec<u8>,
    total_length: Option<u64>,
    /// `ETag` 原样（**跨 CDN 节点一致性门禁的强证据**；见 `seek_fill_gate`）。
    /// 实测：B 站官方 upos 镜像节点会给同一个值，MCDN/PCDN 节点不给——
    /// "缺 ETag" 因此在门禁里等同"证据不足"。
    etag: Option<String>,
    /// `Last-Modified` 原样（ETag 缺失时的**弱**证据；门禁里只做展示，
    /// 因为不同节点的 Last-Modified 可能因回源/打包时间而不同）。
    last_modified: Option<String>,
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
    // ---- 冷区（滑动窗口的磁盘分层）----
    /// 冷区段文件目录（`None` = 测试构造的纯内存状态，段一律内存）。
    dir: Option<PathBuf>,
    /// 段文件名的摘要基（与 `preload_key(uri, slot)` 同源；活动段沿用原名，
    /// 后续段用唯一后缀，见 `DiskStore::create_fresh`）。
    key: u64,
    /// 冷区预算（字节）：超过即从**最久远**的段开始淘汰（`evict_cold_to_budget`）。
    cold_budget: AtomicU64,
    /// 冷区当前字节总量（所有段之和，含活动段）。诊断与预算共用。
    stored: AtomicU64,
    /// 冷区覆盖区间的下界（最小段起点；`u64::MAX` = 空）。
    /// 只用于 `might_cover` 的**无锁预筛**，见那里的注释。
    cold_low: AtomicU64,
    /// 冷区交接通道发送端。`Some` = **归档模式**：本状态不再自己发网络请求，
    /// 只接收热区淘汰下来的字节（"已经有了就留下、而不是丢掉"）。
    /// 由 `HttpRangeSource::enable_full_preload` 在顺序长读路径上装配。
    cold: Mutex<Option<ColdSink>>,
    /// 归档模式标志（诊断/日志用：说明这条桶当前不下载、只做冷区分层）。
    archive: AtomicBool,
    /// 冷区（磁盘层）命中计数与命中字节数：**因为冷区命中而省下的网络请求**。
    /// 宿主轮询差分之；也是"回拖/重播零网络"现场验收的直接证据。
    cold_hits: AtomicU64,
    cold_hit_bytes: AtomicU64,
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
            let path = dir.join(format!("{key:016x}-{}-{sequence}.part", std::process::id()));
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
            Err(error) if error.kind() == io::ErrorKind::NotFound => PreloadFile::create_new(path)?,
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

    /// 新建一个**全新**的段文件（绝不复用残留文件）。
    ///
    /// 与 `open` 的区别是刻意的：`open` 允许复用同名残片（"重开同一集不用重下"），
    /// 而冷区**段**必须与自己记录的逻辑起点严格对齐——复用一份起点不同的旧文件
    /// 会让"段内连续"的判定失准（读出来的是错位字节，比少几个缓存字节严重得多）。
    /// 所以段一律新建，名字取进程内唯一后缀；再次打开同一逻辑区间只会得到新文件。
    fn create_fresh(dir: &Path, key: u64) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        for _ in 0..8 {
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!(
                "{key:016x}s-{}-{sequence}.part",
                std::process::id()
            ));
            let Ok(file) = PreloadFile::create_new(&path) else {
                continue;
            };
            let mut store = Self {
                file,
                data_start: 0,
                data_len: 0,
                tail: Vec::new(),
                tail_offset: 0,
            };
            store.write_header()?;
            return Ok(store);
        }
        Err(io::Error::other("preload: no usable segment file name"))
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

    /// **2026-09-04 起本方法不再是"丢弃已有字节"那条路径的一部分**：整片预载桶
    /// 与冷区都改成"封存旧段 + 另开新段"（见 `FullPreloadState::seal_and_open`），
    /// 旧字节一律保留。现在它只剩**测试**在用——`preload_disk_bucket_round_trips_and_shifts_offsets`
    /// 用它验证"改写物理布局后绝不留空洞"（段文件布局的回归防线）。
    #[cfg(test)]
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

    /// 封存段时释放段内尾部内存窗口（见 `DiskStore::tail`）。
    ///
    /// 只有**活动段**会被顺序追加/尾部读取；封存段是冷数据，留着那 1MiB
    /// 常驻内存会随段数线性增长（32 段 = 32MiB），把"冷区靠磁盘、不占内存"
    /// 这条承诺吃掉。释放只是少一层优化，读路径自动退回文件读（页缓存）。
    fn release_tail(&mut self) {
        if let Self::Disk(disk) = self {
            disk.tail = Vec::new();
            disk.tail_offset = 0;
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

/// 是否把冷区/整片预载桶落到磁盘。默认**开**。
///
/// 两个等价的名字都接受（任一个为 `0/false/off/空` 即关闭）：
///   - `ERIKA_PRELOAD_DISK=0` —— 本次改造新增的名字（冷区分层落盘的总开关）；
///   - `ERIKA_PRELOAD_TO_DISK=0` —— 落盘改造时的旧名字，保留兼容。
///
/// 关闭后走**纯内存**冷区：缓存照常分层、命中照常零网络，但冷区预算会被钳到
/// `WINDOW_COLD_MEMORY_ONLY_MAX`（32MiB），避免"关掉落盘"变成"把 256MiB 搬进
/// 内存"。异常/对照排查时用它一行关掉整条落盘链路。
fn preload_to_disk_enabled() -> bool {
    // 与 `trace::env_flag` 同一套字面量，但默认相反（这条开关默认开）。
    !["ERIKA_PRELOAD_DISK", "ERIKA_PRELOAD_TO_DISK"]
        .iter()
        .any(|name| {
            matches!(
                env::var(name).ok().as_deref(),
                Some("0" | "false" | "FALSE" | "off" | "OFF" | "")
            )
        })
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

/// 冷区里一个**已封存**的段：不再追加，只读。
///
/// 段是冷区的存储/淘汰单位：先按 §`COLD_SEGMENT_BYTES` 切，整段淘汰（丢最久远
/// 的一段）就等价于"滑出回看窗口"，且淘汰 = 段存储 `Drop` = **文件真的被删掉**。
struct SealedSegment {
    /// 段内首字节的绝对文件偏移。
    start: u64,
    store: PreloadStore,
}

impl SealedSegment {
    fn end(&self) -> u64 {
        self.start.saturating_add(self.store.len())
    }

    fn covers(&self, offset: u64, end: u64) -> bool {
        offset >= self.start && end <= self.end()
    }
}

struct FullPreloadInner {
    /// 文件总长（由首个成功响应的 total 回填；None = 未探测到，线程等待）。
    total: Option<u64>,
    /// **活动段**的绝对起点（可追加的那一段）。本次改造后冷区是"多段"结构：
    /// 活动段 + 已封存段（`sealed`）。
    start: u64,
    /// 活动段存储：文件 `[start, start + store.len())` 的连续字节。
    store: PreloadStore,
    /// 已封存的历史段（按逻辑起点升序，互不重叠）。
    ///
    /// **段间允许有洞**：洞表示那段确实没能保住（交接通道背压 / 磁盘写失败），
    /// 回拖到洞里需要重新下载——诚实且不会读出错位字节。段**内**永远连续
    /// （写入路径只允许在段尾追加，见 `FullPreloadState::ingest_evicted`）。
    sealed: Vec<SealedSegment>,
}

impl FullPreloadInner {
    /// 活动段终点（= 冷区当前的最前覆盖）。
    fn active_end(&self) -> u64 {
        self.start.saturating_add(self.store.len())
    }

    /// 全部段（活动 + 封存）的字节总量。诊断与预算判定都用它。
    fn stored_bytes(&self) -> u64 {
        self.sealed
            .iter()
            .map(|segment| segment.store.len())
            .fold(self.store.len(), u64::saturating_add)
    }

    /// 在**任意**段里定位 `[offset, end)`，返回命中段的起点（供算段内偏移）。
    fn locate(&self, offset: u64, end: u64) -> Option<u64> {
        if offset >= self.start && end <= self.active_end() {
            return Some(self.start);
        }
        // 从最新的段往前找：回拖命中的通常是最近封存的那几段。
        self.sealed
            .iter()
            .rev()
            .find(|segment| segment.covers(offset, end))
            .map(|segment| segment.start)
    }

    /// 含 `offset` 的段的**终点**（`None` = 没有任何段含它）。用于"能取多少
    /// 取多少"的前缀命中（跨冷/热边界的读）。
    fn containing_end(&self, offset: u64) -> Option<u64> {
        if offset >= self.start && offset < self.active_end() {
            return Some(self.active_end());
        }
        self.sealed
            .iter()
            .rev()
            .find(|segment| offset >= segment.start && offset < segment.end())
            .map(SealedSegment::end)
    }

    /// 复制命中区间（调用方先 `locate` 成功；本方法内部再定位一次，保证
    /// "绝对偏移 → 段内偏移"的换算与命中判定用的是同一段）。
    fn copy(&mut self, offset: u64, length: u64) -> Option<Vec<u8>> {
        let end = offset.checked_add(length)?;
        let base = self.locate(offset, end)?;
        let rel_start = usize::try_from(offset.checked_sub(base)?).ok()?;
        // 注意是 `end - offset`（请求长度），不是 `end - base`（段内终点）：
        // 段起点可能远在请求起点之前（回拖命中的是段中间），差一个偏移就是
        // "读越界报错"或"读到相邻区间的字节"。
        let rel_len = usize::try_from(end - offset).ok()?;
        let store = if base == self.start {
            &mut self.store
        } else {
            &mut self
                .sealed
                .iter_mut()
                .find(|segment| segment.start == base)?
                .store
        };
        store.read(rel_start as u64, rel_len).ok()
    }

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
            PreloadStore::Disk(disk) => {
                // **退化到内存也要有上限**：冷区预算默认 256MB（`WINDOW_COLD_BUDGET_DEFAULT`），
                // 落盘失败时把整扇窗读回 RAM 就等于"把 256MB 磁盘占用换成 256MB
                // 常驻"——而且恰好发生在磁盘已经出问题的时刻，是最不该再叠内存的时候。
                //
                // 阈值用 `window_cold_budget(false)` = "落盘关闭时的预算"（钳到
                // `WINDOW_COLD_MEMORY_ONLY_MAX`）：语义一致——既然已经退化成纯内存，
                // 就按纯内存的额度办事，不假装自己还有磁盘层。
                //
                // 超限时**不读回、直接丢弃**（与"读不回来"同一条路径）：读路径看到的
                // 是一个空窗口，会从当前位置继续下载。代价是重下这一段前缀，换来内存
                // 有界；这正是本函数原本为"文件被外部删掉"准备的行为，不是新语义。
                let memory_only_max = window_cold_budget(false);
                let window_bytes = disk.len();
                let recovered = if window_bytes > memory_only_max {
                    trace::diagnostic(
                        serde_json::json!({
                            "event": "preload_disk_degrade_too_large",
                            "windowBytes": window_bytes,
                            "memoryOnlyMaxBytes": memory_only_max,
                        })
                        .to_string(),
                    );
                    None
                } else {
                    disk.into_memory()
                };
                match recovered {
                    Some(bytes) => PreloadStore::Memory(bytes),
                    None => {
                        self.start = covered_end;
                        PreloadStore::Memory(Vec::new())
                    }
                }
            }
            memory => memory,
        };
        let _ = self.store.append(bytes);
        Err(error)
    }

    /// **封存**当前活动段（不再追加、释放尾部内存窗口），并让调用方随后用
    /// `open_segment` 起一个新段。空段直接丢掉（不占位、不占盘）。
    fn seal_active(&mut self) {
        let store = std::mem::replace(&mut self.store, PreloadStore::Memory(Vec::new()));
        if store.len() == 0 {
            return;
        }
        let mut store = store;
        store.release_tail();
        self.sealed.push(SealedSegment {
            start: self.start,
            store,
        });
    }

    /// 起一个新段（活动段起点换成 `start`）。
    fn open_segment(&mut self, start: u64, store: PreloadStore) {
        self.start = start;
        self.store = store;
    }
}

/// 整片预载单步：从当前 `end` 下载一段（≤256KB）追加进桶。返回 true 表示
/// 已完成（总量已知且到顶/达上限）或被停止；false 表示还有后续（调用方循
/// 环）。失败时不前进、由调用方退避重试——瞬时断流在这里被消化，不升级。
///
/// **注意（2026-09-03）**：这已经是**兜底实现**。生产路径走
/// [`full_preload_stream_loop`]——一条 open-ended Range 连接读到尾，请求数
/// 从"每 256KB 一个"降到 O(暂停次数)。本函数保留有两个用途：①
/// `ERIKA_PRELOAD_STREAM=0` 时的对照路径；② 桶"整体前移"那一步无网络动作，
/// 流式循环直接复用它，避免两套前移语义（含落盘后端的截断/降级）。
fn full_preload_step(
    state: &FullPreloadState,
    agent: &HttpFetch,
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
    // demux 并行预取已跑赢桶（floor > end）：**2026-09-04 起不再丢弃桶里的旧字节**，
    // 而是把它**封存**成一个独立段（只在冷区里躺一份，回拖/重播零网络命中），
    // 再从 floor 起另开新段继续补 demux 前方——"查重后跳过，而不是丢掉"。
    //
    // 旧实现的"整体前移 = 丢弃 [start, floor)"正是用户实测缺陷 ⑤ 的根因：
    // 已下好的区间被主动丢弃后，回拖那段只能重新下载（"这东西等于已经有了，
    // 就不需要再向网络请求"）。段结构让"跳过"与"保留"不再互斥：跳过的是
    // **下载动作**（floor 之前的区间不重复下），保留的是**已有字节**（封存段）。
    if floor > end {
        {
            let mut g = state.inner.lock().expect("full preload poisoned");
            state.seal_and_open(&mut g, floor);
        }
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
        // 整片预载桶的请求要记到**本条流自己的槽**（桶有 `slot`；音频桶
        // diag_slot=1，不能混进视频的计数）。
        http_request_profile(state.slot),
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
            state.refresh_cold_low(&g);
            state.stored.store(g.stored_bytes(), Ordering::Relaxed);
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

/// 后台整片预载线程循环（**分段实现，兜底路径**，见 [`full_preload_step`]）。
/// 独立 `OS` 线程，与 demux/runtime/UI 都无关联——
/// `core::time` 自然带，`AtomicBool::load(Relaxed)` 唤醒，单调追加直到
/// `step` 返回 true（end ≥ total 或 ≥ cap 或被 stop）。失败退避 200ms 后重试
/// 同一段续传（HTTP range 服务器天然支持），不会破坏桶的连续性。退出条件：
/// 1) stop 信号；2) 完成（end ≥ total/cap）；3) 致命错误（连续失败默认让循环
/// 继续；Drop 时由 `HttpRangeSource::drop` 兜底终止）。
fn full_preload_loop(
    agent: &HttpFetch,
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
            report_audio_first_progress(state);
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

/// 把整片预载进度上报到「音频优先」协调器（仅音频桶用）。
fn report_audio_first_progress(state: &FullPreloadState) {
    if let Some(total) = state.len() {
        AUDIO_FIRST.audio_total_known.store(true, Ordering::Relaxed);
        let covered = state.covered_until();
        let permille = covered.saturating_mul(1000) / total.max(1);
        AUDIO_FIRST
            .audio_permille
            .store(permille.min(1000), Ordering::Relaxed);
    }
}

/// 整片预载桶的**流式**下载循环（2026-09-03 顺序长读改造）。
///
/// 旧实现（`full_preload_loop` + `full_preload_step`）是"每步一个 256KB Range
/// 请求"：一整部 300MB 的片子 = **1200 个请求**，与 demux 侧的分段预取叠加
/// 正是用户实测的"一小时几千次"。这里换成与 demux 侧同构的顺序长读：
/// **一条 open-ended Range 连接从当前位置一直读到文件尾/上限**，逐块 append
/// 进桶。请求量级从 O(文件大小 / 256KB) 降到 **O(暂停次数)**——一次连续播放
/// 通常是 1~3 个请求。
///
/// 与 demux 那条长流的分工（**任一时刻只有一个下载者，不重复下同一区间**）：
///   - demux 活跃（播放中）→ 桶**不建连接**，把带宽完整留给 demux 那条长流；
///   - demux 空闲（暂停/缓冲充足）→ 桶从 demux 已覆盖前沿（`floor`）起一条长
///     连接补到文件尾/上限。
/// 被播放抢回、发生前移、换 host 时都会断开重连（每次 +1 请求）。
///
/// `ERIKA_PRELOAD_STREAM=0` 回退到旧的分段实现（对照/排障用）。
fn full_preload_stream_loop(
    agent: &HttpFetch,
    uri: Arc<Mutex<String>>,
    headers: &[(String, String)],
    state: &FullPreloadState,
    cap: u64,
    // 激进模式（仅音频优先桶）：不向 demux 让路（必须抢在 demux 之前拉完音轨）。
    eager: bool,
    // 是否把预载进度上报到音频优先协调器（仅音频桶）。
    report_audio_first: bool,
    // 是否为视频桶：音频优先激活期间让路。
    video_bucket: bool,
) {
    const IDLE_POLL: Duration = Duration::from_millis(100);
    let mut backoff = Duration::from_millis(200);
    while !state.stop.load(Ordering::Relaxed) {
        if video_bucket && audio_first_throttle_video() {
            thread::sleep(IDLE_POLL);
            continue;
        }
        // demux 正在消费（播放中）→ 桶让路：本轮不建连接。两路同时下载同一
        // 区间会把有效供给砍半；demux 那条长流才是播放期的数据通路。
        if !eager && state.demux_active() {
            thread::sleep(IDLE_POLL);
            continue;
        }
        let Some(total) = state.len() else {
            thread::sleep(IDLE_POLL); // 总长未探测到：等首个响应回填
            continue;
        };
        let end = state.covered_until();
        // demux 跑赢桶（floor > end）：整体前移，**无网络动作**。复用既有 step
        // 实现，避免两套前移语义（含落盘后端的截断与降级）。
        if state.floor.load(Ordering::Relaxed) > end {
            let uri_now = match uri.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            let _ = full_preload_step(state, agent, &uri_now, headers, cap);
            continue;
        }
        if end >= total.min(cap) {
            return; // 已到文件尾或上限 → 线程收工
        }
        let uri_now = match uri.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let mut conn = match open_sequential_response(
            agent,
            &uri_now,
            headers,
            end,
            state.slot,
            SourceCancellation::default(),
        ) {
            Ok(conn) => {
                backoff = Duration::from_millis(200);
                conn
            }
            Err(_) => {
                // 失败退避后重试（同一段续传：HTTP Range 天然支持）。
                thread::sleep(backoff);
                backoff = stream_retry_backoff(backoff);
                continue;
            }
        };
        if let Some(total) = conn.total {
            state.learn_total(total);
        }
        let mut chunk = [0u8; HTTP_RECV_CHUNK_BYTES];
        let mut gained = 0u64;
        loop {
            if state.stop.load(Ordering::Relaxed) {
                return;
            }
            // 播放抢回带宽 / 前移：断开重连（下次从新的 floor 续）。
            if !eager && state.demux_active() {
                break;
            }
            if video_bucket && audio_first_throttle_video() {
                break;
            }
            let end_now = state.covered_until();
            if state.floor.load(Ordering::Relaxed) > end_now {
                break;
            }
            if end_now >= cap || end_now >= state.len().unwrap_or(u64::MAX) {
                return;
            }
            match conn.reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    // 「线路上到达即计」——与旧路径把计数器交给分块读循环同一口径
                    // （含被完整性守卫丢弃的字节：它们确实到过网络）。
                    state.downloaded.fetch_add(n as u64, Ordering::Relaxed);
                    let accepted = state.append_streamed(end_now, &chunk[..n]);
                    if accepted > 0 {
                        gained = gained.saturating_add(accepted);
                        backoff = Duration::from_millis(200);
                    }
                }
                Err(_) => break,
            }
        }
        if report_audio_first {
            report_audio_first_progress(state);
        }
        if gained == 0 {
            thread::sleep(backoff);
            backoff = stream_retry_backoff(backoff);
        }
    }
}

fn full_preload_cap() -> u64 {
    std::env::var("ERIKA_HTTP_FULLPRELOAD_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(512 * 1024 * 1024)
}

/// 整片预载桶是否走**流式**实现（默认是）。`ERIKA_PRELOAD_STREAM=0` 关回旧的
/// "每步一个 256KB 请求"实现——同样作为一行环境变量的回退开关保留。
fn preload_stream_enabled() -> bool {
    std::env::var("ERIKA_PRELOAD_STREAM")
        .map(|value| value != "0" && value != "false")
        .unwrap_or(true)
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
                sealed: Vec::new(),
            },
            None,
            0,
            false,
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
        let disk = matches!(store, PreloadStore::Disk(_));
        Self::build(
            slot,
            FullPreloadInner {
                total: None,
                // 复用残留文件时窗口起点可能非 0（那份文件上次前移过），必须
                // 跟着文件头走，见 `PreloadStore::window_start`。
                start: store.window_start().unwrap_or(0),
                store,
                sealed: Vec::new(),
            },
            Some(dir.to_path_buf()),
            preload_key(uri, slot),
            disk,
        )
    }

    fn build(
        slot: usize,
        inner: FullPreloadInner,
        dir: Option<PathBuf>,
        key: u64,
        disk: bool,
    ) -> Arc<Self> {
        // 复用残留文件时窗口可能非空：`end` 必须从窗口终点起步，否则
        // `covered_until` 会谎报 0，桶会把已经躺在盘上的字节重下一遍。
        let covered = inner.active_end();
        let stored = inner.stored_bytes();
        let inner_low = {
            let mut low = if inner.store.len() > 0 {
                inner.start
            } else {
                u64::MAX
            };
            for segment in &inner.sealed {
                if segment.store.len() > 0 {
                    low = low.min(segment.start);
                }
            }
            low
        };
        Arc::new(Self {
            slot,
            stop: AtomicBool::new(false),
            end: AtomicU64::new(covered),
            downloaded: Arc::new(AtomicU64::new(0)),
            floor: AtomicU64::new(0),
            last_demux_read_ms: AtomicU64::new(0),
            inner: Mutex::new(inner),
            dir,
            key,
            cold_budget: AtomicU64::new(window_cold_budget(disk)),
            stored: AtomicU64::new(stored),
            cold_low: AtomicU64::new(if stored > 0 { inner_low } else { u64::MAX }),
            cold: Mutex::new(None),
            archive: AtomicBool::new(false),
            cold_hits: AtomicU64::new(0),
            cold_hit_bytes: AtomicU64::new(0),
        })
    }

    /// 全文件总长（首个成功响应回填）。None 表示尚未探测完成。
    fn len(&self) -> Option<u64> {
        self.inner.lock().ok().and_then(|g| g.total)
    }

    /// 回填总长（流式下载的首个 206 的 Content-Range）。已知道时不动。
    fn learn_total(&self, total: u64) {
        let mut g = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if g.total.is_none() {
            g.total = Some(total);
        }
    }

    /// **流式**下载用：把一段与新字节连续的字节追加进桶。返回接受的字节数
    /// （0 = 期间发生前移/并发写入，本次作废，调用方应断开重连）。
    ///
    /// 顺序完整性守卫与 `full_preload_step` 同一套：写入偏移必须与快照一致——
    /// 其间若发生窗口前移（`start` 变化）或并发写入，丢弃本次，绝不产生空洞，
    /// 也绝不把新字节追加到已前移的桶上造成错位（那是静默数据损坏）。
    fn append_streamed(&self, expected_end: u64, bytes: &[u8]) -> u64 {
        if bytes.is_empty() {
            return 0;
        }
        let mut g = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.end.load(Ordering::Relaxed) != expected_end {
            return 0;
        }
        // 落盘写失败（磁盘满/权限/杀软锁）已就地降级到内存桶：慢一拍，但字节
        // 照样收下，播放不受影响。
        let degraded = g.append_chunk(bytes).err();
        self.refresh_cold_low(&g);
        self.stored.store(g.stored_bytes(), Ordering::Relaxed);
        drop(g);
        if let Some(reason) = degraded {
            trace::diagnostic(format!(
                "{{\"event\":\"erika_preload_disk_fallback\",\"slot\":{},\"start\":{},\"length\":{},\"error\":\"{}\"}}",
                self.slot,
                expected_end,
                bytes.len(),
                json_escape(&reason),
            ));
        }
        self.end.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        bytes.len() as u64
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
        if self.archive_mode() {
            // 归档模式：本状态**不发任何网络请求**（数据全部来自热区交接），
            // 因此"桶的在飞段"恒为 0——宿主看到的在飞连接只有那条顺序长流。
            return 0;
        }
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
    /// 走零网络直返路径。
    ///
    /// 覆盖判定改为**按段**（`FullPreloadInner::locate`）：冷区现在是"多段 +
    /// 段间允许有洞"的结构，任何一段完整包住请求区间即命中；段外的地址绝不
    /// 谎报命中（谎报 = 读出错位字节或半截数据，比 miss 严重得多）。
    fn covers(&self, offset: u64, length: u64) -> bool {
        let guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        let Some(total) = guard.total else {
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
        guard.locate(offset, end).is_some()
    }

    /// 复制 [offset, offset+length) 字节出冷区。调用方必须先 `covers()` 返回 true。
    /// 绝对偏移需减去命中段的起点才是段内下标。
    fn copy_range(&self, offset: u64, length: u64) -> Option<Vec<u8>> {
        // 读全程持锁（**含磁盘读**）：段的封存/淘汰会改动存储布局，锁外读会
        // 撞上被截断/被重写的区间而读到错位字节——静默数据损坏比"这次回退到
        // demux 自己的路径"严重得多。锁内代价可忽略：命中尾部窗口是纯内存拷贝，
        // 其余也基本落在 page cache 上。
        let mut guard = self.inner.lock().ok()?;
        if guard.total.is_none() {
            return None;
        }
        // 读失败（越界 / IO 错）一律 None：调用方会回退到 demux 自己的缓存与
        // 网络路径，绝不把半截或错位的字节交出去。
        guard.copy(offset, length)
    }

    // ---- 冷区（滑动窗口的磁盘分层，2026-09-04）----

    /// 冷区入口（热区交接通道的发送端）。`None` = 本状态不进归档模式。
    fn cold_sink(&self) -> Option<ColdSink> {
        self.cold.lock().ok().and_then(|guard| guard.clone())
    }

    /// 装配**归档模式**：此后本状态不再自己发网络请求，只接收热区淘汰下来的
    /// 字节（"已有的就留下，而不是丢掉"）。由 `enable_full_preload` 在顺序长读
    /// 路径上调用（音频优先的 eager 桶除外：那是刻意的优先下载者）。
    fn set_cold_sink(&self, sink: ColdSink) {
        if let Ok(mut guard) = self.cold.lock() {
            *guard = Some(sink);
        }
        self.archive.store(true, Ordering::Relaxed);
    }

    fn archive_mode(&self) -> bool {
        self.archive.load(Ordering::Relaxed)
    }

    /// 冷区当前字节总量（所有段之和，含活动段）。
    fn cold_bytes(&self) -> u64 {
        self.stored.load(Ordering::Relaxed)
    }

    /// 冷区覆盖区间的**下界**（最小段起点；`u64::MAX` = 冷区为空）。
    fn cold_low(&self) -> u64 {
        self.cold_low.load(Ordering::Relaxed)
    }

    /// 冷区可能覆盖 `offset` 吗？（**两次原子读、无锁**的廉价预筛。）
    ///
    /// 冷区检查挂在每次 32KB demux 读的热路径上，正常前进播放时读位置一直在
    /// 冷区前沿之后，这一步就把绝大多数读挡在锁外；真正可能命中时才走
    /// `covers` / `copy_range` 的加锁定位。
    fn might_cover(&self, offset: u64) -> bool {
        let low = self.cold_low();
        low != u64::MAX && offset >= low && offset < self.covered_until()
    }

    /// 重算覆盖下界（段封存 / 淘汰 / 写入后调用，全部在 `inner` 锁内）。
    fn refresh_cold_low(&self, guard: &FullPreloadInner) {
        let mut low = if guard.store.len() > 0 {
            guard.start
        } else {
            u64::MAX
        };
        for segment in &guard.sealed {
            if segment.store.len() > 0 {
                low = low.min(segment.start);
            }
        }
        self.cold_low.store(low, Ordering::Relaxed);
    }

    fn cold_budget_bytes(&self) -> u64 {
        self.cold_budget.load(Ordering::Relaxed)
    }

    /// 冷区命中次数 / 命中字节（累计单调）：**因为冷区命中而免掉的网络读**。
    fn cold_hit_count(&self) -> u64 {
        self.cold_hits.load(Ordering::Relaxed)
    }

    fn cold_hit_total_bytes(&self) -> u64 {
        self.cold_hit_bytes.load(Ordering::Relaxed)
    }

    /// 冷区**零网络**命中：完整覆盖时复制返回并计数。
    ///
    /// 命中计数只统计"真的省下一次网络读"的命中（完整覆盖），半覆盖不算——
    /// 后者最终仍要走网络，计进命中会把"绝不重下"的证据稀释掉。
    fn read_cached(&self, offset: u64, length: u64) -> Option<Vec<u8>> {
        if !self.covers(offset, length) {
            return None;
        }
        let bytes = self.copy_range(offset, length)?;
        self.cold_hits.fetch_add(1, Ordering::Relaxed);
        self.cold_hit_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Some(bytes)
    }

    /// `offset` 处**至少一个字节**在冷区里（seek 提示用：目标已在盘上就不该
    /// 重发网络请求——这正是"回拖到已缓冲区域零请求"的判据）。
    fn covers_at(&self, offset: u64) -> bool {
        self.covers(offset, 1)
    }

    /// 冷区里 `offset` 处**当前可得**的字节（可以少于请求量，最多 `max`）。
    ///
    /// 用途：跨越冷/热边界的读（回拖后播放推进到冷区末端时一定有这种读）
    /// 只需要一次"冷区前缀"，剩下的交给上层继续——这样既不会因为"没整段命中"
    /// 而触发一次多余的网络请求，也不会读出错位字节。命中同样计数。
    fn read_available_at(&self, offset: u64, max: u64) -> Option<Vec<u8>> {
        if max == 0 {
            return Some(Vec::new());
        }
        let mut guard = self.inner.lock().ok()?;
        if guard.total.is_none() {
            return None;
        }
        let end_available = guard.containing_end(offset)?;
        let length = end_available.saturating_sub(offset).min(max);
        if length == 0 {
            return None;
        }
        let bytes = guard.copy(offset, length)?;
        self.cold_hits.fetch_add(1, Ordering::Relaxed);
        self.cold_hit_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Some(bytes)
    }

    /// 请求一次**拖拽预读**（页缓存预热）。非阻塞：通道满即放弃（预读只是优化）。
    fn request_warm(&self, offset: u64) {
        if let Some(sink) = self.cold_sink()
            && sink.tx.try_send(ColdMessage::Warm(offset)).is_err()
        {
            // 背压：放弃这次预读（真实读路径仍会命中冷区，只是少一次预热）。
        }
    }

    /// 收下一段**热区淘汰下来的**字节（零网络），按绝对偏移归入段存储。
    ///
    /// 规则（保证**段内**永远连续、无洞）：
    ///   - `chunk_end <= 活动段终点`：完全已有（回拖后重放同一区间的交接）→
    ///     **查重后跳过**，不写第二份；
    ///   - `chunk_start > 活动段终点`：中间有洞（交接通道背压 / 写失败）→
    ///     封存活动段、另开新段（段间允许有洞，但**绝不把不懂的字节硬接上**）；
    ///   - 否则：只写活动段终点之后的那部分（重叠前缀跳过）。
    ///
    /// 返回实际存下的字节数。写入失败（磁盘满/权限/杀软锁）就地降级到内存段，
    /// 与既有整片预载桶的降级语义一致：落盘是省内存的优化，不是可用性依赖。
    fn ingest_evicted(&self, offset: u64, bytes: &[u8]) -> u64 {
        if bytes.is_empty() {
            return 0;
        }
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let active_end = guard.active_end();
        let chunk_end = offset.saturating_add(bytes.len() as u64);
        if chunk_end <= active_end {
            return 0; // 已有 → 跳过（绝不重写、绝不覆盖）
        }
        if offset > active_end {
            self.seal_and_open(&mut guard, offset);
        }
        let write_from = guard.active_end().max(offset);
        let skip = usize::try_from(write_from - offset).unwrap_or(usize::MAX);
        let Some(tail) = bytes.get(skip..) else {
            return 0;
        };
        let degraded = guard.append_chunk(tail).err();
        let stored = guard.stored_bytes();
        let end = guard.active_end();
        self.stored.store(stored, Ordering::Relaxed);
        self.end.store(end, Ordering::Relaxed);
        // 段写满 → 封存 + 另开新段（起点就是当前终点，覆盖依然连续）。
        if guard.store.len() >= COLD_SEGMENT_BYTES {
            self.seal_and_open(&mut guard, end);
            self.end.store(guard.active_end(), Ordering::Relaxed);
        }
        self.refresh_cold_low(&guard);
        drop(guard);
        if let Some(reason) = degraded {
            // diag 是无条件通道（不受 trace 开关影响）：宿主必须知道这条流
            // 已经退回"冷区常驻内存"了。
            trace::diagnostic(format!(
                "{{\"event\":\"erika_preload_disk_fallback\",\"slot\":{},\"start\":{},\"length\":{},\"error\":\"{}\"}}",
                self.slot,
                offset,
                bytes.len(),
                json_escape(&reason),
            ));
        }
        // 超预算 → 丢最久远的段（**保持不丢已有的最近回看区间**）。
        self.evict_cold_to_budget();
        tail.len() as u64
    }

    /// 封存当前活动段，并从 `start` 起一个新段。
    ///
    /// 磁盘模式开新文件（`DiskStore::create_fresh`，绝不复用残留文件——段必须与
    /// 自己的逻辑起点严格对齐）；开不出来就退回内存段，冷区降级绝不影响播放。
    fn seal_and_open(&self, guard: &mut FullPreloadInner, start: u64) {
        guard.seal_active();
        let store = match &self.dir {
            Some(dir) => match DiskStore::create_fresh(dir, self.key) {
                Ok(disk) => PreloadStore::Disk(disk),
                Err(error) => {
                    trace::diagnostic(format!(
                        "{{\"event\":\"erika_cold_segment_unavailable\",\"slot\":{},\"start\":{},\"error\":\"{}\"}}",
                        self.slot,
                        start,
                        json_escape(&error.to_string()),
                    ));
                    PreloadStore::Memory(Vec::new())
                }
            },
            None => PreloadStore::Memory(Vec::new()),
        };
        guard.open_segment(start, store);
    }

    /// 冷区超预算 → 丢**最久远**的段（起点最小的封存段）。
    ///
    /// 策略理由（见文件上方预算整段）：回看命中的价值随时间单调下降
    /// ——"拖回刚才的位置"远多于"拖回片子开头"，所以淘汰必须从最旧的一端开始；
    /// 且淘汰**只动冷区**，永不影响读线程的前向推进（读多少由时间预算决定），
    /// 因此不会退化成"弱网缓冲变薄"。淘汰即整段丢弃 → `PreloadFile::Drop`
    /// **真的删除文件**（不留残渣）。
    fn evict_cold_to_budget(&self) -> u64 {
        let budget = self.cold_budget_bytes();
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut stored = guard.stored_bytes();
        let mut evicted = 0u64;
        while stored > budget && !guard.sealed.is_empty() {
            let Some(index) = guard
                .sealed
                .iter()
                .enumerate()
                .min_by_key(|(_, segment)| segment.start)
                .map(|(index, _)| index)
            else {
                break;
            };
            let segment = guard.sealed.remove(index);
            let length = segment.store.len();
            stored = stored.saturating_sub(length);
            evicted = evicted.saturating_add(length);
            // 整段丢弃：Disk 后端在 Drop 里关句柄并删文件。
            drop(segment);
        }
        self.stored.store(stored, Ordering::Relaxed);
        self.refresh_cold_low(&guard);
        evicted
    }

    /// 拖拽预读：把 `[offset, offset + COLD_WARM_BYTES)` 读进页缓存后丢弃。
    ///
    /// 由**冷区线程**调用（不在 demux 线程上）：真正播放时的冷区读因此命中页
    /// 缓存，"从盘读回"的开销被压到一次 memcpy 量级，而不是一次裸盘随机读。
    fn warm_range(&self, offset: u64) -> u64 {
        const STEPS: u64 = COLD_WARM_BYTES / COLD_CHUNK_BYTES as u64;
        let mut warmed = 0u64;
        for step in 0..STEPS {
            let chunk_offset = offset.saturating_add(step * COLD_CHUNK_BYTES as u64);
            match self.copy_range(chunk_offset, COLD_CHUNK_BYTES as u64) {
                Some(_) => warmed = warmed.saturating_add(COLD_CHUNK_BYTES as u64),
                None => break,
            }
        }
        warmed
    }

    /// 冷区归档线程主体：消费热区交接的字节 + 处理拖拽预读请求。
    ///
    /// 独立线程的意义：磁盘写与页缓存预热都不落在 demux 线程上（关窗/拖拽时
    /// 播放线程只做一次非阻塞 `try_send`）。
    fn run_cold_archive(&self, rx: mpsc::Receiver<ColdMessage>) {
        const TICK: Duration = Duration::from_millis(100);
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            match rx.recv_timeout(TICK) {
                Ok(ColdMessage::Evicted(chunk)) => {
                    let stored = self.ingest_evicted(chunk.offset, &chunk.bytes);
                    http_trace_log(format!(
                        "{{\"event\":\"cold_ingest\",\"slot\":{},\"offset\":{},\"bytes\":{},\"stored\":{},\"cold_bytes\":{}}}",
                        self.slot,
                        chunk.offset,
                        chunk.bytes.len(),
                        stored,
                        self.cold_bytes(),
                    ));
                }
                Ok(ColdMessage::Warm(offset)) => {
                    let warmed = self.warm_range(offset);
                    http_trace_log(format!(
                        "{{\"event\":\"cold_warm\",\"slot\":{},\"offset\":{},\"warmed\":{}}}",
                        self.slot, offset, warmed,
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// 顺序长读的**供给健康度**（换源判据的输入，见 `maybe_switch_host`）。
#[derive(Debug, Clone, Copy)]
struct StreamHealth {
    /// 前向可播秒数 = 前向缓冲字节 ÷ 实测码率估计。
    forward_secs: f64,
    /// 前向窗口是否已达预算（**供给充足的正面证据**：demux 消费得比读得慢）。
    window_full: bool,
    /// 有没有可用的窗口观测（旧分段路径没有长流窗口 → false：只能用
    /// "长时间没字节到达"判死连，**不能**用"前向秒数"判饥饿）。
    measurable: bool,
    /// **网络填充前沿已经到达服务端声明的文件终点**（本片已完整缓冲）。
    ///
    /// 真机事故（2026-09-15）：整片下完之后 `write_pos` 不再前进 →「距上次
    /// 收到字节」涨到 596 秒，与"链路死了"在**任何只看到达字节的指标上完全
    /// 同形**。旧判据因此会把"已经下完了"当成死连证据去换源。
    ///
    /// 特别要注意：`window_full` **挡不住这种情况**。它是 `front_len >=
    /// front_cap`，而 `front_cap` 的下界是宿主配置的 `read_ahead`
    /// （bili-rs 设 32MiB）——**短片子的整片都装不满这个窗口**，
    /// 于是 `window_full` 恒为 false、`!window_full` 这道守卫等于不存在。
    /// 必须用本字段（"不需要更多字节"）独立表达，见 `maybe_switch_host`。
    source_complete: bool,
}

/// 多源无缝换 host（Option A）参数。
/// 死连判定：超过 `MULTI_HOST_STALL_MS` 没收到字节、且 demux 仍在调用
/// read_range（播放活跃）→ 源已死，切到下一个候选。
const MULTI_HOST_STALL_MS: u64 = 6_000;
/// 活跃窗口：read_range 最近被调用的时间须落在此窗口内，才判定"在播"
/// （暂停时不误换）。
const MULTI_HOST_ACTIVE_MS: u64 = 6_000;
/// **恢复余量（秒）：前向可播秒数掉到它以下并持续一段时间 → 触发恢复（换源）。**
///
/// **为什么必须是"十几秒"而不是 2 秒**（2026-09-04 第二次订正，真机日志驱动）：
/// 从"判饥"到"用户看不到卡住"要付四笔时间成本，2 秒的余量根本不够付：
///   1. **检测迟滞** `MULTI_HOST_STARVE_PERSIST_MS`（3s）——防瞬时抖动误判；
///   2. **换源 + 首字节**：新连接 RTT+TTFB 实测量级 0.2~4s（弱网取上限）；
///   3. **重新加速 + 填回安全深度**：调研一手证据是"新连接会重新加速、随后衰减"，
///      但把余量填回去本身就是十几秒的量级；
///   4. **下一轮抖动余量**：CDN 抖动常见量级几秒到十几秒。
/// 取 **20 秒**：① 最坏情形（供给为 0、缓冲按 1× 消耗）下仍给播放留 20 秒，
/// 覆盖上面 1+2+3 的最坏叠加；② 余量不小于"判饥→恢复完成"的最坏耗时
/// （≈3s 迟滞 + 1~4s 首字节 + 15~20s 填回 ≈ 20~27s），才不会"先卡住再恢复"。
/// 真机反例（旧值 2.0s）：`buf=57.7s vstall=16s→21s` 时判据一直 `Hold`——
/// 要等到第 56 秒才动作，而那时用户早就看到卡住了。
///
/// 与 `MULTI_HOST_STALL_MS`（6s 无字节）配合：**连接真死走 6 秒快通路**（与缓冲
/// 多深无关，越早动作越好），**供给跟不上走这条 20 秒余量线**。
const MULTI_HOST_RECOVERY_MARGIN_SECS: f64 = 20.0;
/// 饥饿判定的持续时间要求（迟滞：瞬时抖动不算）。
const MULTI_HOST_STARVE_PERSIST_MS: u64 = 3_000;
/// **健康阈值**：前向可播秒数 ≥ 恢复余量（= 窗口满）就视为"供给充足"，
/// **绝不自发换源**（同时也让复发退避归位）。
const MULTI_HOST_HEALTHY_SECS: f64 = MULTI_HOST_RECOVERY_MARGIN_SECS;
/// 自发换源的**滑动窗口限流**：`MULTI_HOST_SWITCH_WINDOW_MS` 内最多
/// `MULTI_HOST_MAX_SWITCHES` 次；超过就停在当前 host 走自愈链（宿主换源/
/// 重开），而不是继续轮换——用户实测"89 秒 14 次"就是没有这道闸的后果。
const MULTI_HOST_SWITCH_WINDOW_MS: u64 = 300_000;
const MULTI_HOST_MAX_SWITCHES: usize = 5;
/// 复发退避：同一轮里换得越勤，冷却越长（4s 起翻倍，上限 32s）。
const MULTI_HOST_COOLDOWN_MAX: Duration = Duration::from_secs(32);
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

/// 是否 B 站媒体 host（`*.bilivideo.com` / `*.bilivideo.cn` / `*.bilibili.com`）。
/// 用于给这类源补上防盗链必需的 `Referer`（见构造器里的注释）。
fn is_bilibili_media_host(uri: &str) -> bool {
    let host = host_of(uri)
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    ["bilivideo.com", "bilivideo.cn", "bilibili.com"]
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
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
    /// PiliPlus **设置的** mpv `demuxer-max-bytes`（`lib/utils/storage_pref.dart:829-838`，
    /// `Pref.bufferSize` 默认 4.0——那是 PiliPlus 主动写入的取值，不是 mpv
    /// 自身的默认值，`initBuffer()` 里前后向都设成它）。
    /// 2MB 对 1080P（约 3 Mbps）只有约 5.3 秒缓冲，PiliPlus 有约 11 秒
    /// ——**缓冲深度差一倍是它比我们能扛抖动的直接原因**（对照研究见
    /// bili-rs `research_report_cdn_switch_vs_piliplus.md`）。4MB 让策略层
    /// 的 2 秒饥饿线不再被常规 CDN 抖动顶到；`spawn_prefetch_window` 的
    /// `window_cap` 会真正执行该上限，窗口不会无界增长。代价仅常驻 +2MB。
    /// 可用 `ERIKA_HTTP_READAHEAD_BYTES` 覆盖。
    ///
    /// ⚠️ **刻意与上游不同，合并时不要改回 2 MiB**：`origin/main` 的同名常量
    /// 是 2 MiB（且上游没有一行注释说明它的来历）。回退到 2 MiB 等于把上面
    /// 那条对照研究的结论丢掉——抗抖动能力砍一半。宿主若自行设
    /// `ERIKA_HTTP_READAHEAD_BYTES` 或逐请求传 `http_read_ahead_bytes`，本值
    /// 不会生效（bili-rs 两者都设）。
    const DEFAULT_READ_AHEAD_BYTES: u64 = 4 * 1024 * 1024;
    /// 后向缓冲默认大小。**4MB → 16MB（2026-09-29，对齐上游）**：mpv 默认保留
    /// 50 MiB 后向缓冲（`--demuxer-max-back-bytes`），VLC 复用 3×4 MiB ring，
    /// 16 MiB 取两者之间，覆盖 ~13 Mbps 下的 10 秒回退。本地原值 4 MiB 照抄的
    /// 是 PiliPlus 的取值（它把前向/后向都设成 `Pref.bufferSize`=4.0，那是移动端
    /// 省内存的取舍），并不是 mpv 自身的默认——桌面端内存不紧，16 MiB 让往回拖
    /// 不再动辄走网络。代价是每个源常驻 +12 MiB。
    /// 可用环境变量 `ERIKA_HTTP_BACK_BYTES` 覆盖；宿主也可按码率显式传
    /// `http_back_buffer_bytes`（71 Mbps 下 -10 秒覆盖 ~89 MB）。
    const DEFAULT_BACK_BUFFER_BYTES: u64 = 16 * 1024 * 1024;
    /// 顺序预取/窗口外 seek 的目标预取统一用小段 256KB：慢网络（实测
    /// ~1MB/s 级，均摊到 N 段后单段吞吐更低）下大段（2MB+）join 必超时 →
    /// read 放弃预取段走同步 fetch → 缓存窗口退化成同步小步拉取（一顿一顿
    /// 的根因）。256KB 段在总带宽 ≥0.5MB/s 时单段完成时间显著小于 join
    /// 超时（5s），多段滚动叠加带宽后总吞吐远超播放消费，窗口持续向前大步
    /// 缓冲（对齐 PiliPlus：看一点、缓冲一大截）。段小也让 seek 后目标段
    /// 更快就绪（256KB ≈ 慢网 1-2s）。
    ///
    /// **适用范围（2026-09-03 订正）**：这条结论只对**旧的分段路径**成立
    /// ——那里"段"是"一个请求换一段字节"，必须先整段收齐（join）才能交给
    /// demux，段越大越容易撞上 join 超时。**顺序长读没有"join 一个段"这个
    /// 概念**（socket 读到一个 64KB 块就立刻交付给 demux），"段大小"这个
    /// 变量在新路径里根本不存在，因此本常量对新路径无意义（它只剩两个用途：
    /// 旧路径的段大小上限，以及 `stream_seek_hint` 的前向 seek 防抖粒度）。
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
        // **防盗链 Referer**（对外调研：一手实测"带 UA 不带 Referer → 403"，
        // BBDown/bbgui/yt-dlp 全部无条件带 `Referer: https://www.bilibili.com`）。
        // 只在**确实是 B 站媒体 host** 且宿主没给 Referer 时补，避免污染其它源。
        let mut http_headers = http_headers;
        if is_bilibili_media_host(&uri)
            && !http_headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("referer"))
        {
            http_headers.push((
                "Referer".to_string(),
                "https://www.bilibili.com".to_string(),
            ));
        }
        Self {
            uri: Arc::new(Mutex::new(uri)),
            primary_uri: primary,
            agent,
            cancellation: SourceCancellation::default(),
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
            consume_bps: 0.0,
            consume_window_bytes: 0,
            last_consume_at: Instant::now(),
            last_consumed_pos: 0,
            // 冷启动预算 = 配置值（码率/吞吐都还没测到，见 `window_budget`）。
            window: window_budget(
                diag_slot,
                read_ahead
                    .filter(|bytes| *bytes > 0)
                    .unwrap_or_else(|| http_read_ahead_bytes(diag_slot)),
                http_back_bytes(diag_slot),
                0.0,
                None,
            ),
            prefetch: Vec::new(),
            prefetch_target: None,
            downloaded_bytes: 0,
            rx,
            // 整片预载默认关闭（直播源 / 单流场景不需要）：None 即不启用，
            // 由上层在需要时通过 `set_full_preload` 注入 `FullPreloadState`。
            full_preload: None,
            preload_thread: None,
            preload_dir_override: None,
            candidates: Vec::new(),
            active_idx: 0,
            last_byte_ms: now_ms(),
            last_read_call_ms: now_ms(),
            switch_cooldown_until: Instant::now(),
            switch_times: VecDeque::new(),
            switch_backoff: MULTI_HOST_COOLDOWN,
            starve_since_ms: 0,
            last_switch_reason: String::new(),
            // 新源不消费旧请求：同步为当前计数，宿主的 stale 请求不触发切换
            host_epoch: 0,
            last_switch_req: HOST_SWITCH_REQ
                .get(diag_slot)
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(0),
            switching_host: false,
            // 新源一开播不在等待窗口里；只有真正读不到数据时才置位。
            transient_wait: false,
            read_in_flight: false,
            // 顺序长读默认关：由 VOD 工厂 `enable_sequential_stream()` 显式打开
            // （生产路径默认开；直接 new 出来的源走旧分段路径，见字段注释）。
            sequential_enabled: false,
            stream: None,
            stream_epoch: Arc::new(AtomicU64::new(0)),
            stream_reconnects_seen: 0,
            // 0 = 从未发布过 → 第一次读立刻发布（见 publish_diag_throttled）
            last_stream_publish_ms: 0,
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
        // ---- 「最后收到字节」必须取两条路径的较新者 ----
        //
        // 这是一处**真 bug 的修复**（用户实测：87 秒换源 14 次 ≈ 每 6 秒一次）：
        // `self.last_byte_ms` 只在旧分段路径的 `record_downloaded` 里更新，
        // **顺序长读路径从不刷新它** → `stalled` 在开播 6 秒后恒为真 →
        // 每个冷却周期（4s）都触发一次"自发换源" → 新连接重新加速 → 再变老 →
        // 再换……正好是 6 秒一次。现在顺序长读线程自己维护 `last_arrival_ms`。
        let stream_last_byte = self
            .stream
            .as_ref()
            .map(|stream| stream.last_arrival_ms.load(Ordering::Relaxed))
            .unwrap_or(0);
        let last_byte_ms = self.last_byte_ms.max(stream_last_byte);
        // ---- 供给健康度：**正面证据优先** ----
        //
        // 顺序长读的背压语义是"窗口满就（几乎）不读 socket"，此时吞吐数字必然
        // 收敛到消费速率——**低吞吐不代表链路差**。因此只要前向缓冲还厚
        // （≥ `MULTI_HOST_HEALTHY_SECS` 可播秒数）或窗口压根没满，就判"供给充足"，
        // **绝不**自发换源。只有"这条链路正在饿死我们"才换：前向可播秒数掉到
        // `MULTI_HOST_STARVE_SECS` 以下并持续 `MULTI_HOST_STARVE_PERSIST_MS`，
        // 或真的长时间没有字节到达。
        let consuming =
            self.read_in_flight || t.saturating_sub(self.last_read_call_ms) < MULTI_HOST_ACTIVE_MS;
        let health = self.stream_health();
        // ---- 「整片已缓冲完」必须显式排除，两道守卫都挡不住它 ----
        //
        // 真机事故（2026-09-15）：`vstall=596s` 配 `缓冲 前沿18.9/19MB`——
        // 视频整片都快下完了，网络前沿自然不再前进，"距上次字节"自然一路涨。
        // 旧判据里唯一还站着的是 `!window_full`，而它是
        // `front_len >= front_cap`，`front_cap` 下界 = 宿主配的 `read_ahead`
        // （bili-rs 32MiB）——**短片子的整片都装不满它**，于是这道守卫恒真、
        // 等于不存在。只要 demux 有一次读活动（`consuming` 为真），判据就会拿
        // "下完了"当死连证据去换源，并给健康 host 记失败分。
        let eating = consuming
            && health.measurable
            && !health.source_complete
            && !health.window_full
            && health.forward_secs < MULTI_HOST_RECOVERY_MARGIN_SECS;
        let starving_now = eating;
        if starving_now {
            if self.starve_since_ms == 0 {
                self.starve_since_ms = t;
            }
        } else {
            self.starve_since_ms = 0;
        }
        let starving = self.starve_since_ms != 0
            && t.saturating_sub(self.starve_since_ms) >= MULTI_HOST_STARVE_PERSIST_MS;
        // 「长时间没有字节到达」**只在"还要字节、且窗口没满"时才成立**：
        // · 窗口满 = 我们**刻意不读**（背压语义），"没字节"是预期行为；
        // · `source_complete` = 后边**没有字节可要**了（整片已下完），
        //   "没字节"同样是预期行为，且这一条**不是**被"窗口满"覆盖的
        //   （短片子的整片比 `front_cap` 还小，`window_full` 恒 false）。
        // 这两条都是本次修复的核心：旧判据把它们统统当成了换源理由。
        let no_bytes = consuming
            && !health.source_complete
            && !health.window_full
            && t.saturating_sub(last_byte_ms) > MULTI_HOST_STALL_MS;
        // 供给充足（窗口满 / 前向秒数 ≥ 恢复余量 / **整片已下完**）→ 复发退避
        // 归位，下次真需要时不至于还在长冷却里。"整片已下完"要单列：那时前向
        // 秒数会被消费吃掉（`front_len` 只减不增），拿它判"不充足"会让退避
        // 一直停在长冷却上。
        let supplied = health.measurable
            && (health.window_full
                || health.source_complete
                || health.forward_secs >= MULTI_HOST_HEALTHY_SECS);
        if !forced && !starving && !no_bytes {
            if supplied {
                self.switch_backoff = MULTI_HOST_COOLDOWN;
            }
            return;
        }
        // ---- 抖动闸：滑动窗口限流 + 复发退避 ----
        let recent_switches = self
            .switch_times
            .iter()
            .filter(|stamp| t.saturating_sub(**stamp) < MULTI_HOST_SWITCH_WINDOW_MS)
            .count();
        if !forced && recent_switches >= MULTI_HOST_MAX_SWITCHES {
            // 窗口内换得够多了：**停在当前 host**，把"再换一次"的决策交回宿主
            // 自愈链（宿主有整源重建/换画质等更强的手段），而不是继续轮换把
            // 用户拖进无休止抖动。
            http_trace_log(format!(
                "{{\"event\":\"mhost_switch_suppressed\",\"reason\":\"rate_limited\",\"recent\":{recent_switches},\"window_ms\":{MULTI_HOST_SWITCH_WINDOW_MS},\"starving\":{starving},\"no_bytes\":{no_bytes}}}",
            ));
            trace::diagnostic(format!(
                "[erika-host-switch] slot={} 抑制自发换源：最近 {} 分钟内已换 {} 次（上限 {}）——停在当前 host，交给宿主自愈链",
                self.diag_slot,
                MULTI_HOST_SWITCH_WINDOW_MS / 60_000,
                recent_switches,
                MULTI_HOST_MAX_SWITCHES
            ));
            // 抑制也要有冷却，否则每个 read 都来问一次。
            self.switch_cooldown_until = now + MULTI_HOST_COOLDOWN;
            return;
        }
        let next = (self.active_idx + 1) % self.candidates.len();
        if next == self.active_idx {
            return;
        }
        let reason = if forced {
            "host_requested".to_string()
        } else if starving {
            format!(
                "starving(前向{:.1}s<恢复余量{:.1}s 持续{:.1}s)",
                health.forward_secs,
                MULTI_HOST_RECOVERY_MARGIN_SECS,
                t.saturating_sub(self.starve_since_ms) as f64 / 1000.0
            )
        } else {
            format!("no_bytes({}ms 无字节到达)", t.saturating_sub(last_byte_ms))
        };
        // 可读的一行：切前/切后 host、原因、当时的缓冲/停滞/吞吐——用户"看得见
        // 才算数"，面板与日志都吃这条。
        let from_host = self.active_host_label();
        let to_host = self.active_host_label_of(next);
        trace::diagnostic(format!(
            "[erika-host-switch] slot={} {}→{}（候选 {}/{}）原因={} | 前向可播 {:.1}s 窗口{} 吞吐 est={:.0}KB/s fill={:.0}KB/s 距上次字节 {}ms 本轮已换 {}/{}",
            self.diag_slot,
            from_host,
            to_host,
            next + 1,
            self.candidates.len(),
            reason,
            health.forward_secs,
            if health.window_full { "满" } else { "未满" },
            self.est_bps / 1024.0,
            load_fill_bps(self.diag_slot) / 1024.0,
            t.saturating_sub(last_byte_ms),
            recent_switches + 1,
            MULTI_HOST_MAX_SWITCHES,
        ));
        self.last_switch_reason = format!(
            "{} → {}：{}",
            host_of(&self.candidates[self.active_idx].uri),
            host_of(&self.candidates[next].uri),
            reason
        );
        let new_uri = self.candidates[next].uri.clone();
        *self.uri.lock().expect("uri poisoned") = new_uri.clone();
        http_trace_log(format!(
            "{{\"event\":\"mhost_switch\",\"from\":{},\"to\":{},\"reason\":\"{}\",\"est_bps\":{:.0},\"forward_secs\":{:.3},\"window_full\":{},\"since_last_byte_ms\":{},\"recent_switches\":{},\"next_score\":{:.2}}}",
            self.active_idx,
            next,
            json_escape(&reason),
            self.est_bps,
            health.forward_secs,
            health.window_full,
            t.saturating_sub(last_byte_ms),
            recent_switches + 1,
            self.candidates[next].score
        ));
        self.switch_times.push_back(t);
        while self.switch_times.len() > 16 {
            self.switch_times.pop_front();
        }
        self.active_idx = next;
        self.last_byte_ms = t;
        self.host_epoch = self.host_epoch.wrapping_add(1);
        // 顺序长读：把新代数同步给读线程。它在下一个 64KB 块边界发现 epoch 变了
        // 就断开旧 host 的连接、用新 host 从 `write_pos` 续传——已收到的字节全在
        // 窗口里，一个字节都不丢，切换的物理代价 = 新 host 上首块到达的时间。
        // （旧路径那边靠"作废在飞段 + 重发"实现同一件事，见下面的 K1 分支。）
        self.stream_epoch.store(self.host_epoch, Ordering::Relaxed);
        // 冷却：强制请求走短冷却（宿主另有 10s 策略冷却）；自发切换**复发退避**
        // （每次翻倍到 32s），把"切完又判慢"的循环从 4s 一次拉长到 32s 一次。
        self.switch_cooldown_until = now
            + if forced {
                MULTI_HOST_FORCED_COOLDOWN
            } else {
                let backoff = self.switch_backoff;
                self.switch_backoff = (self.switch_backoff * 2).min(MULTI_HOST_COOLDOWN_MAX);
                backoff
            };
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
        let dir = self
            .preload_dir_override
            .clone()
            .unwrap_or_else(preload_dir);
        let state = FullPreloadState::open(&uri, self.diag_slot, cap, &dir);
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
            AUDIO_FIRST
                .audio_total_known
                .store(false, Ordering::Relaxed);
            AUDIO_FIRST
                .enabled_at_ms
                .store(audio_first_now_ms(), Ordering::Relaxed);
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
        // 流式（默认）：一条 open-ended Range 读到尾，请求数 O(暂停次数)。
        // 分段（`ERIKA_PRELOAD_STREAM=0`）：旧的"每步一个 256KB 请求"实现。
        // agent 必须跟着模式走——流式是无限长 body，用带 body 总时长超时的
        // `http_agent` 会让这条长连接在 60s 处被自己掐断（见 `http_stream_agent`）。
        let streaming = preload_stream_enabled();
        let agent = if streaming {
            http_stream_agent()
        } else {
            self.agent.clone()
        };
        // **归档模式**（顺序长读生产路径，2026-09-04）：这条桶不再自己下载
        // （"打开就一路缓冲到整片下完"正是用户要消灭的行为——内存/流量都被它
        // 拖走），改成**滑动窗口的冷区**：只接收热区淘汰下来的字节、按段落盘，
        // 供回拖/重播零网络读回。发请求的只剩那条顺序长流。
        //
        // 例外（保持既有能力不丢）：音频优先的 eager 桶仍是"下载者"——音频
        // 整条只有几 MB，抢在 demux 前拉完是刻意的优先策略（见 `audio_first`）。
        let archive_mode = self.sequential_enabled && !eager;
        let cold_rx = if archive_mode {
            let (tx, rx) = mpsc::sync_channel::<ColdMessage>(COLD_CHANNEL_CHUNKS);
            state.set_cold_sink(ColdSink { tx });
            trace::diagnostic(format!(
                "{{\"event\":\"erika_cold_tier_enabled\",\"slot\":{},\"disk\":{},\"cold_budget\":{}}}",
                self.diag_slot,
                preload_to_disk_enabled(),
                state.cold_budget_bytes(),
            ));
            Some(rx)
        } else {
            None
        };
        let handle = std::thread::Builder::new()
            .name(format!("erika-full-preload-{}", self.diag_slot))
            .spawn(move || {
                if let Some(rx) = cold_rx {
                    // 冷区归档线程：磁盘写 + 拖拽预读都在这里，绝不在 demux 上。
                    state.run_cold_archive(rx);
                } else if streaming {
                    full_preload_stream_loop(
                        &agent,
                        shared_uri,
                        &headers,
                        &state,
                        cap,
                        eager,
                        report,
                        video_bucket,
                    );
                } else {
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
                }
            })
            .expect("failed to spawn full-preload thread");
        self.preload_thread = Some(handle);
        self
    }

    /// 测试专用：把桶/冷区目录指到自己的临时目录（见 `preload_dir_override`
    /// 注释），随后走**与生产完全相同**的 `enable_full_preload` 链路。
    #[cfg(test)]
    fn override_preload_dir(&mut self, dir: PathBuf) {
        self.preload_dir_override = Some(dir);
    }

    /// 启用**顺序长读**（见文件上方的大段说明）。生产路径默认调用它：    /// VOD 工厂 `source_from_uri_with_hint_and_headers_slot_and_read_ahead` 的
    /// HTTP 分支。`ERIKA_HTTP_SEQUENTIAL=0` 可关回旧分段路径（对照/排障用）。
    ///
    /// 线程是**懒启动**的（第一次读 / 第一条 seek 提示时才起），所以这里只置
    /// 标志位，构造 `HttpRangeSource` 不会凭空多出一个后台线程。
    pub fn enable_sequential_stream(mut self) -> Self {
        if http_sequential_enabled() {
            self.sequential_enabled = true;
        }
        self
    }

    /// 懒启动顺序长读线程（起点 = 第一次读的位置）。
    ///
    /// 起点取**第一次读的位置**而不是 0：seek 到 30 分钟处再开播时，从 0 起
    /// 一条连接是白费一个请求（读方马上要重设窗口）。窗口为空时直接以 `base`
    /// 起窗，读线程自然从该处发出第一条 open-ended Range。
    fn ensure_stream(&mut self, base: u64) {
        if self.stream.is_some() || !self.sequential_enabled {
            return;
        }
        // 窗口容量由**预算结算**给出（时间预算 + 内存硬上限，见文件上方整段）：
        // 冷启动（码率/吞吐未测得）等于配置的 read_ahead/back，行为与改造前一致。
        let budget = self.window_budget_now();
        self.window = budget;
        let front_cap = usize::try_from(budget.forward_bytes)
            .unwrap_or(usize::MAX)
            .max(STREAM_MIN_FRONT_CAP);
        let back_cap = usize::try_from(budget.back_bytes).unwrap_or(0);
        let mut buffer = StreamBuffer::new(base, front_cap, back_cap);
        if let Some(state) = self.full_preload.as_ref() {
            // 冷区（磁盘分层）出口：热区淘汰的字节交给冷区，**不丢**。
            // 顺序长读路径上它就是"绝不重下"的载体（回拖/重播从盘读回）。
            buffer.set_cold_sink(state.cold_sink());
        }
        let shared = Arc::new(StreamShared {
            buf: Mutex::new(buffer),
            data_cv: Condvar::new(),
        });
        let arrived = Arc::new(AtomicU64::new(0));
        // 实时计数器登记进本槽（与旧路径的 `rx` 同一套）：宿主 poll 与
        // `publish_diag` 都从 `SOURCE_RX` 取「线路上已到达」的字节总量来算
        // `downloaded_bytes` / `est_bps`。长读路径不登记的话，新路径下
        // est_bps 恒 0 → 自适应与宿主策略层同时失明。
        if let Ok(mut counters) = SOURCE_RX.lock() {
            if let Some(list) = counters.get_mut(self.diag_slot) {
                list.push(arrived.clone());
            }
        }
        let in_flight = Arc::new(AtomicU64::new(0));
        let reconnects = Arc::new(StreamReconnects::default());
        let journal = Arc::new(RangeJournal::default());
        let last_arrival_ms = Arc::new(AtomicU64::new(now_ms()));
        let fill_bps = Arc::new(AtomicU64::new(0.0f64.to_bits()));
        let stop = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = mpsc::channel();
        // 跨节点并行回填的计数与在飞配额：读线程与 worker 共享，demux 线程侧的
        // `stream_seek_hint` 读它的自适应闸门（所以必须在 spawn 之前建好）。
        let seek_fill = Arc::new(SeekFillDiag::default());
        let seek_fill_quota = Arc::new(AtomicUsize::new(0));
        let handle = {
            let shared = Arc::clone(&shared);
            let uri = Arc::clone(&self.uri);
            let headers = self.http_headers.clone();
            let epoch = Arc::clone(&self.stream_epoch);
            let arrived_thread = Arc::clone(&arrived);
            let in_flight_thread = Arc::clone(&in_flight);
            let reconnects_thread = Arc::clone(&reconnects);
            let journal_thread = Arc::clone(&journal);
            let arrival_thread = Arc::clone(&last_arrival_ms);
            let fill_thread = Arc::clone(&fill_bps);
            let stop_thread = Arc::clone(&stop);
            let seek_fill_thread = Arc::clone(&seek_fill);
            let quota_thread = Arc::clone(&seek_fill_quota);
            // 本流的取消域：宿主拆除时置位，长读的 body 泵立刻退出，读线程不必
            // 等到段超时（见 `SourceCancellation`）。
            let cancellation_thread = self.cancellation.child();
            let diag_slot = self.diag_slot;
            thread::Builder::new()
                .name(format!("erika-http-stream-{}", self.diag_slot))
                .spawn(move || {
                    sequential_reader_loop(
                        shared,
                        uri,
                        headers,
                        epoch,
                        arrived_thread,
                        in_flight_thread,
                        reconnects_thread,
                        stop_thread,
                        diag_slot,
                        fill_thread,
                        journal_thread,
                        arrival_thread,
                        done_tx,
                        seek_fill_thread,
                        quota_thread,
                        cancellation_thread,
                    );
                })
                .expect("failed to spawn sequential stream thread")
        };
        self.stream_epoch.store(self.host_epoch, Ordering::Relaxed);
        self.stream = Some(SequentialReader {
            shared,
            arrived,
            fill_bps,
            in_flight,
            reconnects,
            journal,
            last_arrival_ms,
            stop,
            done: done_rx,
            handle: Some(handle),
            // 跨节点并行回填的计数与在飞配额（见 `SeekFillDiag`）。
            // **必须复用上面 spawn 读线程时创建的那两个对象**（`seek_fill` /
            // `seek_fill_quota`）：读线程记账、`stream_seek_hint` 读自适应闸门、
            // `diag_snapshot` 读计数——三处必须看到**同一个** `SeekFillDiag`。
            // 这里若再 `Arc::new` 一份，log 里并行明明成功、诊断快照却是全零
            // （测试 `seek_fill_parallel_beats_single_connection_under_throttle_*`
            // 当场抓住；自适应闸门 `note_gain` 也随之失效）。
            seek_fill: Arc::clone(&seek_fill),
            seek_fill_quota: Arc::clone(&seek_fill_quota),
        });
    }

    /// 当前预算结算：输入 = 实测码率（`consume_bps`，`ERIKA_HTTP_STREAM_BITRATE_BPS`
    /// 可覆盖）+ **实测吞吐（唯一入口 [`supply_estimate_bps`]）**；输出 = 前向/后向
    /// 字节数 + 内存硬上限 + 前向秒数。口径与公式见文件上方"滑动窗口预算"整段。
    /// 顺序长读的**供给健康度**：换源判据只认"前向缓冲够不够看"，不认吞吐数字
    /// （背压语义下吞吐必然收敛到消费速率，见 `MULTI_HOST_STARVE_SECS` 注释）。
    fn stream_health(&self) -> StreamHealth {
        let Some(stream) = self.stream.as_ref() else {
            // 旧分段路径：没有长流窗口，"前向可播秒数"无从谈起 → 只让
            // "长时间没有字节到达"这条死连判据生效（见 `measurable`）。
            return StreamHealth {
                forward_secs: 0.0,
                window_full: false,
                measurable: false,
                // 旧分段路径没有长流前沿，判不了"已读到文件终点"：保守为 false
                // （保持既有行为——那条路径本来就"只能靠长时间没字节判死连"）。
                source_complete: false,
            };
        };
        let (front_len, window_full, source_complete) = {
            let g = lock_stream(&stream.shared);
            (
                g.front_len(),
                g.front_reached_budget(),
                // **「不需要更多字节」**：网络填充前沿到了服务端声明的终点。
                // 与 `diag_snapshot` 里 `whole_file_cached` 同一个判据，两处必须
                // 一致（一处给宿主看，一处给换源判据用）。
                g.total
                    .is_some_and(|total| total > 0 && g.write_pos() >= total),
            )
        };
        // 码率未知（开播最初 ~2s）时按"配置的 read_ahead ≈ 30 秒"折算：宁可保守，
        // 也不要漏判真饥饿。
        let bitrate = window_bitrate_override()
            .or_else(|| (self.consume_bps > 0.0).then_some(self.consume_bps))
            .unwrap_or_else(|| self.read_ahead_bytes.max(1) as f64 / WINDOW_FORWARD_SECS_COMFORT);
        let forward_secs = if bitrate > 0.0 {
            front_len as f64 / bitrate
        } else {
            f64::MAX
        };
        StreamHealth {
            forward_secs,
            window_full,
            measurable: true,
            source_complete,
        }
    }

    /// 当前活跃 host（面板/日志用）。
    fn active_host_label(&self) -> String {
        self.candidates
            .get(self.active_idx)
            .map(|candidate| host_of(&candidate.uri).to_string())
            .unwrap_or_else(|| "?".to_string())
    }

    /// 指定候选的 host（换源日志用）。
    fn active_host_label_of(&self, index: usize) -> String {
        self.candidates
            .get(index)
            .map(|candidate| host_of(&candidate.uri).to_string())
            .unwrap_or_else(|| "?".to_string())
    }

    fn window_budget_now(&self) -> WindowBudget {
        let bitrate = window_bitrate_override()
            .or_else(|| (self.consume_bps > 0.0).then_some(self.consume_bps))
            .unwrap_or(0.0);
        window_budget(
            self.diag_slot,
            self.read_ahead_bytes,
            self.back_bytes,
            bitrate,
            supply_estimate_bps(self.diag_slot),
        )
    }

    /// 按最新吞吐/码率结算预算并同步到读窗口（上限变化会立刻裁剪超出部分，
    /// 保证"内存不超过硬上限"不是渐进承诺）。由 `publish_diag_throttled`
    /// 每 500ms 调一次（首次必调）。
    fn refresh_window_budget(&mut self) {
        let budget = self.window_budget_now();
        self.window = budget;
        // 冷区也要知道文件总长：`covers` 只有在 total 已知时才敢判命中（免得把
        // 尾部之外的地址判成"有字节"）。归档模式不再自行发请求，因此总长必须
        // 由这条长流回填给它——否则冷区永远判不出命中，磁盘分层就白做了。
        if let (Some(state), Some((_, _, Some(total)))) =
            (self.full_preload.as_ref(), self.stream_window())
        {
            state.learn_total(total);
        }
        let Some(shared) = self
            .stream
            .as_ref()
            .map(|stream| Arc::clone(&stream.shared))
        else {
            return;
        };
        let front_cap = usize::try_from(budget.forward_bytes)
            .unwrap_or(usize::MAX)
            .max(STREAM_MIN_FRONT_CAP);
        let back_cap = usize::try_from(budget.back_bytes).unwrap_or(0);
        lock_stream(&shared).set_caps(front_cap, back_cap);
    }

    /// 记录 demux 消费推进，维护**实测消费速率**（= 当前档位码率的代理）。
    ///
    /// 口径：消费前沿推进量 / 时间，2s 窗口差分 + 0.3/0.7 EMA（与带宽估算同款）。
    /// 稳态播放时缓冲被背压钳住，demux 只能按播放速度消费，因此这个速率就是
    /// 当前 ABR 档位的有效码率——正是预算公式要的输入（不需要宿主告诉我们）。
    ///
    /// 两个防失真点：① 回拖/重播（消费前沿倒退）只重置窗口、不更新 EMA；
    /// ② 两次采样间隔超过 `CONSUME_SAMPLE_MAX_GAP`（长暂停 / 长段冷区命中）
    /// 同样不更新 EMA——否则一次跨分钟的累计消费量会被当成瞬时码率，
    /// 把预算公式喂成天文数字。
    fn note_consumed(&mut self, cursor: u64) {
        let now = Instant::now();
        if cursor < self.last_consumed_pos {
            self.last_consumed_pos = cursor;
            self.consume_window_bytes = 0;
            self.last_consume_at = now;
            return;
        }
        let delta = cursor - self.last_consumed_pos;
        self.last_consumed_pos = cursor;
        let elapsed = now.duration_since(self.last_consume_at);
        if elapsed < BANDWIDTH_ESTIMATE_WINDOW {
            self.consume_window_bytes = self.consume_window_bytes.saturating_add(delta);
            return;
        }
        let window = self.consume_window_bytes.saturating_add(delta);
        if elapsed <= CONSUME_SAMPLE_MAX_GAP && window > 0 {
            let inst = window as f64 / elapsed.as_secs_f64();
            let mut bps = if self.consume_bps > 0.0 {
                self.consume_bps * (1.0 - BANDWIDTH_EMA_NEW) + inst * BANDWIDTH_EMA_NEW
            } else {
                inst
            };
            if !bps.is_finite() || bps < 1.0 {
                bps = 0.0; // 与 est_bps 同一套"零供给地板"语义（0 = 未测得）
            }
            self.consume_bps = bps;
        }
        self.consume_window_bytes = 0;
        self.last_consume_at = now;
    }

    /// 冷区（磁盘分层）**零网络**读：先试整段命中，再退化为"能取多少取多少"的
    /// 前缀命中（跨冷/热边界的读靠它避免一次无谓的网络请求）。
    ///
    /// 命中计入 `cold_hits` / `cold_hit_bytes`（既有诊断槽 + 新增字段），
    /// 宿主 500ms 轮询可见——现场"回拖到底有没有打网络"靠它就够判断。
    fn read_cold(&self, offset: u64, max: u64) -> Option<Vec<u8>> {
        let state = self.full_preload.as_ref()?;
        // 无锁预筛：正常前进播放的读大多在这里就被挡掉（冷区在播放位置之后）。
        if !state.might_cover(offset) {
            return None;
        }
        if let Some(bytes) = state.read_cached(offset, max) {
            return Some(bytes);
        }
        state.read_available_at(offset, max)
    }

    /// 长读窗口快照 `(base, write_pos, total)`（诊断用）。
    fn stream_window(&self) -> Option<(u64, u64, Option<u64>)> {
        let shared = Arc::clone(&self.stream.as_ref()?.shared);
        let g = lock_stream(&shared);
        Some((g.base, g.write_pos(), g.total))
    }

    /// 诊断发布**节流**：长读路径每读 32KB 都要经过这里，每次都发布会把三个
    /// 进程级全局锁（`SOURCE_DIAG`/`SOURCE_BW`/`SOURCE_RX`）拉进 demux 热路径
    /// （~100 次/秒）。500ms 一次足够宿主 500ms 轮询读到新鲜值——但**首次**
    /// 一定发布（`last_stream_publish_ms` 初值 0），否则宿主在开播后一段
    /// 时间里读到的是全零快照。
    fn publish_diag_throttled(&mut self) {
        let now = now_ms();
        if now.saturating_sub(self.last_stream_publish_ms) >= STREAM_DIAG_PUBLISH_MS {
            self.last_stream_publish_ms = now;
            // 预算跟着诊断一起结算（同一个 500ms 节流）：读窗口容量随实测
            // 码率/吞吐重新定尺——弱网放宽前向秒数、字节不越内存硬上限。
            self.refresh_window_budget();
            self.publish_diag();
        }
    }

    /// 组装一次 **seek 出窗的跨节点并行回填**计划（`None` = 不并行，走既有单连接）。
    ///
    /// 纯计算 + 从既有候选表取材；**不碰任何状态**（窗口重设由调用点在同一把锁内
    /// 完成），因此可以单测。
    ///
    /// 不并行的情形（每一种都退回改造前的单连接行为，逐字节等价）：
    ///   - `ERIKA_SEEK_PARALLEL=0`：现场回退开关；
    ///   - 自适应闸门已因"实测无叠加"关闭（见 `SeekFillDiag::note_gain`）；
    ///   - 按实测单流通量算出来的条数 < 2（快链路本来就够快，白开连接只涨请求数）；
    ///   - **候选表里少于 2 个不同 host**：用户要求"宁可慢也不用可疑形态"，而
    ///     "单节点多连接"正是上游告警点名的形态，所以**直接不做**；
    ///   - 回填窗口放不下 ≥2 个达标段。
    fn seek_fill_plan(&self, target: u64, generation: u64, front_cap: u64) -> Option<SeekFillPlan> {
        if !seek_parallel_enabled() {
            return None;
        }
        let stream = self.stream.as_ref()?;
        if stream.seek_fill.disabled() {
            return None;
        }
        let per_conn_bps = f64::from_bits(stream.fill_bps.load(Ordering::Relaxed));
        let bytes = SEEK_PARALLEL_FILL_BYTES.min(front_cap);
        let wanted = seek_parallel_connections(bytes, per_conn_bps);
        if wanted < 2 {
            return None;
        }
        let hosts = self.fanout_candidates(wanted);
        if hosts.len() < 2 {
            return None;
        }
        let segments_count = wanted.min(hosts.len());
        let per_segment = bytes / segments_count as u64;
        let mut segments = Vec::with_capacity(segments_count);
        let mut cursor = target;
        for (index, (uri, host)) in hosts.into_iter().enumerate() {
            let length = if index + 1 == segments_count {
                target.saturating_add(bytes).saturating_sub(cursor)
            } else {
                per_segment
            };
            if length == 0 {
                break;
            }
            // 首段没有前向重叠可比对（它直接续在窗口前沿上）；其余段各向前多要
            // [`SEEK_PARALLEL_OVERLAP_BYTES`] 作为与邻段的比对样本——那些字节
            // **本来就在回填窗口里**（前一段的末尾），所以校验不额外多拉数据。
            let overlap = if index == 0 {
                0
            } else {
                SEEK_PARALLEL_OVERLAP_BYTES.min(cursor.saturating_sub(target))
            };
            segments.push(SeekFillSegment {
                start: cursor,
                length,
                overlap,
                uri,
                host,
            });
            cursor = cursor.saturating_add(length);
        }
        if segments.len() < 2 {
            return None;
        }
        let bytes = cursor.saturating_sub(target);
        Some(SeekFillPlan {
            generation,
            start: target,
            bytes,
            segments,
            per_conn_bps,
        })
    }

    /// 取至多 `n` 个**不同 host** 的候选（**活跃 host 优先**），用于跨节点分段。
    ///
    /// 为什么活跃 host 必须在里面：段 0 直接续在窗口前沿上，让 demux 先从**已经在
    /// 用的**那条链路拿到字节；其余节点并行补后面。
    /// 为什么按 `host_of` 去重：**每个节点只允许一条连接**（见文件上方整段）——
    /// 同一个 host 出现两次就没有"跨节点"可言了。
    fn fanout_candidates(&self, n: usize) -> Vec<(String, String)> {
        let len = self.candidates.len();
        if len < 2 {
            return Vec::new();
        }
        let mut out: Vec<(String, String)> = Vec::with_capacity(n.min(len));
        for offset in 0..len {
            let index = (self.active_idx + offset) % len;
            let candidate = &self.candidates[index];
            let host = host_of(&candidate.uri).to_string();
            if out.iter().any(|(_, seen)| *seen == host) {
                continue;
            }
            out.push((candidate.uri.clone(), host));
            if out.len() >= n {
                break;
            }
        }
        out
    }

    /// 窗口外 seek 的目标提示（由 AVIO seek 钩子 `prefetch_hint` 调用）。
    ///
    /// 命中窗口（含后向缓冲）→ 零网络；超过前沿一点点 → 等读线程填上来；
    /// 真正跳出去 → **重设窗口**：`generation` +1，读线程在下一个 64KB 块边界
    /// 放弃当前连接、从新起点重发一条 open-ended Range。
    /// 这就是"一次 seek = 一个请求"的全部实现（旧路径这里是"清空段队列 +
    /// 重发 2 个预取段"，同样是 1~2 个请求，但那是段模型的产物）。
    fn stream_seek_hint(&mut self, target: u64) {
        let Some(shared) = self
            .stream
            .as_ref()
            .map(|stream| Arc::clone(&stream.shared))
        else {
            // 长流还没起（尚未开始读）→ 无需提示：首次读会以那时的位置起窗。
            return;
        };
        {
            let g = lock_stream(&shared);
            // 窗口内（含后向缓冲）→ 零网络；只领先前沿一点点 → 等读线程填上来。
            // **回退到窗口之前必须重设**（`target < base` 落不进下面这条），
            // 那正是 mpv 的行为：回退超出后向缓冲就得重新请求。
            if target >= g.base
                && target <= g.write_pos().saturating_add(STREAM_FORWARD_SLACK_BYTES)
            {
                return;
            }
        }
        // ② **统一缓存视图的第二个持有者**：冷区（磁盘分层）。目标落在冷区里
        //    → 同样零网络，**绝不重设窗口、绝不重发请求**（缺陷 ⑤ 的核心断言：
        //    回拖到已缓冲/已落盘的区域必须 0 个新请求）。同时给冷区线程发一次
        //    **预读**：用户还在拖、demux 还没读到这里，这段时间足够把目标附近
        //    读进页缓存，真播到时是页缓存命中而不是裸盘随机读。
        if let Some(state) = self.full_preload.as_ref()
            && state.covers_at(target)
        {
            state.request_warm(target);
            http_trace_log(format!(
                "{{\"event\":\"http_stream_seek_cold_hit\",\"target\":{},\"cold_bytes\":{}}}",
                target,
                state.cold_bytes(),
            ));
            return;
        }
        // 防抖：`av_seek_frame` 内部可能对相近位置多次 seek，同一窗口不重复重设。
        // **只对前向提示防抖**（`target >= prev`）：回退提示必须生效，否则一次
        // "跳到 30 分钟再拖回 5 分钟"会被上一条前向提示吞掉，读路径随即在窗口外
        // 重新重设——白多一个请求，还多绕一圈。
        if let Some(prev) = self.prefetch_target
            && target >= prev
            && target < prev.saturating_add(Self::SEEK_PREFETCH_SEGMENT_BYTES)
        {
            return;
        }
        self.prefetch_target = Some(target);
        http_trace_log(format!(
            "{{\"event\":\"http_stream_seek_hint\",\"target\":{}}}",
            target,
        ));
        // 窗口重设 + **跨节点并行回填计划**（同一把锁内完成，两者不会交错：
        // 读线程要么看到"新代数 + 新计划"，要么什么都没看到）。
        // 计划里的每一段来自**不同候选 host**，每 host 一条连接；候选不足时
        // `seek_fill_plan` 返回 None，行为与改造前逐字节相同。
        let plan = {
            let mut g = lock_stream(&shared);
            let generation = g.generation.wrapping_add(1); // reset 之后的代数
            let front_cap = g.front_cap as u64;
            let plan = self.seek_fill_plan(target, generation, front_cap);
            g.reset(target);
            g.fill_plan = plan.clone();
            plan
        };
        if let Some(plan) = plan.as_ref() {
            http_trace_log(format!(
                "{{\"event\":\"http_stream_seek_parallel_planned\",\"target\":{},\"bytes\":{},\"segments\":{},\"per_conn_bps\":{:.0},\"hosts\":\"{}\"}}",
                plan.start,
                plan.bytes,
                plan.segments.len(),
                plan.per_conn_bps,
                plan.segments
                    .iter()
                    .map(|segment| segment.host.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            ));
        }
    }

    /// **顺序长读的读路径**：窗口命中直接返回（零网络），窗口外重设（+1 请求），
    /// 其余情况等读线程把字节填上来。
    ///
    /// - 完整命中 → 拷贝返回，并 `consume_to` 推进前向缓冲（**这一步必须做**：
    ///   `front_len` 是读线程唯一的节流依据，忘记推进会永久停读）。
    /// - 等不到字节 → 返回 [`SourceError::Transient`]，外层
    ///   [`HttpRangeSource::read_range_with_transient_wait`] 照旧消化（等待窗口
    ///   语义、`transient_wait` 诊断、换 host 全部不变）。
    /// - 文件尾（`eof`）→ 返回空 Vec（AVIO 层据此判 EOF），与旧路径一致。
    /// - `range.length == None`（"读到文件尾"：弹幕/字幕这类 sidecar）→ 累积到 EOF。
    fn read_range_stream(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        self.ensure_stream(range.start);
        let Some(shared) = self
            .stream
            .as_ref()
            .map(|stream| Arc::clone(&stream.shared))
        else {
            return Err(SourceError::Unsupported(
                "sequential stream is not enabled".to_string(),
            ));
        };
        let front_cap = lock_stream(&shared).front_cap as u64;
        // 0 长度请求：直接返回空。下面 `want` 会把长度钳到 ≥1（"等一个字节"的
        // 语义需要它），因此必须在钳之前拦掉 0——否则 0 长度读会返回 1 个字节。
        if range.length == Some(0) {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + HTTP_FETCH_TOTAL_BUDGET;
        let mut out: Vec<u8> = Vec::new();
        let mut cursor = range.start;
        loop {
            // 单次请求的读取量不超过前向窗口容量（否则永远填不满 —— 那是
            // "窗口外"的语义，由下面的重设分支处理）。
            let want = match range.length {
                Some(length) => length.clamp(1, front_cap.max(1)),
                None => front_cap.max(1),
            };
            // ① 冷区优先（**磁盘分层**）：热区之前的已下好区间从这里零网络读回，
            //    包含跨越冷/热边界的读（拿到冷区能给的字节就返回，上层按短读续）。
            //    必须在这里拦——下面的 `cursor < g.base` 会把"回拖的读"判成
            //    "seek 出窗"并重设窗口（= 重发请求），那正是缺陷 ⑤ 要消灭的行为。
            if let Some(bytes) = self.read_cold(cursor, want) {
                if range.length.is_none() {
                    cursor = cursor.saturating_add(bytes.len() as u64);
                    out.extend_from_slice(&bytes);
                    self.note_consumed(cursor);
                    self.publish_diag_throttled();
                    continue;
                }
                self.note_consumed(cursor.saturating_add(bytes.len() as u64));
                trace_read_return("http_stream_read_cold", range, self, &bytes);
                self.publish_diag_throttled();
                return Ok(bytes);
            }
            {
                let mut g = lock_stream(&shared);
                g.consume_to(cursor);
                let fill = g.write_pos();
                if cursor < g.base || cursor > fill.saturating_add(STREAM_FORWARD_SLACK_BYTES) {
                    // 窗口外的随机读 = seek 语义。正常顺序播放**不会**走到这里。
                    http_trace_log(format!(
                        "{{\"event\":\"http_stream_seek\",\"start\":{},\"base\":{},\"fill\":{}}}",
                        cursor, g.base, fill,
                    ));
                    g.reset(cursor);
                }
                if range.length.is_some() {
                    if let Some(bytes) = g.read(cursor, want) {
                        g.consume_to(cursor.saturating_add(bytes.len() as u64));
                        drop(g);
                        // 消费速率（= 当前档位码率的实测代理）：预算公式的输入，
                        // 见文件上方"滑动窗口预算"整段。
                        self.note_consumed(cursor.saturating_add(bytes.len() as u64));
                        trace_read_return("http_stream_read", range, self, &bytes);
                        // 节流发布：宿主 500ms 轮询要能看到新鲜窗口/请求数，
                        // 但每次 32KB 读都发布会把全局锁拖进热路径。
                        self.publish_diag_throttled();
                        return Ok(bytes);
                    }
                    if g.eof {
                        // 文件尾：给出 cursor 处可得的前缀（可能为空 → 上层按 EOF 处理）。
                        let tail = g
                            .read_available(cursor, usize::try_from(want).unwrap_or(usize::MAX))
                            .unwrap_or_default();
                        drop(g);
                        trace_read_return("http_stream_read_eof", range, self, &tail);
                        self.publish_diag_throttled();
                        return Ok(tail);
                    }
                } else {
                    // 读到文件尾：把当前窗口里 cursor 之后的字节全部取走。
                    if let Some(bytes) =
                        g.read_available(cursor, usize::try_from(want).unwrap_or(usize::MAX))
                    {
                        cursor = cursor.saturating_add(bytes.len() as u64);
                        out.extend_from_slice(&bytes);
                    }
                    if g.eof && g.write_pos() <= cursor {
                        drop(g);
                        return Ok(out);
                    }
                }
                if let Some(error) = g.error.take() {
                    return Err(SourceError::Http(error));
                }
            }
            if Instant::now() >= deadline {
                // 20s 拿不到完整请求 → 交回外层等待窗口（语义与旧路径的总预算一致）。
                return Err(SourceError::Transient(format!(
                    "sequential stream stalled at {cursor} for {:?}",
                    HTTP_FETCH_TOTAL_BUDGET
                )));
            }
            {
                // 分片等待（250ms）：等新字节、期间仍能换 host / 刷新诊断。
                let guard = lock_stream(&shared);
                let (guard, _timeout) = shared
                    .data_cv
                    .wait_timeout(guard, STREAM_WAIT_SLICE)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // **必须显式 drop**：下一轮循环要重新加锁，非递归 Mutex 重入
                // 会自死锁（本文件历史上踩过的同类陷阱）。
                drop(guard);
            }
            self.maybe_switch_host();
            // 仍在等数据 = demux 仍在消费（"在播且饿着"）：刷新活跃时间戳，
            // 否则一次挂在等待循环里的读调用会随时间"过期"，把真饥饿误判成暂停
            // （真机现象：`vstall=16s→21s`，判据却一直 Hold 不动）。
            self.last_read_call_ms = now_ms();
            if let Some(state) = self.full_preload.as_ref() {
                // 仍在消费 → 让桶继续让路（把带宽留给这条长流）。
                state.mark_demux_read();
                if let Some((_, fill, _)) = self.stream_window() {
                    state.set_floor(fill);
                }
            }
            self.publish_diag_throttled();
        }
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
        let snapshot = self.diag_snapshot();
        if let Ok(mut slots) = SOURCE_DIAG.lock() {
            if let Some(slot) = slots.get_mut(self.diag_slot) {
                *slot = snapshot;
            }
        }
    }

    /// 构造本槽的诊断快照（`publish_diag` 负责把它写进全局槽）。
    ///
    /// 拆出来的理由：单测要断言字段语义，而全局槽是**进程级共享**的——并行
    /// 用例互相覆盖会让断言随机失败（本文件多处注释记录过这个坑）。本方法
    /// 直接返回快照，断言不碰共享状态。
    fn diag_snapshot(&mut self) -> SourceDiagSnapshot {
        // 多源无缝换 host：诊断刷新时顺带判定死连/慢速并切到下一候选。
        self.maybe_switch_host();
        // 顺序长读的窗口优先：新模型下 demux 侧的真实窗口就是 `StreamBuffer`
        // （`base..write_pos`），前向深度由读线程的背压维持在 read_ahead 左右。
        // 旧字段 `cache_start`/`cache_bytes` 在长读路径下恒为空，不能再用它们
        // 算窗口，否则宿主读到的是"缓存恒 0"（策略层会立刻判饥饿）。
        let (demux_start, demux_end, stream_in_flight, whole_file_cached) =
            match self.stream.as_ref() {
                Some(stream) => {
                    let (base, write_pos, total) = {
                        let g = lock_stream(&stream.shared);
                        (g.base, g.write_pos(), g.total)
                    };
                    (
                        base,
                        write_pos,
                        stream.in_flight.load(Ordering::Relaxed) as usize,
                        // **「不需要再下载了」的权威判据**：网络填充前沿已经到达
                        // 服务端声明的文件终点。此后 `write_pos` 不再前进、
                        // 「距上次收到字节」必然一路涨——那是预期，不是死连。
                        // 只认长读窗口自己的 `write_pos`：桶的 `covered_until`
                        // 只增不减且 seek 后仍停旧前沿，判不了这件事。
                        total.is_some_and(|total| total > 0 && write_pos >= total),
                    )
                }
                None => {
                    let end = self.cache_end();
                    (
                        self.cache_start,
                        end,
                        0,
                        self.content_length
                            .is_some_and(|total| total > 0 && end >= total),
                    )
                }
            };
        // 长读路径下 total 由首个 206 的 Content-Range 回填：把它并进
        // `content_length`，`len()` 就不必再发一个 HEAD（旧路径同样由首个
        // 206 回填，语义一致）。
        if self.content_length.is_none() {
            if let Some((_, _, Some(total))) = self.stream_window() {
                self.content_length = Some(total);
            }
        }
        // 长读重连计数变化 → trace 一条。这是现场核对"这次播放到底发了几个
        // 请求、以及**为什么**发的"的直接证据：正常顺序播放期间它应当为 0。
        let reconnect_counters = self
            .stream
            .as_ref()
            .map(|stream| Arc::clone(&stream.reconnects));
        let reconnect_count = reconnect_counters.as_ref().map(|counters| counters.total());
        if let (Some(reconnects), Some(count)) = (reconnect_counters.as_ref(), reconnect_count) {
            if count != self.stream_reconnects_seen {
                self.stream_reconnects_seen = count;
                http_trace_log(format!(
                    "{{\"event\":\"http_stream_reconnect\",\"slot\":{},\"count\":{},\"early_close\":{},\"empty\":{},\"reset\":{},\"host_switch\":{},\"chunked\":{},\"fill\":{}}}",
                    self.diag_slot,
                    count,
                    reconnects.get(ReconnectCause::EarlyClose),
                    reconnects.get(ReconnectCause::Empty),
                    reconnects.get(ReconnectCause::Reset),
                    reconnects.get(ReconnectCause::HostSwitch),
                    reconnects.get(ReconnectCause::PlannedChunk),
                    demux_end,
                ));
            }
        }
        // 合并桶数据：缓存窗口前沿 = max(demux 端前沿, 桶 covered_until)。
        // 用户定规的核心表征——「缓冲百分比」「下载速度」—都通过这两个字段
        // 反映：桶独立于 demux 推进，pause 后桶仍覆盖到更前方，诊断面板如实
        // 显示「缓冲 80% 仍在涨」。
        let cache_end = match self.full_preload.as_ref() {
            Some(p) => demux_end.max(p.covered_until()),
            None => demux_end,
        };
        let prefetch_in_flight = match self.full_preload.as_ref() {
            Some(p) => self.prefetch.len() + stream_in_flight + p.in_flight_segments() as usize,
            None => self.prefetch.len() + stream_in_flight,
        };
        // 供给信号一律走**实时**口径：取本槽登记的全部计数器之和（源自身 rx +
        // 整片预载桶），而不是「段完成才累加」的 `self.downloaded_bytes`。
        let downloaded_bytes = live_rx_bytes(self.diag_slot);
        // 拉模型结算（见 `refresh_bandwidth`）：此处刷新后写入 `self.est_bps`，
        // 段大小/join 超时的自适应也跟着用上实时口径，慢网下不再冻结。
        self.est_bps = refresh_bandwidth(self.diag_slot, downloaded_bytes);
        let uri_guard = self.uri.lock().expect("uri poisoned");
        let active_host = host_of(&uri_guard);
        // HTTP 请求画像：进程级单调计数，这里只做快照回填（真正的新鲜值由宿主
        // 轮询时的 `source_diag_snapshot_slot` 直接读计数器给出，见那里的注释）。
        let (http_requests, http_request_bytes) = match http_request_profile(self.diag_slot) {
            Some(profile) => (
                profile.requests.load(Ordering::Relaxed),
                profile.bytes.load(Ordering::Relaxed),
            ),
            None => (0, 0),
        };
        let mut snapshot = SourceDiagSnapshot {
            content_length: self.content_length,
            cache_start: demux_start,
            cache_end,
            prefetch_in_flight,
            downloaded_bytes,
            est_bps: self.est_bps,
            active_host: active_host.to_string(),
            active_idx: self.active_idx,
            candidate_count: self.candidates.len(),
            // demux 自身窗口终点（不含桶）：宿主策略算真实缓冲秒用
            demux_cache_end: demux_end,
            // 「本片已完整缓冲」：宿主用它把"不需要下载"从"链路死了"里分出来。
            whole_file_cached,
            host_epoch: self.host_epoch,
            transient_wait: self.transient_wait,
            http_requests,
            http_request_bytes,
            // ---- 滑动窗口分层缓存（口径见 `SourceDiagSnapshot` 各字段注释）----
            // 内存层 = 热区窗口实实在在躺着的字节；旧分段路径下是 demux 自己的
            // 缓存窗口（两条路径都如实反映"内存里现在有多少"）。
            cache_memory_bytes: match self.stream.as_ref() {
                Some(stream) => lock_stream(&stream.shared).resident_bytes(),
                None => self.cache_bytes.len() as u64,
            },
            cache_disk_bytes: self
                .full_preload
                .as_ref()
                .map(|state| state.cold_bytes())
                .unwrap_or(0),
            cold_hits: self
                .full_preload
                .as_ref()
                .map(|state| state.cold_hit_count())
                .unwrap_or(0),
            cold_hit_bytes: self
                .full_preload
                .as_ref()
                .map(|state| state.cold_hit_total_bytes())
                .unwrap_or(0),
            memory_budget_bytes: self.window.memory_bytes,
            forward_budget_bytes: self.window.forward_bytes,
            // 前向可播秒数 = 已填的前向字节 ÷ 实测码率。码率未知（冷启动）时为 0：
            // 不猜数字，宿主按 0 处理即可（既有策略对本字段是新增读取）。
            forward_playable_secs: match (self.stream.as_ref(), self.window.bitrate_bps) {
                (Some(stream), bitrate) if bitrate > 0.0 => {
                    let front = {
                        let g = lock_stream(&stream.shared);
                        g.front_len() as f64
                    };
                    front / bitrate
                }
                _ => 0.0,
            },
            fill_throughput_bps: match self.stream.as_ref() {
                Some(stream) => f64::from_bits(stream.fill_bps.load(Ordering::Relaxed)),
                None => load_fill_bps(self.diag_slot),
            },
            // 连接重建按原因分桶：宿主/现场据此判"是不是我们的锅"，见字段注释。
            stream_reconnects_total: reconnect_count.unwrap_or(0),
            stream_reconnects_early_close: reconnect_counters
                .as_ref()
                .map(|counters| counters.get(ReconnectCause::EarlyClose))
                .unwrap_or(0),
            stream_reconnects_empty: reconnect_counters
                .as_ref()
                .map(|counters| counters.get(ReconnectCause::Empty))
                .unwrap_or(0),
            stream_reconnects_reset: reconnect_counters
                .as_ref()
                .map(|counters| counters.get(ReconnectCause::Reset))
                .unwrap_or(0),
            stream_reconnects_host_switch: reconnect_counters
                .as_ref()
                .map(|counters| counters.get(ReconnectCause::HostSwitch))
                .unwrap_or(0),
            stream_chunk_resumes: reconnect_counters
                .as_ref()
                .map(|counters| counters.get(ReconnectCause::PlannedChunk))
                .unwrap_or(0),
            // 判决实验：请求起点序列的形态分桶（断点续传 vs demux 跳读）。
            stream_range_contiguous: self
                .stream
                .as_ref()
                .map(|stream| stream.journal.contiguous.load(Ordering::Relaxed))
                .unwrap_or(0),
            stream_range_forward_jumps: self
                .stream
                .as_ref()
                .map(|stream| stream.journal.forward_jump.load(Ordering::Relaxed))
                .unwrap_or(0),
            stream_range_backward_jumps: self
                .stream
                .as_ref()
                .map(|stream| stream.journal.backward_jump.load(Ordering::Relaxed))
                .unwrap_or(0),
            stream_range_last_start: self
                .stream
                .as_ref()
                .map(|stream| stream.journal.last_start.load(Ordering::Relaxed))
                .unwrap_or(0),
            stream_last_connection_bytes: reconnect_counters
                .as_ref()
                .map(|counters| counters.last_connection_bytes())
                .unwrap_or(0),
            stream_max_connection_bytes: reconnect_counters
                .as_ref()
                .map(|counters| counters.max_connection_bytes())
                .unwrap_or(0),
            // 换源可读原因（面板直接显示，见 `last_switch_reason` 注释）。
            last_host_switch_reason: self.last_switch_reason.clone(),
            // ---- seek 出窗的跨节点并行回填 ----
            // 先落默认（旧分段路径没有长流窗口，也就没有这条统计），有长读路径
            // 时再让 `SeekFillDiag` 自己把真实计数写进来。
            seek_fill_batches: 0,
            seek_fill_connections: 0,
            seek_fill_bytes: 0,
            seek_fill_gate_denied: 0,
            seek_fill_fallbacks: 0,
            seek_fill_overlap_ok: 0,
            seek_fill_overlap_mismatch: 0,
            seek_fill_last_segments: 0,
            seek_fill_in_flight: 0,
            seek_fill_last_bytes: 0,
            seek_fill_last_ms: 0,
            seek_fill_last_expected_ms: 0,
            seek_fill_last_fallback: false,
            seek_fill_last_gate_reason: String::new(),
            seek_fill_disabled: false,
        };
        if let Some(stream) = self.stream.as_ref() {
            stream.seek_fill.fill_snapshot(&mut snapshot);
            // "现在有几条分段连接在飞"——用户要的"用了几条连接"当场可读。
            snapshot.seek_fill_in_flight = stream.seek_fill_quota.load(Ordering::Relaxed) as u64;
        }
        snapshot
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
            http_request_profile(self.diag_slot),
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
                // 预取线程是独立 `std::thread`，拿不到 `self.diag_slot`——
                // 必须在 spawn 时把它传进去，否则预取请求的计数会丢（预取
                // 正是 1080P 播放中的主要请求来源）。
                self.diag_slot,
            ));
            self.publish_diag();
            cursor = cursor.saturating_add(len);
        }
    }

    /// 窗口外 seek 的目标预取提示（由 AVIO seek 钩子调用）。
    ///
    /// **顺序长读启用时走 `stream_seek_hint`**（见其注释：命中窗口零网络，否则
    /// 重设窗口让读线程重发**一条** open-ended Range），本函数以下部分只服务
    /// 旧的分段路径：target 超出已缓存窗口时，清掉旧窗口的顺序预取段，从
    /// target 前一个小段起用小段并行拉目标窗口——seek 后 demux 第一次 read
    /// 命中这些小段并 join，从"同步单请求抓大段（可卡 60s）"降为"join 小段
    /// （约 1s 就绪）"。
    fn prefetch_hint(&mut self, target: u64) {
        // 顺序长读：命中窗口零网络；窗口外重设窗口（读线程重发一条连接）。
        // 见 `stream_seek_hint`——这是"seek 后仍然只 +1 个请求"的实现处。
        if self.sequential_enabled {
            self.stream_seek_hint(target);
            return;
        }
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
        // 源诊断槽：预取线程要把本段请求记进对应槽的 HTTP 请求画像
        //（`fetch_http_range` 的第 7 参）。线程闭包捕获不到源实例，故传入。
        diag_slot: usize,
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
                http_request_profile(diag_slot),
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
        // —— 顺序长读线程：置 stop → 唤醒等待 → **有界等待**它退出 ——
        //
        // 不能用裸 `join()`：读线程可能正阻塞在 socket `read()` 上（服务端挂死，
        // 而长读 agent 故意没有 body 总时长超时），join 会把 UI 线程钉在关窗动作
        // 上——正是用户最忌讳的"关窗卡死"。有界等待（500ms）超时即 detach：
        // 线程持有的全是 `Arc`（不借用源实例），它会在下一个 stop 检查点自行退出，
        // 句柄丢弃不构成 use-after-free（detached 线程不引用任何源内存）。
        if let Some(stream) = self.stream.take() {
            unregister(&stream.arrived);
            stream.stop.store(true, Ordering::Relaxed);
            stream.shared.data_cv.notify_all();
            let _ = stream.done.recv_timeout(STREAM_STOP_JOIN_TIMEOUT);
            drop(stream.handle);
        }
        if let Some(state) = self.full_preload.take() {
            unregister(&state.downloaded);
            state.stop();
        }
        // 音频源销毁（播放停止/换源）→ 关闭音频优先：复位协调器，视频源随即
        // 解除节流、恢复全速（双流同生命周期，通常一起销毁）。
        if self.diag_slot == 1 {
            AUDIO_FIRST.enabled.store(false, Ordering::Relaxed);
            AUDIO_FIRST.audio_permille.store(0, Ordering::Relaxed);
            AUDIO_FIRST
                .audio_total_known
                .store(false, Ordering::Relaxed);
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
        // 填窗期吞吐同理清零：宿主读到 0 = "本槽没有在跑的流"，而不是上一个
        // 源留下的旧链路值（那会让新源一开播就被判成"链路很好"）。
        store_fill_bps(self.diag_slot, 0.0);
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
    /// 由 [`BodyReader`] 提供：它把异步 body 泵成同步 `Read`，读取位置自持，
    /// 因此可以脱离 `Response` 独立持有。
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
        let response = send_http_request(request).map_err(|failure| {
            http_trace_log(format!(
                "{{\"event\":\"http_live_open_error\",\"error\":\"{}\"}}",
                json_escape(&failure.message()),
            ));
            SourceError::Http(failure.message())
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
        let reader = BodyReader::new(SourceCancellation::default(), response);
        Ok(Self {
            uri: uri.to_string(),
            reader: Some(Box::new(reader)),
            offset: 0,
            downloaded_bytes: 0,
        })
    }

    fn publish_diag(&self) {
        // 直播流走长连接顺序读，**不发 Range 请求**（不经 `fetch_http_range`），
        // 故这里如实上报槽 0 的进程级请求画像累计值：直播期间它不再增长，
        // 宿主差分为 0，正是"当前这条流没有 Range 请求"的正确表述。
        let (http_requests, http_request_bytes) = match http_request_profile(0) {
            Some(profile) => (
                profile.requests.load(Ordering::Relaxed),
                profile.bytes.load(Ordering::Relaxed),
            ),
            None => (0, 0),
        };
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
                    // 直播流**没有"整片缓冲完"这个状态**（无总长、持续追加）：
                    // 恒 false 是正确语义，宿主不会把直播的停滞读成"下完了"。
                    whole_file_cached: false,
                    // 直播源走 `HttpLiveStreamSource`（无 body 总时长 deadline、
                    // 读阻塞即等待），不经过点播的「暂时读不到」等待窗口。
                    transient_wait: false,
                    http_requests,
                    http_request_bytes,
                    // 直播流没有滑动窗口分层（不可 seek、无缓存区间可言）：
                    // 内存层报当前已读字节在前面的缓冲，冷区恒 0。
                    cache_memory_bytes: 0,
                    cache_disk_bytes: 0,
                    cold_hits: 0,
                    cold_hit_bytes: 0,
                    memory_budget_bytes: 0,
                    forward_budget_bytes: 0,
                    forward_playable_secs: 0.0,
                    fill_throughput_bps: 0.0,
                    stream_reconnects_total: 0,
                    stream_reconnects_early_close: 0,
                    stream_reconnects_empty: 0,
                    stream_reconnects_reset: 0,
                    stream_reconnects_host_switch: 0,
                    stream_chunk_resumes: 0,
                    stream_range_contiguous: 0,
                    stream_range_forward_jumps: 0,
                    stream_range_backward_jumps: 0,
                    stream_range_last_start: 0,
                    stream_last_connection_bytes: 0,
                    stream_max_connection_bytes: 0,
                    last_host_switch_reason: String::new(),
                    // 直播没有"seek 到未缓冲位置"这个概念（不可 seek、无窗口），
                    // 跨节点并行回填对它不适用：全 0 是正确语义，宿主不会把直播
                    // 读成"走过并行分段"。
                    seek_fill_batches: 0,
                    seek_fill_connections: 0,
                    seek_fill_bytes: 0,
                    seek_fill_gate_denied: 0,
                    seek_fill_fallbacks: 0,
                    seek_fill_overlap_ok: 0,
                    seek_fill_overlap_mismatch: 0,
                    seek_fill_last_segments: 0,
                    seek_fill_in_flight: 0,
                    seek_fill_last_bytes: 0,
                    seek_fill_last_ms: 0,
                    seek_fill_last_expected_ms: 0,
                    seek_fill_last_fallback: false,
                    seek_fill_last_gate_reason: String::new(),
                    seek_fill_disabled: false,
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

/// 短请求（长度探测 / 候选测速）专用：整次交换 60s 上限。
///
/// "连接超时 4s" 与 "忽略证书校验" 都在 [`http::data_client`] 里，理由记在那里。
///
/// **已知差异**：旧实现另有 `timeout_recv_response(15s)` 单独约束"发出到收到响应
/// 头"。reqwest 的 `RequestBuilder::timeout` 只有整次请求一个粒度，没有等价项，
/// 因此响应头挂死从 15s 失败变成 60s 失败。这里发起的都是 1 字节级探测请求，
/// 连接超时 4s 也已覆盖 TCP 建连，实际影响面有限。
fn http_agent() -> HttpFetch {
    HttpFetch {
        client: http::data_client(),
        timeout: Some(Duration::from_secs(60)),
    }
}

/// 直播流专用。**不能复用 [`http_agent`]**：直播流是无限长 body，任何"整次请求"
/// 的总时长上限都会让它播放约 N 秒后必断流。旧实现在 ureq 上踩过这个坑——
/// `timeout_recv_body` 是自响应头起算的总时长，直播因此约 15s 断流，表现为
/// "播着播着就自动重连"，与网络无关，根因就记在这里以免重犯。
///
/// 直播源只保留连接超时（慢源快速失败）；CDN 主动断开（FIN/RST）时读返回
/// EOF/错误 → 宿主断流检测触发自动重连（新 URL 换源）。
fn http_live_agent() -> HttpFetch {
    HttpFetch {
        client: http::data_client(),
        timeout: None,
    }
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

/// 一次 HTTP 交换失败的分类依据。
///
/// 存在的理由是一次**真实的行为差异**：reqwest 不把 4xx/5xx 当错误（要显式
/// `error_for_status()` 才转），而旧实现用的 ureq 默认就把它变成 `Err(StatusCode)`。
/// 若照搬 reqwest 的默认行为，5xx 会从"可重试的暂时性失败"退化成终局错误——
/// CDN 抖动一个 502 就会让宿主换源，而不是就地重试。所以分类口径必须显式搬过来，
/// 由调用点在 `status >= 400` 时构造 [`Self::Status`]、在 `send()` 失败时构造
/// [`Self::Transport`]。
#[derive(Debug)]
enum HttpExchangeFailure {
    /// 服务端明确回了失败状态码。
    Status(u16),
    /// 传输层失败（连接复位 / 超时 / DNS-TLS 抖动 / body 中断），没有状态码。
    Transport(String),
}

impl HttpExchangeFailure {
    /// 传输层错误与 5xx 是链路问题，源很可能马上又能用；4xx 是确定性的。
    fn is_retryable(&self) -> bool {
        match self {
            Self::Status(status) => *status >= 500,
            Self::Transport(_) => true,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Status(status) => format!("http status: {status}"),
            Self::Transport(message) => message.clone(),
        }
    }
}

/// 重试预算用尽后，把一次 HTTP 交换失败归类成「暂时性」还是「终局」。
///
/// 分类沿用 [`HttpExchangeFailure::is_retryable`]：传输层错误（连接被复位 / 半开
/// 连接超时 / DNS-TLS 抖动）与 5xx 属于链路问题，源很可能马上又能用 →
/// [`SourceError::Transient`]，由 [`HttpRangeSource::read_range_with_transient_wait`]
/// 吸收并等待；4xx（403 签名过期 / 404 分片不存在）是确定性的，重试无意义 →
/// [`SourceError::Http`]，立即上抛让宿主换源。
fn http_failure_after_retries(error: &HttpExchangeFailure) -> SourceError {
    let message = error.message();
    if error.is_retryable() {
        SourceError::Transient(message)
    } else {
        SourceError::Http(message)
    }
}

/// 「暂时性读取失败」的等待窗口（mpv `paused-for-cache` 等价）：字节完全停止
/// 到达并持续这么久，才放弃并让宿主换源。
///
/// 12s 的取值理由（与宿主策略层分级错开，见 bili-rs `src/playback/policy.rs`）：
/// 内核 `MULTI_HOST_STALL_MS`(6s) 会先零缓存丢失地自发换一轮 host；宿主双流
/// 对照判据 9s 再强制换一轮；宿主「位置冻结 20s + 供给≈0」才整流重开。12s
/// 落在 9s 与 20s 之间——给两级换 host 都留出机会，又在宿主重开之前收手，
/// 避免"内核已判死 + 宿主又重开"的双重重开。
const HTTP_TRANSIENT_GRACE: Duration = Duration::from_secs(12);
/// 等待窗口的**硬上限**：字节虽在来但总量长期不达标（例如 1 字节/秒的涓流、
/// 或永远只回一个字节段）时，不能把 demux 线程永久钉在等待里——超过即放弃，
/// 让宿主换源。30s 远大于正常 CDN 抖动，不影响"读慢但推进"的正常等待。
const HTTP_TRANSIENT_ABS_MAX: Duration = Duration::from_secs(30);
/// 等待分片：每次重试间隔。300ms 足以让 URLE 连接池里的坏连接被换掉，
/// 又不会把 demux 线程的 CPU 打满（每次重试都是一次真实 HTTP 往返）。
const HTTP_TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(300);

/// 一次暂时性失败之后是否继续等待（而不是把整条流判死）。
///
/// `stalled`：距上次「有字节到达」的时长（本函数只按字节判定，不看错误次数）；
/// `elapsed`：本次读的总时长。返回 false 的两个出口：
/// ① 字节停够 `grace`（真死）；② 总等待超过 `abs_max`（防涓流钉死）。
fn transient_wait_should_continue(
    stalled: Duration,
    elapsed: Duration,
    grace: Duration,
    abs_max: Duration,
) -> bool {
    elapsed < abs_max && stalled < grace
}

/// 等待窗口时长（字节完全不动时的容忍度）。默认 [`HTTP_TRANSIENT_GRACE`]，
/// `ERIKA_HTTP_TRANSIENT_GRACE_MS` 可覆盖（**设 0 = 关闭等待**，回到"一次读
/// 失败就报错"的旧行为，供排查对照）。上限钳到
/// [`HTTP_TRANSIENT_ABS_MAX`] 之内，保证硬上限出口始终可达。
fn transient_grace() -> Duration {
    static GRACE: OnceLock<Duration> = OnceLock::new();
    *GRACE.get_or_init(|| {
        // 只读一次：本函数在 demux 线程的每次失败重试里被调用，反复 getenv
        // 会取全局环境锁（其他线程 set_var 时更明显）。
        env::var("ERIKA_HTTP_TRANSIENT_GRACE_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(HTTP_TRANSIENT_GRACE)
            .min(HTTP_TRANSIENT_ABS_MAX)
    })
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
fn response_entity_validator(response: &reqwest::Response) -> Option<String> {
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

/// 弱 ETag（`W/` 前缀）只表达"语义等价"、不保证字节一致，不能授权断点续传
/// 的拼接，也不能放进 `If-Range`（RFC 9110 §13.1.5）。续传前用它做门禁：
/// 拿弱 validator 去续传 = 拿"可能变了"的字节拼进已收前缀，宁可判终局。
fn validate_resume_validator(validator: Option<&str>) -> Result<()> {
    if validator.is_some_and(|value| value.starts_with("W/")) {
        return Err(SourceError::Http(
            "resource changed or cannot be safely validated during Range retry".to_string(),
        ));
    }
    Ok(())
}

/// 取响应头原文（`None` = 头不存在 / 空 / 非 UTF-8）。
///
/// 只负责**如实回报**，不做任何解释——跨节点一致性怎么判由 `seek_fill_gate`
/// 集中决定（内核里只允许有一处判据，避免"两套说法互相打架"）。
fn response_header_value(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Learns the total length from a one-byte GET, for origins that reject HEAD.
///
/// The body is deliberately never read. An origin that rejects HEAD *and*
/// ignores Range answers 200 with the whole object, so buffering the response
/// would turn a `len()` call into a full download of the media -- gigabytes
/// into memory before playback, just to learn a number the headers already
/// carry.
fn probe_http_total_length(
    agent: &HttpFetch,
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
    // reqwest 默认**不**把状态码当错误（要显式 `error_for_status()` 才转），所以这里
    // 不再需要旧实现那句 `http_status_as_error(false)`：4xx 同样能拿到响应头，而探测
    // 必须看得到 416 的 `Content-Range`。失败也不走重试口径——探测是"问一次"，出错就
    // 如实报错。
    let response = send_http_request(request).map_err(|failure| {
        http_trace_log(format!(
            "{{\"event\":\"http_length_probe_error\",\"phase\":\"request\",\"error\":\"{}\"}}",
            json_escape(&failure.message()),
        ));
        SourceError::Http(failure.message())
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

/// **一个 Range 请求换一段字节**（旧分段路径；顺序长读用
/// [`open_sequential_response`]，它只建连接、把 body 留给调用方持续读）。
///
/// `progress`：实时进度计数器（可选）。**不为 None 时**，body 每读到一块就
/// `fetch_add` 一次，使 `downloaded_bytes` 在段传输期间持续增长——慢网下
/// 「正在缓慢下载」不再被宿主差分读成 0（换源/降档误判的根因）。
///
/// `profile`：**HTTP 请求画像**的两个进程级单调计数器（可选，见
/// [`SourceDiagSnapshot::http_requests`]）。计数点在本函数内部、**唯一真正
/// 发请求的那一处**（重试循环里的 `request.call()` 之前）——所以重试与断点
/// 续传发出的每一个请求都会各记 1；放在调用点计数则会把重试合并掉。
/// 三个调用点（顺序预取线程 / 同步 fetch / 整片预载桶）各自把 `diag_slot`
/// 或桶的 `slot` 传进来：预取线程是独立 `std::thread`，捕获不到源实例。
fn fetch_http_range(
    agent: &HttpFetch,
    uri: &str,
    http_headers: &[(String, String)],
    range: ByteRange,
    event: &str,
    progress: Option<&AtomicU64>,
    profile: Option<HttpRequestProfile>,
) -> Result<HttpRangeResponse> {
    let mut bytes = Vec::new();
    let mut total_length = None;
    // Entity validator from the first response. A resumed request replays it as
    // `If-Range` so an origin that re-encoded or load-balanced to a different
    // variant answers 200 (which the status check below rejects for a non-zero
    // start) instead of handing back bytes from a different object to be spliced
    // onto the prefix we already hold.
    let mut validator: Option<String> = None;
    // `ETag` / `Last-Modified` 原样回报：**seek 跨节点并行的下载前门禁**要用它们
    // 判断"几个节点给的是不是同一个对象"（见 `seek_fill_gate`）。与 `validator`
    // 分开记录是因为判据需要知道**到底是哪一种**（ETag 缺失不能拿 Last-Modified
    // 默认顶替——那只是弱证据）。
    let mut etag: Option<String> = None;
    let mut last_modified: Option<String> = None;
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
                etag,
                last_modified,
            });
        }
        if received > 0 {
            validate_resume_validator(validator.as_deref())?;
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
        // —— HTTP 请求画像：粒度 = **一次真正发出的 GET**（含重试与断点续传）——
        // 计数点必须在 `call()` 这一处（唯一打到网络的地方），而不是调用点：
        // 调用点计数会把「一次逻辑 fetch 的 3 次尝试」合并成 1 次，重试放大就
        // 看不出来了；这里每个 attempt 都途经，天然如实。失败（连接错误 / 5xx /
        // body 中断）同样计入——那确实消耗了一次请求，而且均值正是靠它反映
        // "重试浪费"（失败请求只进分母、不进分子）。
        if let Some(profile) = profile.as_ref() {
            profile.record_request();
        }
        let response = match send_http_request(request) {
            Ok(response) => response,
            Err(failure) => {
                http_trace_log(format!(
                    "{{\"event\":\"{}_error\",\"phase\":\"request\",\"attempt\":{},\"start\":{},\"length\":{},\"elapsed_ms\":{:.3},\"error\":\"{}\"}}",
                    event,
                    attempt,
                    resume_range.start,
                    resume_range
                        .length
                        .map_or_else(|| "null".to_string(), |length| length.to_string()),
                    started.elapsed().as_secs_f64() * 1000.0,
                    json_escape(&failure.message()),
                ));
                let backoff = http_retry_backoff(attempt);
                if attempt < HTTP_FETCH_MAX_ATTEMPTS
                    && failure.is_retryable()
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
                // 暂时性 or 终局由错误性质决定（见 `http_failure_after_retries`）；
                // 暂时性错误后续由外层等待窗口吸收，不再直接判死整条流。
                return Err(http_failure_after_retries(&failure));
            }
        };
        let status = response.status().as_u16();
        // 200（服务端忽略 Range）时，body 从字节 0 开始：读循环要丢弃
        // `resume_range.start` 之前的前缀，让保留下来的字节与请求区间对齐。
        let mut skip_bytes = 0u64;
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
                // 跨节点门禁的凭据（原样回报，不在内核里做任何推断）。
                if etag.is_none() {
                    etag = response_header_value(&response, "etag");
                }
                if last_modified.is_none() {
                    last_modified = response_header_value(&response, "last-modified");
                }
            }
            200 => {
                // 服务端忽略 Range（或被 If-Range 判定实体已变），从字节 0
                // 发来整片。首包（received == 0）没有已收前缀：读循环按
                // `skip_bytes` 逐块丢弃请求起点之前的前缀、只保留请求区间，
                // 这就是"服务端不支持 Range"的兜底能力。
                // 续传（received > 0）要把整片尾部拼到已收前缀上，**必须**
                // 证明仍是同一实体：弱 ETag 不能授权拼接（RFC 9110 §13.1.5），
                // 首包带过强 validator 而整片重放对不上/没带，都是换过实体，
                // 一律判终局，绝不拼接。首包本来就没有 validator 时无从校验，
                // 与 206 路径同一信任水平（只信起点契约）。
                if received > 0 {
                    validate_resume_validator(validator.as_deref())?;
                    if validator.is_some()
                        && response_entity_validator(&response).as_deref() != validator.as_deref()
                    {
                        http_trace_log(format!(
                            "{{\"event\":\"{}_error\",\"phase\":\"entity\",\"attempt\":{},\"start\":{},\"status\":200}}",
                            event, attempt, resume_range.start,
                        ));
                        return Err(SourceError::Http(
                            "resource changed or cannot be safely validated during Range retry"
                                .to_string(),
                        ));
                    }
                }
                skip_bytes = resume_range.start;
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
                // 跨节点门禁的凭据（200 分支：整片响应，`total` 来自 Content-Length）。
                if etag.is_none() {
                    etag = response_header_value(&response, "etag");
                }
                if last_modified.is_none() {
                    last_modified = response_header_value(&response, "last-modified");
                }
            }
            _ => {
                // 4xx/5xx 必须走与旧实现一致的重试口径。旧实现用 ureq，它默认把
                // 4xx/5xx 变成 `Err(StatusCode)`，于是 5xx 会被判为可重试的暂时性
                // 失败。reqwest 把状态码原样交回，若不在这里显式补上，CDN 抖一个
                // 502 就会直接判终局并让宿主换源，而不是就地重试。
                let failure = if status >= 400 {
                    Some(HttpExchangeFailure::Status(status))
                } else {
                    // 204/304 等"成功但没有可用区间载荷"：重试无意义，终局。
                    None
                };
                http_trace_log(format!(
                    "{{\"event\":\"{}_error\",\"phase\":\"status\",\"attempt\":{},\"start\":{},\"status\":{}}}",
                    event, attempt, resume_range.start, status,
                ));
                let Some(failure) = failure else {
                    return Err(SourceError::Http(format!(
                        "unexpected HTTP status {status} for Range request"
                    )));
                };
                let backoff = http_retry_backoff(attempt);
                if attempt < HTTP_FETCH_MAX_ATTEMPTS
                    && failure.is_retryable()
                    && http_retry_fits_budget(deadline_started, backoff)
                {
                    http_trace_log(format!(
                        "{{\"event\":\"{}_retry\",\"phase\":\"status\",\"attempt\":{},\"start\":{},\"status\":{},\"backoff_ms\":{}}}",
                        event,
                        attempt,
                        resume_range.start,
                        status,
                        backoff.as_millis(),
                    ));
                    thread::sleep(backoff);
                    continue;
                }
                return Err(http_failure_after_retries(&failure));
            }
        }
        // 分块读替代 `read_to_end`：每块到达即累加实时进度（见 `progress` 参数
        // 注释）。读失败时保留已到达的字节——下面的重试按 `bytes.len()` 续传，
        // 已计入的字节不会被重复计数。
        let mut reader = BodyReader::new(SourceCancellation::default(), response);
        let mut chunk = [0u8; HTTP_RECV_CHUNK_BYTES];
        let mut read_error: Option<std::io::Error> = None;
        // 200 时 `skip_bytes` 是仍要丢弃的前缀；`response_bytes` 记这份 body
        // **一共**收到多少（含被丢弃的前缀），`reached_eof` 区分"自然读完"
        // 与"收满请求区间主动断开"——后者不能拿来推总长。
        let mut response_bytes = 0u64;
        let mut reached_eof = false;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => {
                    reached_eof = true;
                    break;
                }
                Ok(n) => {
                    response_bytes = response_bytes.saturating_add(n as u64);
                    if let Some(counter) = progress {
                        counter.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    // 请求画像的"分子"：与 `progress` **同一处**累加，口径一致
                    // ——「所有已发出的 GET 合起来收回了多少字节」。断点续传不会
                    // 重复计数：续传请求只请求剩余区间，重复的那部分字节从未二次
                    // 到达（已到达的在上一 attempt 里记过）。
                    if let Some(profile) = profile.as_ref() {
                        profile.record_bytes(n as u64);
                    }
                    let skip = skip_bytes.min(n as u64) as usize;
                    skip_bytes -= skip as u64;
                    let payload = &chunk[skip..n];
                    if payload.is_empty() {
                        continue;
                    }
                    // 只保留请求区间还差的部分：整片响应（200）可能比请求区间
                    // 大得多，多收的字节既不该进内存也不该进缓存。
                    let take = match range.length {
                        Some(length) => {
                            let remaining = length.saturating_sub(bytes.len() as u64) as usize;
                            remaining.min(payload.len())
                        }
                        None => payload.len(),
                    };
                    bytes.extend_from_slice(&payload[..take]);
                    // 收满请求区间就断开，绝不为"服务端忽略 Range"读完整片。
                    if range
                        .length
                        .is_some_and(|length| bytes.len() as u64 >= length)
                    {
                        break;
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
            // body 分块读错误（连接中断 / 读超时 / 对端提前收线）在重试预算用尽后
            // 仍是**暂时性**失败：字节只是暂时拿不到，链路恢复后同一个 Range 请求
            // 还能续上（已到达的字节由上面的 `received` 续传，不会重复下载）。
            return Err(SourceError::Transient(error.to_string()));
        }
        if status == 200 && reached_eof && total_length.is_none() {
            // Count the discarded prefix too. This also discovers EOF when
            // the requested offset is beyond a lengthless response's body.
            total_length = Some(response_bytes);
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
            etag,
            last_modified,
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

    /// 交出本源的取消域。宿主拆除时置位即可中断在途的 body 读，让 demux 线程
    /// 不必等到段超时再退出（见 `SourceCancellation`）。
    fn cancellation(&self) -> Option<SourceCancellation> {
        Some(self.cancellation.clone())
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
            match send_http_request(request) {
                Ok(response) => {
                    let status = response.status().as_u16();
                    // reqwest 不把 4xx/5xx 当错误（要 `error_for_status()` 才转），而旧
                    // 实现用的 ureq 默认就把它变成 `Err(StatusCode)`。这条口径必须补上：
                    // 一次 500 若被当成"成功但长度为 0"，会掉进下面的 zero-length 回退、
                    // 白白多发一次 GET，而不是就地重试（5xx 可重试、4xx 终局）。
                    if status >= 400 {
                        let failure = HttpExchangeFailure::Status(status);
                        http_trace_log(format!(
                            "[erika-http-trace] stage=head_error attempt={} elapsed_ms={:.3} error={}",
                            attempt,
                            started.elapsed().as_secs_f64() * 1000.0,
                            json_escape(&failure.message()),
                        ));
                        let backoff = http_retry_backoff(attempt);
                        if attempt < HTTP_FETCH_MAX_ATTEMPTS
                            && failure.is_retryable()
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
                        break failure;
                    }
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
                        return match probe_http_total_length(&self.agent, &uri, &self.http_headers)
                        {
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
                Err(failure) => {
                    http_trace_log(format!(
                        "[erika-http-trace] stage=head_error attempt={} elapsed_ms={:.3} error={}",
                        attempt,
                        started.elapsed().as_secs_f64() * 1000.0,
                        json_escape(&failure.message()),
                    ));
                    let backoff = http_retry_backoff(attempt);
                    if attempt < HTTP_FETCH_MAX_ATTEMPTS
                        && failure.is_retryable()
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
                    break failure;
                }
            }
        };
        // Some servers reject HEAD (e.g. 405) yet still serve ranges. Probe
        // with a one-byte GET and take the total from Content-Range.
        http_trace_log(format!(
            "[erika-http-trace] stage=head_fallback_range error={}",
            json_escape(&head_error.message()),
        ));
        let uri = self.uri.lock().expect("uri poisoned").clone();
        match probe_http_total_length(&self.agent, &uri, &self.http_headers) {
            Ok(total_length) => {
                self.content_length = total_length;
                self.publish_diag();
                Ok(self.content_length)
            }
            Err(_) => Err(SourceError::Http(head_error.message())),
        }
    }

    fn prefetch_hint(&mut self, target: u64) {
        HttpRangeSource::prefetch_hint(self, target)
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        self.read_range_with_transient_wait(range)
    }
}

impl HttpRangeSource {
    /// 一次读尝试（**不含**「暂时读不到就等一会儿」的外层循环，见
    /// [`HttpRangeSource::read_range_with_transient_wait`]）。命中缓存/预取/桶
    /// 的路径在这里直接返回；需要网络的路径可能返回
    /// [`SourceError::Transient`]（可重试）或 [`SourceError::Http`]（终局）。
    ///
    /// **两条数据通路**（2026-09-03）：
    ///   1. `sequential_enabled`（生产默认）→ [`HttpRangeSource::read_range_stream`]：
    ///      一条持续读的长连接覆盖整段顺序播放，只有窗口外的随机读才重发请求；
    ///   2. 其余（`ERIKA_HTTP_SEQUENTIAL=0` / 直接 `HttpRangeSource::new`）→
    ///      下面的旧分段路径（每段一个 Range + join），作为对照与兜底。
    /// 本函数以下的所有代码只服务第 2 条路径。
    fn read_range_once(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        self.last_read_call_ms = now_ms();
        self.maybe_switch_host();
        // 与整片预载桶/冷区的协同信号（都是无锁原子写，每次读刷新，成本可忽略）：
        // ① mark_demux_read：告诉桶"demux 正在消费"，播放中桶让路，把带宽完整
        //    留给 demux 的多段并行预取（聚合被 CDN 限速的单连接带宽）；
        // ② set_floor：把 demux 已覆盖前沿交给桶，桶跳过该偏移之前的区间，
        //    绝不重复下载 demux 已下过的字节（重复下载会让有效供给砍半）。
        if let Some(state) = self.full_preload.as_ref() {
            state.mark_demux_read();
            // 桶的 `floor` = demux 已覆盖前沿。顺序长读下就是 `StreamBuffer` 的
            // `write_pos`（这条长流已经把那些字节拿到手上了），旧路径下是
            // `cache_end`。桶据此**封存旧段、另开新段**（2026-09-04 起不再丢弃
            // 已有字节），见 `full_preload_step` 的 floor 分支。
            let covered = match self.stream_window() {
                Some((_, write_pos, _)) => write_pos,
                None => self.cache_end(),
            };
            state.set_floor(covered);
        }
        // —— 统一缓存视图（磁盘冷区 → 内存热区 → 网络），命中即零网络 ——
        //
        // 顺序：① 冷区（磁盘层：热区淘汰下来、已落盘的区间）→ ② 顺序长流的热区
        // 窗口 → ③ 网络。冷区放在最前面是刻意的：它是**唯一能覆盖 demux 当前
        // 位置之前**的持有者（热区只保留 back_bytes 的后向窗口），用户回拖 /
        // 重播命中的正是它——命中就绝不发网络请求（缺陷 ⑤ 的核心）。
        // 「读到文件尾」（`length == None`，弹幕/字幕这类 sidecar）不走这里：
        // 它的语义是"从 cursor 起一路累积到 EOF"，由 `read_range_stream` 的
        // 循环逐段拼接（那里同样先查冷区），避免一次空返回被上层当作 EOF。
        if let Some(length) = range.length
            && length > 0
            && let Some(bytes) = self.read_cold(range.start, length)
        {
            self.note_consumed(range.start.saturating_add(bytes.len() as u64));
            self.maybe_start_prefetch(range);
            self.publish_diag();
            http_trace_log(format!(
                "{{\"event\":\"cold_cache_hit\",\"start\":{},\"length\":{},\"bytes\":{}}}",
                range.start,
                length,
                bytes.len(),
            ));
            trace_read_return("http_read_cold", range, self, &bytes);
            return Ok(bytes);
        }
        // —— 顺序长读：正常前进播放全部由一条持续读的连接覆盖 ——
        //
        // 放在冷区之后、旧分段路径之前：
        //   ① 冷区命中是**零网络**（磁盘分层），优先；
        //   ② 没命中的读全部交给长流——不再走 `maybe_start_prefetch` /
        //      `spawn_prefetch_window` 那条"每段一个请求"的路径。
        // 只有窗口外的随机读（seek / demuxer 回退 / 音视频不同步导致的跳读）
        // 会在长流内部触发一次重设（= 重发一条 open-ended Range）。
        if self.sequential_enabled {
            return self.read_range_stream(range);
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

    /// 「暂时读不到」的等待窗口（对齐 mpv 的 `paused-for-cache` / `cache-pause-wait`）。
    ///
    /// 背景（用户实测"每播约 5 秒自动暂停一次、连出 4~5 次"的根因）：`read_range`
    /// 失败一次就返回错误 → AVIO 层把它变成 `EIO` → `av_read_frame` 报错 →
    /// demux worker 把整条流判死（`DemuxMessage::Error`）→ 宿主只能重开换源。
    /// 而 CDN 抖动的真实形态是「几秒拿不到字节，随后同一个连接或换一个 host 又能
    /// 用」；mpv 在这种情形下退回缓冲等待（`paused-for-cache`），**不把流判失败**。
    ///
    /// 本函数吸收 [`SourceError::Transient`]，三个出口：
    /// - **有字节到达**（`live_rx_bytes` 增长：涓流、换 host 后恢复、或整片预载桶
    ///   在别的连接上继续下载）→ 只要字节还在来就继续等，**不判死**；
    /// - **字节完全停了**并持续 `transient_grace()` → 放弃（返回错误，宿主自愈换源）；
    /// - **总时长超过 [`HTTP_TRANSIENT_ABS_MAX`]** → 放弃。兜底防 1 字节/秒级涓流
    ///   把 demux 线程永久钉住（用户看到的是长时间无进度，换源反而能救）。
    ///
    /// [`SourceError::Http`]（4xx / Range 被忽略 / Content-Range 违约 / 帧内错位）
    /// 以及 `Io`/`Unsupported` 等**不等待**：那些重试多少次都不会好，必须立刻让
    /// 宿主换源——"等待"只吸收真的可能自愈的失败。
    ///
    /// 等待期间每 [`HTTP_TRANSIENT_RETRY_DELAY`] 重试一次，并且每次都刷新
    /// `last_read_call_ms`（保持"demux 仍在消费"的活跃标记）、调 `maybe_switch_host`
    /// （消费宿主的强制换 host 请求、让 6s 死连与慢速自发换 host 生效）、调
    /// `publish_diag`（刷新 `est_bps` 与 `transient_wait`：宿主策略据此降档/换
    /// host，内核 worker 据此对外发缓冲态）。
    fn read_range_with_transient_wait(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        // 标记"demux 此刻正卡在读上"（换源判据区分"在播且饿着"与"暂停"靠它，
        // 见 `read_in_flight` 注释）——读调用挂 20 秒也不会被误判成暂停。
        self.read_in_flight = true;
        let result = self.read_range_wait_loop(range);
        self.read_in_flight = false;
        result
    }

    /// 等待窗口主体（见 [`HttpRangeSource::read_range_with_transient_wait`] 的说明）。
    fn read_range_wait_loop(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let started = Instant::now();
        let rx_at_entry = live_rx_bytes(self.diag_slot);
        // 「距上次字节到达」的计时**只在本函数内累计**，不复用历史 `last_byte_ms`：
        // 暂停很久后 seek 回来恰好赶上一次失败时，历史值会让 stalled 直接爆表，
        // 把一次普通的瞬时失败误判成死连。
        let mut last_progress_at = started;
        let mut rx_seen = rx_at_entry;
        let mut attempts = 0u32;
        loop {
            attempts = attempts.saturating_add(1);
            match self.read_range_once(range) {
                Ok(bytes) => {
                    if attempts > 1 {
                        trace::diagnostic(
                            serde_json::json!({
                                "event": "http_transient_wait",
                                "stage": "recovered",
                                "attempts": attempts,
                                "waitedSeconds": started.elapsed().as_secs_f64(),
                                "start": range.start,
                                "bytes": bytes.len(),
                            })
                            .to_string(),
                        );
                    }
                    self.set_transient_wait(false);
                    return Ok(bytes);
                }
                Err(SourceError::Transient(message)) => {
                    let rx_now = live_rx_bytes(self.diag_slot);
                    if rx_now != rx_seen {
                        rx_seen = rx_now;
                        last_progress_at = Instant::now();
                    }
                    let stalled = last_progress_at.elapsed();
                    if !transient_wait_should_continue(
                        stalled,
                        started.elapsed(),
                        transient_grace(),
                        HTTP_TRANSIENT_ABS_MAX,
                    ) {
                        // 真死：字节停了足够久（或总等待超上限）→ 交回上层，由
                        // AVIO → av_read_frame → demux worker → 宿主换源自愈。
                        self.set_transient_wait(false);
                        trace::diagnostic(
                            serde_json::json!({
                                "event": "http_transient_wait",
                                "stage": "give_up",
                                "attempts": attempts,
                                "stalledSeconds": stalled.as_secs_f64(),
                                "waitedSeconds": started.elapsed().as_secs_f64(),
                                "start": range.start,
                                "error": message.as_str(),
                            })
                            .to_string(),
                        );
                        return Err(SourceError::Transient(message));
                    }
                    self.set_transient_wait(true);
                    self.last_read_call_ms = now_ms();
                    self.maybe_switch_host();
                    self.publish_diag();
                    http_trace_log(format!(
                        "{{\"event\":\"http_transient_wait_retry\",\"attempt\":{},\"stalled_ms\":{:.3},\"waited_ms\":{:.3},\"est_bps\":{:.0},\"host_epoch\":{},\"error\":\"{}\"}}",
                        attempts,
                        stalled.as_secs_f64() * 1000.0,
                        started.elapsed().as_secs_f64() * 1000.0,
                        self.est_bps,
                        self.host_epoch,
                        json_escape(&message),
                    ));
                    thread::sleep(HTTP_TRANSIENT_RETRY_DELAY);
                }
                Err(other) => {
                    self.set_transient_wait(false);
                    return Err(other);
                }
            }
        }
    }

    /// 发布/清除「正在等字节」标志。只在跳变时刷诊断（低频）；
    /// 内核 worker 与宿主读的都是诊断槽里的 `transient_wait`。
    fn set_transient_wait(&mut self, waiting: bool) {
        if self.transient_wait == waiting {
            return;
        }
        self.transient_wait = waiting;
        self.publish_diag();
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
                // 生产 VOD 路径：**顺序长读**（一条连接覆盖整段播放）+ 整片预载
                // 桶（暂停后在同一条流式连接上补片）。两者的分工见
                // `full_preload_stream_loop` 的注释：demux 活跃时桶不连，
                // 任一时刻只有一个下载者。
                Ok(Box::new(s.enable_sequential_stream().enable_full_preload()))
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
        return Ok(Box::new(s.enable_sequential_stream().enable_full_preload()));
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

/// 是否启用**顺序长读**（默认开）。`ERIKA_HTTP_SEQUENTIAL=0/false` 关回旧分段
/// 路径——保留这条开关是刻意的：新路径一旦在现场发现问题，一行环境变量即可
/// 回退到"每段一个请求"的旧行为，不必回滚二进制。
///
/// 只在源构造时读一次（`enable_sequential_stream`），不在读热路径上。
fn http_sequential_enabled() -> bool {
    std::env::var("ERIKA_HTTP_SEQUENTIAL")
        .map(|value| value != "0" && value != "false")
        .unwrap_or(true)
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
static HTTP_TRACE_ENABLED: LazyLock<bool> = LazyLock::new(|| trace::env_flag("ERIKA_HTTP_TRACE"));

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
    fn transient_wait_keeps_waiting_while_bytes_are_still_arriving() {
        // 「读慢但在推进」必须继续等：stalled 小（每次重试前都有新字节），
        // 即使总等待已经接近/超过 grace 也不判死——这正是 mpv paused-for-cache
        // 的语义，也是"带宽刚好够 + CDN 抖动"场景下不该判死的判据。
        let grace = Duration::from_secs(12);
        let abs_max = Duration::from_secs(30);
        assert!(transient_wait_should_continue(
            Duration::from_millis(0),
            Duration::from_secs(11),
            grace,
            abs_max,
        ));
        assert!(transient_wait_should_continue(
            Duration::from_millis(299),
            Duration::from_secs(25),
            grace,
            abs_max,
        ));
        // 但硬上限仍然生效：涓流不能把 demux 线程永久钉住。
        assert!(!transient_wait_should_continue(
            Duration::ZERO,
            abs_max,
            grace,
            abs_max,
        ));
    }

    #[test]
    fn transient_wait_gives_up_only_after_the_grace_without_bytes() {
        let grace = Duration::from_secs(12);
        let abs_max = Duration::from_secs(30);
        // 尚未到 grace → 继续等（一次失败不判死整条流）。
        assert!(transient_wait_should_continue(
            Duration::from_millis(11_900),
            Duration::from_millis(11_900),
            grace,
            abs_max,
        ));
        // 字节停够 grace → 判死，交回上层让宿主换源。
        assert!(!transient_wait_should_continue(
            grace,
            Duration::from_secs(12),
            grace,
            abs_max,
        ));
        assert!(!transient_wait_should_continue(
            Duration::from_secs(20),
            Duration::from_secs(20),
            grace,
            abs_max,
        ));
    }

    #[test]
    fn http_fetch_failures_are_classified_by_retryability() {
        // 4xx（403 签名过期 / 404）重试无意义 → 终局错误，立即让宿主换源。
        assert!(matches!(
            http_failure_after_retries(&HttpExchangeFailure::Status(403)),
            SourceError::Http(_)
        ));
        assert!(matches!(
            http_failure_after_retries(&HttpExchangeFailure::Status(404)),
            SourceError::Http(_)
        ));
        // 5xx 是链路问题 → 暂时性，进等待窗口重试（而不是判死整条流）。
        assert!(matches!(
            http_failure_after_retries(&HttpExchangeFailure::Status(503)),
            SourceError::Transient(_)
        ));
    }

    #[test]
    fn transient_wait_flag_tracks_the_read_path_state() {
        // 新源不在等待窗口里，且置位/清除是幂等的（`set_transient_wait` 只在
        // 跳变时刷诊断槽——诊断槽是进程级共享的，这里只断言源自身状态，
        // 不读全局槽，避免与并行用例互相干扰）。
        let mut source = HttpRangeSource::with_http_headers_and_diag_slot(
            "http://127.0.0.1:1/never-connected",
            Vec::new(),
            1,
        );
        assert!(!source.transient_wait);
        source.set_transient_wait(true);
        assert!(source.transient_wait);
        source.set_transient_wait(true);
        assert!(source.transient_wait);
        source.set_transient_wait(false);
        assert!(!source.transient_wait);
    }

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
        assert!(!full_preload_step(
            &state,
            &http_agent(),
            &uri,
            &[],
            1024 * 1024
        ));
        assert_eq!(state.covered_until(), 512 * 1024);
        assert!(
            requests.recv_timeout(Duration::from_millis(100)).is_err(),
            "前移步不应发起请求"
        );

        // 第二步：从 demux 前沿（512KB）起下载，绝不回头重下 [0, 512KB)
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
        let previous = std::env::var("ERIKA_HTTP_READAHEAD_BYTES").ok();
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
        // 用完**必须还原**：进程 env 是所有并行用例共享的全局状态。此前这里只
        // set 不还原，8388608 会一直留在进程里，之后任何 `HttpRangeSource::new`
        // （槽 0）都会拿到 8MB 而不是 4MB —— 这正是"并行用例互相干扰"的形态。
        match previous {
            Some(value) => unsafe { std::env::set_var("ERIKA_HTTP_READAHEAD_BYTES", value) },
            None => unsafe { std::env::remove_var("ERIKA_HTTP_READAHEAD_BYTES") },
        }
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
                etag: None,
                last_modified: None,
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
        source
            .prefetch
            .push(pending_from_bytes(seg2, patterned(seg2), 0));
        source
            .prefetch
            .push(pending_from_bytes(seg1, patterned(seg1), 200));

        // 读 seg1 起点：join seg1（等它完成），必须返回 seg1 的字节
        let out = source
            .read_range(ByteRange {
                start: 256 * 1024,
                length: Some(64 * 1024),
            })
            .unwrap();
        assert_eq!(
            out,
            patterned(ByteRange {
                start: 256 * 1024,
                length: Some(64 * 1024)
            })
        );
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
        assert_eq!(
            out,
            patterned(ByteRange {
                start: 512 * 1024,
                length: Some(64 * 1024)
            })
        );
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
    fn http_range_accepts_status_200_for_nonzero_offset() {
        let body: Vec<u8> = (0..100).collect();
        let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_simple_response("200 OK", &body),
        )]);
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(100);
        let bytes = source
            .read_range(ByteRange {
                start: 10,
                length: Some(10),
            })
            .unwrap();
        assert_eq!(bytes, body[10..20]);
        assert_eq!(source.cache_bytes, body[10..]);
        assert!(recv_request_head(&requests).contains("range: bytes=10-99"));
    }

    fn http_200_without_length(body: &[u8], chunked: bool) -> Vec<u8> {
        let mut raw = b"HTTP/1.1 200 OK\r\nConnection: close\r\n".to_vec();
        if chunked {
            raw.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
            for chunk in body.chunks(7) {
                raw.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
                raw.extend_from_slice(chunk);
                raw.extend_from_slice(b"\r\n");
            }
            raw.extend_from_slice(b"0\r\n\r\n");
        } else {
            raw.extend_from_slice(b"\r\n");
            raw.extend_from_slice(body);
        }
        raw
    }

    #[test]
    fn http_range_200_without_length_skips_prefix_and_learns_eof() {
        let body: Vec<u8> = (0..100).collect();
        for chunked in [false, true] {
            for start in [0, 10, 100, 120] {
                let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
                    http_200_without_length(&body, chunked),
                )]);
                let response = fetch_http_range(
                    &http_agent(),
                    &uri,
                    &[],
                    ByteRange {
                        start,
                        length: Some(256),
                    },
                    "test_range",
                    None,
                    None,
                )
                .unwrap();
                assert_eq!(response.bytes, body[(start as usize).min(body.len())..]);
                assert_eq!(response.total_length, Some(body.len() as u64));
                assert!(recv_request_head(&requests).contains(&format!("range: bytes={start}-")));
            }
        }
    }

    #[test]
    fn http_range_200_buffers_only_the_requested_bytes() {
        let body: Vec<u8> = (0..192 * 1024).map(|index| (index % 251) as u8).collect();
        for raw in [
            http_simple_response("200 OK", &body),
            http_200_without_length(&body, false),
        ] {
            let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(raw)]);
            let response = fetch_http_range(
                &http_agent(),
                &uri,
                &[],
                ByteRange {
                    start: 100 * 1024 + 3,
                    length: Some(17),
                },
                "test_range",
                None,
                None,
            )
            .unwrap();
            assert_eq!(response.bytes, body[100 * 1024 + 3..100 * 1024 + 20]);
        }
    }

    #[test]
    fn http_source_200_without_length_handles_reads_at_and_past_eof() {
        for start in [0, 10, 100, 120] {
            let body: Vec<u8> = (0..100).collect();
            let (uri, _requests) = spawn_mock_http_server(vec![MockResponse::immediate(
                http_200_without_length(&body, true),
            )]);
            let mut source = HttpRangeSource::new(uri);
            assert_eq!(
                source.read_range(ByteRange::suffix_from(start)).unwrap(),
                body[(start as usize).min(body.len())..],
            );
            assert_eq!(source.len().unwrap(), Some(100));
            assert!(
                source
                    .read_range(ByteRange {
                        start: 100,
                        length: Some(64)
                    })
                    .unwrap()
                    .is_empty()
            );
        }
    }

    fn http_200_with_validator(body: &[u8], validator: &str, declared_length: usize) -> Vec<u8> {
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {declared_length}\r\nETag: {validator}\r\nConnection: close\r\n\r\n"
        ).into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    #[test]
    fn http_range_weak_etag_allows_reads_but_rejects_resuming_partial_bytes() {
        for start in [0, 10] {
            let (uri, requests) = spawn_mock_http_server(vec![MockResponse::immediate(
                http_200_with_validator(&[b'a'; 100], "W/\"v1\"", 100),
            )]);
            let response = fetch_http_range(
                &http_agent(),
                &uri,
                &[],
                ByteRange {
                    start,
                    length: Some(40),
                },
                "test_range",
                None,
                None,
            )
            .unwrap();
            assert_eq!(response.bytes, vec![b'a'; 40]);
            assert!(!recv_request_head(&requests).contains("if-range:"));

            // 续传时弱 ETag 不能授权拼接：body 只到 20 字节（声明 100）触发
            // 续传，重发请求**发出之前**就该判终局，不会把可能已变的字节
            // 拼进已收前缀。
            let (uri, requests) = spawn_mock_http_server(vec![
                MockResponse::immediate(http_200_with_validator(&[b'a'; 20], "W/\"v1\"", 100)),
                MockResponse::immediate(http_200_with_validator(&[b'b'; 100], "W/\"v1\"", 100)),
            ]);
            let result = fetch_http_range(
                &http_agent(),
                &uri,
                &[],
                ByteRange {
                    start,
                    length: Some(40),
                },
                "test_range",
                None,
                None,
            );
            assert!(
                matches!(result, Err(SourceError::Http(message)) if message.contains("resource changed")),
                "weak validator must not authorize a resume"
            );
            let _ = recv_request_head(&requests);
            assert!(
                requests.try_recv().is_err(),
                "weak ETags must not be sent in If-Range"
            );
        }
    }

    #[test]
    fn http_range_200_retry_skips_prefix_and_preserves_received_bytes() {
        let body: Vec<u8> = (0..100).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_200_with_validator(&body[..20], "\"v1\"", 100)),
            MockResponse::immediate(http_200_with_validator(&body, "\"v1\"", 100)),
        ]);
        let response = fetch_http_range(
            &http_agent(),
            &uri,
            &[],
            ByteRange {
                start: 10,
                length: Some(40),
            },
            "test_range",
            None,
            None,
        )
        .unwrap();
        assert_eq!(response.bytes, body[10..50]);
        assert_eq!(response.total_length, Some(100));
        assert!(recv_request_head(&requests).contains("range: bytes=10-49"));
        let resumed = recv_request_head(&requests);
        assert!(resumed.contains("range: bytes=20-49"));
        assert!(resumed.contains("if-range: \"v1\""));
    }

    #[test]
    fn http_range_200_retry_rejects_a_changed_or_missing_validator() {
        let body: Vec<u8> = (0..100).collect();
        for raw in [
            http_200_with_validator(&body, "\"v2\"", 100),
            http_simple_response("200 OK", &body),
        ] {
            let (uri, _requests) = spawn_mock_http_server(vec![
                MockResponse::immediate(http_200_with_validator(&body[..20], "\"v1\"", 100)),
                MockResponse::immediate(raw),
            ]);
            let error = fetch_http_range(
                &http_agent(),
                &uri,
                &[],
                ByteRange {
                    start: 10,
                    length: Some(40),
                },
                "test_range",
                None,
                None,
            )
            .err()
            .expect("changed entities must not be spliced");
            assert!(error.to_string().contains("resource changed"));
        }
    }

    #[test]
    fn http_lengthless_200_supports_ffmpeg_custom_avio_reads_and_seek() {
        let body = include_bytes!("../testdata/playback/playback-fixture.mkv");
        let (uri, requests) = spawn_mock_http_server(
            (0..32)
                .map(|_| MockResponse::immediate(http_200_without_length(body, false)))
                .collect(),
        );
        let source =
            HttpRangeSource::with_http_headers_and_read_ahead(uri, Vec::new(), Some(64 * 1024));
        let mut demuxer = crate::ffmpeg::Demuxer::open_source(Box::new(source)).unwrap();
        assert_eq!(demuxer.probe().video.len(), 1);
        let mut packets = 0;
        while demuxer.read_packet().unwrap().is_some() {
            packets += 1;
        }
        assert!(packets >= 240);
        demuxer.seek(Duration::from_secs(2)).unwrap();
        assert!(demuxer.read_packet().unwrap().is_some());
        // 断言意图：首个整片 200（无 Content-Length）最多只能覆盖它那一段
        // 窗口，FFmpeg 读到段尾后**必须**继续发新的 Range 请求拿后续字节，
        // 而不是靠"把整片响应读完"蒙混过去（本地的请求粒度是窗口段，
        // 不是远端实现的任意偏移，所以按"起点递增的 GET ≥ 2"来断言）。
        let heads: Vec<String> = requests.try_iter().collect();
        let get_starts: Vec<u64> = heads
            .iter()
            .filter(|head| head.starts_with("GET"))
            .filter_map(|head| {
                head.to_lowercase()
                    .split("range: bytes=")
                    .nth(1)
                    .and_then(|value| value.split('-').next())
                    .and_then(|value| value.trim().parse::<u64>().ok())
            })
            .collect();
        assert!(
            get_starts.len() >= 2 && get_starts.windows(2).all(|pair| pair[0] < pair[1]),
            "FFmpeg must read past the first whole-file 200 through further Range requests; heads={heads:?}"
        );
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

    // —— HTTP 请求画像（把"是不是被 CDN 限速"变成可实测数字）——

    #[test]
    fn http_request_profile_counts_one_request_per_successful_fetch() {
        // 口径 1：一次成功的 Range 请求 → 请求数 +1、字节 +body.len()。
        // 计数点在 `fetch_http_range` 内部（唯一真正发请求的那一处），
        // 不是调用点。
        //
        // 计数目标用**用例自己的静态原子量**（而非进程级槽位数组）：
        // 同进程并行跑的其它用例也在发 HTTP，共享槽位会让断言不可靠。
        static REQUESTS: AtomicU64 = AtomicU64::new(0);
        static BYTES: AtomicU64 = AtomicU64::new(0);
        let body: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let (uri, _heads) = spawn_mock_http_server(vec![MockResponse::immediate(
            http_206_response(0, 4096, &body),
        )]);

        let response = fetch_http_range(
            &http_agent(),
            &uri,
            &[],
            ByteRange {
                start: 0,
                length: Some(4096),
            },
            "test_profile_ok",
            None,
            Some(HttpRequestProfile {
                requests: &REQUESTS,
                bytes: &BYTES,
            }),
        )
        .expect("mock 206 应成功");

        assert_eq!(response.bytes.len(), 4096);
        assert_eq!(REQUESTS.load(Ordering::Relaxed), 1, "成功一次 = 1 次请求");
        assert_eq!(BYTES.load(Ordering::Relaxed), 4096, "字节按实际到达量累计");
    }

    #[test]
    fn http_request_profile_counts_retries_and_resumed_bytes_once() {
        // 口径 2：一次失败（body 截断）后重试续传成功 → **请求数 = 2**。
        // 若把计数点错放在调用点 / 只按"逻辑 fetch"记一次，这里会得到 1，
        // "请求放大"（重试把每请求平均字节拖低）就被掩盖了——而这正是本
        // 计数存在的理由。
        // 字节侧必须只记实际到达量：续传请求只请求缺失区间，重复的 32 字节
        // 不会二次到达，所以总数恰为 64。
        static REQUESTS: AtomicU64 = AtomicU64::new(0);
        static BYTES: AtomicU64 = AtomicU64::new(0);
        let body: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
        let (uri, heads) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_truncated_response(0, 64, 64, &body[..32])),
            MockResponse::immediate(http_206_response(32, 64, &body[32..])),
        ]);

        let response = fetch_http_range(
            &http_agent(),
            &uri,
            &[],
            ByteRange {
                start: 0,
                length: Some(64),
            },
            "test_profile_retry",
            None,
            Some(HttpRequestProfile {
                requests: &REQUESTS,
                bytes: &BYTES,
            }),
        )
        .expect("截断后应续传成功");

        assert_eq!(response.bytes, body);
        assert_eq!(
            REQUESTS.load(Ordering::Relaxed),
            2,
            "失败一次后重试成功 = 2 次请求（每个 attempt 各记 1）"
        );
        assert_eq!(
            BYTES.load(Ordering::Relaxed),
            64,
            "断点续传的字节不重复计数"
        );
        // 两次请求确实是一次逻辑 fetch 的两段（第二次从断点 32 起）。
        assert!(recv_request_head(&heads).contains("range: bytes=0-63"));
        assert!(recv_request_head(&heads).contains("range: bytes=32-63"));
    }

    #[test]
    fn prefetch_spawns_parallel_segments() {
        let body: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let (uri, requests) = spawn_mock_http_server(vec![
            MockResponse::immediate(http_206_response(1024 * 1024, 128 * 1024 * 1024, &body)),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 256 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
            MockResponse::immediate(http_206_response(
                1024 * 1024 + 512 * 1024,
                128 * 1024 * 1024,
                &body,
            )),
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
            0, // diag_slot：测试源走槽 0（请求画像只影响诊断计数）
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
            0, // diag_slot：测试源走槽 0（请求画像只影响诊断计数）
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
            MockResponse::immediate(http_206_response(0, 1024 * 1024, &vec![0u8; 256 * 1024])),
            MockResponse::immediate(http_206_response(0, 1024 * 1024, &vec![0u8; 256 * 1024])),
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
            0, // diag_slot：测试源走槽 0
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
        let dir = env::temp_dir().join(format!("erika-preload-test-{}-{name}", std::process::id()));
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
        assert_eq!(
            store.len(),
            4096,
            "残留文件的窗口必须被恢复（弱网重开省流量）"
        );
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
            sealed: Vec::new(),
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
        let other =
            "https://cn-gotcha.bilivideo.com/upgcxcode/1/2/3/4000000000001-1-30077.m4s?deadline=1";
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
            sealed: Vec::new(),
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

    // =====================================================================
    // 顺序长读（sequential stream）
    //
    // 核心命题：**一次连续播放的 HTTP 请求数与播放时长无关**——只在
    // seek 出窗 / 换 host / 连接被断 / 瞬时错误重试时 +1。改造前是
    // "每 256KB 一个 Range 请求"，1080P 一小时 4700~6000 个。
    // =====================================================================

    /// 流式 mock server：与 `spawn_mock_http_server`（预置响应、只服务有限
    /// 次数）不同，它服务**任意**连接数，按请求里的 `Range: bytes=<start>-`
    /// 现场生成响应体（字节值 = `offset % 256`），因此可以模拟"8 分钟 1080P
    /// ≈ 166MB"而不用真的分配 166MB 内存。返回 (uri, 累计请求数, 请求头通道)。
    ///
    /// `cut_after_first`：**仅第一条连接**在写了这么多 body 字节后直接断开
    /// （模拟 CDN 主动掐断长连接）。
    fn spawn_streaming_mock_http_server(
        total: u64,
        cut_after_first: Option<u64>,
    ) -> (String, Arc<AtomicU64>, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.m4s", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicU64::new(0));
        let (sender, receiver) = mpsc::channel();
        let counter = Arc::clone(&requests);
        thread::spawn(move || {
            let mut connection_index = 0u64;
            loop {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let sender = sender.clone();
                let counter = Arc::clone(&counter);
                let cut = if connection_index == 0 {
                    cut_after_first
                } else {
                    None
                };
                connection_index += 1;
                thread::spawn(move || {
                    serve_streaming_connection(stream, total, cut, counter, sender);
                });
            }
        });
        (uri, requests, receiver)
    }

    /// 解析请求头里的 `Range: bytes=<start>[-<end>]`，同时把请求头记进通道、
    /// 连接计数 +1（新写的"收线/分片"mock 共用，形态与
    /// `serve_streaming_connection` 完全一致）。
    fn read_range_request(
        stream: &mut std::net::TcpStream,
        requests: &AtomicU64,
        heads: &mpsc::Sender<String>,
    ) -> Option<(u64, Option<u64>)> {
        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        requests.fetch_add(1, Ordering::Relaxed);
        let _ = heads.send(head.clone());
        head.lines().find_map(|line| {
            let lower = line.trim().to_ascii_lowercase();
            let rest = lower.strip_prefix("range: bytes=")?.to_string();
            let (start, end) = rest.split_once('-')?;
            let start = start.trim().parse::<u64>().ok()?;
            let end = end.trim();
            let end = if end.is_empty() {
                None
            } else {
                Some(end.parse::<u64>().ok()?)
            };
            Some((start, end))
        })
    }

    /// **「CDN 每 N 字节收线」mock**（用户真机形态的复现）：响应**声明**完整的
    /// `Content-Range`（`bytes <start>-<end>/<total>` + 完整 Content-Length），
    /// 却只写 `cut` 字节就把连接关掉。HTTP 客户端的 body reader 因此报错（而不是
    /// 干净的 EOF），内核走"服务端提前收线 → 从 write_pos 静默续传"这条路径——
    /// 用户实测的"每 ~1MB 重建一次、~0.5 次/秒"就是它。
    fn spawn_cutting_mock_http_server(
        total: u64,
        cut: u64,
    ) -> (String, Arc<AtomicU64>, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicU64::new(0));
        let (sender, receiver) = mpsc::channel();
        let counter = Arc::clone(&requests);
        thread::spawn(move || {
            loop {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let counter = Arc::clone(&counter);
                let sender = sender.clone();
                thread::spawn(move || {
                    serve_cutting_connection(stream, total, cut, counter, sender)
                });
            }
        });
        (uri, requests, receiver)
    }

    fn serve_cutting_connection(
        mut stream: std::net::TcpStream,
        total: u64,
        cut: u64,
        requests: Arc<AtomicU64>,
        heads: mpsc::Sender<String>,
    ) {
        let Some((start, requested_end)) = read_range_request(&mut stream, &requests, &heads)
        else {
            return;
        };
        if start >= total {
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            );
            return;
        }
        let end = requested_end.unwrap_or(total - 1).min(total - 1);
        let declared = end - start + 1;
        let header = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(header.as_bytes()).is_err() {
            return;
        }
        let limit = declared.min(cut);
        if write_stream_body(&mut stream, start, limit).is_err() {
            return;
        }
        let _ = stream.flush();
        // 直接收线（不发完声明的 Content-Length）——这正是 CDN 的行为。
    }

    /// **分片式 CDN mock**：每次响应只声明并发送 `chunk` 字节，`Content-Range`
    /// 的 `end + 1 < total`（合法分片响应）。内核必须**续传**而不是把流判成
    /// EOF——真机实测的音频"停在 2.7MB/8MB"就是旧实现把分片尾当成了文件尾。
    fn spawn_chunked_mock_http_server(
        total: u64,
        chunk: u64,
    ) -> (String, Arc<AtomicU64>, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicU64::new(0));
        let (sender, receiver) = mpsc::channel();
        let counter = Arc::clone(&requests);
        thread::spawn(move || {
            loop {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let counter = Arc::clone(&counter);
                let sender = sender.clone();
                thread::spawn(move || {
                    serve_chunked_connection(stream, total, chunk, counter, sender)
                });
            }
        });
        (uri, requests, receiver)
    }

    fn serve_chunked_connection(
        mut stream: std::net::TcpStream,
        total: u64,
        chunk: u64,
        requests: Arc<AtomicU64>,
        heads: mpsc::Sender<String>,
    ) {
        let Some((start, _)) = read_range_request(&mut stream, &requests, &heads) else {
            return;
        };
        if start >= total {
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            );
            return;
        }
        let end = start.saturating_add(chunk.max(1) - 1).min(total - 1);
        let length = end - start + 1;
        let header = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(header.as_bytes()).is_err() {
            return;
        }
        if write_stream_body(&mut stream, start, length).is_err() {
            return;
        }
        let _ = stream.flush();
    }

    /// 单条流式连接：解析 `Range: bytes=<start>-` → 206 头 + 现场生成的 body。
    fn serve_streaming_connection(
        mut stream: std::net::TcpStream,
        total: u64,
        cut_after: Option<u64>,
        requests: Arc<AtomicU64>,
        heads: mpsc::Sender<String>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        requests.fetch_add(1, Ordering::Relaxed);
        let _ = heads.send(head.clone());
        // `Range: bytes=<start>-`（开放结尾）→ 一直服务到文件尾；
        // `bytes=<start>-<end>`（旧分段路径的闭区间）→ 只服务请求的那一段。
        let (start, requested_end) = head
            .lines()
            .find_map(|line| {
                let lower = line.trim().to_ascii_lowercase();
                let rest = lower.strip_prefix("range: bytes=")?.to_string();
                let (start, end) = rest.split_once('-')?;
                let start = start.trim().parse::<u64>().ok()?;
                let end = end.trim();
                let end = if end.is_empty() {
                    None
                } else {
                    Some(end.parse::<u64>().ok()?)
                };
                Some((start, end))
            })
            .unwrap_or((0, None));
        if start >= total {
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            );
            return;
        }
        let end = requested_end.unwrap_or(total - 1).min(total - 1);
        let declared = end - start + 1;
        // 请求是开放结尾（bytes=N-）时，服务端如实把 [N, total) 全部声明出来
        // ——这正是 CDN 对 mpv/浏览器的回答形态。
        let header = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(header.as_bytes()).is_err() {
            return;
        }
        let limit = match cut_after {
            Some(cut) => declared.min(cut),
            None => declared,
        };
        if write_stream_body(&mut stream, start, limit).is_err() {
            return;
        }
        let _ = stream.flush();
    }

    /// 按 `offset % 256` 生成 body：256 字节周期整块复制，166MB 也是毫秒级，
    /// 且任何偏移错位都会在断言里立刻暴露。
    fn write_stream_body(
        stream: &mut std::net::TcpStream,
        mut offset: u64,
        mut remaining: u64,
    ) -> std::io::Result<()> {
        let template: Vec<u8> = (0..256u32).map(|value| value as u8).collect();
        let mut block = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let n = remaining.min(block.len() as u64) as usize;
            let mut position = 0usize;
            let mut shift = (offset % 256) as usize;
            while position < n {
                let take = (n - position).min(256 - shift);
                block[position..position + take].copy_from_slice(&template[shift..shift + take]);
                position += take;
                shift = 0;
            }
            stream.write_all(&block[..n])?;
            offset += n as u64;
            remaining -= n as u64;
        }
        Ok(())
    }

    /// 从请求头里取出 `Range` 的值（已小写、去空白），供形态断言。
    fn range_header_of(head: &str) -> String {
        head.lines()
            .find(|line| line.trim_start().to_ascii_lowercase().starts_with("range:"))
            .map(|line| line.trim().to_ascii_lowercase())
            .unwrap_or_default()
    }

    /// 顺序读 `length` 字节（模拟 demux 的连续读），断言内容按偏移生成。
    fn read_forward(source: &mut HttpRangeSource, from: u64, length: u64) -> Vec<u8> {
        let mut collected = Vec::new();
        let mut cursor = from;
        while (cursor - from) < length {
            let want = (from + length - cursor).min(32 * 1024);
            let bytes = source
                .read_range(ByteRange {
                    start: cursor,
                    length: Some(want),
                })
                .unwrap_or_else(|error| panic!("offset {cursor} 读取失败: {error}"));
            assert_eq!(bytes.len() as u64, want, "offset {cursor} 短读");
            assert_eq!(bytes[0], (cursor % 256) as u8, "offset {cursor} 字节错位");
            assert_eq!(
                bytes[bytes.len() - 1],
                ((cursor + want - 1) % 256) as u8,
                "offset {cursor} 字节错位"
            );
            collected.extend_from_slice(&bytes);
            cursor += want;
        }
        collected
    }

    #[test]
    fn legacy_segmented_path_request_count_baseline_for_the_same_playback() {
        // 对照基线（**刻意保留旧路径的实测数字**）：同一段播放走
        // `ERIKA_HTTP_SEQUENTIAL=0` 的旧分段路径时到底发多少个请求。
        // 读 8MB（≈2.3 分钟 1080P），按 256KB 段算 ≈32 个；顺序长读路径在同
        // 一场景里是 1 个。这个数字就是"一小时几千次"的来源，留着当回归标尺。
        const BYTES: u64 = 8 * 1024 * 1024;
        let (uri, requests, _heads) = spawn_streaming_mock_http_server(BYTES, None);
        // 不经 `enable_sequential_stream` → 旧分段路径（与生产回退开关等价）。
        let mut source = HttpRangeSource::new(uri);
        source.content_length = Some(BYTES);
        read_forward(&mut source, 0, BYTES);
        let legacy = requests.load(Ordering::Relaxed);
        // 跑 `--nocapture` 时打印实测数字（回归时一眼看出量级）。
        println!(
            "legacy segmented path: {legacy} HTTP requests for {BYTES} bytes \
             (sequential path = 1 for the same traffic)"
        );
        assert!(
            legacy >= 16,
            "旧分段路径是「每段一个请求」（256KB 段 → 8MB ≈32 个），实际 {legacy}"
        );
    }

    #[test]
    fn sequential_stream_covers_a_whole_playback_in_one_request() {
        // **本次改造的核心验收**：8 分钟 1080P（347KB/s × 480s ≈ 166MB）连续
        // 播放 → **1 个** HTTP 请求。改造前同样一段播放按 256KB 段切分是 ≈660
        // 个请求（一小时 4700~6000），官方客户端全程约 79 个。
        const PLAYBACK_BYTES: u64 = 166 * 1024 * 1024;
        let (uri, requests, heads) = spawn_streaming_mock_http_server(PLAYBACK_BYTES, None);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        assert!(source.sequential_enabled);
        // 按 demux 的真实读粒度（AVIO 32KB 块）从头读到尾。
        read_forward(&mut source, 0, PLAYBACK_BYTES);
        assert_eq!(
            requests.load(Ordering::Relaxed),
            1,
            "8 分钟连续播放必须只有 1 个 HTTP 请求（改造前 ≈660 个）"
        );
        // 且必须是**开放结尾**的 Range（`bytes=<start>-`，无结束偏移）——
        // 这是 mpv / 浏览器 / 官方客户端看视频的请求形态。
        let head = recv_request_head(&heads);
        assert_eq!(
            range_header_of(&head),
            "range: bytes=0-",
            "必须是开放结尾的 Range，实际请求头: {head}"
        );
        // 窗口应当已经填到文件尾（读完 166MB 后读线程收工，不再重连）。
        let (_, fill, total) = source.stream_window().unwrap();
        assert_eq!(total, Some(PLAYBACK_BYTES));
        assert!(
            fill >= PLAYBACK_BYTES - 256 * 1024,
            "读到尾后填充位置: {fill}"
        );
    }

    #[test]
    fn sequential_stream_seek_costs_exactly_one_more_request() {
        const TOTAL: u64 = 64 * 1024 * 1024;
        let (uri, requests, heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        // 顺序读 1MB：1 个请求。
        read_forward(&mut source, 0, 1024 * 1024);
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        let first = recv_request_head(&heads);
        assert_eq!(range_header_of(&first), "range: bytes=0-");

        // AVIO seek 钩子（`prefetch_hint`）→ 窗口外目标：读线程放弃旧连接、
        // 从目标重发**一条** open-ended Range。
        let target = 32 * 1024 * 1024;
        source.prefetch_hint(target);
        let bytes = source
            .read_range(ByteRange {
                start: target,
                length: Some(64 * 1024),
            })
            .unwrap();
        assert_eq!(bytes[0], (target % 256) as u8);
        assert_eq!(requests.load(Ordering::Relaxed), 2, "seek 只应多一个请求");
        let second = recv_request_head(&heads);
        assert_eq!(
            range_header_of(&second),
            format!("range: bytes={target}-"),
            "seek 后的请求必须从目标起、且开放结尾"
        );

        // 窗口内回退（后向缓冲命中）：**零请求**。
        source.prefetch_hint(target + 32 * 1024);
        let back = source
            .read_range(ByteRange {
                start: target + 32 * 1024,
                length: Some(16 * 1024),
            })
            .unwrap();
        assert_eq!(back[0], ((target + 32 * 1024) % 256) as u8);
        assert_eq!(
            requests.load(Ordering::Relaxed),
            2,
            "窗口内 seek 不得产生请求"
        );

        // 回退到**后向缓冲之外**（回到文件开头）：后向缓冲只覆盖已消费的
        // back_bytes，超出就得重发一条——这才是诚实的边界。
        source.prefetch_hint(0);
        let rewind = source
            .read_range(ByteRange {
                start: 0,
                length: Some(64 * 1024),
            })
            .unwrap();
        assert_eq!(rewind[0], 0);
        assert_eq!(
            requests.load(Ordering::Relaxed),
            3,
            "超出后向缓冲的回退必须重发（恰好 +1）"
        );
        let third = recv_request_head(&heads);
        assert_eq!(range_header_of(&third), "range: bytes=0-");
    }

    #[test]
    fn sequential_stream_stops_reading_when_the_window_is_full() {
        // **背压**：前向窗口填满后必须停止从 socket 读（不关连接、不发新请求）。
        // 若背压失效，读线程会在几百毫秒内把 8MB 全下完——断言直接抓住。
        //
        // 边界口径（2026-09-15 修正；这条测试此前 flaky）：窗口满后**不是零增长**
        // —— `stream_drip_enabled()` 默认开启，满窗后每 200ms 仍会再滴灌 16KiB，
        // 上限是 `front_cap + STREAM_DRIP_HEADROOM_BYTES`（1MiB）。旧断言写的是
        // `fill <= 256KB + 128KB`：本机实测（临时探针）**400ms 就已经是 352KB**，
        // 余量只有 32KB；机器一忙（599 个用例并行时 sleep 被拉长）就越界失败。
        // 也就是说随机失败的不是背压，是断言没把滴灌余量算进去。
        //
        // 现在的口径与兄弟用例 `window_stops_at_the_budget_without_discarding_buffered_bytes`
        // 一致：① 绝对上界 = 前向预算 + 滴灌余量 + 小块余量；② 满窗后的**增长速率**
        // 必须仍是滴灌量级（按实测时间间隔归一，避免"忙 → sleep 拉长"再次误判）。
        const TOTAL: u64 = 8 * 1024 * 1024;
        const FRONT_CAP: u64 = 256 * 1024; // = STREAM_MIN_FRONT_CAP
        let (uri, requests, _heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        source.read_ahead_bytes = 256 * 1024; // = STREAM_MIN_FRONT_CAP
        let first = source
            .read_range(ByteRange {
                start: 0,
                length: Some(64 * 1024),
            })
            .unwrap();
        assert_eq!(first.len(), 64 * 1024);
        // 有界等待"窗口真的填满"，而不是"睡够 400ms 就该填满"（后者在并行压力下
        // 会变成"还没填满就断言"）。
        let fill_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut fill = 0u64;
        while std::time::Instant::now() < fill_deadline {
            let (_, current, _) = source.stream_window().unwrap();
            fill = current;
            if fill >= FRONT_CAP {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(fill >= FRONT_CAP, "窗口必须在超时前填满：fill={fill}");
        assert!(
            fill <= FRONT_CAP + STREAM_DRIP_HEADROOM_BYTES + 128 * 1024,
            "窗口写满后不得继续读：fill={fill}（8MB 全下完即背压失效）"
        );
        assert_eq!(
            requests.load(Ordering::Relaxed),
            1,
            "背压不得靠重发请求实现"
        );
        // 满窗后的增长速率：标称滴灌 = 16KiB/200ms ≈ 80KB/s；本机实测最坏
        // ≈245KB/s（352KB@400ms）。上界取 1MiB/s —— 仍比"本地环回全速下载"
        // （百 MB/s 量级）低两个数量级，所以"背压失效"依然会被抓住。
        let before_growth = fill;
        let growth_started = std::time::Instant::now();
        thread::sleep(Duration::from_millis(400));
        let growth_seconds = growth_started.elapsed().as_secs_f64().max(0.001);
        let (_, after_growth, _) = source.stream_window().unwrap();
        let grown = after_growth.saturating_sub(before_growth);
        let grown_per_sec = grown as f64 / growth_seconds;
        assert!(
            grown_per_sec <= 1024.0 * 1024.0,
            "满窗后必须只剩滴灌量级的读取：{grown} 字节 / {growth_seconds:.3}s = {grown_per_sec:.0} B/s"
        );
        // 消费之后必须能继续推进，且**仍然只有那一个请求**（连接没被关掉）。
        let next = source
            .read_range(ByteRange {
                start: 64 * 1024,
                length: Some(64 * 1024),
            })
            .unwrap();
        assert_eq!(next.len(), 64 * 1024);
        let progress_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut fill_after = fill;
        while std::time::Instant::now() < progress_deadline {
            let (_, current, _) = source.stream_window().unwrap();
            fill_after = current;
            if fill_after > fill {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            fill_after > fill,
            "消费后应继续填充：{fill} -> {fill_after}"
        );
        assert_eq!(requests.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn sequential_stream_resumes_silently_after_the_server_closes_early() {
        // CDN 主动掐断长连接的真实形态：声明了 Content-Length 却没发完就收线
        // （HTTP 客户端的 body reader 会报错而不是 EOF）。读线程必须从 `write_pos`
        // **静默续传**：demux 侧看不到任何错误，请求数恰好 +1。
        const TOTAL: u64 = 8 * 1024 * 1024;
        let (uri, requests, heads) = spawn_streaming_mock_http_server(TOTAL, Some(16 * 1024));
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        let bytes = source
            .read_range(ByteRange {
                start: 0,
                length: Some(64 * 1024),
            })
            .unwrap();
        assert_eq!(bytes.len(), 64 * 1024, "demux 侧必须无感（不得短读/报错）");
        assert_eq!(bytes[0], 0);
        assert_eq!(bytes[64 * 1024 - 1], ((64 * 1024 - 1) % 256) as u8);
        assert_eq!(
            requests.load(Ordering::Relaxed),
            2,
            "掐断后从断点续传：恰好 +1 个请求"
        );
        let first = recv_request_head(&heads);
        let second = recv_request_head(&heads);
        assert_eq!(range_header_of(&first), "range: bytes=0-");
        // 续传起点取决于客户端在被掐断前**实际收到**多少字节（TCP 分段不确定），
        // 断言它落在 (0, 掐断点] 内即可——语义"从已收字节之后续上"完全一致。
        let resumed: u64 = range_header_of(&second)
            .trim_start_matches("range: bytes=")
            .trim_end_matches('-')
            .parse()
            .unwrap_or_else(|_| panic!("续传请求头无法解析: {second}"));
        assert!(
            resumed > 0 && resumed <= 16 * 1024,
            "续传必须从已收字节之后起（0 < {resumed} <= 16384）"
        );
    }

    #[test]
    fn sequential_stream_skips_a_whole_file_200_prefix_after_reconnect() {
        // 服务端忽略 Range：断线续传时它从字节 0 重发整片。读线程必须丢弃
        // `write_pos` 之前的前缀再续上——把整片直接当"从 write_pos 开始的
        // 续传字节"写窗就是静默错位（每个字节都会落到错误的绝对偏移上）。
        const TOTAL: u64 = 512 * 1024;
        let body: Vec<u8> = (0..TOTAL).map(|index| (index % 251) as u8).collect();
        let (uri, heads) = spawn_mock_http_server(vec![
            // 声明整片长度却只发 64KB 就收线：读线程从 write_pos 重连。
            MockResponse::immediate(http_206_truncated_response(
                0,
                TOTAL,
                TOTAL as usize,
                &body[..64 * 1024],
            )),
            // 重连时服务端忽略 Range：200 又从字节 0 发整片。
            MockResponse::immediate(http_200_without_length(&body, false)),
            // 防御性第 3 份：实现若多发请求，下面的断言失败而不是把测试挂死。
            MockResponse::immediate(http_200_without_length(&body, false)),
        ]);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        let bytes = source
            .read_range(ByteRange {
                start: 0,
                length: Some(256 * 1024),
            })
            .unwrap();
        assert_eq!(bytes.len(), 256 * 1024);
        assert!(
            bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| *byte == (index % 251) as u8),
            "续传后每个字节都必须落在自己的绝对偏移上（前缀被正确丢弃）"
        );
        let heads: Vec<String> = heads.try_iter().collect();
        assert_eq!(heads.len(), 2, "断线只需续传一次：{heads:?}");
        assert_eq!(range_header_of(&heads[0]), "range: bytes=0-");
    }

    #[test]
    fn full_preload_bucket_downloads_the_whole_file_in_one_request() {
        // 整片预载桶也必须是**流式**：旧实现（`full_preload_step`）是"每步一个
        // 256KB Range 请求"，4MB 桶 = 16 个请求、一整部 300MB 片子上千个——
        // 与 demux 侧的分段预取叠加正是用户实测的"一小时几千次"。
        // 这里断言整个桶只花 **1** 个请求，且桶的语义（按绝对偏移读回）不变。
        const TOTAL: u64 = 4 * 1024 * 1024;
        let (uri, requests, heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let state = FullPreloadState::new();
        state.learn_total(TOTAL);
        full_preload_stream_loop(
            &http_stream_agent(),
            Arc::new(Mutex::new(uri)),
            &[],
            &state,
            TOTAL, // cap
            false, // eager
            false, // report_audio_first
            false, // video_bucket
        );
        assert_eq!(
            requests.load(Ordering::Relaxed),
            1,
            "整片预载必须是 1 个请求（旧实现 4MB 桶 = 16 个请求）"
        );
        assert_eq!(
            range_header_of(&recv_request_head(&heads)),
            "range: bytes=0-"
        );
        assert_eq!(state.covered_until(), TOTAL);
        let head = state.copy_range(0, 4096).unwrap();
        assert_eq!(head[0], 0);
        assert_eq!(head[4095], (4095 % 256) as u8);
        let tail = state.copy_range(TOTAL - 4096, 4096).unwrap();
        assert_eq!(tail[0], ((TOTAL - 4096) % 256) as u8);
    }

    #[test]
    fn sequential_stream_reports_the_live_window_in_diagnostics() {
        // 宿主（bili-rs）直接吃这些字段，语义一个都不能变：
        //   - `cache_start` / `demux_cache_end` 必须是**长流窗口**（旧字段
        //     `cache_bytes` 在长读路径下恒空，再用它算窗口就是"缓存恒 0"，
        //     策略层立刻判饥饿 → 无故换源/降档）；
        //   - `content_length` 由首个 206 的 Content-Range 回填（**不发 HEAD**）；
        //   - `prefetch_in_flight` 是 O(1)（旧模型 5~6 条连接）；
        //   - 请求画像如实计数（含重试与续传的口径不变）。
        const TOTAL: u64 = 8 * 1024 * 1024;
        let (uri, requests, _heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        read_forward(&mut source, 0, 512 * 1024);
        let snapshot = source.diag_snapshot();
        assert_eq!(snapshot.cache_start, 0);
        assert!(
            snapshot.demux_cache_end >= 512 * 1024,
            "demux 窗口前沿必须反映长流已填位置，实际 {}",
            snapshot.demux_cache_end
        );
        assert_eq!(
            snapshot.content_length,
            Some(TOTAL),
            "总长应由首个 206 的 Content-Range 回填（不发 HEAD）"
        );
        assert!(
            snapshot.prefetch_in_flight <= 3,
            "长读路径的并发连接数是 O(1)，实际 {}",
            snapshot.prefetch_in_flight
        );
        // 进程级单调计数：本源的请求确实被记进去了（其它并行用例只会累加）。
        assert!(
            snapshot.http_requests >= requests.load(Ordering::Relaxed),
            "请求画像必须反映真实请求数"
        );
        assert!(snapshot.active_host.contains("127.0.0.1"));
        assert_eq!(snapshot.host_epoch, 0);
    }

    #[test]
    fn sequential_stream_host_switch_resumes_on_the_new_host_without_losing_bytes() {
        // 换源自愈：切 host 时**已收字节全在窗口里**，读线程只是换一条连接、
        // 从 `write_pos` 续——demux 侧一个字节都不丢、一个错误都看不到。
        // 旧模型要"作废在飞段 + 重发"，这里没有"在飞段"这个概念。
        const TOTAL: u64 = 32 * 1024 * 1024;
        let (uri_a, requests_a, _heads_a) = spawn_streaming_mock_http_server(TOTAL, None);
        let (uri_b, requests_b, heads_b) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source =
            HttpRangeSource::with_http_headers_and_diag_slot(uri_a.clone(), Vec::new(), 0)
                .enable_sequential_stream();
        source.set_candidates(vec![(uri_a.clone(), 1.0), (uri_b.clone(), 0.5)]);
        read_forward(&mut source, 0, 1024 * 1024);
        assert_eq!(requests_a.load(Ordering::Relaxed), 1);
        let fill_before = source.stream_window().unwrap().1;
        // 模拟死连：久无字节但 demux 仍在读。
        //
        // **注意（2026-09-04 订正）**：顺序长读的"最后收到字节"由读线程自己维护
        // （`SequentialReader::last_arrival_ms`，修复了"该时间戳在顺序长读下从不
        // 刷新 → 每 6 秒换一次 host"的真机 bug）。所以死连必须把**那个**时间戳
        // 做旧；只改 `source.last_byte_ms` 已经不再影响判据（它取两者较新者）。
        source
            .stream
            .as_ref()
            .expect("顺序长读已起")
            .last_arrival_ms
            .store(now_ms().saturating_sub(10_000), Ordering::Relaxed);
        source.last_byte_ms = now_ms().saturating_sub(10_000);
        source.last_read_call_ms = now_ms();
        source.maybe_switch_host();
        assert_eq!(source.active_idx, 1);
        assert_eq!(source.host_epoch, 1);
        // 继续读：数据必须**继续**（偏移连续、内容正确）——跨 host 无缝。
        read_forward(&mut source, 1024 * 1024, 2 * 1024 * 1024);
        assert_eq!(
            requests_a.load(Ordering::Relaxed),
            1,
            "换源后不得再打旧 host"
        );
        assert_eq!(
            requests_b.load(Ordering::Relaxed),
            1,
            "新 host 上从 write_pos 续传：恰好一个请求"
        );
        let head = recv_request_head(&heads_b);
        let resumed: u64 = range_header_of(&head)
            .trim_start_matches("range: bytes=")
            .trim_end_matches('-')
            .parse()
            .unwrap_or_else(|_| panic!("续传请求头无法解析: {head}"));
        // 续传起点 = 换 host 那一刻的已收到前沿（采样后再涨一点是正常的：切换
        // 前旧 host 还在推字节，那些字节照样保留）。
        assert!(
            resumed >= fill_before && resumed < fill_before + 8 * 1024 * 1024,
            "续传起点必须是已收到的前沿（{resumed} vs 采样 {fill_before}）"
        );
    }

    #[test]
    fn sequential_stream_drop_returns_without_waiting_for_the_connection() {
        // 关窗/换源时的**有界退出**：读线程可能正阻塞在 socket read 上，
        // 裸 join 会把 UI 线程钉住（用户最忌讳的"关窗卡死"）。Drop 走有界等待
        // （500ms）+ detach，这里断言它真的很快返回。
        const TOTAL: u64 = 256 * 1024 * 1024;
        let (uri, _requests, _heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        read_forward(&mut source, 0, 256 * 1024);
        // 窗口远没填满（4MB vs 256MB），读线程此刻正在猛读。
        let started = Instant::now();
        drop(source);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "Drop 必须有界返回（实际 {:?}）",
            started.elapsed()
        );
    }

    // =====================================================================
    // 滑动窗口 + 分层缓存（2026-09-04 架构改造）
    //
    // 覆盖用户实测的四条缺陷与两条追加需求：
    //   ④ 整段缓冲完之后重播仍在"重复大量下载"；
    //   ⑤ 回拖/重播到已缓冲区域必须**零网络**命中（内存层或磁盘层）；
    //   ① 缓冲不再"一路冲到整片下完"：达到预算就停（背压），消费推进再补；
    //   ③ 缓存有明确字节预算，且**能在诊断里看到**；
    //   （追加）弱网按**秒数**更激进地缓冲，同时内存不越字节硬上限；
    //   （追加）冷区落盘 + 磁盘预算 + 淘汰真的删文件。
    // =====================================================================

    /// 按生产路径装配顺序长读 + 整片桶（含归档冷区），但把桶目录指向测试
    /// 自己的临时目录（绝不碰 `ERIKA_PRELOAD_DIR`，避免与并行用例互污）。
    fn streaming_source_with_cold_tier(
        uri: String,
        dir: &Path,
    ) -> (HttpRangeSource, Arc<FullPreloadState>) {
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        // 目录必须在 `enable_full_preload` 之前指好（桶/冷区在那一刻开）。
        source.override_preload_dir(dir.to_path_buf());
        let source = source.enable_full_preload();
        let state = source.full_preload.clone().expect("冷区状态必须已装配");
        assert!(state.archive_mode(), "顺序长读路径必须进归档模式");
        (source, state)
    }

    #[test]
    fn replay_of_a_fully_buffered_file_costs_zero_requests() {
        // **缺陷 ④**：整个窗口都缓冲结束之后，重播（把播放位置挪回开头再放）
        // 不得再产生任何网络请求——"事实上都已经缓冲结束了，就不需要再对它
        // 进行缓冲了"。数据仍在预算内（这里 2MB < 4MB 前向预算），所以窗口
        // 从头到尾都在内存里，既不淘汰也不重下。
        const TOTAL: u64 = 2 * 1024 * 1024;
        let dir = preload_test_dir("replay");
        let (uri, requests, _heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let (mut source, state) = streaming_source_with_cold_tier(uri, &dir);

        // 第一次播完（整片 2MB）。
        read_forward(&mut source, 0, TOTAL);
        let first_play = requests.load(Ordering::Relaxed);
        println!("replay: 第一次播完 {TOTAL} 字节 → {first_play} 个请求");
        assert_eq!(first_play, 1, "整片顺序播放只应有 1 个请求");

        // 重播：AVIO seek 钩子 + 从头读。
        source.prefetch_hint(0);
        read_forward(&mut source, 0, TOTAL);
        let after_replay = requests.load(Ordering::Relaxed);
        println!("replay: 重播整片后 → {after_replay} 个请求（必须仍为 1）");
        assert_eq!(
            after_replay, 1,
            "整段仍在预算内时重播必须零新请求（缺陷 ④），实际 {after_replay}"
        );
        // 诚实边界：这条路径没用到磁盘层（窗口全在内存），冷区字节应为 0。
        assert_eq!(state.cold_bytes(), 0, "2MB 全在前向预算内，不该有淘汰");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seek_back_into_the_disk_tier_costs_zero_requests_and_keeps_the_bytes() {
        // **缺陷 ⑤ + 冷区落盘的核心验收**：往前播过一段（比回看窗口大得多）
        // 之后，回拖到很久以前的已缓冲区间：
        //   ① 必须 **0 个新网络请求**（字节来自磁盘冷区，而不是网络）；
        //   ② 读回的字节必须与原始字节**完全一致**（错位 = 静默数据损坏）。
        // 总长与前进量都按后向窗口推算，不写死 16MB：后向窗口默认值 4MB → 16MB
        // 之后，"前进 16MB 就必然把早期区间淘汰出热区"这条前提不再成立
        // （实测：冷区 0 字节）。
        const TOTAL: u64 = HttpRangeSource::DEFAULT_BACK_BUFFER_BYTES * 4;
        const FORWARD: u64 = HttpRangeSource::DEFAULT_BACK_BUFFER_BYTES * 2;
        let dir = preload_test_dir("cold-seek-back");
        let (uri, requests, _heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let (mut source, state) = streaming_source_with_cold_tier(uri, &dir);

        // 往前播 2 倍后向窗口：远超回看预算 → 早期区间被淘汰进冷区。
        read_forward(&mut source, 0, FORWARD);
        // 冷区写入是异步的（归档线程），等它把交接块落完。
        let cold_ready = Instant::now();
        while state.cold_bytes() == 0 && cold_ready.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        let cold_bytes = state.cold_bytes();
        let forward_requests = requests.load(Ordering::Relaxed);
        println!(
            "cold-tier: 前向 {}MB 后 → 请求 {forward_requests} 个，冷区 {cold_bytes} 字节",
            FORWARD / (1024 * 1024)
        );
        assert!(
            cold_bytes > 0,
            "前向播过 2 倍后向窗口后，早期区间必须被淘汰进冷区"
        );
        assert!(
            state.covers(0, 64 * 1024),
            "最早期的那一块必须真的在冷区里（覆盖下界 = 0）"
        );

        // 回拖到原点：AVIO seek 钩子 + 读第一块。
        source.prefetch_hint(0);
        let back = source
            .read_range(ByteRange {
                start: 0,
                length: Some(64 * 1024),
            })
            .unwrap_or_else(|error| panic!("回拖到已落盘区间读取失败: {error}"));
        let after_seek_back = requests.load(Ordering::Relaxed);
        println!(
            "cold-tier: 回拖到 0 再读 64KB → 请求 {after_seek_back} 个（必须仍为 {forward_requests}），冷区命中 {} 次",
            state.cold_hit_count()
        );
        assert_eq!(
            after_seek_back, forward_requests,
            "回拖到已落盘区间必须零新请求（缺陷 ⑤），实际 {after_seek_back}"
        );
        assert_eq!(back.len(), 64 * 1024);
        // 字节一致性：服务器按 `offset % 256` 生成内容，逐字节核对。
        for (index, byte) in back.iter().enumerate() {
            assert_eq!(
                *byte,
                (index % 256) as u8,
                "冷区读回的第 {index} 字节错位（静默数据损坏）"
            );
        }
        assert!(state.cold_hit_count() > 0, "命中必须被计入诊断");
        // 诊断字段：内存层/磁盘层字节数都必须如实可见。
        let snapshot = source.diag_snapshot();
        println!(
            "cold-tier: 诊断 cache_memory_bytes={} cache_disk_bytes={} cold_hits={} cold_hit_bytes={}",
            snapshot.cache_memory_bytes,
            snapshot.cache_disk_bytes,
            snapshot.cold_hits,
            snapshot.cold_hit_bytes
        );
        assert!(
            snapshot.cache_disk_bytes >= 64 * 1024,
            "磁盘层至少要能看到回拖命中的那一块，实际 {}",
            snapshot.cache_disk_bytes
        );
        assert!(snapshot.cold_hit_bytes >= back.len() as u64);
        // 内存层不得越过硬上限。
        assert!(
            snapshot.cache_memory_bytes <= snapshot.memory_budget_bytes,
            "内存层 {} 越过硬上限 {}",
            snapshot.cache_memory_bytes,
            snapshot.memory_budget_bytes
        );
        drop(source);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_stops_at_the_budget_without_discarding_buffered_bytes() {
        // **缺陷 ① + 需求 ②③**：缓冲不再"一路冲到整片下完"。前向窗口填到预算
        // 就停下（背压，不关连接、不发新请求），并且**不丢**任何已缓冲字节
        // （旧实现在 demux 跑赢时会把桶整体前移丢弃旧字节）。
        const TOTAL: u64 = 8 * 1024 * 1024;
        let (uri, requests, _heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        // 明确的小预算（不改 env，避免影响并行用例）：前向 512KB / 后向 256KB。
        source.read_ahead_bytes = 512 * 1024;
        source.back_bytes = 256 * 1024;

        read_forward(&mut source, 0, 64 * 1024);
        thread::sleep(Duration::from_millis(400));
        let (base, fill, _) = source.stream_window().unwrap();
        println!("budget: 填窗后 base={base} fill={fill}（前向预算 524288 + 滴灌余量）");
        // 窗口停在"前向预算 + 滴灌余量"附近：滴灌的存在只允许再多读 1MiB 上限
        // （见 `STREAM_DRIP_HEADROOM_BYTES`），**不再是"一路下到整片"**。
        let drip_room = STREAM_DRIP_HEADROOM_BYTES;
        assert!(
            fill <= 512 * 1024 + drip_room + 128 * 1024,
            "前向窗口必须停在预算+滴灌余量附近，实际 fill={fill}（8MB 全下完 = 预算失效）"
        );
        thread::sleep(Duration::from_millis(700));
        let (_, fill_later, _) = source.stream_window().unwrap();
        let grown = fill_later.saturating_sub(fill);
        // 滴灌语义（2026-09-04）：窗口满之后**仍会以 ~80KB/s 的小口继续读**
        // （避免 CDN 判定写空闲而重置连接），但增长必须是"滴灌速率"量级，
        // 而不是全速下载——700ms 最多几十 KB。
        println!(
            "budget: 700ms 后 fill={fill_later}（增长 {grown} 字节，滴灌上限 ≈{}）",
            80 * 1024
        );
        assert!(
            grown <= 256 * 1024,
            "满窗后的增长必须是滴灌量级（≤256KB/700ms），实际 {grown}"
        );
        assert!(
            fill_later <= 512 * 1024 + drip_room + 128 * 1024,
            "滴灌不得越出预算+余量：{fill_later}"
        );
        assert_eq!(
            requests.load(Ordering::Relaxed),
            1,
            "停在预算是靠背压/滴灌实现的，不得靠重发请求"
        );

        // 不丢弃：窗口内（后向 256KB）回读必须零请求且字节正确。
        let back = source
            .read_range(ByteRange {
                start: 0,
                length: Some(32 * 1024),
            })
            .unwrap();
        assert_eq!(back[0], 0, "回读第一字节必须仍是文件第 0 字节");
        assert_eq!(
            requests.load(Ordering::Relaxed),
            1,
            "回读已缓冲区间不得产生新请求"
        );
        // 内存硬上限：常驻字节不得超过预算。
        let snapshot = source.diag_snapshot();
        println!(
            "budget: 诊断 cache_memory_bytes={} memory_budget={} forward_budget={}",
            snapshot.cache_memory_bytes,
            snapshot.memory_budget_bytes,
            snapshot.forward_budget_bytes
        );
        assert!(
            snapshot.cache_memory_bytes <= snapshot.memory_budget_bytes,
            "常驻内存 {} 越过硬上限 {}",
            snapshot.cache_memory_bytes,
            snapshot.memory_budget_bytes
        );
        assert!(
            snapshot.forward_budget_bytes >= 512 * 1024,
            "前向预算不得低于配置的 read-ahead"
        );
    }

    #[test]
    fn advancing_past_the_window_edge_refills_incrementally_on_one_connection() {
        // 需求 ②的后半句：播放推进到窗口边缘后**按预算继续往前补**，而且补的
        // 代价是"同一个连接继续读"，不是"每补一次重发一个请求"。
        const TOTAL: u64 = 8 * 1024 * 1024;
        let (uri, requests, _heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        source.read_ahead_bytes = 512 * 1024;
        source.back_bytes = 256 * 1024;

        read_forward(&mut source, 0, 128 * 1024);
        thread::sleep(Duration::from_millis(300));
        let (_, fill_first, _) = source.stream_window().unwrap();
        let requests_first = requests.load(Ordering::Relaxed);

        // 推进到窗口边缘附近再往前消费（模拟播放继续）。
        read_forward(&mut source, 128 * 1024, 256 * 1024);
        thread::sleep(Duration::from_millis(300));
        let (_, fill_second, _) = source.stream_window().unwrap();
        let requests_second = requests.load(Ordering::Relaxed);
        println!(
            "refill: 推进 256KB → fill {fill_first} -> {fill_second}，请求 {requests_first} -> {requests_second}"
        );
        assert!(
            fill_second > fill_first,
            "消费推进后必须继续往前补：{fill_first} -> {fill_second}"
        );
        assert_eq!(
            requests_second, requests_first,
            "补窗必须复用同一条连接（增量请求上界 = 0），实际 {requests_second}"
        );
        // 仍然不越内存硬上限。
        let snapshot = source.diag_snapshot();
        assert!(
            snapshot.cache_memory_bytes <= snapshot.memory_budget_bytes,
            "补窗后常驻内存 {} 越过硬上限 {}",
            snapshot.cache_memory_bytes,
            snapshot.memory_budget_bytes
        );
    }

    #[test]
    fn cold_budget_eviction_drops_the_oldest_segment_and_deletes_its_file() {
        // **追加需求**：磁盘也要有预算；超预算时按策略淘汰（丢**最久远**的段，
        // 保留最近回看窗口），并且**淘汰必须真的删文件**（不留残渣）。
        let dir = preload_test_dir("cold-evict");
        let body = vec![9u8; 1024 * 1024];
        let state = FullPreloadState::open("https://example.invalid/x.m4s", 0, 1024 * 1024, &dir);
        state.learn_total(64 * 1024 * 1024);
        // 预算 16MiB：塞 3 段（每段 8MiB）必然触发一次淘汰。
        state.cold_budget.store(16 * 1024 * 1024, Ordering::Relaxed);

        // 预算是"温"的：先塞两段（16MiB，不超），记录它们各自的文件路径。
        let mut first_segment_path = None;
        for index in 0..2u64 {
            for step in 0..8u64 {
                let offset = index * 8 * 1024 * 1024 + step * 1024 * 1024;
                state.ingest_evicted(offset, &body);
            }
            if index == 0 {
                let guard = state.inner.lock().unwrap();
                first_segment_path =
                    guard
                        .sealed
                        .first()
                        .and_then(|segment| match &segment.store {
                            PreloadStore::Disk(disk) => Some(disk.file.path.clone()),
                            PreloadStore::Memory(_) => None,
                        });
            }
        }
        let sealed_after_two = state.inner.lock().unwrap().sealed.len();
        let cold_after_two = state.cold_bytes();
        println!(
            "cold-evict: 两段后 sealed={sealed_after_two} cold_bytes={cold_after_two} first_file={:?}",
            first_segment_path
        );
        assert_eq!(cold_after_two, 16 * 1024 * 1024, "两段应恰好 16MiB");
        let first_path = first_segment_path;
        if let Some(path) = first_path.as_ref() {
            assert!(path.exists(), "未超预算时不许删任何段文件");
        }

        // 第三段 → 超预算 → 丢最久远的一段（起点最小的封存段）。
        for step in 0..8u64 {
            let offset = 16 * 1024 * 1024 + step * 1024 * 1024;
            state.ingest_evicted(offset, &body);
        }
        let cold_after_evict = state.cold_bytes();
        println!(
            "cold-evict: 三段后 cold_bytes={cold_after_evict}（预算 16MiB），最旧段文件还在吗={:?}",
            first_path.as_ref().map(|path| path.exists())
        );
        assert!(
            cold_after_evict <= 16 * 1024 * 1024,
            "冷区字节 {} 越过磁盘预算",
            cold_after_evict
        );
        assert!(
            !state.covers(0, 4096),
            "最久远的一段必须已被淘汰（[0, 8MiB) 不再命中）"
        );
        assert!(
            state.covers(16 * 1024 * 1024, 4096),
            "最近回看窗口必须保留（[16MiB, ...) 仍命中）"
        );
        if let Some(path) = first_path.as_ref() {
            assert!(
                !path.exists(),
                "淘汰必须**真的删除段文件**，不能留残渣：{}",
                path.display()
            );
        }
        // 关窗收尾：整个冷区文件也一并删除。
        let live = state
            .inner
            .lock()
            .unwrap()
            .sealed
            .iter()
            .find_map(|segment| match &segment.store {
                PreloadStore::Disk(disk) => Some(disk.file.path.clone()),
                PreloadStore::Memory(_) => None,
            });
        drop(state);
        if let Some(live) = live {
            assert!(!live.exists(), "冷区释放后不得留下文件：{}", live.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_only_cold_tier_serves_hits_without_touching_the_disk() {
        // **回退开关**（`ERIKA_PRELOAD_DISK=0` / 旧名 `ERIKA_PRELOAD_TO_DISK=0`）：
        // 关掉落盘后冷区走**纯内存**，分层与"命中即零网络"的语义完全不变；
        // 预算同时被钳到 `WINDOW_COLD_MEMORY_ONLY_MAX`（32MiB），
        // **不会**因为关掉落盘就把 256MiB 搬进内存。
        assert!(
            window_cold_budget(false) <= WINDOW_COLD_MEMORY_ONLY_MAX,
            "纯内存冷区预算必须被钳住"
        );
        assert!(
            window_cold_budget(true) >= window_cold_budget(false),
            "落盘开关只应放宽（不是收紧）冷区预算"
        );
        // 无 `dir` 的状态 = 段一律内存（等价于关掉落盘）。
        let state = FullPreloadState::new();
        state.learn_total(64 * 1024 * 1024);
        state.cold_budget.store(16 * 1024 * 1024, Ordering::Relaxed);
        let chunk = vec![3u8; 1024 * 1024];
        for step in 0..24u64 {
            state.ingest_evicted(step * 1024 * 1024, &chunk);
        }
        let disk_segments = state
            .inner
            .lock()
            .unwrap()
            .sealed
            .iter()
            .filter(|segment| matches!(segment.store, PreloadStore::Disk(_)))
            .count();
        println!(
            "memory-only: cold_bytes={} 预算={} 落盘段数={disk_segments}",
            state.cold_bytes(),
            state.cold_budget_bytes()
        );
        assert_eq!(disk_segments, 0, "纯内存冷区不得开任何段文件");
        assert!(
            state.cold_bytes() <= 16 * 1024 * 1024,
            "纯内存冷区同样受预算约束"
        );
        assert!(state.covers(23 * 1024 * 1024, 4096), "最近的区间必须可命中");
        assert_eq!(
            state.read_cached(23 * 1024 * 1024, 4096).unwrap(),
            vec![3u8; 4096]
        );
        assert!(state.cold_hit_count() >= 1, "命中必须被计数");
        assert!(!state.covers(0, 4096), "最久远的段必须已被淘汰");
    }

    #[test]
    fn weak_network_widens_the_forward_window_without_breaking_the_memory_cap() {
        // **追加需求（弱网更激进）**：预算不能只按字节卡——那样网速越差、
        // 折算出的可播秒数越短，恰好是反的。这里断言：
        //   ① 吞吐越低 → 前向**可播秒数**越长（弱网堆更多时间垫）；
        //   ② 任何情况下 前向 + 后向 ≤ 内存硬上限（字节守内存）。
        // 参照行为：官方网页端在弱网下"会直接缓冲后面好大一段"（同类客户端
        // issue cotaku/Bibi-Release#344 的对照结论），本公式的目标是达到并
        // 超过它的量级。
        const READ_AHEAD: u64 = 4 * 1024 * 1024;
        const BACK: u64 = 4 * 1024 * 1024;
        const BITRATE: f64 = 100_000.0; // 360P 级：100KB/s

        let fast = window_budget(0, READ_AHEAD, BACK, BITRATE, Some(1_000_000.0)); // 链路 10x
        let normal = window_budget(0, READ_AHEAD, BACK, BITRATE, Some(100_000.0)); // 刚好够播
        let weak = window_budget(0, READ_AHEAD, BACK, BITRATE, Some(25_000.0)); // 四分之一速
        let terrible = window_budget(0, READ_AHEAD, BACK, BITRATE, Some(5_000.0)); // 1/20 速

        for (name, budget) in [
            ("fast", fast),
            ("normal", normal),
            ("weak", weak),
            ("terrible", terrible),
        ] {
            let secs = budget.forward_bytes as f64 / BITRATE;
            println!(
                "{name}: 前向预算 {} 字节（≈{secs:.0}s 可播），后向 {} 字节，内存上限 {} 字节",
                budget.forward_bytes, budget.back_bytes, budget.memory_bytes
            );
            assert!(
                budget.forward_bytes + budget.back_bytes <= budget.memory_bytes,
                "{name}: 前向+后向越内存硬上限"
            );
        }

        // ① 弱网更激进：可播秒数单调上升。
        assert!(
            normal.forward_secs >= 30.0,
            "充裕/正常链路的前向时间预算不应低于 30s（网页端量级）"
        );
        assert!(
            weak.forward_secs > normal.forward_secs,
            "弱网（1/4 速）的前向时间预算必须更长：{} vs {}",
            weak.forward_secs,
            normal.forward_secs
        );
        assert!(
            terrible.forward_secs > weak.forward_secs,
            "更弱网（1/20 速）必须继续放宽：{} vs {}",
            terrible.forward_secs,
            weak.forward_secs
        );
        assert!(
            terrible.forward_secs <= WINDOW_FORWARD_SECS_MAX,
            "弱网放开必须有上限（内存/磁盘成本兜底）"
        );
        // ② 字节守内存：极弱网也不会把内存撑爆。
        assert!(
            terrible.forward_bytes + terrible.back_bytes <= terrible.memory_bytes,
            "极弱网下仍在内存硬上限内"
        );

        // ③ **吞吐未知 ≠ 充裕**：拿不到吞吐时回退到保守固定秒数（20s），
        // 既不是"不知道就多缓"（`weak` 的 80s），也不是"少缓"（`fast` 的 30s 之下）。
        let unknown = window_budget(0, READ_AHEAD, BACK, BITRATE, None);
        println!(
            "unknown: 吞吐未知 → 前向 {:.0}s / {} 字节（保守回退 {:.0}s）",
            unknown.forward_secs, unknown.forward_bytes, WINDOW_FORWARD_SECS_CONSERVATIVE
        );
        assert_eq!(
            unknown.forward_secs, WINDOW_FORWARD_SECS_CONSERVATIVE,
            "吞吐未知必须回退到保守固定秒数"
        );
        assert!(
            unknown.forward_secs < weak.forward_secs,
            "未知不得被当成弱网（那会凭空多缓）"
        );
        assert_eq!(unknown.throughput_bps, None, "未知语义必须如实保留");
        assert!(unknown.forward_bytes + unknown.back_bytes <= unknown.memory_bytes);
        // 码率也未知（开播最初 ~2s）→ 退回配置值，与改造前行为一致。
        let cold_start = window_budget(0, READ_AHEAD, BACK, 0.0, None);
        assert_eq!(cold_start.forward_bytes, READ_AHEAD, "冷启动前向 = 配置值");
        assert_eq!(cold_start.back_bytes, BACK, "冷启动后向 = 配置值");
        assert_eq!(cold_start.forward_secs, 0.0, "算不出秒数就如实报 0");

        // 运行时那一半：窗口容量被预算调小时，已缓冲字节**立即**被裁到上限内
        // （不是"下次消费才生效"）。
        let shared = Arc::new(StreamShared {
            buf: Mutex::new(StreamBuffer::new(0, 4 * 1024 * 1024, 4 * 1024 * 1024)),
            data_cv: Condvar::new(),
        });
        {
            let mut guard = lock_stream(&shared);
            guard.push(&vec![7u8; 6 * 1024 * 1024]);
            guard.consume_to(5 * 1024 * 1024);
            // consume_to 已经按后向预算（4MB）裁掉 1MB：常驻 = 5MB。
            assert_eq!(guard.resident_bytes(), 5 * 1024 * 1024, "裁剪前应有 5MB");
            guard.set_caps(1024 * 1024, 1024 * 1024);
            let resident = guard.resident_bytes();
            println!("weak: 预算收紧后常驻 {resident} 字节（上限 2MB）");
            assert!(
                resident <= 2 * 1024 * 1024,
                "预算收紧后常驻 {} 字节越过上限 2MB",
                resident
            );
            // 前向内容必须完好（裁剪只动后向历史，不许伤到待播字节）。
            assert_eq!(guard.front_len(), 1_048_576, "前向深度应保持 1MB");
        }
    }

    #[test]
    fn whole_playback_with_layered_cache_stays_at_one_request_and_bounded_memory() {
        // 「整片播放总请求数」的回归标尺（用户实测同一条：328MB 一次播放）。
        // 旧模型 ≈1310 个请求，官方 ≈79，上一轮改造（长连接 + 整片桶流式）84。
        // 本次（滑动窗口 + 分层缓存）的答案：**1 个请求**，与文件大小无关；
        // 内存被预算钉住、磁盘被冷区预算钉住，回拖仍零网络。
        const TOTAL: u64 = 328 * 1024 * 1024;
        let dir = preload_test_dir("whole-playback");
        let (uri, requests, _heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let (mut source, state) = streaming_source_with_cold_tier(uri, &dir);
        // 冷区预算压到 32MiB：既让淘汰路径真的跑起来，也不让测试写掉几百 MB 盘。
        state.cold_budget.store(32 * 1024 * 1024, Ordering::Relaxed);

        read_forward(&mut source, 0, TOTAL);
        let total_requests = requests.load(Ordering::Relaxed);
        let snapshot = source.diag_snapshot();
        println!(
            "whole-playback: {TOTAL} 字节 → {total_requests} 个请求；内存 {} 字节（上限 {}），磁盘 {} 字节（预算 {}）",
            snapshot.cache_memory_bytes,
            snapshot.memory_budget_bytes,
            snapshot.cache_disk_bytes,
            state.cold_budget_bytes()
        );
        assert_eq!(total_requests, 1, "整片播放必须只有 1 个请求");
        assert!(
            snapshot.cache_memory_bytes <= snapshot.memory_budget_bytes,
            "内存 {} 越过硬上限 {}",
            snapshot.cache_memory_bytes,
            snapshot.memory_budget_bytes
        );
        assert!(
            snapshot.cache_disk_bytes <= state.cold_budget_bytes(),
            "磁盘 {} 越过冷区预算 {}",
            snapshot.cache_disk_bytes,
            state.cold_budget_bytes()
        );
        assert!(
            state.cold_bytes() > 8 * 1024 * 1024,
            "整片播放应当把大量历史区间沉到冷区，实际 {}",
            state.cold_bytes()
        );

        // 回拖到**冷区确实覆盖**的位置（不猜：问状态自己哪个偏移真的在盘上）。
        let mut target = None;
        let mut probe = TOTAL - 8 * 1024 * 1024;
        while probe > 0 {
            if state.covers(probe, 32 * 1024) {
                target = Some(probe);
                break;
            }
            probe -= 1024 * 1024;
        }
        let target = target.expect("整片播放后冷区应当仍覆盖相当一段历史区间");
        source.prefetch_hint(target);
        let back = source
            .read_range(ByteRange {
                start: target,
                length: Some(32 * 1024),
            })
            .unwrap();
        let after_rewind = requests.load(Ordering::Relaxed);
        println!(
            "whole-playback: 回拖到 {target}（距尾部 {} 字节）→ 请求 {after_rewind} 个（必须仍为 {total_requests}），冷区命中 {} 次",
            TOTAL - target,
            state.cold_hit_count()
        );
        assert_eq!(after_rewind, total_requests, "回拖到已落盘区间必须零新请求");
        for (index, byte) in back.iter().enumerate() {
            assert_eq!(
                *byte,
                ((target as usize + index) % 256) as u8,
                "冷区读回字节错位"
            );
        }
        drop(source);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sequential_stream_never_rebuilds_the_connection_across_a_whole_playback() {
        // **本次追加的头号验收指标**：顺序播放期间**不该重建连接**。
        //
        // 真机反例（用户 631 秒 / 176MB 视频的实测，`[perf][CLIENT][1s]` 行）：
        //   `HTTP v=387次(均2002KB,≈1.0/s)` → `v=398次(均1972KB,≈1.0/s)`
        //   边际：21 秒 +11 个请求、每请求只传 ≈917KB（≈0.52 次/秒）——
        //   而窗口基本是满的（29.4MB/32MB），**不是**背压导致的。
        // 本测试断言"内核自己绝不重建"：只要服务端不主动收线，8 分钟连续播放
        // 必须只有 1 个请求、重建计数全 0（尤其是 `reset` 必须为 0）。
        // 真机那种"每 ~0.9MB 重建一次"由下面两条服务端行为测试分别复现与归因。
        const PLAYBACK_BYTES: u64 = 200 * 1024 * 1024; // ≈8 分钟 1080P
        let (uri, requests, _heads) = spawn_streaming_mock_http_server(PLAYBACK_BYTES, None);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        read_forward(&mut source, 0, PLAYBACK_BYTES);
        let snapshot = source.diag_snapshot();
        println!(
            "no-rebuild: {PLAYBACK_BYTES} 字节 → 请求 {} 个，重建 total={} (early_close={} empty={} reset={} host_switch={} chunked={})",
            requests.load(Ordering::Relaxed),
            snapshot.stream_reconnects_total,
            snapshot.stream_reconnects_early_close,
            snapshot.stream_reconnects_empty,
            snapshot.stream_reconnects_reset,
            snapshot.stream_reconnects_host_switch,
            snapshot.stream_chunk_resumes
        );
        assert_eq!(
            requests.load(Ordering::Relaxed),
            1,
            "顺序播放必须一条连接读到底"
        );
        assert_eq!(snapshot.stream_reconnects_total, 0, "内核自己不得重建连接");
        assert_eq!(
            snapshot.stream_reconnects_reset, 0,
            "顺序播放期间不得有 reset"
        );
    }

    #[test]
    fn cdn_cutting_every_megabyte_is_resumed_transparently_and_attributed_to_the_cdn() {
        // 复现真机形态：服务端**声明**完整范围，却每 1MB 收线一次。
        // 断言 ① 字节一个不丢、顺序正确；② 重建全部归因于 `early_close`（CDN），
        // `reset` 必须为 0（不是我们的锅）；③ 请求数 = 分片数（这是请求数下界）。
        const TOTAL: u64 = 8 * 1024 * 1024;
        const CUT: u64 = 1024 * 1024;
        let (uri, requests, heads) = spawn_cutting_mock_http_server(TOTAL, CUT);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        read_forward(&mut source, 0, TOTAL);
        let snapshot = source.diag_snapshot();
        let expected = TOTAL.div_ceil(CUT);
        println!(
            "cdn-cut: {TOTAL} 字节 / 每 {CUT} 字节收线 → 请求 {} 个（预期 {expected}），early_close={} reset={} total={}",
            requests.load(Ordering::Relaxed),
            snapshot.stream_reconnects_early_close,
            snapshot.stream_reconnects_reset,
            snapshot.stream_reconnects_total
        );
        assert_eq!(
            requests.load(Ordering::Relaxed),
            expected,
            "服务端每收线一次就必然多一个请求：请求数 = 分片数（这是下界，不是我们的 bug）"
        );
        // 恒等式（宿主可直接用它核算请求来源）：**请求数 = 1 + 重建次数**。
        // 首条连接不算"重建"，所以这里重建数 = 分片数 - 1。
        assert_eq!(
            snapshot.stream_reconnects_early_close,
            expected - 1,
            "重建必须全部归因于服务端提前收线"
        );
        assert_eq!(
            snapshot.stream_reconnects_total,
            expected - 1,
            "请求数 = 1 + 重建次数"
        );
        assert_eq!(
            snapshot.stream_reconnects_reset, 0,
            "不得有任何本地 reset（否则是我们的 bug）"
        );
        // 续传必须从断点精确接上：请求头的 Range 起点逐片递增。
        let mut starts = Vec::new();
        for _ in 0..expected {
            let head = recv_request_head(&heads);
            let value = range_header_of(&head);
            let start: u64 = value
                .trim_start_matches("range: bytes=")
                .trim_end_matches('-')
                .parse()
                .unwrap_or_else(|_| panic!("请求头无法解析: {head}"));
            starts.push(start);
        }
        for (index, start) in starts.iter().enumerate() {
            assert_eq!(
                *start,
                index as u64 * CUT,
                "第 {index} 片必须从断点续传（字节不丢、不重下）"
            );
        }
    }

    #[test]
    fn bounded_content_range_chunks_are_resumed_and_not_mistaken_for_eof() {
        // 分片式 CDN（`Content-Range` 的 end+1 < total）**不是**文件尾：
        // 旧实现在分片末尾直接 `eof = true` → 流永远停在半途（真机音频
        // 2.7MB/8MB 停滞的形态）。现在必须续传，且整片字节一个不丢。
        const TOTAL: u64 = 8 * 1024 * 1024;
        const CHUNK: u64 = 1024 * 1024;
        let (uri, requests, _heads) = spawn_chunked_mock_http_server(TOTAL, CHUNK);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        read_forward(&mut source, 0, TOTAL);
        let snapshot = source.diag_snapshot();
        let expected = TOTAL.div_ceil(CHUNK);
        println!(
            "chunked: {TOTAL} 字节 / 每片 {CHUNK} → 请求 {} 个（预期 {expected}），chunk_resumes={} reset={} eof 前沿={}",
            requests.load(Ordering::Relaxed),
            snapshot.stream_chunk_resumes,
            snapshot.stream_reconnects_reset,
            source.stream_window().map(|(_, fill, _)| fill).unwrap_or(0)
        );
        assert_eq!(requests.load(Ordering::Relaxed), expected, "每片一个请求");
        assert!(
            snapshot.stream_chunk_resumes >= expected - 1,
            "分片续传必须被计入 chunk_resumes，实际 {}",
            snapshot.stream_chunk_resumes
        );
        assert_eq!(snapshot.stream_reconnects_reset, 0);
        // 读到整片末尾：前沿必须到 TOTAL（没有被分片尾误判成 EOF）。
        let (_, fill, total) = source.stream_window().unwrap();
        assert_eq!(total, Some(TOTAL));
        assert_eq!(fill, TOTAL, "分片尾不得被当成文件尾（旧实现会停在 1MB）");
    }

    #[test]
    fn range_journal_classifies_resume_versus_seek() {
        // **判决实验（假说 D）的仪器自检**：请求起点序列必须能把
        // "断点续传"（服务端/传输侧断了我们）与"demux 跳读"（与 CDN 无关）
        // 分开。判据：Contiguous = 断点续；Backward/ForwardJump = 在 seek。
        // 总长必须显著大于后向窗口，否则"回拖到窗口之外"这个前提会失效：
        // 后向窗口默认值 4MB → 16MB 后，原来"前进 9MB 再回拖到 0"已落在窗口内，
        // backward_jump 不再被计数（实测 backward=0）。
        const TOTAL: u64 = HttpRangeSource::DEFAULT_BACK_BUFFER_BYTES * 4;
        const FORWARD: u64 = HttpRangeSource::DEFAULT_BACK_BUFFER_BYTES * 2;
        let (uri, requests, _heads) = spawn_cutting_mock_http_server(TOTAL, 1024 * 1024);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        read_forward(&mut source, 0, FORWARD);
        let snapshot = source.diag_snapshot();
        println!(
            "journal: contiguous={} forward={} backward={} last_start={} last_conn={}KB max_conn={}KB requests={}",
            snapshot.stream_range_contiguous,
            snapshot.stream_range_forward_jumps,
            snapshot.stream_range_backward_jumps,
            snapshot.stream_range_last_start,
            snapshot.stream_last_connection_bytes / 1024,
            snapshot.stream_max_connection_bytes / 1024,
            requests.load(Ordering::Relaxed),
        );
        assert!(
            snapshot.stream_range_contiguous >= 1,
            "每 1MB 被收线后从断点续 → 必须计为 contiguous，实际 {}",
            snapshot.stream_range_contiguous
        );
        assert_eq!(
            snapshot.stream_range_backward_jumps, 0,
            "顺序前进播放期间不该出现回拖（backward_jump）"
        );
        // 真回拖（seek 到窗口之外）→ 必须被识别为 backward_jump：
        // 这才是"请求放大可能来自 demux 而非 CDN"的那一类。
        source.prefetch_hint(0);
        let _ = source
            .read_range(ByteRange {
                start: 0,
                length: Some(32 * 1024),
            })
            .unwrap();
        thread::sleep(Duration::from_millis(200));
        let after = source.diag_snapshot();
        println!(
            "journal: 回拖后 backward={} forward={} contiguous={}",
            after.stream_range_backward_jumps,
            after.stream_range_forward_jumps,
            after.stream_range_contiguous
        );
        assert!(
            after.stream_range_backward_jumps >= 1,
            "回拖到窗口外必须被计为 backward_jump（判决实验的分界）"
        );
    }

    #[test]
    fn a_dead_connection_recovers_long_before_a_deep_buffer_drains() {
        // **本轮核心断言（真机 vstall=16→21s 却 Hold 的修复）**：缓冲很深时，
        // 连接真死必须**早于**缓冲耗尽就触发恢复。
        //
        // 真机反例：`buf=57.7s vstall=16s→21s` → 判据一直 Hold，要等第 56 秒
        // 才动作，用户先看到卡住。现在两条恢复通路都与缓冲深度解耦：
        //   ① 连接真死：6s 无字节（`MULTI_HOST_STALL_MS`）就走；
        //   ② 供给跟不上：前向可播 < 20s 恢复余量并持续 3s 就走。
        // 本测试构造"缓冲还剩几十秒 + 连接真死"，断言在**剩余可播时间仍远超
        // 恢复余量**时就换了源（即触发时刻早于耗尽，且留足恢复时间）。
        const TOTAL: u64 = 256 * 1024 * 1024;
        let (uri_a, _requests_a, _heads_a) = spawn_streaming_mock_http_server(TOTAL, None);
        let (uri_b, _requests_b, _heads_b) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri_a.clone()).enable_sequential_stream();
        source.set_candidates(vec![(uri_a.clone(), 1.0), (uri_b.clone(), 0.5)]);
        // 深缓冲：8MB 前向 + 低码率（0.1MB/s）→ 满窗 ≈ 80 秒可播。
        source.read_ahead_bytes = 8 * 1024 * 1024;
        source.back_bytes = 4 * 1024 * 1024;
        source.consume_bps = 100_000.0;
        read_forward(&mut source, 0, 256 * 1024);
        thread::sleep(Duration::from_millis(400));
        let before = source.stream_health();
        println!(
            "recover-early: 触发前 前向可播 {:.1}s 窗口满={}",
            before.forward_secs, before.window_full
        );
        assert!(
            before.forward_secs > MULTI_HOST_RECOVERY_MARGIN_SECS * 1.5,
            "前置条件：缓冲必须远深于恢复余量（实际 {:.1}s）",
            before.forward_secs
        );
        // 连接真死：读线程再没收到字节（顺序长读的到达时间戳做旧），但 demux
        // 仍在读（`read_in_flight`/活跃时间戳都新鲜）。
        source
            .stream
            .as_ref()
            .expect("长读已起")
            .last_arrival_ms
            .store(
                now_ms().saturating_sub(MULTI_HOST_STALL_MS + 500),
                Ordering::Relaxed,
            );
        source.last_byte_ms = now_ms().saturating_sub(MULTI_HOST_STALL_MS + 500);
        source.last_read_call_ms = now_ms();
        source.read_in_flight = true; // demux 正挂在读上（真机形态）
        // ① **窗口满 + 连接真死 → 不换源**：窗口满是"供给充足"的正面证据，而
        //    连接死掉由读线程自己从 `write_pos` 重连兜底（+1 请求，字节不丢），
        //    换源判据在这里必须按住不动（防抖，上一轮的核心修复）。
        let full_health = source.stream_health();
        assert!(full_health.window_full, "前置条件：窗口必须已满");
        source.maybe_switch_host();
        println!(
            "recover-early: 满窗+真死 → host_epoch={}（必须 0，由读线程重连兜底）",
            source.host_epoch
        );
        assert_eq!(
            source.host_epoch, 0,
            "窗口满时不得换源（真死由读线程重连兜底）"
        );
        // ② **链路还活着但供给跟不上**（真机 `ratio=0.68/0.33` 的形态：字节仍在
        //    到达，只是比消费慢）→ 前向掉到恢复余量附近并持续 3s 必须换源，
        //    且触发时前向仍剩约一个恢复余量（**早于缓冲耗尽**）。
        //    （这里刻意保持 `last_arrival_ms` 新鲜，隔离出"余量线"这条判据；
        //      路径①的"6 秒无字节"是另一条更快的通路，不受 3s 迟滞约束。）
        {
            let stream = source.stream.as_ref().expect("长读已起");
            let shared = Arc::clone(&stream.shared);
            let mut guard = lock_stream(&shared);
            let write_pos = guard.write_pos();
            // 前向压到 ~15s（码率 0.1MB/s → 1.5MB），即低于 20s 恢复余量。
            let keep = (1.5 * 1024.0 * 1024.0) as u64;
            guard.consume_to(write_pos.saturating_sub(keep));
        }
        source.last_read_call_ms = now_ms();
        source.read_in_flight = true;
        source
            .stream
            .as_ref()
            .expect("长读已起")
            .last_arrival_ms
            .store(now_ms(), Ordering::Relaxed);
        source.maybe_switch_host(); // 只开始计时（迟滞）
        assert_eq!(source.host_epoch, 0, "刚判饥饿不该立刻换（需要持续 3s）");
        source.starve_since_ms = now_ms().saturating_sub(MULTI_HOST_STARVE_PERSIST_MS + 200);
        source.last_read_call_ms = now_ms();
        source.read_in_flight = true;
        source
            .stream
            .as_ref()
            .expect("长读已起")
            .last_arrival_ms
            .store(now_ms(), Ordering::Relaxed);
        source.maybe_switch_host();
        let after = source.stream_health();
        println!(
            "recover-early: 前向掉到余量线 → host_epoch={} 原因={} 触发时前向仍剩 {:.1}s（缓冲总深 {:.1}s）",
            source.host_epoch, source.last_switch_reason, after.forward_secs, before.forward_secs
        );
        assert_eq!(
            source.host_epoch, 1,
            "供给跟不上必须触发恢复（不得 Hold 到耗尽）"
        );
        assert!(
            source.last_switch_reason.contains("starving"),
            "原因必须可读且指向饥饿：{}",
            source.last_switch_reason
        );
        assert!(
            after.forward_secs < MULTI_HOST_RECOVERY_MARGIN_SECS + 1.0,
            "触发点应当在恢复余量附近（实际 {:.1}s）",
            after.forward_secs
        );
        assert!(
            after.forward_secs > 0.0,
            "必须**早于**缓冲耗尽触发：触发时前向仍剩 {:.1}s",
            after.forward_secs
        );
    }

    #[test]
    fn a_paused_source_never_accumulates_starvation() {
        // **暂停态必须显式排除**：暂停时 demux 不读、字节自然不来——若把它算成
        // 饥饿，用户一暂停就会被换源（真机日志最后一行就是 `state=Paused`）。
        const TOTAL: u64 = 64 * 1024 * 1024;
        let (uri_a, _requests_a, _heads_a) = spawn_streaming_mock_http_server(TOTAL, None);
        let (uri_b, _requests_b, _heads_b) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri_a.clone()).enable_sequential_stream();
        source.set_candidates(vec![(uri_a.clone(), 1.0), (uri_b.clone(), 0.5)]);
        source.read_ahead_bytes = 512 * 1024;
        source.back_bytes = 256 * 1024;
        source.consume_bps = 4_000_000.0; // 大码率：前向秒数会很小（更容易误判饥饿）
        read_forward(&mut source, 0, 64 * 1024);
        thread::sleep(Duration::from_millis(300));
        // 进入"暂停"：没有读调用在飞、活跃时间戳过期、字节很长时间没来。
        source.read_in_flight = false;
        source.last_read_call_ms = now_ms().saturating_sub(MULTI_HOST_ACTIVE_MS + 5_000);
        source.last_byte_ms = now_ms().saturating_sub(60_000);
        source
            .stream
            .as_ref()
            .expect("长读已起")
            .last_arrival_ms
            .store(now_ms().saturating_sub(60_000), Ordering::Relaxed);
        // 把消费前沿推到下载前沿：前向可播 = 0（若判据不看"在播"，这里必触发）。
        {
            let stream = source.stream.as_ref().expect("长读已起");
            let shared = Arc::clone(&stream.shared);
            let mut guard = lock_stream(&shared);
            let write_pos = guard.write_pos();
            guard.consume_to(write_pos);
        }
        for _ in 0..10 {
            source.maybe_switch_host();
            thread::sleep(Duration::from_millis(20));
        }
        println!(
            "paused: host_epoch={} starve_since={} 原因='{}'",
            source.host_epoch, source.starve_since_ms, source.last_switch_reason
        );
        assert_eq!(source.host_epoch, 0, "暂停态绝不能换源");
        assert_eq!(source.starve_since_ms, 0, "暂停态不得累计饥饿计时");
    }

    #[test]
    fn a_full_window_with_low_throughput_never_triggers_a_host_switch() {
        // **真机"87 秒换源 14 次"的核心修复断言**：窗口满 = 供给充足的正面证据。
        //
        // 旧判据用 `est_bps < 25KB/s` 判"慢"。但顺序长读的**背压语义**是"窗口满就
        // 几乎不读 socket"，此时到达速率必然收敛到**消费速率** → 健康的满窗口被
        // 判成慢链路 → 切 host → 新连接重新加速（rate shapers throttle by connection
        // age）→ 窗口又满 → 再判慢 → 无限抖动。
        // 本测试构造"窗口满 + 吞吐极低 + demux 一直在读"，断言**一次都不换**。
        const TOTAL: u64 = 64 * 1024 * 1024;
        let (uri_a, _requests_a, _heads_a) = spawn_streaming_mock_http_server(TOTAL, None);
        let (uri_b, _requests_b, _heads_b) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri_a.clone()).enable_sequential_stream();
        source.set_candidates(vec![(uri_a.clone(), 1.0), (uri_b.clone(), 0.5)]);
        // 小预算：读一点就把窗口填满（本地 mock 很快）。
        source.read_ahead_bytes = 512 * 1024;
        source.back_bytes = 256 * 1024;
        read_forward(&mut source, 0, 64 * 1024);
        thread::sleep(Duration::from_millis(500));

        // 制造旧判据必然误判的现场：吞吐很低（5KB/s < 25KB/s 阈值）。
        source.est_bps = 5_000.0;
        // 连做 40 轮"播放中"的换源判定（旧实现在第 6 秒就会开始换、每 4 秒一次）。
        let mut switches = 0;
        for _ in 0..40 {
            source.last_read_call_ms = now_ms();
            let before = source.host_epoch;
            source.maybe_switch_host();
            if source.host_epoch != before {
                switches += 1;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let health = source.stream_health();
        println!(
            "full-window: 前向可播 {:.1}s 窗口满={} est={:.0}KB/s → 换源 {switches} 次（必须 0）",
            health.forward_secs,
            health.window_full,
            source.est_bps / 1024.0
        );
        assert!(
            health.window_full,
            "前置条件：窗口必须已满（否则本测试无意义）"
        );
        assert_eq!(switches, 0, "窗口满时绝不能换源（否则就是抖动循环）");
        assert_eq!(source.host_epoch, 0);
    }

    /// **整片已缓冲完 → "没有字节到达"不是死连证据**（真机事故 2026-09-15）。
    ///
    /// 现场：19MB 的片子已经缓冲到 18.9/19MB，`vstall` 却涨到 596 秒——整片下完
    /// 之后网络前沿不再前进，"距上次字节"自然一路涨。只要 demux 还有一次读活动
    /// （`consuming` 为真），旧判据就会拿"下完了"当死连证据去换源，并给健康
    /// host 记失败分污染下次排序。
    ///
    /// **关键前提**：这里必须用"整片比 `read_ahead` 还小"的片子——真机就是这样
    /// （宿主配 32MiB，片子 19MB）。此时 `window_full` 恒为 false，那道守卫等于
    /// 不存在，只有 `source_complete` 能拦住它。
    #[test]
    fn a_fully_cached_source_does_not_look_like_a_dead_host() {
        const TOTAL: u64 = 2 * 1024 * 1024;
        let (uri_a, _ra, _ha) = spawn_streaming_mock_http_server(TOTAL, None);
        let (uri_b, _rb, _hb) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri_a.clone()).enable_sequential_stream();
        source.set_candidates(vec![(uri_a.clone(), 1.0), (uri_b.clone(), 0.5)]);
        // read_ahead > 整片：窗口永远填不满（真机形态；窗满的下界是 32MiB）。
        source.read_ahead_bytes = 8 * 1024 * 1024;
        source.back_bytes = 256 * 1024;
        // 完整读一遍：网络前沿应当走到服务端声明的 total。
        read_forward(&mut source, 0, TOTAL);
        let mut complete = false;
        for _ in 0..100 {
            if source.stream_health().source_complete {
                complete = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(complete, "前置条件：整片必须已缓冲完（source_complete）");
        assert!(
            !source.stream_health().window_full,
            "前置条件：整片比 read_ahead 小 → window_full 必须为 false（真机形态）"
        );
        // 真机现场：距上次字节远超 6s 死连阈值；两条"最后字节"来源都推旧
        //（判据取两者的较新者，所以要一起推）。
        let stale = now_ms().saturating_sub(MULTI_HOST_STALL_MS * 50);
        source.last_byte_ms = stale;
        if let Some(stream) = source.stream.as_ref() {
            stream.last_arrival_ms.store(stale, Ordering::Relaxed);
        }
        source.est_bps = 0.0;
        // 码率取大 → 前向可播秒数贴近 0，把"饥饿"判据也压成成立（对照组）。
        source.consume_bps = 4_000_000.0;
        let mut switches = 0;
        for _ in 0..40 {
            // demux 仍在读（真机上 demux 从窗口里取数据同样走这条路径）。
            source.last_read_call_ms = now_ms();
            // 饥饿迟滞已过（否则第一次判定只是开始计时，测不出东西）。
            source.starve_since_ms = now_ms().saturating_sub(MULTI_HOST_STARVE_PERSIST_MS + 200);
            let before = source.host_epoch;
            source.maybe_switch_host();
            if source.host_epoch != before {
                switches += 1;
            }
        }
        let health = source.stream_health();
        println!(
            "full-cache: 前向 {:.1}s 窗口满={} 已完整={} → 换源 {switches} 次（必须 0）",
            health.forward_secs, health.window_full, health.source_complete
        );
        assert_eq!(
            switches, 0,
            "整片已缓冲完时绝不能换源：那是「不需要」，不是「来不了」"
        );
        assert_eq!(source.host_epoch, 0);
    }

    #[test]
    fn only_real_starvation_triggers_a_host_switch() {
        // 换源的**唯一**正当理由：这条链路正在饿死我们——前向缓冲见底
        // （< 2s 可播）并持续 ≥3s，或长时间没有字节到达。
        const TOTAL: u64 = 64 * 1024 * 1024;
        let (uri_a, _requests_a, _heads_a) = spawn_streaming_mock_http_server(TOTAL, None);
        let (uri_b, _requests_b, _heads_b) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri_a.clone()).enable_sequential_stream();
        source.set_candidates(vec![(uri_a.clone(), 1.0), (uri_b.clone(), 0.5)]);
        source.read_ahead_bytes = 8 * 1024 * 1024;
        source.back_bytes = 512 * 1024;
        read_forward(&mut source, 0, 64 * 1024);
        thread::sleep(Duration::from_millis(200));
        // 构造真饥饿：把消费前沿推到下载前沿（前向 = 0 秒），并让码率估计很大
        // 使"可播秒数"确实见底；同时保持"demux 在读"。
        {
            let stream = source.stream.as_ref().expect("长读已起");
            let shared = Arc::clone(&stream.shared);
            let mut guard = lock_stream(&shared);
            let write_pos = guard.write_pos();
            guard.consume_to(write_pos);
        }
        source.consume_bps = 4_000_000.0; // 4MB/s：0 前向 → 0 秒可播
        source.last_read_call_ms = now_ms();
        // 第一次判定只**开始计时**（迟滞），不该立刻换。
        source.maybe_switch_host();
        assert_eq!(source.host_epoch, 0, "刚判饥饿不该立刻换（需要持续 3s）");
        // 让"饥饿起点"过去 3.2 秒（比真机等待便宜：直接做旧计时起点）。
        source.starve_since_ms = now_ms().saturating_sub(MULTI_HOST_STARVE_PERSIST_MS + 200);
        source.last_read_call_ms = now_ms();
        source.maybe_switch_host();
        println!(
            "starving: host_epoch={} 原因={}",
            source.host_epoch, source.last_switch_reason
        );
        assert_eq!(source.host_epoch, 1, "持续饥饿必须换源");
        assert!(
            source.last_switch_reason.contains("starving"),
            "换源原因必须可读且说明是饥饿：{}",
            source.last_switch_reason
        );
    }

    #[test]
    fn switch_cooldown_and_rate_limit_cap_the_rotation() {
        // ③ 抖动抑制：即便判据一直成立，也必须 ① 冷却期内不换；
        // ② 5 分钟窗口内最多 `MULTI_HOST_MAX_SWITCHES` 次，之后停在当前 host。
        const TOTAL: u64 = 64 * 1024 * 1024;
        let mut candidates = Vec::new();
        for index in 0..3 {
            let (uri, _r, _h) = spawn_streaming_mock_http_server(TOTAL, None);
            candidates.push((uri, 1.0 - index as f64 * 0.1));
        }
        let mut source = HttpRangeSource::new(candidates[0].0.clone()).enable_sequential_stream();
        source.set_candidates(candidates.clone());
        source.read_ahead_bytes = 8 * 1024 * 1024;
        // 先把长流起起来（`maybe_switch_host` 的饥饿判据读窗口状态）。
        read_forward(&mut source, 0, 64 * 1024);
        thread::sleep(Duration::from_millis(150));
        source.consume_bps = 4_000_000.0;
        {
            let stream = source.stream.as_ref().expect("长读已起");
            let shared = Arc::clone(&stream.shared);
            let mut guard = lock_stream(&shared);
            let write_pos = guard.write_pos();
            guard.consume_to(write_pos);
        }
        // 模拟"持续劣化"：每轮都把饥饿计时做旧、把冷却清掉，只留限流闸生效。
        for _ in 0..12 {
            source.starve_since_ms = now_ms().saturating_sub(MULTI_HOST_STARVE_PERSIST_MS + 100);
            source.last_read_call_ms = now_ms();
            source.switch_cooldown_until = Instant::now();
            source.maybe_switch_host();
        }
        let switches = source.host_epoch;
        println!(
            "rotation-cap: 连续劣化 12 轮 → 实际换源 {switches} 次（上限 {MULTI_HOST_MAX_SWITCHES}）"
        );
        assert!(
            switches as usize <= MULTI_HOST_MAX_SWITCHES,
            "轮换必须被限流闸钳住，实际 {switches}"
        );
        // 冷却：立刻再判一次（不清冷却）→ 不得再切。
        let before = source.host_epoch;
        source.starve_since_ms = now_ms().saturating_sub(MULTI_HOST_STARVE_PERSIST_MS + 100);
        source.last_read_call_ms = now_ms();
        source.maybe_switch_host();
        assert_eq!(
            source.host_epoch, before,
            "冷却期内不得再换（复发退避 4s 起翻倍到 32s）"
        );
    }

    #[test]
    fn bilibili_sources_send_the_referer_anti_leech_header() {
        // 对外调研（一手）：B 站防盗链——带 UA 不带 Referer → 403。成熟客户端
        // 全部无条件带 `Referer: https://www.bilibili.com`。我们没带 → 现在补上。
        const TOTAL: u64 = 4 * 1024 * 1024;
        let (uri, _requests, _heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let bilibili_uri = uri.replace("127.0.0.1", "upos-sz-mirrorcos.bilivideo.com");
        // 真实连不了该域名，所以只验证**请求头装配**：用本地 uri 但手工构造
        // 与生产同样的 header 集合。
        let source = HttpRangeSource::with_http_headers_and_diag_slot(uri.clone(), Vec::new(), 0);
        source.enable_sequential_stream();
        // 直接检查 is_bilibili_media_host + 构造器补头逻辑。
        assert!(is_bilibili_media_host(&bilibili_uri));
        assert!(!is_bilibili_media_host(&uri));
        let with_referer =
            HttpRangeSource::with_http_headers_and_diag_slot(bilibili_uri.clone(), Vec::new(), 0);
        assert!(
            with_referer
                .http_headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("referer")
                    && value == "https://www.bilibili.com"),
            "B 站媒体源必须自动带上防盗链 Referer：{:?}",
            with_referer.http_headers
        );
        // 宿主自己给了 Referer 时不重复添加。
        let custom = HttpRangeSource::with_http_headers_and_diag_slot(
            bilibili_uri,
            vec![("Referer".to_string(), "https://example.test/".to_string())],
            0,
        );
        assert_eq!(
            custom
                .http_headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("referer"))
                .count(),
            1,
            "宿主已给 Referer 时不得重复添加"
        );
        // 本地源不该被塞 Referer。
        let local = HttpRangeSource::with_http_headers_and_diag_slot(uri, Vec::new(), 0);
        assert!(
            !local
                .http_headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("referer")),
            "非 B 站源不得被塞 Referer"
        );
        // 顺带：我们发的是**自己实现的 HTTP 层**（reqwest），不是 FFmpeg 的 HTTP——
        // 调研里"FFmpeg 默认发 Connection: close"那条**不适用**于我们。这里真读
        // 一次、抓真实请求头，断言里面**没有** `connection: close`。
        let (local_uri, _requests, heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut reader = HttpRangeSource::new(local_uri.clone()).enable_sequential_stream();
        read_forward(&mut reader, 0, 64 * 1024);
        let head = recv_request_head(&heads);
        println!("referer-head: {head}");
        assert!(
            !head.to_ascii_lowercase().contains("connection: close"),
            "我们不该主动发 Connection: close（那是 FFmpeg HTTP 层的固有行为）：{head}"
        );
        assert!(
            head.to_ascii_lowercase().contains("range: bytes=0-"),
            "请求形态仍是开放结尾长连接（本轮明确不改有界分片）：{head}"
        );
    }

    #[test]
    fn fill_throughput_is_measured_only_while_the_window_is_not_full() {
        // 任务三：宿主判"能不能升画质"要链路真实吞吐；窗口满时客户端故意不读、
        // 吞吐会被钳成消费速率。`fill_throughput_bps` 的口径就是"只在窗口未满
        // 期间采样"，并且宿主 500ms 轮询能读到新鲜值。
        const TOTAL: u64 = 64 * 1024 * 1024;
        let (uri, _requests, _heads) = spawn_streaming_mock_http_server(TOTAL, None);
        let mut source = HttpRangeSource::new(uri).enable_sequential_stream();
        // 16MB 前向窗口：本地回环下填满也要上百毫秒，足以留下一个真实样本。
        source.read_ahead_bytes = 16 * 1024 * 1024;

        // 先消费一点，然后让窗口自己填满（本地 mock 很快）。
        read_forward(&mut source, 0, 256 * 1024);
        thread::sleep(Duration::from_millis(900));
        let filled = source.diag_snapshot();
        println!(
            "fill-bps: 填窗期 fill_throughput_bps={:.0} B/s，窗口内 {:.0}s",
            filled.fill_throughput_bps, filled.forward_playable_secs
        );
        assert!(
            filled.fill_throughput_bps > 0.0,
            "窗口未满期间必须测到非零链路吞吐"
        );
        // 本地 mock 是"快链路"：填窗期测得的值应显著高于 1080P 码率量级，
        // 说明它反映的是链路能力、而不是消费速率。
        assert!(
            filled.fill_throughput_bps > 400_000.0,
            "本地回环链路吞吐应远高于 1080P 码率，实际 {:.0} B/s",
            filled.fill_throughput_bps
        );
        // 背压期间（窗口满、读线程不读）该值必须**冻结**而不是衰减到 0——
        // 否则宿主会把"我们故意不读"误判成"链路垮了"。
        thread::sleep(Duration::from_millis(700));
        let after_backpressure = source.diag_snapshot();
        println!(
            "fill-bps: 背压 700ms 后 fill_throughput_bps={:.0} B/s",
            after_backpressure.fill_throughput_bps
        );
        assert!(
            after_backpressure.fill_throughput_bps >= filled.fill_throughput_bps * 0.5,
            "背压期间吞吐读数不得塌陷：{:.0} -> {:.0}",
            filled.fill_throughput_bps,
            after_backpressure.fill_throughput_bps
        );
    }

    // =====================================================================
    // seek 出窗的跨 CDN 节点并行回填（`run_seek_fill` 侧）
    //
    // 前序代理已实现全部机制；这里的测试闭环用户验收的四条：
    //   1. 限速对照：单连接慢、并行快 → 墙钟真实缩短；
    //   2. 并行拼接结果与单连接结果**逐字节一致**；
    //   3. ETag 不一致 → 下载前门禁否决、退回单连接（宁慢不脏）；
    //   4. 测试本身能失败（故障注入后应先红后绿，见各用例注释）。
    //
    // mock 设计：与 `spawn_streaming_mock_http_server` 同构（任意连接数、
    // body = offset%256），但**每条连接独立限速**并可注入 ETag —— 这才能在
    // 本地复现"单连接被限速、多节点并行叠加"的 CDN 形态。
    // =====================================================================

    /// seek 并行测试专用 mock：每条连接独立限速 + 可注入 ETag。
    /// 返回 `(uri, 请求计数, 请求头通道)`。
    fn spawn_seek_parallel_mock_http_server(
        total: u64,
        etag: &str,
        bytes_per_sec: Option<f64>,
    ) -> (String, Arc<AtomicU64>, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}/video.m4s", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicU64::new(0));
        let (sender, receiver) = mpsc::channel();
        let etag = etag.to_string();
        // 闭包拿走计数器的所有权；这里另留一份句柄给调用方核对请求数。
        let requests_handle = Arc::clone(&requests);
        thread::spawn(move || {
            loop {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let requests = Arc::clone(&requests);
                let sender = sender.clone();
                let etag = etag.clone();
                thread::spawn(move || {
                    serve_seek_parallel_connection(
                        stream,
                        total,
                        &etag,
                        bytes_per_sec,
                        requests,
                        sender,
                    );
                });
            }
        });
        (uri, requests_handle, receiver)
    }

    /// 单条连接：解析 `Range: bytes=<start>[-<end>]` → 206 + ETag + body
    /// （内容 = `offset % 256`，与既有流式 mock 同一模式 → 拼接结果可直接与
    /// 单连接结果逐字节断言）。
    fn serve_seek_parallel_connection(
        mut stream: std::net::TcpStream,
        total: u64,
        etag: &str,
        bytes_per_sec: Option<f64>,
        requests: Arc<AtomicU64>,
        heads: mpsc::Sender<String>,
    ) {
        let Some((start, requested_end)) = read_range_request(&mut stream, &requests, &heads)
        else {
            return;
        };
        if start >= total {
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            );
            return;
        }
        let end = requested_end.unwrap_or(total - 1).min(total - 1);
        let declared = end - start + 1;
        let header = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {declared}\r\nETag: \"{etag}\"\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(header.as_bytes()).is_err() {
            return;
        }
        if write_stream_body_throttled(&mut stream, start, declared, bytes_per_sec).is_err() {
            return;
        }
        let _ = stream.flush();
    }

    /// 按 `offset % 256` 生成 body，并**按本连接累计字节 ÷ 速率**节流
    /// （每条连接独立计时 = "CDN 按单连接限速"的本地复现；`None` = 全速）。
    fn write_stream_body_throttled(
        stream: &mut std::net::TcpStream,
        mut offset: u64,
        mut remaining: u64,
        bytes_per_sec: Option<f64>,
    ) -> std::io::Result<()> {
        let Some(bps) = bytes_per_sec else {
            return write_stream_body(stream, offset, remaining);
        };
        if !(bps > 0.0) {
            return write_stream_body(stream, offset, remaining);
        }
        let template: Vec<u8> = (0..256u32).map(|value| value as u8).collect();
        let block = 64 * 1024u64;
        let mut buf = vec![0u8; block as usize];
        let started = Instant::now();
        let mut written: u64 = 0;
        while remaining > 0 {
            let n = remaining.min(block) as usize;
            let mut pos = 0usize;
            let mut shift = (offset % 256) as usize;
            while pos < n {
                let take = (n - pos).min(256 - shift);
                buf[pos..pos + take].copy_from_slice(&template[shift..shift + take]);
                pos += take;
                shift = 0;
            }
            stream.write_all(&buf[..n])?;
            offset += n as u64;
            remaining -= n as u64;
            written += n as u64;
            // 限速：把"已写字节 ÷ 速率"的应到时刻补足（与 `ThrottledSource`
            // 同一个线性模型；64KB 块粒度下 640KB/s ≈ 100ms 一次 sleep）。
            let due = Duration::from_secs_f64(written as f64 / bps);
            let elapsed = started.elapsed();
            if due > elapsed {
                thread::sleep(due - elapsed);
            }
        }
        Ok(())
    }

    /// seek 出窗拉回一个回填窗口并计墙钟。返回 `(墙钟, 读到的字节)`。
    ///
    /// 先顺序读 64KB 建窗，再 `prefetch_hint(target)` 触发出窗与并行计划，
    /// 最后从 target 起顺序读 `bytes` 字节（`read_forward` 内部逐 32KB 断言
    /// `offset % 256` 内容模式）。墙钟从 `prefetch_hint` 起算 = 用户体感的
    /// "松手到画面跳过去"。
    fn seek_out_of_window_and_read(
        source: &mut HttpRangeSource,
        target: u64,
        bytes: u64,
    ) -> (Duration, Vec<u8>) {
        // 先顺序读一段（建窗、让读线程有货；首个 64KB 后窗口已非空）。
        let seeded = source
            .read_range(ByteRange {
                start: 0,
                length: Some(64 * 1024),
            })
            .expect("seed read");
        assert_eq!(seeded.len(), 64 * 1024, "seed 读应一次拿满");
        let started = Instant::now();
        source.prefetch_hint(target);
        let read = read_forward(source, target, bytes);
        (started.elapsed(), read)
    }

    /// ① 限速对照 + 字节一致（端到端，真实 HTTP）：
    /// 三个候选各限速 640KB/s，seek 出窗并行拉一个回填窗口（≈1.5MiB）；
    /// 对照组（单候选 → 不并行）拉同一窗口。断言：
    ///   - 并行明显更快（≈ 1/2~1/3 墙钟）；
    ///   - 两者字节**逐字节一致**；
    ///   - 诊断：并行批次发生、门禁放行、重叠比对通过、顺播路径零并行。
    #[test]
    fn seek_fill_parallel_beats_single_connection_under_throttle_and_matches_bytes() {
        const TOTAL: u64 = 32 * 1024 * 1024;
        const RATE_BPS: f64 = 640.0 * 1024.0; // 每连接限速（本地真机值 2 倍，比例关系不变）
        const TARGET: u64 = 8 * 1024 * 1024;
        const ETAG: &str = "c3925abf0154d98095b824a712641d23"; // 实测 ETag == 内容 MD5
        let (uri_a, _, _) = spawn_seek_parallel_mock_http_server(TOTAL, ETAG, Some(RATE_BPS));
        let (uri_b, _, _) = spawn_seek_parallel_mock_http_server(TOTAL, ETAG, Some(RATE_BPS));
        let (uri_c, _, _) = spawn_seek_parallel_mock_http_server(TOTAL, ETAG, Some(RATE_BPS));

        // ---- 并行组：3 个候选 ----
        let mut parallel = HttpRangeSource::new(uri_a.clone()).enable_sequential_stream();
        parallel.set_candidates(vec![
            (uri_a.clone(), 1.0),
            (uri_b.clone(), 0.9),
            (uri_c.clone(), 0.8),
        ]);
        let (parallel_ms, parallel_bytes) =
            seek_out_of_window_and_read(&mut parallel, TARGET, SEEK_PARALLEL_FILL_BYTES);
        let parallel_diag = parallel.diag_snapshot();

        // ---- 单连接组：同一个 mock（限速一致），但只有 1 个候选 → 不并行 ----
        let (uri_d, _, _) = spawn_seek_parallel_mock_http_server(TOTAL, ETAG, Some(RATE_BPS));
        let mut single = HttpRangeSource::new(uri_d.clone()).enable_sequential_stream();
        let (single_ms, single_bytes) =
            seek_out_of_window_and_read(&mut single, TARGET, SEEK_PARALLEL_FILL_BYTES);
        let single_diag = single.diag_snapshot();

        eprintln!(
            "seek-fill 限速对照: 并行 3 段 = {:.0}ms，单连接 = {:.0}ms（回填 {}B @ {:.0}KB/s 每连接）",
            parallel_ms.as_secs_f64() * 1000.0,
            single_ms.as_secs_f64() * 1000.0,
            SEEK_PARALLEL_FILL_BYTES,
            RATE_BPS / 1024.0,
        );

        // ---- 断言 1：限速确实生效（单连接对照被拖慢）----
        // 单连接应从 write_pos 起一条 open-ended Range：拉满整个回填窗口 ≈
        // 1.5MiB ÷ 640KB/s ≈ 2.4s。低于期望的一半说明 mock 限速没生效，
        // 整个对照实验失去意义（必须失败，不能让"并行"在假对照上自证）。
        let single_expected = SEEK_PARALLEL_FILL_BYTES as f64 / RATE_BPS;
        assert!(
            single_ms.as_secs_f64() >= single_expected * 0.45,
            "单连接对照应在限速下被拖慢（期望 ≈{single_expected:.1}s，实际 {:.1}s）—— mock 限速失效，对照实验无效",
            single_ms.as_secs_f64()
        );

        // ---- 断言 2：并行显著缩短墙钟（目标：压到 1/2 以下）----
        assert!(
            parallel_ms < single_ms.mul_f64(0.55),
            "跨节点并行应将回填墙钟明显压在单连接之下：并行 {:.0}ms vs 单连接 {:.0}ms",
            parallel_ms.as_secs_f64() * 1000.0,
            single_ms.as_secs_f64() * 1000.0,
        );

        // ---- 断言 3：拼接字节逐字节一致（并行结果 == 单连接结果）----
        assert_eq!(
            parallel_bytes.len(),
            single_bytes.len(),
            "两种路径拿到的字节数应相同"
        );
        assert_eq!(
            parallel_bytes, single_bytes,
            "并行分段拼接结果必须与单连接拉到的那一段逐字节一致（任一字节错位/重复/缺失都会暴露）"
        );
        // 双重校验：再独立对一次内容模式（read_forward 已做逐块断言，这里是全量）。
        for (i, byte) in parallel_bytes.iter().enumerate() {
            assert_eq!(
                *byte,
                ((TARGET + i as u64) % 256) as u8,
                "偏移 {} 字节与 offset%256 模式不符",
                TARGET + i as u64
            );
        }

        // ---- 断言 4：诊断口径区分三形态 ----
        // 并行组：patch 发生、门禁放行、重叠比对通过、无回退。
        assert!(
            parallel_diag.seek_fill_batches >= 1,
            "并行组应至少有一次 seek 并行批次"
        );
        assert!(
            parallel_diag.seek_fill_connections >= 3,
            "并行组应建立 ≥3 条分段连接（实际 {}）",
            parallel_diag.seek_fill_connections
        );
        assert_eq!(
            parallel_diag.seek_fill_gate_denied, 0,
            "ETag 一致场景门禁不得否决：{}",
            parallel_diag.seek_fill_last_gate_reason
        );
        assert!(
            parallel_diag.seek_fill_overlap_ok >= 1,
            "重叠比对应至少通过一批当次批次"
        );
        assert_eq!(
            parallel_diag.seek_fill_fallbacks, 0,
            "限速对照场景不应发生回退"
        );
        // 对照组：只 1 个候选 → 计划恒 None → 顺播路径零并行（诊断恒 0）。
        assert_eq!(
            single_diag.seek_fill_batches, 0,
            "单候选源不应产生并行批次（它走的是既有单连接路径）"
        );
    }

    /// ② ETag 不一致 → 下载前门禁否决、退回单连接（故障注入场景）。
    ///
    /// 两个候选 mock **内容一致**（同一 offset%256 模式、同一 total），但
    /// ETag 不同（`"aaa"` vs `"bbb"`）——这正是"同一份数据的两个镜像"里
    /// 凭据对不上的情形。门禁必须在 **body 下载之前**否决（预检只有 1 字节），
    /// 读线程回退单连接。断言：
    ///   - 读到的字节**仍然正确**（回退路径兜住了，seek 不失败、不花屏）；
    ///   - `seek_fill_gate_denied` 与 `seek_fill_last_gate_reason` 可读；
    ///   - 未发生跨节点并行（`seek_fill_connections == 0`）。
    #[test]
    fn seek_fill_etag_mismatch_denies_parallel_and_falls_back() {
        const TOTAL: u64 = 32 * 1024 * 1024;
        const TARGET: u64 = 8 * 1024 * 1024;
        // 不限速：回退到单连接后本地全速，测试快；"拒绝并行"与限速无关。
        let (uri_a, _, _) = spawn_seek_parallel_mock_http_server(TOTAL, "aaa", None);
        let (uri_b, _, _) = spawn_seek_parallel_mock_http_server(TOTAL, "bbb", None);

        let mut source = HttpRangeSource::new(uri_a.clone()).enable_sequential_stream();
        source.set_candidates(vec![(uri_a.clone(), 1.0), (uri_b.clone(), 0.9)]);
        let (_elapsed, bytes) =
            seek_out_of_window_and_read(&mut source, TARGET, SEEK_PARALLEL_FILL_BYTES);
        let diag = source.diag_snapshot();

        // 回退路径兜底：字节必须仍然正确（一个字节都不能少/错/花屏）。
        for (i, byte) in bytes.iter().enumerate() {
            assert_eq!(
                *byte,
                ((TARGET + i as u64) % 256) as u8,
                "ETag 不一致回退后字节错位（偏移 {}）",
                TARGET + i as u64
            );
        }
        assert_eq!(bytes.len() as u64, SEEK_PARALLEL_FILL_BYTES);

        // 门禁必须否决：这正是"宁可慢，不冒险"的判据落点。
        assert!(
            diag.seek_fill_gate_denied >= 1,
            "ETag 不一致必须触发下载前门禁否决（gate_denied），实际 {}",
            diag.seek_fill_gate_denied
        );
        assert!(
            diag.seek_fill_last_gate_reason.contains("ETag"),
            "否决理由必须点名 ETag（用户要读懂「为什么没并行」）：{}",
            diag.seek_fill_last_gate_reason
        );
        assert!(
            diag.seek_fill_fallbacks >= 1,
            "门禁否决后应记录一次回退到单连接"
        );
        // 未放行的批次不得建立分段连接（预检可发，但 body 分段一行都不该有）。
        assert_eq!(
            diag.seek_fill_connections, 0,
            "门禁否决后不得有分段连接（body 下载前被拦截）"
        );
    }

    /// ③ 纯函数：`seek_fill_gate` 的门禁语义（不依赖网络，最快最稳）。
    #[test]
    fn seek_fill_gate_requires_matching_etag_and_total() {
        let probe = |total: Option<u64>, etag: Option<&str>| SeekFillProbe {
            total,
            etag: etag.map(str::to_string),
            last_modified: None,
            elapsed: Duration::ZERO,
        };
        // 全一致 → 放行。
        let (verdict, reason) = seek_fill_gate(
            &[probe(Some(100), Some("aaa")), probe(Some(100), Some("aaa"))],
            false,
        );
        assert_eq!(verdict, SeekFillVerdict::Allow, "{reason}");
        // ETag 不一致 → 默认否决（宁慢不脏）。
        let (verdict, reason) = seek_fill_gate(
            &[probe(Some(100), Some("aaa")), probe(Some(100), Some("bbb"))],
            false,
        );
        assert_eq!(verdict, SeekFillVerdict::Deny, "{reason}");
        assert!(reason.contains("ETag"), "否决理由应点名 ETag：{reason}");
        // ETag 缺失 → 默认否决（证据不足 = 缺证据，不是"看起来像"）。
        let (verdict, reason) = seek_fill_gate(
            &[probe(Some(100), Some("aaa")), probe(Some(100), None)],
            false,
        );
        assert_eq!(verdict, SeekFillVerdict::Deny, "{reason}");
        // 显式 trust_nodes → 允许（用户接受风险，重叠比对兜底）。
        let (verdict, reason) = seek_fill_gate(
            &[probe(Some(100), Some("aaa")), probe(Some(100), Some("bbb"))],
            true,
        );
        assert_eq!(verdict, SeekFillVerdict::Allow, "{reason}");
        // 文件总长缺失 → 否决。
        let (verdict, _) = seek_fill_gate(
            &[probe(Some(100), Some("aaa")), probe(None, Some("aaa"))],
            false,
        );
        assert_eq!(verdict, SeekFillVerdict::Deny);
        // 总长不一致 → 否决（不是同一个对象）。
        let (verdict, reason) = seek_fill_gate(
            &[probe(Some(100), Some("aaa")), probe(Some(101), Some("aaa"))],
            false,
        );
        assert_eq!(verdict, SeekFillVerdict::Deny, "{reason}");
        assert!(reason.contains("总长"), "{reason}");
        // 空 probes → 否决。
        let (verdict, _) = seek_fill_gate(&[], false);
        assert_eq!(verdict, SeekFillVerdict::Deny);
    }

    /// ④ 纯函数：`seek_parallel_connections` 的条数推导语义。
    #[test]
    fn seek_parallel_connections_derives_conn_count_from_link_rate() {
        // 快链路（单流已够目标吞吐）→ 1（不并行，白开连接只涨请求数）。
        assert_eq!(seek_parallel_connections(1536 * 1024, 2_000_000.0), 1);
        // 单流 320KB/s + 1.5MiB 窗口 + 1.6s 目标 → 3 条。
        assert_eq!(seek_parallel_connections(1536 * 1024, 320.0 * 1024.0), 3);
        // 单流估算未知 → 默认档（SEEK_PARALLEL_DEFAULT_CONNECTIONS = 3）。
        assert_eq!(seek_parallel_connections(1536 * 1024, 0.0), 3);
        // 窗口太小（放不下 ≥2 个达标段）→ 1（不并行）。
        assert_eq!(seek_parallel_connections(300 * 1024, 320_000.0), 1);
        // 上限 4：极慢链路也不会开爆（真机教训：8 并发触发 CDN 限流惩罚）。
        assert_eq!(seek_parallel_connections(1536 * 1024, 10_000.0), 4);
    }
}
