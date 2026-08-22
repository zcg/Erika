//! 临时诊断测试：对比 Erika `HttpRangeSource` 下载的字节与 curl 下载的
//! 同一 range——定位"avcodec_send_packet: Invalid data"（视频解码坏包）
//! 是否是 Erika HTTP 多段预取的数据拼接损坏。
//!
//! 前置：`%TEMP%\erika_http_trace.jsonl`（bili-rs 播放产生的 trace，含
//! 未过期的视频 URL）+ `%TEMP%\cur_video_1MB.bin`（curl 下载的 0-1MB 对照，
//! 已用 ffprobe 验证有效）。

use erika::source::{ByteRange, HttpRangeSource, MediaSource};

#[test]
#[ignore = "deps on session trace %TEMP%/erika_http_trace.jsonl (expiring signed URLs); run: cargo test -- --ignored"]
fn http_download_matches_curl_reference() {
    // 1. 从 trace 提取视频 URL（head_request uri=... cache_start）
    let trace_path = std::env::temp_dir().join("erika_http_trace.jsonl");
    let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
    let url = trace.lines().find_map(|line| {
        if !line.contains("head_request uri=https://upos-sz-mirrorali")
            || !line.contains(" cache_start")
        {
            return None;
        }
        let start = line.find("uri=").map(|i| i + 4)?;
        let end = line.find(" cache_start")?;
        Some(line[start..end].to_string())
    });
    let Some(url) = url else {
        eprintln!("skip: trace 中无视频 head_request（先播放一次）");
        return;
    };

    // 2. Erika 下载 0-1MB（走真实的多段并行预取路径）
    let mut source = HttpRangeSource::with_http_headers(
        url,
        vec![
            ("Referer".to_string(), "https://www.bilibili.com".to_string()),
            (
                "User-Agent".to_string(),
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.2 Safari/605.1.15".to_string(),
            ),
        ],
    );
    let bytes = source
        .read_range(ByteRange {
            start: 0,
            length: Some(1024 * 1024),
        })
        .expect("Erika 下载 0-1MB");

    // 3. 与 curl 对照文件逐字节比较
    let curl_path = std::env::temp_dir().join("cur_video_1MB.bin");
    let curl = std::fs::read(&curl_path).expect("读取 curl 对照文件");
    assert_eq!(
        bytes.len(),
        curl.len(),
        "长度不一致: erika={} curl={}",
        bytes.len(),
        curl.len()
    );
    let mut first_diff = None;
    for (i, (a, b)) in bytes.iter().zip(curl.iter()).enumerate() {
        if a != b {
            first_diff = Some(i);
            break;
        }
    }
    if let Some(offset) = first_diff {
        panic!(
            "字节不一致 @ offset {offset}: erika=0x{:02X} curl=0x{:02X}",
            bytes[offset], curl[offset]
        );
    }
    eprintln!(
        "OK: Erika 下载 0-1MB 与 curl 完全一致（{} 字节）",
        bytes.len()
    );
}
