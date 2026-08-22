//! 临时诊断测试：HTTP 双流（HttpRangeSource）+ Software 解码前 200 包，
//! 并同时跑一个本地双流 demux（同序列）逐包对照——第一个差异包即 HTTP
//! 读取 bug 的字节级证据（stream/size/pos/head）。
//!
//! 前置：`%TEMP%\erika_http_trace.jsonl`（含未过期 URL）+ `%TEMP%\cur_video_1MB.bin`
//! 与 `%TEMP%\cur_audio_1MB.bin`（本地对照）。

use erika::ffmpeg::{Decoder, DecoderBackend, DecoderConfig, DecoderOutputFrame, Demuxer};
use erika::source::{ByteRange, HttpRangeSource, LocalFileSource, MediaSource};

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.2 Safari/605.1.15";

/// 提取 trace 里的 URL：第 1 条 head_request = 视频，第 2 条 = 音频。
/// **取最后两条**（trace 是追加的，含历史播放；最末两条是最近一次 open 的
/// 双流，URL 未过期）。生产音频节点可能是 coso1/mirror08h 等任意 mirror。
fn extract_urls(trace: &str) -> (String, String) {
    let mut urls = Vec::new();
    for line in trace.lines() {
        if line.contains("head_request uri=https://") && line.contains(" cache_start") {
            let start = line.find("uri=").map(|i| i + 4).expect("uri=");
            let end = line.find(" cache_start").expect("cache_start");
            urls.push(line[start..end].to_string());
        }
    }
    let n = urls.len();
    (urls[n - 2].clone(), urls[n - 1].clone())
}

#[derive(Debug, Clone, PartialEq)]
struct PacketRec {
    stream: i32,
    size: usize,
    pos: i64,
    head: Vec<u8>,
}

fn collect_demux(mut demuxer: Demuxer, count: usize) -> Vec<PacketRec> {
    let _ = demuxer.probe(); // 与 decode 循环完全同序列（probe 只读已解析信息）
    let mut out = Vec::new();
    while out.len() < count {
        let Some(p) = demuxer.read_packet().expect("read_packet") else {
            break;
        };
        out.push(PacketRec {
            stream: p.stream_index(),
            size: p.size(),
            pos: p.pos().unwrap_or(-1),
            head: p.data().iter().take(16).copied().collect(),
        });
    }
    out
}

