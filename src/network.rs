use std::{
    collections::BTreeSet,
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::{image, model_config};

const LOOPBACK_BYPASS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];
const LOG_LIMIT: i64 = 600;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProxySource {
    None,
    Environment,
    WindowsStatic,
    WindowsAutoConfig,
    UnsupportedSocks,
}

impl ProxySource {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "未检测到",
            Self::Environment => "进程环境变量",
            Self::WindowsStatic => "Windows 系统代理",
            Self::WindowsAutoConfig => "Windows 自动代理",
            Self::UnsupportedSocks => "仅检测到 SOCKS 代理",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TransportKind {
    HttpSse,
    WebSocket,
}

impl TransportKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::HttpSse => "HTTPS/SSE",
            Self::WebSocket => "WebSocket",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NetworkIssue {
    WebSocketTimeout,
    WebSocketRejected,
    RateLimited,
    UpstreamUnavailable,
    Authentication,
    StreamInterrupted,
    Unknown,
}

impl NetworkIssue {
    pub fn label(self) -> &'static str {
        match self {
            Self::WebSocketTimeout => "WebSocket 连接超时",
            Self::WebSocketRejected => "WebSocket 握手或策略拒绝",
            Self::RateLimited => "上游请求限流",
            Self::UpstreamUnavailable => "上游服务暂时不可用",
            Self::Authentication => "服务鉴权失败",
            Self::StreamInterrupted => "响应流提前中断",
            Self::Unknown => "未识别的网络错误",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkReport {
    pub transport_mode: model_config::TransportMode,
    pub proxy_inheritance_enabled: bool,
    pub proxy_source: ProxySource,
    pub recent_transport: Option<TransportKind>,
    pub recent_issue: Option<NetworkIssue>,
    pub recent_retry_count: Option<u32>,
    pub recommendation: String,
    pub summary: String,
}

#[derive(Clone, Debug, Default)]
struct LogSummary {
    transport: Option<TransportKind>,
    issue: Option<NetworkIssue>,
    retries: Option<u32>,
}

#[derive(Clone, Debug, Default)]
struct ProxyDiscovery {
    source: Option<ProxySource>,
    http_proxy: Option<OsString>,
    https_proxy: Option<OsString>,
}

pub fn diagnose() -> Result<NetworkReport> {
    let settings = model_config::load_settings()?;
    let proxy = discover_proxy(settings.inherit_system_proxy);
    let logs_path = settings.codex_home.join("logs_2.sqlite");
    let logs = diagnose_logs(&logs_path).unwrap_or_default();
    let source = proxy.source.unwrap_or(ProxySource::None);
    let recommendation = recommendation(settings.transport_mode, &logs);
    let summary = format_summary(
        settings.transport_mode,
        settings.inherit_system_proxy,
        source,
        &logs,
        &recommendation,
    );
    Ok(NetworkReport {
        transport_mode: settings.transport_mode,
        proxy_inheritance_enabled: settings.inherit_system_proxy,
        proxy_source: source,
        recent_transport: logs.transport,
        recent_issue: logs.issue,
        recent_retry_count: logs.retries,
        recommendation,
        summary,
    })
}

pub fn apply_child_proxy_environment(command: &mut Command, inherit_system_proxy: bool) {
    let proxy = discover_proxy(inherit_system_proxy);
    if proxy.source == Some(ProxySource::WindowsStatic) {
        if let Some(value) = proxy.http_proxy.as_ref() {
            command.env("HTTP_PROXY", value).env("http_proxy", value);
        }
        if let Some(value) = proxy.https_proxy.as_ref() {
            command.env("HTTPS_PROXY", value).env("https_proxy", value);
        }
    }
    if proxy.http_proxy.is_some() || proxy.https_proxy.is_some() {
        let bypass = merged_no_proxy();
        command.env("NO_PROXY", &bypass).env("no_proxy", bypass);
    }
}

fn recommendation(mode: model_config::TransportMode, logs: &LogSummary) -> String {
    match logs.issue {
        Some(NetworkIssue::WebSocketTimeout | NetworkIssue::WebSocketRejected) => {
            "建议切换到 HTTPS/SSE 兼容模式".to_owned()
        }
        Some(NetworkIssue::RateLimited) => "等待服务端限流窗口恢复".to_owned(),
        Some(NetworkIssue::UpstreamUnavailable) => "中转站上游异常，请稍后重试".to_owned(),
        Some(NetworkIssue::Authentication) => "检查 API Key 与服务端权限".to_owned(),
        Some(NetworkIssue::StreamInterrupted | NetworkIssue::Unknown) => {
            "重新检测；持续出现时导出脱敏诊断".to_owned()
        }
        None if mode == model_config::TransportMode::WebSocket => {
            "WebSocket 已由用户明确启用".to_owned()
        }
        None => "当前配置优先使用 HTTPS/SSE".to_owned(),
    }
}

fn format_summary(
    mode: model_config::TransportMode,
    inherit_system_proxy: bool,
    proxy_source: ProxySource,
    logs: &LogSummary,
    recommendation: &str,
) -> String {
    let transport = logs
        .transport
        .map(TransportKind::label)
        .unwrap_or("暂无记录");
    let issue = logs.issue.map(NetworkIssue::label).unwrap_or("未发现");
    let retries = logs
        .retries
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned());
    format!(
        "传输模式: {}\r\n最近实际传输: {transport}\r\n代理继承: {}\r\n代理来源: {}\r\n最近网络问题: {issue}\r\n最近重试次数: {retries}\r\n建议: {recommendation}",
        mode.label(),
        if inherit_system_proxy { "已开启" } else { "已关闭" },
        proxy_source.label(),
    )
}

fn discover_proxy(inherit_system_proxy: bool) -> ProxyDiscovery {
    let http_proxy = first_environment_value(&["HTTP_PROXY", "http_proxy"]);
    let https_proxy = first_environment_value(&["HTTPS_PROXY", "https_proxy"]);
    if let Some(proxy) = explicit_proxy_discovery(http_proxy, https_proxy) {
        return proxy;
    }
    if !inherit_system_proxy {
        return ProxyDiscovery::default();
    }
    discover_windows_proxy()
}

fn explicit_proxy_discovery(
    http_proxy: Option<OsString>,
    https_proxy: Option<OsString>,
) -> Option<ProxyDiscovery> {
    if http_proxy.is_none() && https_proxy.is_none() {
        return None;
    }
    Some(ProxyDiscovery {
        source: Some(ProxySource::Environment),
        http_proxy: http_proxy.clone().or_else(|| https_proxy.clone()),
        https_proxy: https_proxy.or(http_proxy),
    })
}

fn first_environment_value(names: &[&str]) -> Option<OsString> {
    names
        .iter()
        .find_map(|name| env::var_os(name).filter(|value| !value.is_empty()))
}

fn merged_no_proxy() -> OsString {
    let existing = first_environment_value(&["NO_PROXY", "no_proxy"])
        .and_then(|value| value.into_string().ok())
        .unwrap_or_default();
    merge_no_proxy_value(&existing).into()
}

fn merge_no_proxy_value(existing: &str) -> String {
    let mut values = Vec::new();
    let mut normalized = BTreeSet::new();
    for value in existing.split([',', ';']).map(str::trim) {
        if value.is_empty() {
            continue;
        }
        if normalized.insert(value.to_ascii_lowercase()) {
            values.push(value.to_owned());
        }
    }
    for value in LOOPBACK_BYPASS {
        if normalized.insert(value.to_owned()) {
            values.push(value.to_owned());
        }
    }
    values.join(",")
}

#[cfg(windows)]
fn discover_windows_proxy() -> ProxyDiscovery {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    let root = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(settings) =
        root.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings")
    else {
        return ProxyDiscovery::default();
    };
    let enabled = settings.get_value::<u32, _>("ProxyEnable").unwrap_or(0) != 0;
    if enabled {
        if let Ok(raw) = settings.get_value::<String, _>("ProxyServer") {
            let parsed = parse_windows_proxy_server(&raw);
            if parsed.http_proxy.is_some() || parsed.https_proxy.is_some() {
                return ProxyDiscovery {
                    source: Some(ProxySource::WindowsStatic),
                    ..parsed
                };
            }
            if parsed.source == Some(ProxySource::UnsupportedSocks) {
                return parsed;
            }
        }
    }
    if settings
        .get_value::<String, _>("AutoConfigURL")
        .is_ok_and(|value| !value.trim().is_empty())
    {
        return ProxyDiscovery {
            source: Some(ProxySource::WindowsAutoConfig),
            ..ProxyDiscovery::default()
        };
    }
    ProxyDiscovery::default()
}

#[cfg(not(windows))]
fn discover_windows_proxy() -> ProxyDiscovery {
    ProxyDiscovery::default()
}

fn parse_windows_proxy_server(raw: &str) -> ProxyDiscovery {
    let raw = raw.trim();
    if raw.is_empty() {
        return ProxyDiscovery::default();
    }
    if !raw.contains('=') {
        let lower = raw.to_ascii_lowercase();
        if lower.starts_with("socks://") || lower.starts_with("socks5://") {
            return ProxyDiscovery {
                source: Some(ProxySource::UnsupportedSocks),
                ..ProxyDiscovery::default()
            };
        }
        let proxy = normalize_http_proxy(raw);
        return ProxyDiscovery {
            http_proxy: proxy.clone(),
            https_proxy: proxy,
            ..ProxyDiscovery::default()
        };
    }
    let mut discovery = ProxyDiscovery::default();
    let mut socks_seen = false;
    for entry in raw.split(';') {
        let Some((kind, value)) = entry.split_once('=') else {
            continue;
        };
        match kind.trim().to_ascii_lowercase().as_str() {
            "http" => discovery.http_proxy = normalize_http_proxy(value),
            "https" => discovery.https_proxy = normalize_http_proxy(value),
            "socks" | "socks5" => socks_seen = true,
            _ => {}
        }
    }
    if discovery.http_proxy.is_none() {
        discovery.http_proxy = discovery.https_proxy.clone();
    }
    if discovery.https_proxy.is_none() {
        discovery.https_proxy = discovery.http_proxy.clone();
    }
    if discovery.http_proxy.is_none() && discovery.https_proxy.is_none() && socks_seen {
        discovery.source = Some(ProxySource::UnsupportedSocks);
    }
    discovery
}

fn normalize_http_proxy(value: &str) -> Option<OsString> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        format!("http://{value}")
    };
    let parsed = url::Url::parse(&candidate).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    Some(candidate.into())
}

