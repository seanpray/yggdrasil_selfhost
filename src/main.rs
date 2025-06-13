use axum::{
    Router,
    extract::Json,
    extract::Query,
    extract::State,
    routing::{get, post},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    net::IpAddr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use trust_dns_resolver::{TokioAsyncResolver, config::*};

#[derive(Debug, Deserialize, Serialize)]
struct JoinRequest {
    selectedProfile: String,
    serverId: String,
    #[serde(default)]
    authString: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct HasJoinedQuery {
    username: String,
    serverId: String,
}

type SessionMap = Arc<Mutex<HashMap<String, HashMap<String, u64>>>>;
type AccountMap = Arc<HashMap<String, String>>;

#[tokio::main]
async fn main() {
    let account_path = std::env::var("ACCOUNT_JSON").unwrap_or(String::from("accounts.json"));
    let accounts: AccountMap =
        Arc::new(serde_json::from_str(&fs::read_to_string(account_path).unwrap()).unwrap());
    let sessions_path = std::env::var("SESSIONS_JSON").unwrap_or(String::from("sessions.json"));
    let sessions: SessionMap = Arc::new(Mutex::new(
        serde_json::from_str(&fs::read_to_string(sessions_path).unwrap()).unwrap_or_default(),
    ));

    let app = Router::new()
        .route("/session/minecraft/join", post(join_handler))
        .route("/session/minecraft/hasJoined", get(has_joined_handler))
        .with_state((accounts, sessions));

    let address = std::env::var("YGG_BIND_ADDRESS").unwrap_or("0.0.0.0:3000".to_string());
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .expect("failed to parse tcp port");
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}

async fn resolve_mojang_ip() -> IpAddr {
    let resolver = TokioAsyncResolver::tokio(ResolverConfig::cloudflare(), ResolverOpts::default());

    let response = resolver
        .lookup_ip("sessionserver.mojang.com.")
        .await
        .unwrap();
    response.iter().next().unwrap()
}

async fn join_handler(
    State((accounts, sessions)): State<(AccountMap, SessionMap)>,
    Json(payload): Json<JoinRequest>,
) -> axum::http::StatusCode {
    let ip = resolve_mojang_ip().await;
    let url = format!("https://{}/session/minecraft/", ip);
    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    if let Some(auth) = payload.authString {
        if let Some(stored) = accounts.get(&payload.selectedProfile) {
            if stored == &auth {
                let profile: serde_json::Value = client
                    .get(format!("{}profile/{}", url, payload.selectedProfile))
                    .header("Host", "sessionserver.mojang.com")
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();

                if let Some(name) = profile.get("name").and_then(|v| v.as_str()) {
                    let username = name.to_lowercase();
                    let mut sessions = sessions.lock().await;

                    sessions
                        .entry(username.clone())
                        .or_default()
                        .retain(|_, &mut t| t >= now - 60);

                    sessions
                        .entry(username.clone())
                        .or_default()
                        .insert("uuid".to_string(), payload.selectedProfile.parse().unwrap());
                    sessions
                        .get_mut(&username)
                        .unwrap()
                        .insert(payload.serverId, now);

                    fs::write("sessions.json", serde_json::to_string(&*sessions).unwrap()).unwrap();
                    return axum::http::StatusCode::NO_CONTENT;
                } else {
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE;
                }
            }
        }
        return axum::http::StatusCode::UNAUTHORIZED;
    }

    // Proxy join request
    let resp = client
        .post(format!("{}join", url))
        .header("Host", "sessionserver.mojang.com")
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .unwrap();

    axum::http::StatusCode::from_u16(resp.status().as_u16()).unwrap()
}

async fn has_joined_handler(
    Query(query): Query<HasJoinedQuery>,
    axum::extract::State((_, sessions)): axum::extract::State<(AccountMap, SessionMap)>,
) -> (axum::http::StatusCode, String) {
    let ip = resolve_mojang_ip().await;
    let url = format!("https://{}/session/minecraft/", ip);
    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let username = query.username.to_lowercase();

    let sessions = sessions.lock().await;
    if let Some(user_sessions) = sessions.get(&username) {
        if let Some(&timestamp) = user_sessions.get(&query.serverId) {
            if timestamp >= now - 60 {
                let uuid = &user_sessions["uuid"];
                let resp = client
                    .get(format!("{}profile/{}?unsigned=false", url, uuid))
                    .header("Host", "sessionserver.mojang.com")
                    .send()
                    .await
                    .unwrap();
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap();
                return (axum::http::StatusCode::from_u16(status).unwrap(), body);
            }
        }
    }

    let resp = client
        .get(format!(
            "{}hasJoined?serverId={}&username={}",
            url, query.serverId, username
        ))
        .header("Host", "sessionserver.mojang.com")
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    if let Ok(body) = resp.text().await {
        (axum::http::StatusCode::from_u16(status).unwrap(), body)
    } else {
        unreachable!();
    }
}
