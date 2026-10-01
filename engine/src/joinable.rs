//! Joins the private Matroska replay into one continuous stream for a single
//! copy-only progressive reader.
//!
//! The private segment muxer writes every `segment-*.mkv` as a complete
//! Matroska file (EBML header, Segment, Tracks, Clusters, Cues). Concatenated
//! byte for byte they are not one stream: a single demuxer meets a second EBML
//! header inside its Segment and resyncs about one segment backwards, which a
//! copy-only Matroska muxer cannot absorb. This feeder instead writes the EBML
//! header, an unknown-size Segment, Info and Tracks once, then only the
//! Clusters of each listed segment in order. Clusters carry absolute
//! timestamps and every segment starts on a keyframe, so the header plus any
//! run of later segments is one valid, continuous Matroska stream.
//!
//! The replay on disk is unchanged: the shared HLS outputs that read the same
//! private playlist keep their qualified path.
use super::*;
use tokio::io::{AsyncRead, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CLUSTER: u32 = 0x1F43_B675;
/// Header elements are small. Media is copied in bounded blocks, never
/// buffered whole.
const HEADER_LIMIT: u64 = 1024 * 1024;
/// A Segment element whose 8-byte size is "unknown": the joined stream has no
/// end the reader could reach early.
const UNKNOWN_SEGMENT: [u8; 12] = [
    0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
];
/// How often the feeder looks for a newly listed segment.
const POLL: Duration = Duration::from_millis(200);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum JoinError {
    /// The replay ended, was torn down, or evicted a segment this reader had
    /// not reached yet. The reader cannot continue without a gap.
    Replay,
    /// A segment is not the Matroska layout the private muxer writes.
    Layout,
    /// The reader stopped accepting input.
    Reader,
}

/// One EBML element header: its raw bytes, ID and size (`None` if unknown).
struct Element {
    raw: Vec<u8>,
    id: u32,
    size: Option<u64>,
}

async fn byte(input: &mut (impl AsyncRead + Unpin)) -> Result<Option<u8>, JoinError> {
    let mut one = [0u8; 1];
    match input.read(&mut one).await {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(one[0])),
        Err(_) => Err(JoinError::Replay),
    }
}

/// Reads an element header, or `None` at a clean end of file.
async fn element(input: &mut (impl AsyncRead + Unpin)) -> Result<Option<Element>, JoinError> {
    let Some(first) = byte(input).await? else {
        return Ok(None);
    };
    let id_len = first.leading_zeros() as usize + 1;
    if id_len > 4 {
        return Err(JoinError::Layout);
    }
    let mut raw = vec![first];
    let mut id = u32::from(first);
    for _ in 1..id_len {
        let next = byte(input).await?.ok_or(JoinError::Layout)?;
        raw.push(next);
        id = id << 8 | u32::from(next);
    }
    let first = byte(input).await?.ok_or(JoinError::Layout)?;
    let size_len = first.leading_zeros() as usize + 1;
    if size_len > 8 {
        return Err(JoinError::Layout);
    }
    raw.push(first);
    let mask = if size_len == 8 { 0 } else { 0xFFu8 >> size_len };
    let mut size = u64::from(first & mask);
    let mut unknown = first & mask == mask;
    for _ in 1..size_len {
        let next = byte(input).await?.ok_or(JoinError::Layout)?;
        raw.push(next);
        unknown &= next == 0xFF;
        size = size << 8 | u64::from(next);
    }
    Ok(Some(Element {
        raw,
        id,
        size: (!unknown).then_some(size),
    }))
}

async fn skip(input: &mut (impl AsyncSeek + Unpin), size: u64) -> Result<(), JoinError> {
    let offset = i64::try_from(size).map_err(|_| JoinError::Layout)?;
    input
        .seek(std::io::SeekFrom::Current(offset))
        .await
        .map(drop)
        .map_err(|_| JoinError::Replay)
}

/// Copies exactly `size` bytes; a short segment is a layout failure.
async fn copy(
    input: &mut (impl AsyncRead + Unpin),
    output: &mut (impl AsyncWrite + Unpin),
    size: u64,
) -> Result<(), JoinError> {
    let mut block = vec![0u8; 64 * 1024];
    let mut left = size;
    while left > 0 {
        let want = block.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let read = input
            .read(&mut block[..want])
            .await
            .map_err(|_| JoinError::Replay)?;
        if read == 0 {
            return Err(JoinError::Layout);
        }
        output
            .write_all(&block[..read])
            .await
            .map_err(|_| JoinError::Reader)?;
        left -= read as u64;
    }
    Ok(())
}