fn diagnose_logs(path: &Path) -> Result<LogSummary> {
    if !path.is_file() {
        return Ok(LogSummary::default());
    }
    let mut uri = url::Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("failed to convert log database path to a file URI"))?;
    uri.query_pairs_mut().append_pair("mode", "ro");
    let connection = Connection::open_with_flags(
        uri.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open {} read-only", path.display()))?;
    connection.pragma_update(None, "query_only", true)?;
    let mut statement = connection.prepare(
        "SELECT target, COALESCE(feedback_log_body, '') FROM logs \
         WHERE target IN ('codex_core::responses_retry', \
                          'codex_core::client', \
                          'codex_http_client::client', \
                          'codex_api::endpoint::responses_websocket') \
         ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = statement.query_map([LOG_LIMIT], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut summary = LogSummary::default();
    for row in rows {
        let (target, body) = row?;
        if summary.transport.is_none() {
            summary.transport = transport_from_log(&body);
        }
        if summary.issue.is_none() && target == "codex_core::responses_retry" {
            summary.issue = issue_from_log(&body);
            summary.retries = retry_count_from_log(&body);
        }
        if summary.transport.is_some() && summary.issue.is_some() {
            break;
        }
    }
    Ok(summary)
}

fn transport_from_log(body: &str) -> Option<TransportKind> {
    if body.contains("responses_websocket") || body.contains("transport=responses_websocket") {
        Some(TransportKind::WebSocket)
    } else if body.contains("responses_http") || body.contains("transport=responses_http") {
        Some(TransportKind::HttpSse)
    } else {
        None
    }
}

fn issue_from_log(body: &str) -> Option<NetworkIssue> {
    let lower = body.to_ascii_lowercase();
    let websocket = lower.contains("responses_websocket") || lower.contains("websocket");
    if websocket
        && (lower.contains("1008")
            || lower.contains("policy")
            || lower.contains("403 forbidden")
            || lower.contains("upgrade required"))
    {
        Some(NetworkIssue::WebSocketRejected)
    } else if websocket && (lower.contains("timed out") || lower.contains("timeout")) {
        Some(NetworkIssue::WebSocketTimeout)
    } else if lower.contains("429") || lower.contains("rate limit") {
        Some(NetworkIssue::RateLimited)
    } else if [
        "502",
        "503",
        "504",
        "bad gateway",
        "upstream request failed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        Some(NetworkIssue::UpstreamUnavailable)
    } else if lower.contains("401 unauthorized") || lower.contains("403 forbidden") {
        Some(NetworkIssue::Authentication)
    } else if lower.contains("stream disconnected") || lower.contains("error decoding response") {
        Some(NetworkIssue::StreamInterrupted)
    } else if lower.contains("retrying sampling request") {
        Some(NetworkIssue::Unknown)
    } else {
        None
    }
}

fn retry_count_from_log(body: &str) -> Option<u32> {
    let marker = "retrying sampling request (";
    let start = body.find(marker)? + marker.len();
    let count = body[start..].split_once('/')?.0.trim();
    count.parse().ok()
}

pub fn logs_path() -> PathBuf {
    image::codex_home().join("logs_2.sqlite")
}

pub fn environment_has_proxy() -> bool {
    first_environment_value(&["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]).is_some()
}

pub fn is_loopback_bypassed(value: &OsStr) -> bool {
    value
        .to_str()
        .is_some_and(|value| LOOPBACK_BYPASS.iter().all(|item| value.contains(item)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn test_logs_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "codex-image-fix-network-{name}-{}-{unique}.sqlite",
            std::process::id()
        ))
    }

    fn create_log_database(path: &Path, rows: &[(&str, &str)]) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE logs (
                    id INTEGER PRIMARY KEY,
                    target TEXT NOT NULL,
                    feedback_log_body TEXT
                );",
            )
            .unwrap();
        for (target, body) in rows {
            connection
                .execute(
                    "INSERT INTO logs (target, feedback_log_body) VALUES (?1, ?2)",
                    (target, body),
                )
                .unwrap();
        }
    }

    #[test]
    fn parses_static_and_protocol_specific_windows_proxies() {
        let simple = parse_windows_proxy_server("127.0.0.1:7890");
        assert_eq!(
            simple.http_proxy.as_deref(),
            Some(OsStr::new("http://127.0.0.1:7890"))
        );
        assert_eq!(simple.https_proxy, simple.http_proxy);

        let split = parse_windows_proxy_server(
            "http=127.0.0.1:8080;https=https://proxy.example:8443;socks=127.0.0.1:1080",
        );
        assert_eq!(
            split.http_proxy.as_deref(),
            Some(OsStr::new("http://127.0.0.1:8080"))
        );
        assert_eq!(
            split.https_proxy.as_deref(),
            Some(OsStr::new("https://proxy.example:8443"))
        );
    }

    #[test]
    fn refuses_socks_only_proxy_for_http_environment() {
        let proxy = parse_windows_proxy_server("socks=127.0.0.1:1080");
        assert_eq!(proxy.source, Some(ProxySource::UnsupportedSocks));
        assert!(proxy.http_proxy.is_none());
        assert!(proxy.https_proxy.is_none());

        let proxy = parse_windows_proxy_server("socks5://127.0.0.1:1080");
        assert_eq!(proxy.source, Some(ProxySource::UnsupportedSocks));
    }

    #[test]
    fn merges_loopback_bypass_without_duplicates() {
        assert_eq!(
            merge_no_proxy_value("example.com,LOCALHOST"),
            "example.com,LOCALHOST,127.0.0.1,::1"
        );
    }

    #[test]
    fn classifies_websocket_and_upstream_retry_failures() {
        let websocket = "model_client.stream_responses_websocket request timed out retrying sampling request (5/5 in 3s)";
        assert_eq!(
            issue_from_log(websocket),
            Some(NetworkIssue::WebSocketTimeout)
        );
        assert_eq!(retry_count_from_log(websocket), Some(5));

        let upstream = "retrying sampling request (5/5 in 3s) unexpected status 502 Bad Gateway: Upstream request failed";
        assert_eq!(
            issue_from_log(upstream),
            Some(NetworkIssue::UpstreamUnavailable)
        );
    }

    #[test]
    fn explicit_environment_proxy_takes_priority_and_fills_missing_protocol() {
        let proxy =
            explicit_proxy_discovery(Some(OsString::from("http://proxy.example:8080")), None)
                .unwrap();
        assert_eq!(proxy.source, Some(ProxySource::Environment));
        assert_eq!(proxy.http_proxy, proxy.https_proxy);
    }

    #[test]
    fn read_only_logs_detect_websocket_fallback_to_http() {
        let path = test_logs_path("websocket-fallback");
        create_log_database(
            &path,
            &[
                (
                    "codex_core::responses_retry",
                    "responses_websocket request timed out; retrying sampling request (5/5 in 3s)",
                ),
                (
                    "codex_core::client",
                    "request completed transport=responses_http",
                ),
            ],
        );

        let summary = diagnose_logs(&path).unwrap();
        assert_eq!(summary.transport, Some(TransportKind::HttpSse));
        assert_eq!(summary.issue, Some(NetworkIssue::WebSocketTimeout));
        assert_eq!(summary.retries, Some(5));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn read_only_logs_distinguish_http_502_and_rate_limits() {
        for (name, body, expected) in [
            (
                "http-502",
                "responses_http retrying sampling request (5/5 in 3s): 502 Bad Gateway",
                NetworkIssue::UpstreamUnavailable,
            ),
            (
                "http-429",
                "responses_http retrying sampling request (2/5 in 3s): 429 rate limit",
                NetworkIssue::RateLimited,
            ),
        ] {
            let path = test_logs_path(name);
            create_log_database(&path, &[("codex_core::responses_retry", body)]);
            let summary = diagnose_logs(&path).unwrap();
            assert_eq!(summary.transport, Some(TransportKind::HttpSse));
            assert_eq!(summary.issue, Some(expected));
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn missing_unrelated_and_corrupt_logs_degrade_safely() {
        let missing = test_logs_path("missing");
        let summary = diagnose_logs(&missing).unwrap();
        assert_eq!(summary.transport, None);
        assert_eq!(summary.issue, None);

        let unrelated = test_logs_path("unrelated");
        create_log_database(&unrelated, &[("other_target", "prompt and request body")]);
        let summary = diagnose_logs(&unrelated).unwrap();
        assert_eq!(summary.transport, None);
        assert_eq!(summary.issue, None);
        fs::remove_file(unrelated).unwrap();

        let corrupt = test_logs_path("corrupt");
        fs::write(&corrupt, b"not a SQLite database").unwrap();
        assert!(diagnose_logs(&corrupt).is_err());
        fs::remove_file(corrupt).unwrap();
    }

    #[test]
    fn network_summary_never_contains_proxy_credentials() {
        let logs = LogSummary::default();
        let summary = format_summary(
            model_config::TransportMode::Auto,
            true,
            ProxySource::WindowsStatic,
            &logs,
            "当前配置优先使用 HTTPS/SSE",
        );
        assert!(!summary.contains("user:secret"));
        assert!(!summary.contains("proxy.example"));
    }
}
