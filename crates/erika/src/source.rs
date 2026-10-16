//! 媒体源：薄转发层。
//!
//! 本地自研实现（HTTP Range 源、HTTP 直播源、诊断家族与工厂函数）整体在
//! [`legacy`]；异步 HTTP 原语在 [`http`]，多源换源包装在 [`multi_host`]。
//! 本文件只声明子模块，并把它们的名字按原有可见性转发出去——上游的
//! `source.rs` 将来可以被整体替换，把合并冲突降到最低。

mod http;
mod legacy;
mod multi_host;

use std::sync::atomic::{AtomicU64, Ordering};

pub use http::SourceCancellation;
pub use multi_host::MultiHostSource;

#[cfg(target_os = "android")]
pub use legacy::{
    AndroidOwnedFdRegistration, OwnedFileDescriptorSource, register_android_owned_fd,
};
pub use legacy::{
    ByteRange, HttpLiveStreamSource, HttpRangeSource, LocalFileSource, MediaSource, Result,
    SourceDiagSnapshot, SourceError, read_uri_to_end, source_diag_snapshot,
    source_diag_snapshot_slot, source_from_uri, source_from_uri_with_hint,
    source_from_uri_with_hint_and_headers, source_from_uri_with_options,
};

pub(crate) use legacy::{
    SOURCE_DIAG_SLOTS, source_from_uri_with_hint_and_headers_slot,
    source_from_uri_with_hint_and_headers_slot_and_read_ahead,
};

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
