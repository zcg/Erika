//! 终极对比：同一视频，HTTP 双流 demux 与本地双流 demux 的 packet 序列
//! 逐包对比（stream/size/pos/data 头）——第一个差异点即 HTTP 读取 bug 位置。

use erika::ffmpeg::Demuxer;
use erika::source::{ByteRange, HttpRangeSource, LocalFileSource, MediaSource};

const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.2 Safari/605.1.15";

fn extract_url(trace: &str, host_fragment: &str) -> Option<String> {
    trace.lines().find_map(|line| {
        if !line.contains("head_request uri=https://") || !line.contains(host_fragment) {
            return None;
        }
        if !line.contains(" cache_start") {
            return None;
        }
        let start = line.find("uri=").map(|i| i + 4)?;
        let end = line.find(" cache_start")?;
        Some(line[start..end].to_string())
    })
}

#[derive(Debug, Clone, PartialEq)]
struct PacketRec {
    stream: i32,
    size: usize,
    pos: i64,
    head: Vec<u8>,
}

fn collect(mut demuxer: Demuxer, count: usize) -> Vec<PacketRec> {
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

#[test]
fn compare_http_vs_local_packet_sequences() {
    // 独立 HTTP trace：观察 demux 期间 HTTP 源的实际请求序列（start/length）
    unsafe {
        std::env::set_var("ERIKA_HTTP_TRACE", "1");
        std::env::set_var(
            "ERIKA_HTTP_TRACE_FILE",
            std::env::temp_dir().join("compare_http_trace.jsonl"),
        );
    }
    let _ = std::fs::remove_file(std::env::temp_dir().join("compare_http_trace.jsonl"));
    let trace = std::fs::read_to_string(std::env::temp_dir().join("erika_http_trace.jsonl"))
        .unwrap_or_default();
    let video_url = extract_url(&trace, "upos-sz-mirrorali").expect("trace 视频 URL");
    let audio_url = extract_url(&trace, "upos-sz-mirror08h").expect("trace 音频 URL");
    let headers = vec![
        ("Referer".to_string(), "https://www.bilibili.com".to_string()),
        ("User-Agent".to_string(), UA.to_string()),
    ];

    // HTTP 双流
    let http_video = HttpRangeSource::with_http_headers(video_url.clone(), headers.clone());
    let http_audio = HttpRangeSource::with_http_headers_and_diag_slot_limits(
        audio_url,
        headers.clone(),
        1,
        2,
    );
    let http_demux = Demuxer::open_video_audio(Box::new(http_video), Box::new(http_audio))
        .expect("http demux");
    let http_recs = collect(http_demux, 200);
    eprintln!("[diag] http packets={}", http_recs.len());

    // —— 顺序模拟读取对比：先 0-256KB 再 262932（avformat 的实际读取模式）——
    // 定位"先读大块再读小块"的窗口状态 bug（本地文件对照）。
    let local_file =
        std::fs::read(std::env::temp_dir().join("cur_video_1MB.bin")).expect("read local file");
    let mut http_seq = HttpRangeSource::with_http_headers(video_url, headers.clone());
    let first = http_seq
        .read_range(ByteRange {
            start: 0,
            length: Some(262144),
        })
        .expect("seq read 0-256K");
    assert_eq!(
        &first[..],
        &local_file[..262144],
        "顺序读取 0-256K 已不一致"
    );
    eprintln!("[diag] 顺序 0-256K 一致");
    let second = http_seq
        .read_range(ByteRange {
            start: 262932,
            length: Some(512),
        })
        .expect("seq read 262932");
    let expect = &local_file[262932..262932 + 512];
    if &second[..] != expect {
        let diff_at = second
            .iter()
            .zip(expect.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(usize::MAX);
        eprintln!(
            "[diag] 顺序读取 262932 不一致（首个差异 @ {diff_at}）: http_head={:02X?} expect_head={:02X?}",
            &second[..8],
            &expect[..8]
        );
        panic!("顺序读取 262932 错位（窗口状态 bug 锁定）");
    }
    eprintln!("[diag] 顺序读取 262932 一致");

    // 本地双流
    let temp = std::env::temp_dir();
    let local_video = LocalFileSource::open(temp.join("cur_video_1MB.bin")).expect("local video");
    let local_audio = LocalFileSource::open(temp.join("cur_audio_1MB.bin")).expect("local audio");
    let local_demux = Demuxer::open_video_audio(Box::new(local_video), Box::new(local_audio))
        .expect("local demux");
    let local_recs = collect(local_demux, 200);
    eprintln!("[diag] local packets={}", local_recs.len());

    // 对比
    let common = http_recs.len().min(local_recs.len());
    let mut diffs = Vec::new();
    for i in 0..common {
        if http_recs[i] != local_recs[i] {
            diffs.push((
                i,
                local_recs[i].clone(),
                http_recs[i].clone(),
            ));
        }
    }
    eprintln!("[diag] 共对比 {common} 包，差异 {}(个)", diffs.len());
    for (idx, local, http) in diffs.iter().take(5) {
        eprintln!(
            "差异 @ #{idx}:\n  本地: stream={} size={} pos={} head={:02X?}\n  HTTP: stream={} size={} pos={} head={:02X?}",
            local.stream, local.size, local.pos, local.head,
            http.stream, http.size, http.pos, http.head
        );
    }
    assert!(
        diffs.is_empty(),
        "HTTP 与本地 demux 序列不一致（HTTP 读取 bug，首个差异 @ #{}）",
        diffs.first().map(|(i, _, _)| *i).unwrap_or(usize::MAX)
    );
}
