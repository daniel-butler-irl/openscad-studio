//! Session-only static web hosting. No desktop IPC or project files are exposed.
use axum::{http::HeaderValue, Router};
use axum_server::{tls_rustls::RustlsConfig, Handle};
use chrono::Datelike;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    time::Duration,
};
use tauri::{Manager, State};
use tokio::{sync::Mutex, task::JoinHandle};
use tower_http::{services::ServeDir, set_header::SetResponseHeaderLayer};

const PORT: u16 = 3443;

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanStatus {
    running: bool,
    urls: Vec<String>,
    message: Option<String>,
}

struct RunningServer {
    handle: Handle,
    task: JoinHandle<std::io::Result<()>>,
    urls: Vec<String>,
}

#[derive(Default)]
pub struct LanServerState(Mutex<Option<RunningServer>>);

fn private_addresses() -> Result<Vec<Ipv4Addr>, String> {
    let mut addresses: Vec<_> = local_ip_address::list_afinet_netifas()
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter_map(|(_, ip)| match ip {
            IpAddr::V4(ip) if ip.is_private() => Some(ip),
            _ => None,
        })
        .collect();
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() {
        return Err(
            "No local network address found. Connect to Wi-Fi or Ethernet and try again.".into(),
        );
    }
    Ok(addresses)
}

// Store the certificate and private key together, so interruption cannot leave a mismatched pair.
#[derive(Serialize, Deserialize)]
struct Authority {
    certificate: String,
    key: String,
}

fn load_authority(directory: &Path) -> Result<Authority, String> {
    let path = directory.join("lan-authority.json");
    if path.exists() {
        return serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("Could not read LAN certificate: {e}"));
    }
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let key = KeyPair::generate().map_err(|e| e.to_string())?;
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params
        .distinguished_name
        .push(DnType::CommonName, "OpenSCAD Studio LAN");
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
    let authority = Authority {
        certificate: cert.pem(),
        key: key.serialize_pem(),
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|e| e.to_string())?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec(&authority).map_err(|e| e.to_string())?)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(authority)
}

async fn tls_config(authority: &Authority, addresses: &[Ipv4Addr]) -> Result<RustlsConfig, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca_key = KeyPair::from_pem(&authority.key).map_err(|e| e.to_string())?;
    let ca = CertificateParams::from_ca_cert_pem(&authority.certificate)
        .and_then(|params| params.self_signed(&ca_key))
        .map_err(|e| e.to_string())?;
    let key = KeyPair::generate().map_err(|e| e.to_string())?;
    let mut params = CertificateParams::new(
        addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    )
    .map_err(|e| e.to_string())?;
    params
        .distinguished_name
        .push(DnType::CommonName, "OpenSCAD Studio");
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let now = chrono::Utc::now();
    // Apple clients require short-lived leaf certificates. Allow clock skew at startup.
    let start = now - chrono::Duration::days(1);
    let end = now + chrono::Duration::days(90);
    params.not_before = rcgen::date_time_ymd(start.year(), start.month() as u8, start.day() as u8);
    params.not_after = rcgen::date_time_ymd(end.year(), end.month() as u8, end.day() as u8);
    let cert = params
        .signed_by(&key, &ca, &ca_key)
        .map_err(|e| e.to_string())?;
    RustlsConfig::from_pem(
        format!("{}{}", cert.pem(), authority.certificate).into_bytes(),
        key.serialize_pem().into_bytes(),
    )
    .await
    .map_err(|e| e.to_string())
}

fn asset_router(directory: &Path) -> Router {
    Router::new()
        .fallback_service(ServeDir::new(directory).append_index_html_on_directories(true))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("cross-origin-embedder-policy"),
            HeaderValue::from_static("require-corp"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
}

fn asset_directory(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let directory = if cfg!(dev) {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/dist")
    } else {
        app.path()
            .resource_dir()
            .map_err(|e| e.to_string())?
            .join("lan-web")
    };
    if !directory.join("index.html").is_file() {
        return Err("The browser app is missing. Rebuild Studio with its LAN web assets.".into());
    }
    Ok(directory)
}

async fn status(server: &mut Option<RunningServer>) -> LanStatus {
    if server.as_ref().is_some_and(|s| s.task.is_finished()) {
        let finished = server.take().expect("server exists");
        let message = match finished.task.await {
            Ok(Err(error)) => format!("LAN server stopped: {error}"),
            Err(error) => format!("LAN server stopped: {error}"),
            Ok(Ok(())) => "LAN server stopped. Start it again to reconnect.".into(),
        };
        return LanStatus {
            message: Some(message),
            ..Default::default()
        };
    }
    server
        .as_ref()
        .map_or_else(LanStatus::default, |s| LanStatus {
            running: true,
            urls: s.urls.clone(),
            message: None,
        })
}

#[tauri::command]
pub async fn get_lan_access_status(state: State<'_, LanServerState>) -> Result<LanStatus, String> {
    Ok(status(&mut *state.0.lock().await).await)
}

#[tauri::command]
pub async fn get_lan_certificate(
    app: tauri::AppHandle,
    state: State<'_, LanServerState>,
) -> Result<String, String> {
    let _guard = state.0.lock().await;
    let directory = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(load_authority(&directory)?.certificate)
}

#[tauri::command]
pub async fn set_lan_access(
    app: tauri::AppHandle,
    enabled: bool,
    state: State<'_, LanServerState>,
) -> Result<LanStatus, String> {
    let mut server = state.0.lock().await;
    let current = status(&mut server).await;
    if enabled && current.running {
        return Ok(current);
    }
    if !enabled {
        if let Some(running) = server.take() {
            running.handle.shutdown();
            let _ = running.task.await;
        }
        return Ok(LanStatus::default());
    }
    let directory = asset_directory(&app)?;
    let addresses = private_addresses()?;
    let authority = load_authority(&app.path().app_config_dir().map_err(|e| e.to_string())?)?;
    let config = tls_config(&authority, &addresses).await?;
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, PORT))
        .map_err(|e| format!("Could not start LAN access on port {PORT}: {e}"))?;
    *server = Some(
        start_server(
            listener,
            config,
            &directory,
            addresses
                .iter()
                .map(|ip| format!("https://{ip}:{PORT}"))
                .collect(),
        )
        .await?,
    );
    Ok(status(&mut server).await)
}

