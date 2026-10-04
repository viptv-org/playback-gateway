use playback_engine::{Config, PlaybackManager};
use playback_gateway::{
    api::{self, Api},
    credentials::Credentials,
    network::NetworkPolicy,
    workers::Workers,
};
use std::{
    fs::{File, OpenOptions},
    future::IntoFuture,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Settings {
    key: String,
    data: PathBuf,
    bind: SocketAddr,
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    outputs: usize,
    replay_max_bytes: u64,
    qsv: Option<PathBuf>,
    vaapi: Option<PathBuf>,
}
impl Settings {
    fn read() -> Result<Self, &'static str> {
        let key = std::env::var("API_KEY").map_err(|_| "API_KEY is required")?;
        // Called before starting any runtime threads. Media subprocesses must
        // not inherit the bootstrap administration credential.
        std::env::remove_var("API_KEY");
        let outputs = std::env::var("MAX_OUTPUTS")
            .unwrap_or_else(|_| "8".into())
            .parse::<usize>()
            .ok()
            .filter(|value| (1..=128).contains(value))
            .ok_or("invalid MAX_OUTPUTS")?;
        Ok(Self {
            key,
            outputs,
            replay_max_bytes: std::env::var("LIVE_REPLAY_MAX_BYTES")
                .unwrap_or_else(|_| "268435456".into())
                .parse::<u64>()
                .ok()
                .filter(|bytes| *bytes >= 1024 * 1024)
                .ok_or("invalid LIVE_REPLAY_MAX_BYTES")?,
            qsv: std::env::var_os("QSV_DEVICE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            vaapi: std::env::var_os("VAAPI_DEVICE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            data: std::env::var_os("DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("data")),
            bind: std::env::var("BIND_ADDRESS")
                .unwrap_or_else(|_| "127.0.0.1:8090".into())
                .parse()
                .map_err(|_| "invalid BIND_ADDRESS")?,
            ffmpeg: std::env::var_os("FFMPEG_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/usr/bin/ffmpeg")),
            ffprobe: std::env::var_os("FFPROBE_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/usr/bin/ffprobe")),
        })
    }
}
fn main() {
    // Engine warnings contain closed categories, not source URLs or headers.
    // Exclude dependency transport events and arbitrary environment filters.
    tracing_subscriber::fmt()
        .with_env_filter("off,playback_engine=warn")
        .with_ansi(false)
        .init();
    let result = Settings::read().and_then(|settings| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|_| "runtime unavailable")?;
        runtime.block_on(run(settings))
    });
    if let Err(message) = result {
        eprintln!("Gateway startup/shutdown failed: {message}");
        std::process::exit(1);
    }
}
fn lock_storage(data: &PathBuf) -> Result<File, &'static str> {
    let existed = data.exists();
    std::fs::create_dir_all(data).map_err(|_| "data directory unavailable")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !existed {
            std::fs::set_permissions(data, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "data permissions unavailable")?;
        }
        if std::fs::metadata(data)
            .map_err(|_| "data directory unavailable")?
            .permissions()
            .mode()
            & 0o077
            != 0
        {
            return Err("data directory must be private to its owner");
        }
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.join("instance.lock"))
        .map_err(|_| "instance lock unavailable")?;
    lock.try_lock()
        .map_err(|_| "data directory already in use")?;
    Ok(lock)
}
async fn verify_media_binary(path: &PathBuf) -> Result<(), &'static str> {
    let mut command = tokio::process::Command::new(path);
    playback_engine::contain_media_process(&mut command);
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        command
            .kill_on_drop(true)
            .args(["-hide_banner", "-h", "protocol=tls"])
            .output(),
    )
    .await
    .map_err(|_| "media binary check timed out")?
    .map_err(|_| "media binary unavailable")?;
    if !output.status.success()
        || !String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.contains("-tls_verify") && line.contains("(default true)"))
    {
        return Err("media binaries must verify TLS by default");
    }
    let mut command = tokio::process::Command::new(path);
    playback_engine::contain_media_process(&mut command);
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        command
            .kill_on_drop(true)
            .args(["-hide_banner", "-buildconf"])
            .output(),
    )
    .await
    .map_err(|_| "media binary check timed out")?
    .map_err(|_| "media binary unavailable")?;
    if !output.status.success()
        || !String::from_utf8_lossy(&output.stdout).contains("--enable-gnutls")
    {
        return Err("this gateway currently requires a GnuTLS media build");
    }
    Ok(())
}
async fn run(settings: Settings) -> Result<(), &'static str> {
    let _storage_lock = lock_storage(&settings.data)?;
    verify_media_binary(&settings.ffmpeg).await?;
    verify_media_binary(&settings.ffprobe).await?;
    let credentials = Arc::new(Credentials::open(
        settings.data.join("credentials.sqlite"),
        &settings.key,
    )?);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            settings.data.join("credentials.sqlite"),
            std::fs::Permissions::from_mode(0o600),
        )
        .map_err(|_| "credential permissions unavailable")?;
    }
    let engine = PlaybackManager::new_with_hardware_and_replay_budget(
        Config {
            ffmpeg: settings.ffmpeg,
            ffprobe: settings.ffprobe,
            root: settings.data.join("media"),
            max_sessions: settings.outputs,
            ttl: Duration::from_secs(120),
        },
        settings.qsv,
        settings.vaapi,
        settings.replay_max_bytes,
    );
    let workers = Arc::new(Workers::new(engine, credentials, NetworkPolicy::default()).await?);
    let ready = Arc::new(AtomicBool::new(true));
    let listener = tokio::net::TcpListener::bind(settings.bind)
        .await
        .map_err(|_| "listener unavailable")?;
    let app = api::router(Api {
        workers: workers.clone(),
        ready: ready.clone(),
    });
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let reaper = {
        let workers = workers.clone();
        let ready = ready.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    _ = interval.tick() => {
                        let Some(now) = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|elapsed| elapsed.as_secs()) else { ready.store(false, Ordering::Release); continue; };
                        ready.store(workers.reap(now).await.is_ok(), Ordering::Release);
                    }
                }
            }
        })
    };
    let shutdown_ready = ready.clone();
    let (shutdown_started, mut shutdown_requested) = tokio::sync::watch::channel(false);
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown_signal().await;
        shutdown_ready.store(false, Ordering::Release);
        let _ = shutdown_started.send(true);
    });
    eprintln!("Playback gateway started");
    let server = server.into_future();
    tokio::pin!(server);
    let result = tokio::select! {
        result = &mut server => result.map_err(|_| "HTTP server failed"),
        _ = shutdown_requested.changed() => {
            // A client can stop consuming a media response indefinitely. Finish
            // graceful HTTP draining within a bound so owned media processes
            // are terminated/reaped before the runtime and storage lock exit.
            match tokio::time::timeout(Duration::from_secs(5), &mut server).await {
                Ok(result) => result.map_err(|_| "HTTP server failed"),
                Err(_) => {
                    eprintln!("HTTP drain deadline reached; stopping owned media workers");
                    Ok(())
                }
            }
        }
    };
    let _ = stop.send(true);
    let _ = reaper.await;
    workers.shutdown().await;
    result
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exclusive_storage_lock_is_recoverable_after_owner_exits() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state");
        let first = lock_storage(&path).unwrap();
        assert!(lock_storage(&path).is_err());
        drop(first);
        assert!(lock_storage(&path).is_ok());
    }
}
