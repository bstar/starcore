//! Bounded continuous HLS. One supervised FFmpeg timeline per rendition preserves
//! audio/video timestamps across segments. Encoding pauses ahead of the consumer
//! and restarts at the requested source position for uncached seeks.
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

const CACHE_BYTES: u64 = 384 * 1024 * 1024;
const MAX_SEGMENT_BYTES: u64 = 48 * 1024 * 1024;
#[derive(Clone, Copy, Debug)]
enum Encoder {
    Vaapi,
    VideoToolbox,
    SoftwareHevc,
    Software,
}
impl Encoder {
    fn name(self) -> &'static str {
        match self {
            Self::Vaapi => "hevc_vaapi",
            Self::VideoToolbox => "hevc_videotoolbox",
            Self::SoftwareHevc => "libx265",
            Self::Software => "libx264",
        }
    }
    fn hevc(self) -> bool {
        !matches!(self, Self::Software)
    }
    fn default_max_height(self) -> u32 {
        // Software 4K can take several playback intervals per fragment on grainy
        // features. Advertising it lets AVPlayer select by network bandwidth even
        // though the just-in-time encoder cannot replenish that buffer in time.
        match self {
            Self::SoftwareHevc | Self::Software => 1080,
            Self::Vaapi | Self::VideoToolbox => 2160,
        }
    }
    fn input(self, c: &mut Command) {
        if matches!(self, Self::Vaapi) {
            c.args(["-vaapi_device", "/dev/dri/renderD128"]);
        }
    }
}
fn max_height(encoder: Encoder, setting: Option<&str>) -> u32 {
    setting
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|height| [480, 720, 1080, 2160].contains(height))
        .unwrap_or_else(|| encoder.default_max_height())
}
fn ffmpeg() -> String {
    std::env::var("STAR_STREAM_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}
fn ffprobe() -> String {
    std::env::var("STAR_STREAM_FFPROBE").unwrap_or_else(|_| "ffprobe".into())
}
fn run(mut command: Command, stop: &AtomicBool, seconds: u64) -> Result<Vec<u8>> {
    let mut stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let mut child = command.spawn().context("Start streaming codec tool")?;
    let began = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!(
                    "Streaming codec failed ({}); check host codec support",
                    status
                );
            }
            ensure!(
                stdout.metadata()?.len() <= 1024 * 1024,
                "Codec metadata exceeds limit"
            );
            stdout.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
        if stop.load(Ordering::Acquire)
            || began.elapsed() > Duration::from_secs(seconds)
            || stderr.metadata()?.len() > 64 * 1024
        {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Streaming codec cancelled or timed out");
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}
fn encoder(stop: &AtomicBool) -> Encoder {
    static SELECTED: OnceLock<Encoder> = OnceLock::new();
    *SELECTED.get_or_init(|| {
        if std::env::var("STAR_STREAM_ENCODER").as_deref() == Ok("software") {
            return Encoder::Software;
        }
        let candidates = if cfg!(target_os = "linux") {
            // The Radeon 8060S can report successful HEVC encoding and decoding
            // while producing visibly corrupt frames, including in CQP mode.
            // A successful codec probe cannot establish picture correctness.
            // Keep VAAPI opt-in until its driver is verified with real footage.
            if std::env::var("STAR_STREAM_ENCODER").as_deref() == Ok("vaapi") {
                vec![Encoder::Vaapi, Encoder::SoftwareHevc]
            } else {
                vec![Encoder::SoftwareHevc]
            }
        } else if cfg!(target_os = "macos") {
            vec![Encoder::VideoToolbox]
        } else {
            vec![]
        };
        for candidate in candidates {
            let mut c = Command::new(ffmpeg());
            c.args(["-hide_banner", "-loglevel", "error"]);
            candidate.input(&mut c);
            c.args([
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=192x128:rate=24",
                "-frames:v",
                "2",
            ]);
            if matches!(candidate, Encoder::Vaapi) {
                c.args(["-vf", "format=nv12,hwupload"]);
            }
            c.args(["-c:v", candidate.name()]);
            if matches!(candidate, Encoder::Vaapi) {
                c.args(["-rc_mode", "CQP", "-qp", "26"]);
            } else {
                c.args(["-b:v", "500k"]);
            }
            if matches!(candidate, Encoder::SoftwareHevc) {
                c.args([
                    "-preset",
                    "veryfast",
                    "-forced-idr",
                    "1",
                    "-x265-params",
                    "pools=4:frame-threads=2:log-level=error",
                ]);
            }
            c.args(["-f", "null", "-"]);
            if run(c, stop, 10).is_ok() {
                return candidate;
            }
        }
        Encoder::Software
    })
}
#[derive(Clone)]
struct Quality {
    name: String,
    width: u32,
    height: u32,
    rate: u32,
}
/// Convert MP4 edit-list offsets into explicit fragment decode timestamps.
/// HLS has one stable initialization section; seek offsets cannot live in a
/// different edit list for every encoder job. All tracks use a shared 1-second
/// origin, leaving room for AAC preroll without unsigned timestamp underflow.
fn mp4_boxes(bytes: &[u8]) -> Result<Vec<(usize, usize, [u8; 4])>> {
    let mut result = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        ensure!(at + 8 <= bytes.len(), "Truncated MP4 box");
        let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        ensure!(
            size >= 8 && size <= bytes.len() - at,
            "Invalid MP4 box size"
        );
        result.push((at, size, bytes[at + 4..at + 8].try_into().unwrap()));
        at += size;
    }
    Ok(result)
}
fn box_child<'a>(bytes: &'a [u8], kind: &[u8; 4]) -> Result<&'a [u8]> {
    for (at, size, name) in mp4_boxes(bytes)? {
        if &name == kind {
            return Ok(&bytes[at + 8..at + size]);
        }
    }
    bail!("Missing MP4 {}", String::from_utf8_lossy(kind))
}
fn fullbox_time_scale(bytes: &[u8]) -> Result<u32> {
    ensure!(bytes.len() >= 4, "Truncated MP4 full box");
    let at = if bytes[0] == 1 { 20 } else { 12 };
    ensure!(at + 4 <= bytes.len(), "Truncated MP4 time scale");
    let value = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    ensure!(value > 0, "Invalid MP4 time scale");
    Ok(value)
}
fn without_box(bytes: &[u8], kind: &[u8; 4]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (at, size, name) in mp4_boxes(bytes)? {
        if &name != kind {
            out.extend_from_slice(&bytes[at..at + size]);
        }
    }
    Ok(out)
}
fn wrap_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(body.len() + 8);
    result.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    result.extend_from_slice(kind);
    result.extend_from_slice(body);
    result
}
fn continuous_init(bytes: &[u8], logical_start: f64) -> Result<(Vec<u8>, HashMap<u32, i64>)> {
    ensure!(
        bytes.len() <= 256 * 1024,
        "MP4 initialization exceeds limit"
    );
    let moov = box_child(bytes, b"moov")?;
    let movie_scale = fullbox_time_scale(box_child(moov, b"mvhd")?)?;
    let mut tracks = Vec::new();
    let mut video_offset = None;
    for (at, size, name) in mp4_boxes(moov)? {
        if &name != b"trak" {
            continue;
        }
        let track = &moov[at + 8..at + size];
        let header = box_child(track, b"tkhd")?;
        ensure!(header.len() >= 24, "Truncated MP4 track header");
        let id_at = if header[0] == 1 { 20 } else { 12 };
        let id = u32::from_be_bytes(header[id_at..id_at + 4].try_into().unwrap());
        let mdia = box_child(track, b"mdia")?;
        let scale = fullbox_time_scale(box_child(mdia, b"mdhd")?)?;
        let handler = box_child(mdia, b"hdlr")?;
        ensure!(handler.len() >= 12, "Truncated MP4 handler");
        let mut offset = 0.;
        if let Ok(edts) = box_child(track, b"edts") {
            let list = box_child(edts, b"elst")?;
            ensure!(list.len() >= 8, "Truncated edit list");
            let count = u32::from_be_bytes(list[4..8].try_into().unwrap()) as usize;
            let width = if list[0] == 1 { 20 } else { 12 };
            ensure!(
                count <= 16 && list.len() == 8 + count * width,
                "Invalid MP4 edit list"
            );
            for n in 0..count {
                let p = 8 + n * width;
                let (duration, media) = if list[0] == 1 {
                    (
                        u64::from_be_bytes(list[p..p + 8].try_into().unwrap()) as f64,
                        i64::from_be_bytes(list[p + 8..p + 16].try_into().unwrap()),
                    )
                } else {
                    (
                        u32::from_be_bytes(list[p..p + 4].try_into().unwrap()) as f64,
                        i32::from_be_bytes(list[p + 4..p + 8].try_into().unwrap()) as i64,
                    )
                };
                if media == -1 {
                    offset += duration / f64::from(movie_scale);
                } else {
                    ensure!(media >= 0, "Invalid media edit");
                    offset -= media as f64 / f64::from(scale);
                    break;
                }
            }
        }
        if &handler[8..12] == b"vide" {
            video_offset = Some(offset);
        }
        tracks.push((id, scale, offset));
    }
    let shift =
        1. + logical_start - video_offset.context("MP4 initialization has no video clock")?;
    let offsets = tracks
        .into_iter()
        .map(|(id, scale, offset)| (id, ((offset + shift) * f64::from(scale)).round() as i64))
        .collect();
    let mut new_moov = Vec::new();
    for (at, size, name) in mp4_boxes(moov)? {
        if &name == b"trak" {
            new_moov.extend(wrap_box(
                b"trak",
                &without_box(&moov[at + 8..at + size], b"edts")?,
            ));
        } else {
            new_moov.extend_from_slice(&moov[at..at + size]);
        }
    }
    let mut init = Vec::new();
    for (at, size, name) in mp4_boxes(bytes)? {
        if &name == b"moov" {
            init.extend(wrap_box(b"moov", &new_moov));
        } else {
            init.extend_from_slice(&bytes[at..at + size]);
        }
    }
    Ok((init, offsets))
}
fn continuous_fragment(path: &Path, offsets: &HashMap<u32, i64>, index: u32) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let size = file.metadata()?.len();
    ensure!(
        size <= MAX_SEGMENT_BYTES,
        "Encoded HLS segment exceeds size limit"
    );
    let mut at = 0;
    let mut moofs = 0;
    while at + 8 <= size {
        file.seek(SeekFrom::Start(at))?;
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let length = u64::from(u32::from_be_bytes(header[..4].try_into().unwrap()));
        ensure!(
            length >= 8 && length <= size - at,
            "Invalid HLS MP4 fragment"
        );
        ensure!(
            &header[4..] != b"sidx",
            "HLS encoder must omit segment indexes"
        );
        if &header[4..] == b"moof" {
            ensure!(length <= 1024 * 1024, "MP4 fragment metadata exceeds limit");
            let mut body = vec![0; (length - 8) as usize];
            file.read_exact(&mut body)?;
            for (offset, n, name) in mp4_boxes(&body)? {
                if &name == b"mfhd" {
                    ensure!(n >= 16, "Invalid fragment sequence header");
                    body[offset + 12..offset + 16].copy_from_slice(&(index + 1).to_be_bytes());
                }
                if &name != b"traf" {
                    continue;
                }
                let traf = &body[offset + 8..offset + n];
                let tfhd = box_child(traf, b"tfhd")?;
                ensure!(tfhd.len() >= 8, "Truncated fragment track header");
                let id = u32::from_be_bytes(tfhd[4..8].try_into().unwrap());
                let adjustment = *offsets.get(&id).context("Unknown fragment track clock")?;
                let mut found = false;
                for (child_at, child_size, kind) in mp4_boxes(traf)? {
                    if &kind != b"tfdt" {
                        continue;
                    }
                    let p = offset + 8 + child_at + 8;
                    let version = body[p];
                    let value = if version == 1 {
                        ensure!(child_size >= 20, "Truncated 64-bit decode time");
                        u64::from_be_bytes(body[p + 4..p + 12].try_into().unwrap())
                    } else {
                        ensure!(version == 0 && child_size >= 16, "Invalid decode time");
                        u64::from(u32::from_be_bytes(body[p + 4..p + 8].try_into().unwrap()))
                    };
                    let adjusted = i128::from(value) + i128::from(adjustment);
                    ensure!(
                        adjusted >= 0 && adjusted <= u64::MAX as i128,
                        "Invalid absolute media clock"
                    );
                    if version == 1 {
                        body[p + 4..p + 12].copy_from_slice(&(adjusted as u64).to_be_bytes());
                    } else {
                        ensure!(adjusted <= u32::MAX as i128, "32-bit media clock overflow");
                        body[p + 4..p + 8].copy_from_slice(&(adjusted as u32).to_be_bytes());
                    }
                    found = true;
                }
                ensure!(found, "Missing fragment decode clock");
            }
            file.seek(SeekFrom::Start(at + 8))?;
            file.write_all(&body)?;
            moofs += 1;
        }
        at += length;
    }
    ensure!(at == size && moofs == 1, "Invalid HLS fragment layout");
    Ok(())
}