async fn start_server(
    listener: TcpListener,
    config: RustlsConfig,
    directory: &Path,
    urls: Vec<String>,
) -> Result<RunningServer, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let handle = Handle::new();
    let service = axum_server::from_tcp_rustls(listener, config)
        .handle(handle.clone())
        .serve(asset_router(directory).into_make_service());
    let task = tokio::spawn(service);
    if !matches!(
        tokio::time::timeout(Duration::from_secs(5), handle.listening()).await,
        Ok(Some(_))
    ) {
        handle.shutdown();
        let _ = task.await;
        return Err("LAN server could not start.".into());
    }
    Ok(RunningServer { handle, task, urls })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn static_host_is_isolated_and_cannot_read_parent_files() {
        let root = std::env::temp_dir().join(format!("studio-lan-test-{}", uuid::Uuid::new_v4()));
        let public = root.join("web");
        std::fs::create_dir_all(&public).unwrap();
        std::fs::write(public.join("index.html"), "Studio").unwrap();
        std::fs::write(root.join("secret.txt"), "private").unwrap();
        let app = asset_router(&public);
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["cross-origin-opener-policy"],
            "same-origin"
        );
        assert_eq!(
            response.headers()["cross-origin-embedder-policy"],
            "require-corp"
        );
        for path in [
            "/../secret.txt",
            "/%2e%2e/secret.txt",
            "/mcp",
            "/lan-authority.json",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_ne!(response.status(), StatusCode::OK, "{path}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn authority_is_reused_and_produces_valid_tls_configuration() {
        let root = std::env::temp_dir().join(format!("studio-lan-cert-{}", uuid::Uuid::new_v4()));
        let first = load_authority(&root).unwrap();
        let second = load_authority(&root).unwrap();
        assert_eq!(first.certificate, second.certificate);
        assert_eq!(first.key, second.key);
        tls_config(&first, &[Ipv4Addr::new(192, 168, 1, 2)])
            .await
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(root.join("lan-authority.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn https_handshake_trusts_exported_ca_and_stop_releases_port() {
        use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
        use std::{
            io::{Read, Write},
            net::TcpStream,
            sync::Arc,
        };
        let root = std::env::temp_dir().join(format!("studio-lan-https-{}", uuid::Uuid::new_v4()));
        let authority = load_authority(&root).unwrap();
        let public = root.join("web");
        std::fs::create_dir(&public).unwrap();
        std::fs::write(public.join("index.html"), "Studio HTTPS").unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let config = tls_config(&authority, &[Ipv4Addr::LOCALHOST])
            .await
            .unwrap();
        let running = start_server(
            listener,
            config,
            &public,
            vec![format!("https://{address}")],
        )
        .await
        .unwrap();
        assert!(TcpListener::bind(address).is_err());
        tokio::task::spawn_blocking(move || {
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(CertificateDer::from_pem_slice(authority.certificate.as_bytes()).unwrap())
                .unwrap();
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connection = rustls::ClientConnection::new(
                Arc::new(config),
                ServerName::IpAddress(Ipv4Addr::LOCALHOST.into()),
            )
            .unwrap();
            let socket = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream = rustls::StreamOwned::new(connection, socket);
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .unwrap();
            let mut response = String::new();
            // HTTP connection closure may arrive without a TLS close_notify.
            let result = stream.read_to_string(&mut response);
            if let Err(error) = result {
                assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
            }
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            assert!(response.contains("Studio HTTPS"));
            assert!(response.contains("cross-origin-opener-policy: same-origin"));
        })
        .await
        .unwrap();
        running.handle.shutdown();
        running.task.await.unwrap().unwrap();
        let listener = TcpListener::bind(address).expect("port released on stop");
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }
}
