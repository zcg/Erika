//! 宿主（bili-rs）接口契约测试。
//!
//! 这里的每一行都**逐字复刻**宿主对 erika 的真实调用：集成测试是外部 crate，
//! 只能看到 `pub` 项，所以「本文件编译通过」即等价于「宿主那部分能编译通过」。
//! 一旦有人改动这些名字/签名/字段，本文件会先于宿主构建失败。
//!
//! 覆盖范围（来源：D:/Projects/bili-rs/src 全量扫描）：
//! - `MediaRequest` 的全部 builder 链（VOD 与直播两条路径）
//! - `SourceDiagSnapshot` 宿主读取的全部字段
//! - `source_diag_snapshot_slot` / `request_host_switch` 的函数签名

#![allow(unused_imports, dead_code)]

use erika::core::{MediaRequest, MediaSourceHint};
use erika::source::{SourceDiagSnapshot, request_host_switch, source_diag_snapshot_slot};

/// 复刻 `bili-rs/src/playback/mod.rs:1445-1468` 的点播请求组装。
fn build_vod_request(
    url: String,
    video_candidates: Vec<(String, f64)>,
    audio_candidates: Vec<(String, f64)>,
    read_ahead: Option<u64>,
    audio_url: Option<String>,
) -> MediaRequest {
    let mut request = MediaRequest::new(url)
        .with_video_candidates(video_candidates)
        .with_audio_candidates(audio_candidates);
    if let Some(bytes) = read_ahead {
        request = request.with_http_read_ahead_bytes(bytes);
    }
    if let Some(audio) = audio_url {
        request = request.with_audio_uri(audio);
    }
    request.with_http_headers(
        [("User-Agent".to_string(), "contract".to_string())]
            .into_iter()
            .collect(),
    )
}

/// 复刻 `bili-rs/src/playback/mod.rs:1340-1341` 的直播请求组装。
fn build_live_request(url: String) -> MediaRequest {
    MediaRequest::new(url)
        .with_source_hint(MediaSourceHint::HttpStream)
        .with_http_headers(
            [("User-Agent".to_string(), "contract".to_string())]
                .into_iter()
                .collect(),
        )
}

/// 复刻 `bili-rs/src/playback/mod.rs:322-359`：宿主从快照取出的每一个字段。
/// 返回值只为让每个字段都被真正读到（少一个都会编译失败）。
fn read_host_diag_fields(snap: SourceDiagSnapshot) -> HostDiag {
    HostDiag {
        content_length: snap.content_length,
        cache_start: snap.cache_start,
        cache_end: snap.cache_end,
        prefetch_in_flight: snap.prefetch_in_flight,
        downloaded_bytes: snap.downloaded_bytes,
        est_bps: snap.est_bps,
        active_host: snap.active_host,
        active_idx: snap.active_idx,
        candidate_count: snap.candidate_count,
        demux_cache_end: snap.demux_cache_end,
        whole_file_cached: snap.whole_file_cached,
        host_epoch: snap.host_epoch,
        last_host_switch_reason: snap.last_host_switch_reason,
        http_requests: snap.http_requests,
        http_request_bytes: snap.http_request_bytes,
        transient_wait: snap.transient_wait,
        fill_throughput_bps: snap.fill_throughput_bps,
    }
}

#[derive(Debug)]
struct HostDiag {
    content_length: Option<u64>,
    cache_start: u64,
    cache_end: u64,
    prefetch_in_flight: usize,
    downloaded_bytes: u64,
    est_bps: f64,
    active_host: String,
    active_idx: usize,
    candidate_count: usize,
    demux_cache_end: u64,
    whole_file_cached: bool,
    host_epoch: u64,
    last_host_switch_reason: String,
    http_requests: u64,
    http_request_bytes: u64,
    transient_wait: bool,
    fill_throughput_bps: f64,
}

/// 复刻 `bili-rs/src/playback/mod.rs:327-328` 的「本槽是否活跃」判据。
fn slot_is_active(snap: &SourceDiagSnapshot) -> bool {
    snap.content_length.is_some() || snap.cache_end > 0 || snap.prefetch_in_flight > 0
}

#[test]
fn host_can_build_both_request_kinds() {
    let vod = build_vod_request(
        "https://example.invalid/video.m4s".to_string(),
        vec![
            ("https://cdn-a.invalid/video.m4s".to_string(), 1.0),
            ("https://cdn-b.invalid/video.m4s".to_string(), 0.5),
        ],
        vec![("https://cdn-a.invalid/audio.m4s".to_string(), 1.0)],
        Some(8 << 20),
        Some("https://example.invalid/audio.m4s".to_string()),
    );
    assert_eq!(vod.uri, "https://example.invalid/video.m4s");
    assert_eq!(vod.video_candidates.len(), 2);
    assert_eq!(vod.audio_candidates.len(), 1);
    assert_eq!(vod.http_read_ahead_bytes, Some(8 << 20));
    assert_eq!(vod.audio_uri.as_deref(), Some("https://example.invalid/audio.m4s"));
    assert_eq!(vod.http_headers.len(), 1);
    assert_eq!(vod.source_hint, MediaSourceHint::Auto);

    let live = build_live_request("https://live.example.invalid/room.flv".to_string());
    assert_eq!(live.source_hint, MediaSourceHint::HttpStream);
}

#[test]
fn host_can_read_every_diag_field_and_switch_hosts() {
    // 纯读：不改变任何内核状态。
    let video = source_diag_snapshot_slot(0);
    let audio = source_diag_snapshot_slot(1);
    let _ = slot_is_active(&video);
    let _ = slot_is_active(&audio);
    let diag: HostDiag = read_host_diag_fields(video);
    assert!(diag.candidate_count < usize::MAX);

    // 只做签名绑定，不真的触发一次换源（避免影响同进程其他用例）。
    let switch: fn(usize) -> bool = request_host_switch;
    let snapshot_slot: fn(usize) -> SourceDiagSnapshot = source_diag_snapshot_slot;
    assert!(!switch(usize::MAX), "越界槽位必须返回 false");
    let _ = snapshot_slot(0);
}