struct Cache {
    jobs: HashMap<String, Arc<EncoderJob>>,
    inits: HashMap<String, tempfile::NamedTempFile>,
}
struct EncoderJob {
    directory: Arc<tempfile::TempDir>,
    start: u32,
    wanted: Arc<AtomicU32>,
    touched: Arc<Mutex<Instant>>,
    cancelled: Arc<AtomicBool>,
    state: Arc<Mutex<JobState>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}
#[derive(Default)]
struct JobState {
    finished: bool,
    error: Option<String>,
    bytes: u64,
    segments: usize,
    ready: HashSet<u32>,
}
impl EncoderJob {
    fn segment(&self, index: u32) -> PathBuf {
        self.directory
            .path()
            .join(format!("segment-{index:010}.m4s"))
    }
    fn signal(pid: u32, signal: rustix::process::Signal) {
        if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
            let _ = rustix::process::kill_process_group(pid, signal);
        }
    }
    fn start(
        mut command: Command,
        directory: tempfile::TempDir,
        start: u32,
        budget: u64,
        logical_start: f64,
    ) -> Result<Arc<Self>> {
        use std::os::unix::process::CommandExt;
        let stderr = tempfile::tempfile()?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr.try_clone()?)
            .process_group(0);
        let mut child = command.spawn().context("Start continuous HLS encoder")?;
        let job = Arc::new(Self {
            directory: Arc::new(directory),
            start,
            wanted: Arc::new(AtomicU32::new(start)),
            touched: Arc::new(Mutex::new(Instant::now())),
            cancelled: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(JobState::default())),
            worker: Mutex::new(None),
        });
        let directory = job.directory.clone();
        let wanted = job.wanted.clone();
        let touched = job.touched.clone();
        let cancelled = job.cancelled.clone();
        let state = job.state.clone();
        let worker = std::thread::Builder::new()
            .name("star-hls-encoder".into())
            .spawn(move || {
                let outcome = (|| -> Result<()> {
                    let mut paused = false;
                    let mut clocks = None;
                    loop {
                        if cancelled.load(Ordering::Acquire)
                            || touched.lock().unwrap().elapsed() > Duration::from_secs(120)
                        {
                            break;
                        }
                        let exited = child.try_wait()?;
                        if let Some(status) = &exited {
                            if !status.success() {
                                let mut stderr = stderr;
                                stderr.seek(SeekFrom::Start(0))?;
                                let mut details = String::new();
                                stderr.take(4096).read_to_string(&mut details)?;
                                bail!("Continuous HLS encoder failed: {details}");
                            }
                        }
                        let target = wanted.load(Ordering::Acquire);
                        let mut files = Vec::new();
                        let mut bytes = 0;
                        for entry in std::fs::read_dir(directory.path())? {
                            let entry = entry?;
                            let length = entry.metadata()?.len();
                            bytes += length;
                            let name = entry.file_name().to_string_lossy().into_owned();
                            if let Some(index) = name
                                .strip_prefix("segment-")
                                .and_then(|n| n.strip_suffix(".m4s"))
                                .and_then(|n| n.parse::<u32>().ok())
                            {
                                ensure!(
                                    length <= MAX_SEGMENT_BYTES,
                                    "Encoded HLS segment exceeds size limit"
                                );
                                files.push((index, entry.path(), length));
                            }
                        }
                        files.sort_by_key(|v| v.0);
                        if !files.is_empty() && clocks.is_none() {
                            let (init, offsets) = continuous_init(
                                &std::fs::read(directory.path().join("init.mp4"))?,
                                logical_start,
                            )?;
                            std::fs::write(directory.path().join("clock-init.mp4"), init)?;
                            clocks = Some(offsets);
                        }
                        for (index, path, _) in &files {
                            if !state.lock().unwrap().ready.contains(index) {
                                continuous_fragment(
                                    path,
                                    clocks.as_ref().context("Missing HLS clock")?,
                                    *index,
                                )?;
                                state.lock().unwrap().ready.insert(*index);
                            }
                        }

                        // Reserve one maximum-sized in-progress segment in every job's budget.
                        let retain = budget.saturating_sub(MAX_SEGMENT_BYTES);
                        let mut retained = files.len();
                        for (index, path, length) in &files {
                            if *index < target.saturating_sub(16)
                                || bytes > retain && *index < target.saturating_sub(1)
                            {
                                std::fs::remove_file(path)?;
                                bytes = bytes.saturating_sub(*length);
                                retained -= 1;
                            }
                        }
                        ensure!(
                            bytes <= budget + 1024 * 1024,
                            "Continuous encoder exceeded rolling cache budget"
                        );
                        {
                            let mut value = state.lock().unwrap();
                            value.bytes = bytes;
                            value.segments = retained;
                            value.ready.retain(|index| {
                                directory
                                    .path()
                                    .join(format!("segment-{index:010}.m4s"))
                                    .is_file()
                            });
                        }
                        if exited.is_some() {
                            break;
                        }
                        let ahead = files
                            .iter()
                            .any(|(index, _, _)| *index >= target.saturating_add(2));
                        let should_pause = ahead
                            || bytes >= retain
                                && files.iter().any(|(index, _, _)| *index >= target);
                        if should_pause != paused {
                            Self::signal(
                                child.id(),
                                if should_pause {
                                    rustix::process::Signal::STOP
                                } else {
                                    rustix::process::Signal::CONT
                                },
                            );
                            paused = should_pause;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(())
                })();
                // SIGKILL works even when the bounded encoder is SIGSTOPped.
                if child.try_wait().ok().flatten().is_none() {
                    Self::signal(child.id(), rustix::process::Signal::KILL);
                    let _ = child.wait();
                }
                let mut value = state.lock().unwrap();
                value.finished = true;
                if let Err(error) = outcome {
                    value.error = Some(format!("{error:#}"));
                }
            })?;
        *job.worker.lock().unwrap() = Some(worker);
        Ok(job)
    }
    fn usable(&self, index: u32) -> bool {
        if index < self.start {
            return false;
        }
        if self.segment(index).is_file() {
            return true;
        }
        if self.state.lock().unwrap().finished {
            return false;
        }
        let target = self.wanted.load(Ordering::Acquire);
        index >= target.saturating_sub(1) && index <= target.saturating_add(4)
    }
    fn part(&self, index: u32, init: bool, stop: &AtomicBool) -> Result<Part> {
        *self.touched.lock().unwrap() = Instant::now();
        self.wanted.fetch_max(index, Ordering::AcqRel);
        let began = Instant::now();
        loop {
            ensure!(!stop.load(Ordering::Acquire), "HLS request cancelled");
            let path = if init {
                self.directory.path().join("clock-init.mp4")
            } else {
                self.segment(index)
            };
            // The segment is atomically renamed by FFmpeg. Its completion also
            // guarantees the initialization section has been fully written.
            if self.state.lock().unwrap().ready.contains(&index) {
                if let Ok(file) = File::open(&path) {
                    return Ok(Part {
                        length: file.metadata()?.len(),
                        file,
                        offset: 0,
                        mime: if init {
                            "video/mp4"
                        } else {
                            "video/iso.segment"
                        },
                    });
                }
            }
            let state = self.state.lock().unwrap();
            if let Some(error) = &state.error {
                bail!("{error}");
            }
            ensure!(
                !state.finished,
                "HLS encoder ended before the requested segment"
            );
            drop(state);
            ensure!(
                began.elapsed() < Duration::from_secs(60),
                "HLS segment encoding timed out"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for EncoderJob {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
    }
}
pub(super) struct Adaptive {
    source: PathBuf,
    duration: f64,
    step: f64,
    fps: String,
    hdr: bool,
    audio: bool,
    encoder: Encoder,
    qualities: Vec<Quality>,
    cache: Mutex<Cache>,
}
pub(super) struct Part {
    pub file: File,
    pub offset: u64,
    pub length: u64,
    pub mime: &'static str,
}
#[derive(Default)]
pub(super) struct Hls {
    movies: Mutex<HashMap<String, Arc<Adaptive>>>,
}
impl Hls {
    pub fn diagnostics(&self) -> Value {
        let Ok(movies) = self.movies.try_lock() else {
            return serde_json::json!({"busy":true});
        };
        let entries=movies.iter().map(|(id,movie)| {
            let cache=movie.cache.try_lock();
            serde_json::json!({"id":id,"fps":movie.fps,"qualities":movie.qualities.iter().map(|q|serde_json::json!({"name":q.name,"width":q.width,"height":q.height,"bitrate_bps":q.rate})).collect::<Vec<_>>(),"movie":movie.source.file_name().map(|n|n.to_string_lossy()),"encoder":movie.encoder.name(),"segment_seconds":movie.step,
                "cache_bytes":cache.as_ref().ok().map(|c|c.jobs.values().map(|j|j.state.lock().unwrap().bytes).sum::<u64>()),"cached_segments":cache.as_ref().ok().map(|c|c.jobs.values().map(|j|j.state.lock().unwrap().segments).sum::<usize>()),"timeline":"continuous","busy":cache.is_err()})
        }).collect::<Vec<_>>();
        serde_json::json!({"movies":entries,"cache_limit_per_movie_bytes":CACHE_BYTES,"segment_limit_bytes":MAX_SEGMENT_BYTES,
            "encoder_override":std::env::var("STAR_STREAM_ENCODER").ok()})
    }
    pub fn clear_idle(&self) -> Value {
        let mut cleared = 0;
        if let Ok(movies) = self.movies.try_lock() {
            for movie in movies.values() {
                if Arc::strong_count(movie) != 1 {
                    continue;
                }
                let Ok(mut cache) = movie.cache.try_lock() else {
                    continue;
                };
                cache.jobs.retain(|_, job| {
                    let state = job.state.lock().unwrap();
                    if state.finished {
                        cleared += state.segments;
                    }
                    !state.finished
                });
            }
        }
        serde_json::json!({"cleared_segments":cleared})
    }
    pub fn movie(&self, id: &str, source: &Path, stop: &AtomicBool) -> Result<Arc<Adaptive>> {
        let mut movies = self
            .movies
            .lock()
            .map_err(|_| anyhow::anyhow!("HLS manager unavailable"))?;
        if let Some(movie) = movies.get(id) {
            return Ok(movie.clone());
        }
        // At most two movie caches / encoder jobs per SSH service. An active
        // request retains its movie, so eviction never cancels a serving worker.
        if movies.len() >= 2 {
            let victim = movies
                .iter()
                .find(|(_, m)| Arc::strong_count(m) == 1)
                .map(|(id, _)| id.clone());
            if let Some(victim) = victim {
                movies.remove(&victim);
            } else {
                bail!("Two movies are already being prepared");
            }
        }
        let movie = Arc::new(Adaptive::new(source, stop)?);
        movies.insert(id.into(), movie.clone());
        Ok(movie)
    }
}
impl Adaptive {
    fn new(source: &Path, stop: &AtomicBool) -> Result<Self> {
        let mut c = Command::new(ffprobe());
        c.args([
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=codec_type,width,height,avg_frame_rate,color_transfer",
            "-of",
            "json",
        ])
        .arg(source);
        let info: Value = serde_json::from_slice(&run(c, stop, 15)?)?;
        let streams = info["streams"].as_array().context("No media streams")?;
        let video = streams
            .iter()
            .find(|s| s["codec_type"] == "video")
            .context("No movie video")?;
        let width = video["width"].as_u64().context("Missing movie width")? as u32;
        let height = video["height"].as_u64().context("Missing movie height")? as u32;
        ensure!(
            (2..=8192).contains(&width) && (2..=8192).contains(&height),
            "Unsupported movie dimensions"
        );
        let duration = info["format"]["duration"]
            .as_str()
            .context("Missing movie duration")?
            .parse::<f64>()?;
        ensure!(
            duration.is_finite() && duration > 0.0 && duration <= 24.0 * 3600.0,
            "Unsupported movie duration"
        );
        let fps_text = video["avg_frame_rate"].as_str().unwrap_or("24/1");
        let fps = fps_text
            .split_once('/')
            .and_then(|(a, b)| Some(a.parse::<f64>().ok()? / b.parse::<f64>().ok()?))
            .filter(|f| f.is_finite() && *f >= 1.0 && *f <= 60.0)
            .unwrap_or(24.0);
        let step = (6.0 * fps).round() / fps;
        let encoder = encoder(stop);
        let max_height = max_height(
            encoder,
            std::env::var("STAR_STREAM_MAX_HEIGHT").ok().as_deref(),
        );
        let mut qualities = Vec::new();
        for (name, w, h, rate) in [
            ("480", 854, 480, 1_200_000),
            ("720", 1280, 720, 2_800_000),
            ("1080", 1920, 1080, 6_000_000),
            ("2160", 3840, 2160, 20_000_000),
        ] {
            if h > max_height || (name == "2160" && !encoder.hevc()) {
                continue;
            }
            let scale = (f64::from(w) / f64::from(width))
                .min(f64::from(h) / f64::from(height))
                .min(1.0);
            let out_w = ((f64::from(width) * scale / 2.0).floor() as u32 * 2).max(2);
            let out_h = ((f64::from(height) * scale / 2.0).floor() as u32 * 2).max(2);
            if qualities
                .last()
                .is_some_and(|q: &Quality| q.width == out_w && q.height == out_h)
            {
                continue;
            }
            qualities.push(Quality {
                name: name.into(),
                width: out_w,
                height: out_h,
                rate,
            });
            if scale >= 1.0 {
                break;
            }
        }
        Ok(Self {
            source: source.into(),
            duration,
            step,
            fps: format!("{fps:.9}"),
            hdr: matches!(
                video["color_transfer"].as_str(),
                Some("smpte2084" | "arib-std-b67")
            ),
            audio: streams.iter().any(|s| s["codec_type"] == "audio"),
            encoder,
            qualities,
            cache: Mutex::new(Cache {
                jobs: HashMap::new(),
                inits: HashMap::new(),
            }),
        })
    }
    pub fn master(&self, id: &str, token: &str) -> Vec<u8> {
        let mut text = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");
        // Start near 1080p rather than asking for the largest encode on startup.
        let mut qualities = self.qualities.iter().collect::<Vec<_>>();
        qualities.sort_by_key(|q| if q.name == "1080" { 0 } else { q.rate });
        for q in qualities {
            let codec = if self.encoder.hevc() {
                "hvc1.1.6.L153.B0"
            } else {
                "avc1.640033"
            };
            let audio = if self.audio { ",mp4a.40.2" } else { "" };
            text.push_str(&format!("#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={},RESOLUTION={}x{},FRAME-RATE={},CODECS=\"{codec}{audio}\"\n/v1/hls/{id}/{}/index.m3u8?token={token}\n",q.rate*13/10+256_000,q.rate+192_000,q.width,q.height,self.fps,q.name));
        }
        text.into_bytes()
    }
    pub fn playlist(&self, id: &str, quality: &str, token: &str) -> Result<Vec<u8>> {
        ensure!(
            self.qualities.iter().any(|q| q.name == quality),
            "Unknown quality"
        );
        let mut text=format!("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n",self.step.ceil() as u32);
        // A continuous rendition has one initialization identity. Repeating a
        // different MAP URI at each join makes AVFoundation reinitialize tracks.
        text.push_str(&format!(
            "#EXT-X-MAP:URI=\"/v1/hls/{id}/{quality}/0/init.mp4?token={token}\"\n"
        ));
        for index in 0..self.count() {
            let base = format!("/v1/hls/{id}/{quality}/{index}");
            let duration = (self.duration - f64::from(index) * self.step).min(self.step);
            text.push_str(&format!(
                "#EXTINF:{duration:.6},\n{base}/media.m4s?token={token}\n"
            ));
        }
        text.push_str("#EXT-X-ENDLIST\n");
        Ok(text.into_bytes())
    }
    fn count(&self) -> u32 {
        (self.duration / self.step).ceil() as u32
    }
    pub fn part(&self, quality: &str, index: u32, init: bool, stop: &AtomicBool) -> Result<Part> {
        ensure!(index < self.count(), "Unknown segment");
        let q = self
            .qualities
            .iter()
            .find(|q| q.name == quality)
            .context("Unknown quality")?;
        if init {
            let cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("HLS cache unavailable"))?;
            if let Some(file) = cache.inits.get(quality) {
                let file = File::open(file.path())?;
                return Ok(Part {
                    length: file.metadata()?.len(),
                    file,
                    offset: 0,
                    mime: "video/mp4",
                });
            }
        }
        let job = {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("HLS cache unavailable"))?;
            if !cache.jobs.get(quality).is_some_and(|j| j.usable(index)) {
                // Drop the old job only after any in-flight readers release it.
                cache
                    .jobs
                    .insert(quality.into(), self.encoder_job(q, index)?);
            }
            cache.jobs[quality].clone()
        };
        let mut part = job.part(index, init, stop)?;
        if init {
            ensure!(
                part.length <= 256 * 1024,
                "HLS initialization exceeds limit"
            );
            let mut saved = tempfile::NamedTempFile::new()?;
            std::io::copy(&mut part.file, saved.as_file_mut())?;
            part.file.seek(SeekFrom::Start(0))?;
            self.cache
                .lock()
                .unwrap()
                .inits
                .entry(quality.into())
                .or_insert(saved);
        }
        Ok(part)
    }
    fn encoder_job(&self, q: &Quality, index: u32) -> Result<Arc<EncoderJob>> {
        let directory = tempfile::tempdir()?;
        let start = f64::from(index) * self.step;
        let duration = self.duration - start;
        // All jobs start AAC on the same global 1024-sample grid. Decode a
        // small preroll and shift BOTH streams by the same amount, preserving
        // their relative timing even when a rendition starts after a seek.
        let decode_start = (start * 48000. / 1024.).floor() * 1024. / 48000.;
        let preroll = start - decode_start;
        let mut c = Command::new(ffmpeg());
        c.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
        self.encoder.input(&mut c);
        c.args(["-ss", &format!("{decode_start:.9}"), "-threads", "4", "-i"])
            .arg(&self.source);
        c.args(["-t", &format!("{duration:.9}"), "-map", "0:v:0"]);
        if self.audio {
            c.args(["-map", "0:a:0"]);
        }
        let tone = if self.hdr {
            "zscale=t=linear:npl=100,format=gbrpf32le,tonemap=hable:desat=0,zscale=p=bt709:t=bt709:m=bt709:r=tv,"
        } else {
            ""
        };
        let upload = if matches!(self.encoder, Encoder::Vaapi) {
            ",format=nv12,hwupload"
        } else {
            ",format=yuv420p"
        };
        c.args([
            "-vf",
            &format!(
                "{tone}scale={}:{},setsar=1,setpts=PTS-{preroll:.9}/TB{upload}",
                q.width, q.height
            ),
            "-r",
            &self.fps,
            "-c:v",
            self.encoder.name(),
        ]);
        c.args([
            "-b:v",
            &q.rate.to_string(),
            "-maxrate",
            &(q.rate * 12 / 10).to_string(),
            "-bufsize",
            &(q.rate * 2).to_string(),
            "-bf",
            "0",
            "-g",
            &(self.step * self.fps.parse::<f64>().unwrap_or(24.))
                .round()
                .to_string(),
            "-force_key_frames",
            &format!(
                "expr:gte(n,n_forced*{})",
                (self.step * self.fps.parse::<f64>().unwrap_or(24.)).round() as u32
            ),
        ]);
        if matches!(self.encoder, Encoder::Vaapi) {
            c.args(["-rc_mode", "VBR"]);
        }
        if self.encoder.hevc() {
            c.args(["-tag:v", "hvc1", "-profile:v", "main", "-level:v", "5.1"]);
            if matches!(self.encoder, Encoder::SoftwareHevc) {
                c.args([
                    "-preset",
                    "veryfast",
                    "-forced-idr", "1",
                    "-x265-params",
                    &format!("pools=4:frame-threads=2:log-level=error:open-gop=0:scenecut=0:keyint={}:min-keyint={}",(self.step*self.fps.parse::<f64>().unwrap_or(24.)).round() as u32,(self.step*self.fps.parse::<f64>().unwrap_or(24.)).round() as u32),
                ]);
            }
        } else {
            c.args([
                "-preset",
                "veryfast",
                "-sc_threshold",
                "0",
                "-keyint_min",
                &(self.step * self.fps.parse::<f64>().unwrap_or(24.))
                    .round()
                    .to_string(),
                "-profile:v",
                "high",
                "-level:v",
                "5.1",
                "-threads",
                "4",
            ]);
        }
        if self.hdr {
            c.args([
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-colorspace",
                "bt709",
            ]);
        }
        if self.audio {
            c.args([
                "-af",
                &format!("asetpts=PTS-{preroll:.9}/TB"),
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-ac",
                "2",
                "-ar",
                "48000",
            ]);
        }
        // Preserve the input's shared A/V clock. The native HLS muxer owns
        // AAC packet boundaries, encoder priming and continuous fragment tfdt.
        c.args([
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
            "-output_ts_offset",
            &format!("{start:.9}"),
            "-f",
            "hls",
            "-hls_time",
            &format!("{:.9}", self.step),
            "-hls_segment_type",
            "fmp4",
            "-hls_segment_options",
            "movflags=+skip_sidx",
            "-hls_flags",
            "independent_segments+temp_file",
            "-hls_list_size",
            "8",
            "-start_number",
            &index.to_string(),
            "-hls_fmp4_init_filename",
            "init.mp4",
            "-hls_segment_filename",
        ])
        .arg(directory.path().join("segment-%010d.m4s"))
        .arg(directory.path().join("index.m3u8"));
        EncoderJob::start(
            c,
            directory,
            index,
            CACHE_BYTES / self.qualities.len().max(1) as u64,
            start,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_mp4_layouts_do_not_reach_timestamp_rewriting() {
        for bytes in [
            vec![],
            vec![0; 4],
            b"\0\0\0\x04moov".to_vec(),
            b"\0\0\0\x30moov".to_vec(),
        ] {
            assert!(continuous_init(&bytes, 0.).is_err());
        }
        for start in 0..256 {
            let bytes = vec![start as u8; start];
            let _ = continuous_init(&bytes, 0.);
        }
    }
    #[test]
    fn software_auto_avoids_unsustainable_4k_but_allows_an_explicit_cap() {
        assert_eq!(max_height(Encoder::SoftwareHevc, None), 1080);
        assert_eq!(max_height(Encoder::Software, None), 1080);
        assert_eq!(max_height(Encoder::VideoToolbox, None), 2160);
        assert_eq!(max_height(Encoder::SoftwareHevc, Some("2160")), 2160);
        assert_eq!(max_height(Encoder::SoftwareHevc, Some("720")), 720);
        assert_eq!(max_height(Encoder::SoftwareHevc, Some("0")), 1080);
        assert_eq!(max_height(Encoder::SoftwareHevc, Some("9999")), 1080);
    }
    #[test]
    fn cancelled_codec_is_terminated_promptly() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let began = Instant::now();
        assert!(run(command, &AtomicBool::new(true), 30).is_err());
        assert!(began.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn continuous_jobs_cancel_and_reap_paused_encoders() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let job =
            EncoderJob::start(command, tempfile::tempdir().unwrap(), 0, CACHE_BYTES, 0.).unwrap();
        EncoderJob::signal(0, rustix::process::Signal::STOP); // invalid PID is ignored
        let began = Instant::now();
        drop(job);
        assert!(began.elapsed() < Duration::from_secs(2));
    }
}