/// Writes one complete private segment into the joined stream. With `header`,
/// the EBML header, an unknown-size Segment, Info and Tracks come first. Only
/// Clusters are carried from every segment; SeekHead, Cues, Tags and Void
/// describe that one file and are dropped.
pub(super) async fn write_segment(
    input: &mut (impl AsyncRead + AsyncSeek + Unpin),
    output: &mut (impl AsyncWrite + Unpin),
    header: bool,
) -> Result<(), JoinError> {
    let ebml = element(input).await?.ok_or(JoinError::Layout)?;
    let ebml_size = ebml
        .size
        .filter(|size| ebml.id == EBML && *size <= HEADER_LIMIT)
        .ok_or(JoinError::Layout)?;
    if header {
        output
            .write_all(&ebml.raw)
            .await
            .map_err(|_| JoinError::Reader)?;
        copy(input, output, ebml_size).await?;
    } else {
        skip(input, ebml_size).await?;
    }
    let segment = element(input).await?.ok_or(JoinError::Layout)?;
    if segment.id != SEGMENT {
        return Err(JoinError::Layout);
    }
    let mut left = segment.size;
    if header {
        output
            .write_all(&UNKNOWN_SEGMENT)
            .await
            .map_err(|_| JoinError::Reader)?;
    }
    let (mut info, mut tracks, mut clusters) = (false, false, 0usize);
    while left != Some(0) {
        let Some(child) = element(input).await? else {
            // A known-size Segment that ends early is a truncated file.
            if left.is_some() {
                return Err(JoinError::Layout);
            }
            break;
        };
        // Every element of a finished private segment has a known size.
        let size = child.size.ok_or(JoinError::Layout)?;
        if let Some(remaining) = left.as_mut() {
            *remaining = remaining
                .checked_sub(child.raw.len() as u64 + size)
                .ok_or(JoinError::Layout)?;
        }
        let carried = match child.id {
            CLUSTER => {
                if header && !(info && tracks) {
                    return Err(JoinError::Layout);
                }
                clusters += 1;
                true
            }
            INFO | TRACKS if clusters == 0 => {
                let seen = if child.id == INFO {
                    &mut info
                } else {
                    &mut tracks
                };
                if *seen || size > HEADER_LIMIT {
                    return Err(JoinError::Layout);
                }
                *seen = true;
                header
            }
            _ => false,
        };
        if carried {
            output
                .write_all(&child.raw)
                .await
                .map_err(|_| JoinError::Reader)?;
            copy(input, output, size).await?;
        } else {
            skip(input, size).await?;
        }
    }
    if clusters == 0 {
        return Err(JoinError::Layout);
    }
    Ok(())
}

/// The listed segment numbers, and whether the replay has ended.
async fn listing(dir: &std::path::Path) -> Result<(Vec<u64>, bool), JoinError> {
    let text = tokio::fs::read_to_string(dir.join("index.m3u8"))
        .await
        .map_err(|_| JoinError::Replay)?;
    let numbers = ingest::matroska_playlist(&text).map_err(|_| JoinError::Replay)?;
    Ok((numbers, text.lines().any(|line| line == "#EXT-X-ENDLIST")))
}

