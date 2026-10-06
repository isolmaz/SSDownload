//! Opt-in browser transport. Data is bounded, acknowledged only after durable writes,
//! and never published without the engine's ordinary output verification.
use crate::{
    engine::Engine,
    model::{AddRequest, DownloadKind, TransferControl},
    paths::AppPaths,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub const MAX_CHUNK_BYTES: usize = 512 * 1024;
const MAX_STREAMS: usize = 64;
const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Begin {
        id: String,
        total: Option<u64>,
        etag: Option<String>,
        #[serde(default)]
        restart: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender_pid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender_started_at: Option<u64>,
    },
    OpenStream {
        id: String,
        token: String,
        stream_id: u32,
        role: String,
        format_id: Option<String>,
        language: Option<String>,
        total: Option<u64>,
        etag: Option<String>,
    },
    Chunk {
        id: String,
        token: String,
        stream_id: u32,
        sequence: u64,
        offset: u64,
        data: String,
        sha256: String,
        #[serde(default)]
        client_checkpoint: Option<serde_json::Value>,
    },
    SealStream {
        id: String,
        token: String,
        stream_id: u32,
        sha256: String,
    },
    Status {
        id: String,
        token: String,
    },
    BeginRequest {
        id: String,
        token: String,
        generation: u64,
        request_id: String,
    },
    EndRequest {
        id: String,
        token: String,
        generation: u64,
        request_id: String,
    },
    Finish {
        id: String,
        token: String,
        #[serde(default)]
        source_duration: Option<f64>,
    },
    Pause {
        id: String,
        token: String,
    },
    Release {
        id: String,
        generation: u64,
    },
    Cancel {
        id: String,
        token: String,
    },
}
impl Command {
    pub fn validate_frame(&self) -> Result<()> {
        if let Self::Finish {
            source_duration: Some(duration),
            ..
        } = self
        {
            if !duration.is_finite() || *duration <= 0.0 {
                bail!("Tarayıcı manifest süresi geçersiz");
            }
        }
        if let Self::Chunk { data, sha256, .. } = self {
            if data.len() > MAX_CHUNK_BYTES.div_ceil(3) * 4 || sha256.len() != 64 {
                bail!("Tarayıcı aktarım parçası sınırı aşıldı");
            }
        }
        if let Self::BeginRequest {
            generation,
            request_id,
            ..
        }
        | Self::EndRequest {
            generation,
            request_id,
            ..
        } = self
        {
            if *generation == 0 || request_id.is_empty() || request_id.len() > 256 {
                bail!("Tarayıcı bağlantı izni geçersiz");
            }
        }
        Ok(())
    }
    fn id(&self) -> &str {
        match self {
            Self::Begin { id, .. }
            | Self::OpenStream { id, .. }
            | Self::Chunk { id, .. }
            | Self::SealStream { id, .. }
            | Self::Status { id, .. }
            | Self::BeginRequest { id, .. }
            | Self::EndRequest { id, .. }
            | Self::Finish { id, .. }
            | Self::Pause { id, .. }
            | Self::Release { id, .. }
            | Self::Cancel { id, .. } => id,
        }
    }
    fn token(&self) -> Option<&str> {
        match self {
            Self::Begin { .. } | Self::Release { .. } => None,
            Self::OpenStream { token, .. }
            | Self::Chunk { token, .. }
            | Self::SealStream { token, .. }
            | Self::Status { token, .. }
            | Self::BeginRequest { token, .. }
            | Self::EndRequest { token, .. }
            | Self::Finish { token, .. }
            | Self::Pause { token, .. }
            | Self::Cancel { token, .. } => Some(token),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStatus {
    pub stream_id: u32,
    pub offset: u64,
    pub next_sequence: u64,
    pub sealed: bool,
    pub total: Option<u64>,
    pub etag: Option<String>,
    pub client_checkpoint: Option<serde_json::Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub job_id: String,
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_granted: Option<bool>,
    pub streams: Vec<StreamStatus>,
    pub max_chunk_bytes: usize,
    pub request: Option<Box<AddRequest>>,
    pub completed: bool,
}
#[derive(Debug, Clone)]
pub struct StreamFile {
    pub path: PathBuf,
    pub role: String,
    pub format_id: Option<String>,
    pub language: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct StreamState {
    role: String,
    format_id: Option<String>,
    language: Option<String>,
    total: Option<u64>,
    etag: Option<String>,
    offset: u64,
    sequence: u64,
    last_offset: u64,
    last_hash: String,
    sealed: bool,
    #[serde(default)]
    client_checkpoint: Option<serde_json::Value>,
}
#[derive(Serialize, Deserialize)]
struct State {
    version: u32,
    job_id: String,
    token: String,
    source: String,
    cancelled: bool,
    streams: BTreeMap<u32, StreamState>,
}
fn source_key(request: &AddRequest) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(request)?)))
}
fn checked_path(path: &Path) -> Result<()> {
    if path.exists() && fs::symlink_metadata(path)?.file_attributes() & 0x400 != 0 {
        bail!("Tarayıcı çalışma alanı yönlendirme içeriyor");
    }
    Ok(())
}
fn state_path(root: &Path) -> PathBuf {
    root.join("state.dpapi")
}
fn stream_path(root: &Path, id: u32) -> PathBuf {
    root.join(format!("stream-{id:08}.bin"))
}
fn save(root: &Path, state: &State) -> Result<()> {
    let sealed = crate::secure::seal(serde_json::to_string(state)?)?;
    crate::recovery::atomic_write(&state_path(root), sealed.as_bytes())
}
fn load(root: &Path) -> Result<State> {
    checked_path(&state_path(root))?;
    if fs::metadata(state_path(root))?.len() > 1024 * 1024 {
        bail!("Tarayıcı aktarım kaydı çok büyük");
    }
    let state: State = serde_json::from_str(&crate::secure::unseal(&fs::read_to_string(
        state_path(root),
    )?)?)?;
    if state.version != STATE_VERSION || state.streams.len() > MAX_STREAMS {
        bail!("Tarayıcı aktarım kayıt sürümü geçersiz");
    }
    Ok(state)
}
fn digest_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    let mut hash = Sha256::new();
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}
fn response(state: &State, request: Option<Box<AddRequest>>) -> Response {
    Response {
        job_id: state.job_id.clone(),
        token: request.as_ref().map(|_| state.token.clone()),
        sender_generation: None,
        request_granted: None,
        max_chunk_bytes: MAX_CHUNK_BYTES,
        request,
        completed: false,
        streams: state
            .streams
            .iter()
            .map(|(id, s)| StreamStatus {
                stream_id: *id,
                offset: s.offset,
                next_sequence: s.sequence,
                sealed: s.sealed,
                total: s.total,
                etag: s.etag.clone(),
                client_checkpoint: s.client_checkpoint.clone(),
            })
            .collect(),
    }
}
fn new_stream(
    role: String,
    format_id: Option<String>,
    language: Option<String>,
    total: Option<u64>,
    etag: Option<String>,
) -> Result<StreamState> {
    if !matches!(role.as_str(), "file" | "video" | "audio" | "subtitle")
        || format_id.as_ref().is_some_and(|v| v.len() > 200)
        || language.as_ref().is_some_and(|v| v.len() > 64)
        || etag
            .as_ref()
            .is_some_and(|v| v.len() > 8192 || v.contains(['\r', '\n']))
    {
        bail!("Geçersiz tarayıcı akışı tanımı");
    }
    Ok(StreamState {
        role,
        format_id,
        language,
        total,
        etag,
        offset: 0,
        sequence: 0,
        last_offset: 0,
        last_hash: String::new(),
        sealed: false,
        client_checkpoint: None,
    })
}