fn decode_http_dash(disable_prefetch: bool) {
    // 进程级 env：跑完必须恢复，否则按字母序先跑的 no_prefetch 会把
    // streams_software 也毒化成无预取（两次都"通过"却没有验证预取路径）。
    let prev = std::env::var("ERIKA_HTTP_READAHEAD_BYTES").ok();
    if disable_prefetch {
        unsafe { std::env::set_var("ERIKA_HTTP_READAHEAD_BYTES", "0") };
    } else {
        unsafe { std::env::remove_var("ERIKA_HTTP_READAHEAD_BYTES") };
    }
    let trace = std::fs::read_to_string(std::env::temp_dir().join("erika_http_trace.jsonl"))
        .unwrap_or_default();
    // 干净环境下默认 trace 可能不存在：回退读 compare 测试生成的 trace
    //（URL 同源未过期，仅用于提取 URL）。
    let trace = if trace.is_empty() {
        std::fs::read_to_string(std::env::temp_dir().join("compare_http_trace.jsonl"))
            .unwrap_or_default()
    } else {
        trace
    };
    let (video_url, audio_url) = extract_urls(&trace);
    eprintln!("[diag] prefetch_disabled={disable_prefetch}");
    eprintln!("[diag] video={}", &video_url[..80.min(video_url.len())]);
    eprintln!("[diag] audio={}", &audio_url[..80.min(audio_url.len())]);

    let headers = vec![
        (
            "Referer".to_string(),
            "https://www.bilibili.com".to_string(),
        ),
        ("User-Agent".to_string(), UA.to_string()),
    ];
    let video_source = HttpRangeSource::with_http_headers(video_url.clone(), headers.clone());
    let audio_source =
        HttpRangeSource::with_http_headers_and_diag_slot(audio_url.clone(), headers.clone(), 1);
    let mut demuxer = Demuxer::open_video_audio(Box::new(video_source), Box::new(audio_source))
        .expect("open dual http");

    // 本地对照：用**独立 source 实例**一次性全量下载 0-1MB（不经过 demux
    // 读取序列，与播放路径唯一的共享点是 CDN 数据本身）。若 demux 包与
    // 全量下载一致 → Erika 无拼接错位，坏数据来自服务器/CDN。
    let temp = std::env::temp_dir();
    let headers_for_fetch = headers.clone();
    let fetch_all = {
        let temp = temp.clone();
        move |url: String, name: &'static str| -> (std::path::PathBuf, Vec<u8>) {
            let path = temp.join(name);
            if !path.exists() {
                let mut src = HttpRangeSource::with_http_headers(url, headers_for_fetch.clone());
                let data = src
                    .read_range(ByteRange {
                        start: 0,
                        length: Some(1_048_576),
                    })
                    .expect("full fetch");
                std::fs::write(&path, &data).expect("write fetch file");
                eprintln!("[diag] 全量下载 {name} {}B", data.len());
            }
            let data = std::fs::read(&path).expect("read fetch file");
            (path, data)
        }
    };
    let (_, full_video) = fetch_all(video_url.clone(), "diag_video_1MB.bin");
    let (_, _full_audio) = fetch_all(audio_url.clone(), "diag_audio_1MB.bin");
    let local_video = LocalFileSource::open(temp.join("diag_video_1MB.bin")).expect("local video");
    let local_audio = LocalFileSource::open(temp.join("diag_audio_1MB.bin")).expect("local audio");
    let local_demux = Demuxer::open_video_audio(Box::new(local_video), Box::new(local_audio))
        .expect("local demux");
    let local_recs = collect_demux(local_demux, 200);
    eprintln!("[diag] local packets={}", local_recs.len());

    let video_params = demuxer.owned_codec_parameters(0).expect("video params");
    let audio_params = demuxer.owned_codec_parameters(1).expect("audio params");
    let mut video_dec = Decoder::open_owned_with_config(
        &video_params,
        DecoderConfig {
            backend: DecoderBackend::Software,
            ..Default::default()
        },
    )
    .expect("video decoder");
    let mut audio_dec = Decoder::open_owned_with_config(
        &audio_params,
        DecoderConfig {
            backend: DecoderBackend::Software,
            ..Default::default()
        },
    )
    .expect("audio decoder");

    let mut sent = 0usize;
    let mut v_frames = 0usize;
    let mut a_frames = 0usize;
    let mut send_errors: Vec<String> = Vec::new();
    let mut http_recs: Vec<PacketRec> = Vec::new();
    let mut misalign_reported = false;
    while sent < 200 {
        let Some(packet) = demuxer.read_packet().expect("read_packet") else {
            eprintln!("[diag] EOF at {sent}");
            break;
        };
        let rec = PacketRec {
            stream: packet.stream_index(),
            size: packet.size(),
            pos: packet.pos().unwrap_or(-1),
            head: packet.data().iter().take(16).copied().collect(),
        };
        http_recs.push(rec.clone());
        let is_video = packet.stream_index() == 0;
        // 字节级定位：视频包数据 vs 全量下载文件同偏移（pos 若为时间戳则
        // 跳过；只报第一个错位包，并在文件中搜索垃圾数据的来源位置）。
        if !misalign_reported && is_video && rec.pos >= 0 {
            let start = rec.pos as usize;
            let end = (start + rec.size).min(full_video.len());
            let file_bytes = &full_video[start..end];
            let pkt = packet.data();
            let n = pkt.len().min(file_bytes.len()).min(16);
            if n >= 8 && pkt[..n] != file_bytes[..n] {
                let needle = &pkt[..8];
                let found = full_video.windows(8).position(|w| w == needle);
                eprintln!(
                    "[diag] 错位 V #{} pos={} size={} http_head={:02X?} file_head={:02X?} needle8@{:?}",
                    http_recs.len() - 1,
                    rec.pos,
                    rec.size,
                    &pkt[..n],
                    &file_bytes[..n],
                    found,
                );
                misalign_reported = true;
            }
        }
        let r = if is_video {
            video_dec.send_packet(&packet)
        } else {
            audio_dec.send_packet(&packet)
        };
        match r {
            Ok(()) => sent += 1,
            Err(e) => {
                // 错误包与本地对照同 index 包对比，打印 pos/head 差异
                let local_p = local_recs.get(http_recs.len() - 1);
                let local_desc = local_p
                    .map(|lp| {
                        format!(
                            "local: stream={} size={} pos={} head={:02X?}",
                            lp.stream, lp.size, lp.pos, lp.head
                        )
                    })
                    .unwrap_or_else(|| "local: 无对应包".into());
                send_errors.push(format!(
                    "{} packet #{} stream={} size={} pos={} head={:02X?} err={e} | {local_desc}",
                    if is_video { "V" } else { "A" },
                    http_recs.len() - 1,
                    packet.stream_index(),
                    packet.size(),
                    rec.pos,
                    rec.head,
                ));
                sent += 1;
            }
        }
        loop {
            match video_dec.receive_frame().expect("v receive") {
                DecoderOutputFrame::Frame(_) => v_frames += 1,
                _ => break,
            }
        }
        loop {
            match audio_dec.receive_frame().expect("a receive") {
                DecoderOutputFrame::Frame(_) => a_frames += 1,
                _ => break,
            }
        }
        if sent % 50 == 0 {
            eprintln!("[diag] sent={sent} v={v_frames} a={a_frames}");
        }
    }
    eprintln!(
        "[diag] done sent={sent} v_frames={v_frames} a_frames={a_frames} errors={}",
        send_errors.len()
    );
    for e in send_errors.iter().take(6) {
        eprintln!("  {e}");
    }

    // 诊断槽验证：双流应各自 publish（video=槽 0 / audio=槽 1），宿主 UI
    // 据此显示缓存进度。若两槽都空/串槽 → 宿主诊断显示 0%（用户反馈的
    // "视频缓存 0% 但正常播放"）。
    {
        let v0 = erika::source::source_diag_snapshot_slot(0);
        let v1 = erika::source::source_diag_snapshot_slot(1);
        eprintln!(
            "[diag] 槽0(video): content_length={:?} cache=[{},{}] dl={} est={:.0}B/s",
            v0.content_length, v0.cache_start, v0.cache_end, v0.downloaded_bytes, v0.est_bps
        );
        eprintln!(
            "[diag] 槽1(audio): content_length={:?} cache=[{},{}] dl={} est={:.0}B/s",
            v1.content_length, v1.cache_start, v1.cache_end, v1.downloaded_bytes, v1.est_bps
        );
    }

    // 逐包对比（与 compare 测试同标准）：第一个差异即 HTTP 读取 bug 位置
    let common = http_recs.len().min(local_recs.len());
    let mut diffs = Vec::new();
    for i in 0..common {
        if http_recs[i] != local_recs[i] {
            diffs.push((i, local_recs[i].clone(), http_recs[i].clone()));
        }
    }
    eprintln!("[diag] 共对比 {common} 包，差异 {}(个)", diffs.len());
    for (idx, local, http) in diffs.iter().take(5) {
        eprintln!(
            "差异 @ #{idx}:\n  本地: stream={} size={} pos={} head={:02X?}\n  HTTP: stream={} size={} pos={} head={:02X?}",
            local.stream,
            local.size,
            local.pos,
            local.head,
            http.stream,
            http.size,
            http.pos,
            http.head
        );
    }
    assert!(
        send_errors.is_empty() && diffs.is_empty(),
        "HTTP 读取路径 bug 锁定（errors={} diffs={}）",
        send_errors.len(),
        diffs.len()
    );

    // 恢复 env（进程级）：同进程下一个用例按自己的 disable_prefetch 重设
    match prev {
        Some(v) => unsafe { std::env::set_var("ERIKA_HTTP_READAHEAD_BYTES", v) },
        None => unsafe { std::env::remove_var("ERIKA_HTTP_READAHEAD_BYTES") },
    }
}

#[test]
#[ignore = "deps on session trace %TEMP%/erika_http_trace.jsonl (expiring signed URLs); run: cargo test -- --ignored"]
fn decode_http_dash_streams_software() {
    decode_http_dash(false);
}

#[test]
#[ignore = "deps on session trace %TEMP%/erika_http_trace.jsonl (expiring signed URLs); run: cargo test -- --ignored"]
fn decode_http_dash_software_no_prefetch() {
    decode_http_dash(true);
}