/// Follows the private Matroska replay in `dir` from the live edge (the
/// second-newest listed segment, a keyframe boundary) and writes the joined
/// stream to `output`. It never returns success: a live stream has no clean
/// end, so the replay ending or failing, or the reader stopping, is an error.
pub(super) async fn follow(
    dir: &std::path::Path,
    output: &mut (impl AsyncWrite + Unpin),
) -> JoinError {
    let mut next = None;
    loop {
        let (numbers, ended) = match listing(dir).await {
            Ok(listing) => listing,
            Err(error) => return error,
        };
        let header = next.is_none();
        if header {
            next = numbers.get(numbers.len().saturating_sub(2)).copied();
        }
        if let (Some(start), Some(&newest)) = (next.as_mut(), numbers.last()) {
            let mut first = header;
            while *start <= newest {
                // A segment evicted before this reader reached it is a gap the
                // reader cannot cross: fail rather than skip media.
                let Ok(file) =
                    tokio::fs::File::open(dir.join(format!("segment-{start:09}.mkv"))).await
                else {
                    return JoinError::Replay;
                };
                let mut file = tokio::io::BufReader::new(file);
                if let Err(error) = write_segment(&mut file, output, first).await {
                    return error;
                }
                first = false;
                *start += 1;
            }
            if output.flush().await.is_err() {
                return JoinError::Reader;
            }
        }
        if ended {
            return JoinError::Replay;
        }
        sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One element with a 1-byte size (bodies here are under 127 bytes).
    fn el(id: &[u8], body: &[u8]) -> Vec<u8> {
        assert!(body.len() < 127);
        [id, &[0x80 | body.len() as u8], body].concat()
    }
    fn ebml() -> Vec<u8> {
        el(&[0x1A, 0x45, 0xDF, 0xA3], &el(&[0x42, 0x82], b"matroska"))
    }
    fn info(tag: u8) -> Vec<u8> {
        el(&[0x15, 0x49, 0xA9, 0x66], &[tag; 4])
    }
    fn tracks(tag: u8) -> Vec<u8> {
        el(&[0x16, 0x54, 0xAE, 0x6B], &[tag; 6])
    }
    fn cluster(tag: u8) -> Vec<u8> {
        el(&[0x1F, 0x43, 0xB6, 0x75], &[tag; 9])
    }
    /// A complete private segment as the segment muxer writes it: a known-size
    /// Segment with SeekHead, Void, Info, Tracks, Clusters, Cues and Tags.
    fn segment(tag: u8) -> Vec<u8> {
        let body = [
            el(&[0x11, 0x4D, 0x9B, 0x74], &[1; 3]),
            el(&[0xEC], &[0; 2]),
            info(tag),
            tracks(tag),
            cluster(tag),
            cluster(tag + 1),
            el(&[0x1C, 0x53, 0xBB, 0x6B], &[2; 5]),
            el(&[0x12, 0x54, 0xC3, 0x67], &[3; 5]),
        ]
        .concat();
        // An 8-byte known Segment size, as FFmpeg patches it on a seekable file.
        let mut size = vec![0x01, 0, 0, 0, 0, 0, 0];
        size.push(body.len() as u8);
        [ebml(), vec![0x18, 0x53, 0x80, 0x67], size, body].concat()
    }
    async fn join(bytes: &[u8], header: bool) -> Result<Vec<u8>, JoinError> {
        let mut out = Vec::new();
        write_segment(&mut std::io::Cursor::new(bytes), &mut out, header).await?;
        Ok(out)
    }

    #[tokio::test]
    async fn header_once_then_only_clusters_from_complete_segments() {
        let first = join(&segment(0x10), true).await.unwrap();
        assert_eq!(
            first,
            [
                ebml(),
                UNKNOWN_SEGMENT.to_vec(),
                info(0x10),
                tracks(0x10),
                cluster(0x10),
                cluster(0x11)
            ]
            .concat(),
            "no SeekHead, Void, Cues or Tags; the Segment never ends"
        );
        assert_eq!(
            join(&segment(0x20), false).await.unwrap(),
            [cluster(0x20), cluster(0x21)].concat(),
            "a later segment adds media only: no second EBML header"
        );
    }

    #[tokio::test]
    async fn unexpected_layouts_fail_closed() {
        let good = segment(0x10);
        // Truncated before the Segment's end or inside a cluster, or no
        // Matroska header at all.
        for cut in [20, 25] {
            assert_eq!(
                join(&good[..good.len() - cut], false).await,
                Err(JoinError::Layout)
            );
        }
        assert_eq!(
            join(&good[ebml().len()..], true).await,
            Err(JoinError::Layout)
        );
        // An unknown-size element: an unfinished file is never joined.
        let mut open = good.clone();
        let at = open
            .windows(4)
            .position(|w| w == [0x1F, 0x43, 0xB6, 0x75])
            .unwrap();
        open[at + 4] = 0xFF;
        assert_eq!(join(&open, false).await, Err(JoinError::Layout));
        // Media before Tracks cannot start a stream.
        let bare = [ebml(), vec![0x18, 0x53, 0x80, 0x67, 0x80 | 11], cluster(1)].concat();
        assert_eq!(join(&bare, true).await, Err(JoinError::Layout));
        // A segment without media is not a keyframe boundary.
        let empty = [ebml(), vec![0x18, 0x53, 0x80, 0x67, 0x80 | 6], info(1)].concat();
        assert_eq!(join(&empty, false).await, Err(JoinError::Layout));
        // A reader that stopped reading ends the join.
        struct Closed;
        impl AsyncWrite for Closed {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        assert_eq!(
            write_segment(&mut std::io::Cursor::new(&good), &mut Closed, true).await,
            Err(JoinError::Reader)
        );
    }

    fn playlist(numbers: std::ops::Range<u64>, ended: bool) -> String {
        let mut text = "#EXTM3U\n#EXT-X-VERSION:3\n".to_owned();
        for number in numbers {
            text += &format!("#EXTINF:2.0,\nsegment-{number:09}.mkv\n");
        }
        if ended {
            text += "#EXT-X-ENDLIST\n";
        }
        text
    }

    #[tokio::test]
    async fn follows_from_the_live_edge_and_never_ends_cleanly() {
        let root = tempfile::tempdir().unwrap();
        for number in 4..9u8 {
            tokio::fs::write(
                root.path().join(format!("segment-{number:09}.mkv")),
                segment(number * 2),
            )
            .await
            .unwrap();
        }
        tokio::fs::write(root.path().join("index.m3u8"), playlist(4..7, false))
            .await
            .unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(1024 * 1024);
        let dir = root.path().to_owned();
        let follower = tokio::spawn(async move { follow(&dir, &mut writer).await });
        sleep(POLL * 2).await;
        // Two more segments are listed, then the replay ends.
        tokio::fs::write(root.path().join("index.m3u8"), playlist(4..9, true))
            .await
            .unwrap();
        let ended = timeout(Duration::from_secs(5), follower)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ended, JoinError::Replay, "a live join has no clean end");
        let mut joined = Vec::new();
        reader.read_to_end(&mut joined).await.unwrap();
        // Starts at the second-newest listed segment (5), header once.
        let expected = [
            ebml(),
            UNKNOWN_SEGMENT.to_vec(),
            info(10),
            tracks(10),
            cluster(10),
            cluster(11),
            cluster(12),
            cluster(13),
            cluster(14),
            cluster(15),
            cluster(16),
            cluster(17),
        ]
        .concat();
        assert_eq!(joined, expected);
    }

    #[tokio::test]
    async fn an_evicted_or_vanished_replay_fails_the_join() {
        let root = tempfile::tempdir().unwrap();
        // Listed 3..6 but segment 4 is already gone: never skip media.
        for number in [3u8, 5] {
            tokio::fs::write(
                root.path().join(format!("segment-{number:09}.mkv")),
                segment(number),
            )
            .await
            .unwrap();
        }
        tokio::fs::write(root.path().join("index.m3u8"), playlist(3..6, false))
            .await
            .unwrap();
        let mut sink = Vec::new();
        assert_eq!(follow(root.path(), &mut sink).await, JoinError::Replay);
        // A torn-down replay directory, or a playlist naming foreign paths.
        let missing = root.path().join("missing");
        assert_eq!(follow(&missing, &mut sink).await, JoinError::Replay);
        tokio::fs::write(root.path().join("index.m3u8"), "#EXTM3U\n../private.mkv\n")
            .await
            .unwrap();
        assert_eq!(follow(root.path(), &mut sink).await, JoinError::Replay);
    }

    /// Packet timestamps per stream from ffprobe's CSV output.
    fn backward_jumps(csv: &str) -> usize {
        let mut last: HashMap<String, f64> = HashMap::new();
        let mut jumps = 0;
        for line in csv.lines() {
            let fields: Vec<_> = line.split(',').collect();
            let (Some(stream), Some(Ok(time))) =
                (fields.get(1), fields.get(2).map(|t| t.parse::<f64>()))
            else {
                continue;
            };
            if last.get(*stream).is_some_and(|prev| time < prev - 0.5) {
                jumps += 1;
            }
            last.insert((*stream).to_owned(), time);
        }
        jumps
    }

    #[tokio::test]
    #[ignore = "requires configured real FFmpeg/ffprobe; local Matroska replay only"]
    async fn real_matroska_replay_joins_into_continuous_copy_only_output() {
        let ffmpeg = std::env::var("PLAYBACK_TEST_FFMPEG").unwrap();
        let ffprobe = std::env::var("PLAYBACK_TEST_FFPROBE").unwrap();
        for video in ["ffv1", "libx264"] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("source.mkv");
            let made = std::process::Command::new(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
                .arg("testsrc=size=320x180:rate=25")
                .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
                .args(["-t", "14", "-c:v", video, "-g", "25", "-bf", "2"])
                .args(["-c:a", "flac", "-f", "matroska"])
                .arg(&source)
                .status()
                .unwrap();
            assert!(made.success());
            // The exact private replay muxer the shared ingest runs.
            let replay = root.path().join("replay");
            std::fs::create_dir(&replay).unwrap();
            let mut ingest = Command::new(&ffmpeg);
            ingest.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"]);
            ingest.arg(&source).args([
                "-map",
                "0:0",
                "-map",
                "0:1",
                "-c",
                "copy",
                "-dn",
                "-map_metadata",
                "-1",
                "-map_chapters",
                "-1",
            ]);
            ingest::matroska_replay_args(&mut ingest, &replay);
            assert!(ingest.status().await.unwrap().success());
            let segments = ingest::matroska_playlist(
                &std::fs::read_to_string(replay.join("index.m3u8")).unwrap(),
            )
            .unwrap();
            assert!(segments.len() >= 6, "{video}: {segments:?}");

            let packets = |path: &std::path::Path| {
                let out = std::process::Command::new(&ffprobe)
                    .args([
                        "-v",
                        "quiet",
                        "-show_entries",
                        "packet=stream_index,pts_time",
                    ])
                    .args(["-of", "csv"])
                    .arg(path)
                    .output()
                    .unwrap();
                String::from_utf8(out.stdout).unwrap()
            };
            let newest = |csv: &str| {
                csv.lines()
                    .filter_map(|l| l.split(',').nth(2)?.parse::<f64>().ok())
                    .fold(0.0, f64::max)
            };
            let expected = 2.0 * (segments.len() - 1) as f64;

            // The root cause: the HLS demuxer joins complete segment files
            // into one byte stream, meets a second EBML header and resyncs,
            // so the copy-only output jumps backwards or loses media.
            let hls_path = root.path().join("hls-reader.mkv");
            let mut hls = Command::new(&ffmpeg);
            progressive::reader_args(
                &mut hls,
                progressive::ReaderInput::Playlist {
                    extensions: "mkv,m3u8",
                    playlist: &replay.join("index.m3u8"),
                },
                Some(1),
                progressive::Container::Matroska,
            );
            let hls = hls.stdout(Stdio::piped()).stderr(Stdio::null());
            let hls = timeout(Duration::from_secs(60), hls.output())
                .await
                .unwrap()
                .unwrap();
            std::fs::write(&hls_path, &hls.stdout).unwrap();
            let csv = packets(&hls_path);
            assert!(
                backward_jumps(&csv) > 0 || newest(&csv) < expected,
                "{video}: the HLS-demuxer reader reproduces the defect"
            );

            // A late joiner: header plus segments from the second, through the
            // copy-only reader exactly as a progressive viewer runs it.
            let mut joined = Vec::new();
            for (position, number) in segments.iter().enumerate().skip(1) {
                let file = tokio::fs::File::open(replay.join(format!("segment-{number:09}.mkv")))
                    .await
                    .unwrap();
                write_segment(
                    &mut tokio::io::BufReader::new(file),
                    &mut joined,
                    position == 1,
                )
                .await
                .unwrap();
            }
            let mut reader = Command::new(&ffmpeg);
            progressive::reader_args(
                &mut reader,
                progressive::ReaderInput::Joined,
                Some(1),
                progressive::Container::Matroska,
            );
            let mut child = reader
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let feed = tokio::spawn(async move {
                stdin.write_all(&joined).await.unwrap();
            });
            let output = child.wait_with_output().await.unwrap();
            feed.await.unwrap();
            assert!(output.status.success(), "{video}: reader");
            let out_path = root.path().join("viewer.mkv");
            std::fs::write(&out_path, &output.stdout).unwrap();
            let csv = packets(&out_path);
            assert_eq!(backward_jumps(&csv), 0, "{video}: continuous timestamps");
            let last = newest(&csv);
            assert!(
                (last - expected).abs() < 2.5,
                "{video}: every joined segment is delivered ({last} of ~{expected} s)"
            );
            let first_video = std::process::Command::new(&ffprobe)
                .args(["-v", "error", "-select_streams", "v:0", "-read_intervals"])
                .args(["%+#1", "-show_entries", "packet=flags", "-of", "csv=p=0"])
                .arg(&out_path)
                .output()
                .unwrap();
            assert!(
                String::from_utf8_lossy(&first_video.stdout).starts_with('K'),
                "{video}: a late joiner starts on a keyframe"
            );
            let decoded = std::process::Command::new(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-xerror", "-i"])
                .arg(&out_path)
                .args(["-f", "null", "-"])
                .status()
                .unwrap();
            assert!(decoded.success(), "{video}: decodes with -xerror");
        }
    }
}