pub fn dispatch(paths: &AppPaths, engine: &Engine, command: Command) -> Result<Response> {
    command.validate_frame()?;
    uuid::Uuid::parse_str(command.id()).context("Tarayıcı iş kimliği geçersiz")?;
    if let Command::Release { id, generation } = &command {
        engine.release_browser_transfer(id, *generation)?;
        return Ok(Response {
            job_id: id.clone(),
            token: None,
            sender_generation: None,
            request_granted: None,
            streams: Vec::new(),
            max_chunk_bytes: MAX_CHUNK_BYTES,
            request: None,
            completed: false,
        });
    }
    let sender = match &command {
        Command::Begin {
            sender_pid: Some(pid),
            sender_started_at: Some(started_at),
            ..
        } => Some((*pid, *started_at)),
        Command::Begin { .. } => bail!("Tarayıcı aktarım gönderici kimliği eksik"),
        _ => None,
    };
    let is_begin = sender.is_some();
    let (job, generation) = if let Some((sender_pid, sender_started_at)) = sender {
        engine.begin_browser_transfer(command.id(), sender_pid, sender_started_at)?
    } else {
        engine.browser_transfer_job(command.id())?
    };
    let mut authenticated = is_begin;
    let operation = (|| {
        let scratch = crate::recovery::prepare_work(&job)?;
        let parent = scratch.parent().context("Çalışma alanı yok")?;
        let root = parent.join("browser-transfer");
        checked_path(&root)?;
        fs::create_dir_all(&root)?;
        let _lock = crate::output::OutputLock::acquire(&root.join("receiver"))?;
        if let Command::Begin {
            total,
            etag,
            restart,
            ..
        } = &command
        {
            let fingerprint = source_key(&job.request)?;
            let mut state = if state_path(&root).exists() && !restart {
                let value = load(&root)?;
                if value.job_id != job.id || value.source != fingerprint {
                    bail!("Kaynak veya seçim değişti; eski aktarım korundu. Açık yeniden başlama gerekli.");
                }
                if value.cancelled {
                    bail!("İptal edilen aktarım için açık yeniden başlama gerekli");
                }
                if let Some(stream) = value.streams.get(&0) {
                    if stream.offset > 0
                        && (stream.total != *total
                            || stream.etag != *etag
                            || etag.as_deref().is_none_or(|v| v.starts_with("W/")))
                    {
                        bail!("Sürdürme kimliği doğrulanamadı; eski veriler korundu");
                    }
                }
                value
            } else {
                if *restart && (state_path(&root).exists() || scratch.exists()) {
                    checked_path(&scratch)?;
                    let archive = parent.join(format!(
                        "browser-transfer-previous-{}",
                        uuid::Uuid::new_v4()
                    ));
                    fs::rename(&root, &archive).context("Eski tarayıcı verileri korunamadı")?;
                    fs::create_dir_all(&root)?;
                    if scratch.exists() {
                        fs::rename(
                            &scratch,
                            archive.join(scratch.file_name().context("Doğrulama hedef adı yok")?),
                        )
                        .context("Önceki doğrulama çıktısı korunamadı")?;
                    }
                }
                let mut streams = BTreeMap::new();
                streams.insert(
                    0,
                    new_stream("file".into(), None, None, *total, etag.clone())?,
                );
                State {
                    version: STATE_VERSION,
                    job_id: job.id.clone(),
                    token: String::new(),
                    source: fingerprint,
                    cancelled: false,
                    streams,
                }
            };
            state.token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            save(&root, &state)?;
            let mut request = job.request.clone();
            request.session_cookies.clear();
            request.headers.retain(|name, _| {
                !name.eq_ignore_ascii_case("cookie") && !name.eq_ignore_ascii_case("authorization")
            });
            request.directory = None;
            let mut result = response(&state, Some(Box::new(request)));
            result.sender_generation = Some(generation);
            return Ok(result);
        }
        let mut state = load(&root)?;
        if state.job_id != job.id
            || state.cancelled
            || command.token() != Some(state.token.as_str())
            || state.source != source_key(&job.request)?
        {
            bail!("Tarayıcı aktarım yetkisi geçersiz veya iptal edilmiş");
        }
        authenticated = true;
        match &command {
            Command::BeginRequest {
                generation: request_generation,
                request_id,
                ..
            } => {
                if *request_generation != generation {
                    bail!("Tarayıcı bağlantı izni aktarım nesliyle eşleşmiyor");
                }
                let mut result = response(&state, None);
                result.request_granted =
                    Some(engine.try_begin_browser_request(&job.id, generation, request_id)?);
                return Ok(result);
            }
            Command::EndRequest {
                generation: request_generation,
                request_id,
                ..
            } => {
                if *request_generation != generation {
                    bail!("Tarayıcı bağlantı izni aktarım nesliyle eşleşmiyor");
                }
                engine.end_browser_request(&job.id, generation, request_id)?;
                return Ok(response(&state, None));
            }
            _ => {}
        }
        match command {
            Command::OpenStream {
                stream_id,
                role,
                format_id,
                language,
                total,
                etag,
                ..
            } => {
                if stream_id == 0
                    || (!state.streams.contains_key(&stream_id)
                        && state.streams.len() >= MAX_STREAMS)
                {
                    bail!("Akış kimliği veya sayısı sınırı aşıldı");
                }
                if let Some(old) = state.streams.get(&stream_id) {
                    if old.role != role
                        || old.format_id != format_id
                        || old.language != language
                        || old.total != total
                        || old.etag != etag
                    {
                        bail!("Akış kimliği değişti; var olan parça korunuyor");
                    }
                    if let Some(total) = old.total {
                        crate::output::require_space(
                            &stream_path(&root, stream_id),
                            total.saturating_sub(old.offset),
                        )?;
                    }
                } else {
                    let stream = new_stream(role, format_id, language, total, etag)?;
                    if let Some(total) = stream.total {
                        crate::output::require_space(&stream_path(&root, stream_id), total)?;
                    }
                    state.streams.insert(stream_id, stream);
                    save(&root, &state)?;
                }
            }
            Command::Chunk {
                stream_id,
                sequence,
                offset,
                data,
                sha256,
                client_checkpoint,
                ..
            } => {
                if client_checkpoint
                    .as_ref()
                    .is_some_and(|v| v.to_string().len() > 4096)
                {
                    bail!("Tarayıcı sürdürme kontrol kaydı çok büyük");
                }
                let stream = state.streams.get_mut(&stream_id).context("Akış açılmadı")?;
                let bytes = STANDARD.decode(data).context("Geçersiz parça kodlaması")?;
                if bytes.is_empty()
                    || bytes.len() > MAX_CHUNK_BYTES
                    || hex::encode(Sha256::digest(&bytes)) != sha256
                {
                    bail!("Tarayıcı parça hash veya boyutu geçersiz");
                }
                if sequence.checked_add(1) == Some(stream.sequence)
                    && offset == stream.last_offset
                    && sha256 == stream.last_hash
                {
                    return Ok(response(&state, None));
                }
                if stream.sealed || sequence != stream.sequence || offset != stream.offset {
                    bail!("Tarayıcı parçası sıra/ofset eşleşmiyor");
                }
                let end = offset
                    .checked_add(bytes.len() as u64)
                    .context("Parça ofseti taştı")?;
                if stream.total.is_some_and(|total| end > total) {
                    bail!("Tarayıcı verisi beklenen boyutu aşıyor");
                }
                let path = stream_path(&root, stream_id);
                checked_path(&path)?;
                let mut file = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&path)?;
                if file.metadata()?.len() < offset {
                    bail!("Onaylanmış tarayıcı verisi eksik; sürdürme durduruldu");
                }
                file.set_len(offset)?; // An unacknowledged crash tail is never treated as committed data.
                file.seek(SeekFrom::Start(offset))?;
                file.write_all(&bytes).map_err(|error| {
                    let message = format!("Tarayıcı verisi diske yazılamadı: {error}");
                    anyhow::Error::from(error).context(message)
                })?;
                file.sync_all()?;
                stream.last_offset = offset;
                stream.offset = end;
                stream.sequence = sequence.checked_add(1).context("Parça sırası taştı")?;
                stream.last_hash = sha256;
                stream.client_checkpoint = client_checkpoint;
                save(&root, &state)?;
                let committed = state
                    .streams
                    .values()
                    .try_fold(0u64, |sum, s| sum.checked_add(s.offset))
                    .context("Toplam aktarım boyutu taştı")?;
                let total = state
                    .streams
                    .values()
                    .filter(|s| s.offset > 0 || s.total.is_some())
                    .try_fold(0u64, |sum, s| sum.checked_add(s.total?));
                engine.browser_transfer_progress(&job.id, committed, total)?;
            }
            Command::SealStream {
                stream_id, sha256, ..
            } => {
                let stream = state.streams.get_mut(&stream_id).context("Akış açılmadı")?;
                if stream.offset == 0 || stream.total.is_some_and(|total| total != stream.offset) {
                    bail!("Tarayıcı akışı eksik kaldı");
                }
                let path = stream_path(&root, stream_id);
                checked_path(&path)?;
                if fs::metadata(&path)?.len() != stream.offset || digest_file(&path)? != sha256 {
                    bail!("Tarayıcı akışı SHA-256 doğrulaması başarısız");
                }
                stream.sealed = true;
                save(&root, &state)?;
            }
            Command::Status { .. } => {}
            Command::Pause { .. } | Command::Cancel { .. } => {
                state.cancelled = matches!(command, Command::Cancel { .. });
                state.token.clear();
                save(&root, &state)?;
                engine.pause_browser_transfer(&job.id, generation)?;
            }
            Command::Finish {
                source_duration, ..
            } => {
                let streams: Vec<_> = state
                    .streams
                    .iter()
                    .filter(|(id, s)| **id != 0 || s.offset > 0)
                    .collect();
                if streams.is_empty() || streams.iter().any(|(_, s)| s.offset == 0 || !s.sealed) {
                    bail!("Bütün akışlar hash ile doğrulanmadan tamamlanamaz");
                }
                if scratch.exists() {
                    bail!("Doğrulama hedefi zaten var; eski dosya korundu");
                }
                let files: Vec<_> = streams
                    .iter()
                    .map(|(id, s)| StreamFile {
                        path: stream_path(&root, **id),
                        role: s.role.clone(),
                        format_id: s.format_id.clone(),
                        language: s.language.clone(),
                    })
                    .collect();
                if streams.len() == 1 && streams[0].1.role == "file" {
                    fs::rename(stream_path(&root, *streams[0].0), &scratch)?;
                } else {
                    if job.request.kind == DownloadKind::File {
                        bail!("Ham dosya isteği çoklu medya akışına çevrilemez");
                    }
                    crate::media::mux_browser_streams(
                        paths,
                        &job.request,
                        &files,
                        &scratch,
                        &TransferControl::default(),
                    )?;
                }
                let bytes = fs::metadata(&scratch)?.len();
                let verified =
                    if matches!(job.request.kind, DownloadKind::Video | DownloadKind::Audio) {
                        crate::media::verify_external_result(
                            paths,
                            &job.request,
                            &scratch,
                            &files,
                            source_duration,
                            job.request.full_verification,
                        )?
                    } else {
                        crate::engine::verify_file_result(
                            &scratch,
                            job.request.checksum.as_deref(),
                        )?
                    };
                if verified != bytes {
                    bail!("Tarayıcı aktarımı doğrulama boyutuyla eşleşmiyor");
                }
                engine.finish_browser_transfer(&job.id, generation, scratch, bytes)?;
                let mut result = response(&state, None);
                result.completed = true;
                return Ok(result);
            }
            Command::Begin { .. }
            | Command::BeginRequest { .. }
            | Command::EndRequest { .. }
            | Command::Release { .. } => unreachable!(),
        }
        Ok(response(&state, None))
    })();
    if operation.is_err() && authenticated {
        let _ = engine.pause_browser_transfer(&job.id, generation);
    }
    operation
}
