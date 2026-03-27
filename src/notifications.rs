#![allow(dead_code)]

use chrono::{DateTime, NaiveDateTime, Utc};
use log::warn;
use rmpv::Value;
use serde::{Deserialize, Serialize};
use worker::{wasm_bindgen::JsValue, Env, Method, Request, RequestInit};

use crate::{error::AppError, push};

const INTERNAL_FANOUT_URL: &str = "https://notify.internal/fanout";
pub const RECORD_SEPARATOR: u8 = 0x1e;
pub const INITIAL_RESPONSE: [u8; 3] = [b'{', b'}', RECORD_SEPARATOR];
pub const USER_KIND_TAG: &str = "k:user";
pub const ANONYMOUS_KIND_TAG: &str = "k:anon";

// ── UpdateType ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum UpdateType {
    SyncCipherUpdate = 0,
    SyncCipherCreate = 1,
    SyncLoginDelete = 2,
    SyncFolderDelete = 3,
    SyncCiphers = 4,
    SyncVault = 5,
    SyncOrgKeys = 6,
    SyncFolderCreate = 7,
    SyncFolderUpdate = 8,
    SyncSettings = 10,
    LogOut = 11,
    SyncSendCreate = 12,
    SyncSendUpdate = 13,
    SyncSendDelete = 14,
    AuthRequest = 15,
    AuthRequestResponse = 16,
    None = 100,
}

// ── Connection model ────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionKind {
    User,
    Anonymous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionAttachment {
    pub kind: ConnectionKind,
    pub user_id: Option<String>,
    pub token: Option<String>,
    pub device_id: Option<String>,
    pub protocol_initialized: bool,
    pub connected_at: String,
}

impl ConnectionAttachment {
    pub fn user(user_id: String, device_id: Option<String>, connected_at: String) -> Self {
        Self {
            kind: ConnectionKind::User,
            user_id: Some(user_id),
            token: None,
            device_id,
            protocol_initialized: false,
            connected_at,
        }
    }

    pub fn anonymous(token: String, connected_at: String) -> Self {
        Self {
            kind: ConnectionKind::Anonymous,
            user_id: None,
            token: Some(token),
            device_id: None,
            protocol_initialized: false,
            connected_at,
        }
    }

    pub fn matches_selector(&self, selector: &PublishSelector) -> bool {
        match selector {
            PublishSelector::ByUser { user_id } => {
                self.kind == ConnectionKind::User
                    && self.user_id.as_deref() == Some(user_id.as_str())
            }
            PublishSelector::ByAnonymousToken { token } => {
                self.kind == ConnectionKind::Anonymous
                    && self.token.as_deref() == Some(token.as_str())
            }
        }
    }
}

// ── Selector ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PublishSelector {
    ByUser { user_id: String },
    ByAnonymousToken { token: String },
}

impl PublishSelector {
    pub fn user(user_id: impl Into<String>) -> Self {
        Self::ByUser {
            user_id: user_id.into(),
        }
    }

    pub fn anonymous(token: impl Into<String>) -> Self {
        Self::ByAnonymousToken {
            token: token.into(),
        }
    }

    pub fn tag(&self) -> String {
        match self {
            PublishSelector::ByUser { user_id } => user_tag(user_id),
            PublishSelector::ByAnonymousToken { token } => anonymous_tag(token),
        }
    }
}

// ── Tag helpers ─────────────────────────────────────────────────────

pub fn user_tag(user_id: &str) -> String {
    format!("u:{user_id}")
}

pub fn anonymous_tag(token: &str) -> String {
    format!("a:{token}")
}

// ── Protocol helpers ────────────────────────────────────────────────

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct InitialMessage {
    protocol: String,
    version: i32,
}

pub fn is_initial_message(message: &str) -> bool {
    let message = message
        .strip_suffix(RECORD_SEPARATOR as char)
        .unwrap_or(message);

    serde_json::from_str::<InitialMessage>(message).ok()
        == Some(InitialMessage {
            protocol: "messagepack".to_string(),
            version: 1,
        })
}

// ── MessagePack message builders (used by NotifyDo) ─────────────────

pub fn create_ping() -> Vec<u8> {
    serialize(&Value::Array(vec![6.into()]))
}

pub fn build_user_update_message(
    update_type: i32,
    user_id: &str,
    date: &str,
    context_id: Option<&str>,
) -> Vec<u8> {
    create_update(
        vec![
            ("UserId".into(), user_id.into()),
            ("Date".into(), serialize_date(parse_timestamp(date))),
        ],
        update_type,
        context_id,
    )
}

pub fn build_folder_update_message(
    update_type: i32,
    folder_id: &str,
    user_id: &str,
    revision_date: &str,
    context_id: Option<&str>,
) -> Vec<u8> {
    create_update(
        vec![
            ("Id".into(), folder_id.into()),
            ("UserId".into(), user_id.into()),
            (
                "RevisionDate".into(),
                serialize_date(parse_timestamp(revision_date)),
            ),
        ],
        update_type,
        context_id,
    )
}

pub fn build_cipher_update_message(
    update_type: i32,
    cipher_id: &str,
    user_id: Option<&str>,
    organization_id: Option<&str>,
    collection_ids: Option<Vec<String>>,
    revision_date: Option<&str>,
    context_id: Option<&str>,
) -> Vec<u8> {
    let (payload_user_id, payload_collection_ids, payload_revision_date) =
        if let Some(collection_ids) = collection_ids {
            (
                Value::Nil,
                Value::Array(
                    collection_ids
                        .into_iter()
                        .map(Into::into)
                        .collect::<Vec<Value>>(),
                ),
                serialize_date(Utc::now().naive_utc()),
            )
        } else {
            (
                convert_option(user_id),
                Value::Nil,
                serialize_date(parse_timestamp(revision_date.unwrap_or_else(|| {
                    warn!("Missing revision date for cipher update; falling back to now");
                    ""
                }))),
            )
        };

    create_update(
        vec![
            ("Id".into(), cipher_id.into()),
            ("UserId".into(), payload_user_id),
            ("OrganizationId".into(), convert_option(organization_id)),
            ("CollectionIds".into(), payload_collection_ids),
            ("RevisionDate".into(), payload_revision_date),
        ],
        update_type,
        context_id,
    )
}

pub fn build_send_update_message(
    update_type: i32,
    send_id: &str,
    user_id: Option<&str>,
    revision_date: &str,
) -> Vec<u8> {
    create_update(
        vec![
            ("Id".into(), send_id.into()),
            ("UserId".into(), convert_option(user_id)),
            (
                "RevisionDate".into(),
                serialize_date(parse_timestamp(revision_date)),
            ),
        ],
        update_type,
        None,
    )
}

pub fn build_auth_request_message(
    user_id: &str,
    auth_request_id: &str,
    context_id: Option<&str>,
) -> Vec<u8> {
    create_update(
        vec![
            ("Id".into(), auth_request_id.into()),
            ("UserId".into(), user_id.into()),
        ],
        UpdateType::AuthRequest as i32,
        context_id,
    )
}

pub fn build_auth_response_message(
    user_id: &str,
    auth_request_id: &str,
    context_id: Option<&str>,
) -> Vec<u8> {
    create_update(
        vec![
            ("Id".into(), auth_request_id.into()),
            ("UserId".into(), user_id.into()),
        ],
        UpdateType::AuthRequestResponse as i32,
        context_id,
    )
}

pub fn build_anonymous_auth_response_message(user_id: &str, auth_request_id: &str) -> Vec<u8> {
    create_anonymous_update(
        vec![
            ("Id".into(), auth_request_id.into()),
            ("UserId".into(), user_id.into()),
        ],
        UpdateType::AuthRequestResponse as i32,
        user_id,
    )
}

// ── DO fan-out protocol ─────────────────────────────────────────────

#[derive(Serialize)]
struct DoFanoutRequest<'a> {
    selector: &'a PublishSelector,
    message: String,
}

async fn send_ws_to_do(
    env: &Env,
    selector: &PublishSelector,
    ws_bytes: &[u8],
) -> Result<(), AppError> {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let namespace = env.durable_object("NOTIFY_DO").map_err(AppError::Worker)?;
    let stub = namespace.get_by_name("global").map_err(AppError::Worker)?;

    let body = serde_json::to_string(&DoFanoutRequest {
        selector,
        message: STANDARD.encode(ws_bytes),
    })
    .map_err(|_| AppError::Internal)?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(JsValue::from_str(&body)));

    let mut request =
        Request::new_with_init(INTERNAL_FANOUT_URL, &init).map_err(AppError::Worker)?;
    request
        .headers_mut()
        .map_err(AppError::Worker)?
        .set("Content-Type", "application/json")
        .map_err(AppError::Worker)?;

    let mut response = stub
        .fetch_with_request(request)
        .await
        .map_err(AppError::Worker)?;
    if !(200..300).contains(&response.status_code()) {
        let body = response.text().await.unwrap_or_else(|_| String::new());
        return Err(AppError::Worker(worker::Error::RustError(format!(
            "NotifyDo fanout failed with status {}: {}",
            response.status_code(),
            body
        ))));
    }

    Ok(())
}

// ── Publish helpers (called by handlers) ────────────────────────────

pub async fn publish_user_update(
    env: &Env,
    user_id: &str,
    update_type: UpdateType,
    date: &str,
    context_id: Option<&str>,
) -> Result<(), AppError> {
    let ws_bytes = build_user_update_message(update_type as i32, user_id, date, context_id);
    let selector = PublishSelector::user(user_id);
    send_ws_to_do(env, &selector, &ws_bytes).await?;
    push::push_user_update(env, user_id, update_type as i32, date, context_id).await;
    Ok(())
}

pub async fn publish_user_logout(
    env: &Env,
    user_id: &str,
    date: &str,
    context_id: Option<&str>,
) -> Result<(), AppError> {
    publish_user_update(env, user_id, UpdateType::LogOut, date, context_id).await
}

pub async fn publish_folder_update(
    env: &Env,
    user_id: &str,
    update_type: UpdateType,
    folder_id: &str,
    revision_date: &str,
    context_id: Option<&str>,
) -> Result<(), AppError> {
    let ws_bytes = build_folder_update_message(
        update_type as i32,
        folder_id,
        user_id,
        revision_date,
        context_id,
    );
    let selector = PublishSelector::user(user_id);
    send_ws_to_do(env, &selector, &ws_bytes).await?;
    push::push_folder_update(
        env,
        user_id,
        update_type as i32,
        folder_id,
        revision_date,
        context_id,
    )
    .await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn publish_cipher_update(
    env: &Env,
    selector_user_id: &str,
    update_type: UpdateType,
    cipher_id: &str,
    payload_user_id: Option<&str>,
    organization_id: Option<&str>,
    collection_ids: Option<Vec<String>>,
    revision_date: Option<&str>,
    context_id: Option<&str>,
) -> Result<(), AppError> {
    let ws_bytes = build_cipher_update_message(
        update_type as i32,
        cipher_id,
        payload_user_id,
        organization_id,
        collection_ids,
        revision_date,
        context_id,
    );
    let selector = PublishSelector::user(selector_user_id);
    send_ws_to_do(env, &selector, &ws_bytes).await?;
    push::push_cipher_update(
        env,
        selector_user_id,
        update_type as i32,
        cipher_id,
        payload_user_id,
        revision_date,
        context_id,
    )
    .await;
    Ok(())
}

pub async fn publish_send_update(
    env: &Env,
    selector_user_id: &str,
    update_type: UpdateType,
    send_id: &str,
    payload_user_id: Option<&str>,
    revision_date: &str,
) -> Result<(), AppError> {
    let ws_bytes =
        build_send_update_message(update_type as i32, send_id, payload_user_id, revision_date);
    let selector = PublishSelector::user(selector_user_id);
    send_ws_to_do(env, &selector, &ws_bytes).await?;
    push::push_send_update(
        env,
        selector_user_id,
        update_type as i32,
        send_id,
        payload_user_id,
        revision_date,
    )
    .await;
    Ok(())
}

pub async fn publish_auth_request(
    env: &Env,
    user_id: &str,
    auth_request_id: &str,
    context_id: Option<&str>,
) -> Result<(), AppError> {
    let ws_bytes = build_auth_request_message(user_id, auth_request_id, context_id);
    let selector = PublishSelector::user(user_id);
    send_ws_to_do(env, &selector, &ws_bytes).await?;
    push::push_auth_request(env, user_id, auth_request_id, context_id).await;
    Ok(())
}

pub async fn publish_auth_response(
    env: &Env,
    user_id: &str,
    auth_request_id: &str,
    context_id: Option<&str>,
) -> Result<(), AppError> {
    let ws_bytes = build_auth_response_message(user_id, auth_request_id, context_id);
    let selector = PublishSelector::user(user_id);
    send_ws_to_do(env, &selector, &ws_bytes).await?;
    push::push_auth_response(env, user_id, auth_request_id, context_id).await;
    Ok(())
}

pub async fn publish_anonymous_update(
    env: &Env,
    token: &str,
    user_id: &str,
    auth_request_id: &str,
) -> Result<(), AppError> {
    let ws_bytes = build_anonymous_auth_response_message(user_id, auth_request_id);
    let selector = PublishSelector::anonymous(token);
    send_ws_to_do(env, &selector, &ws_bytes).await?;
    Ok(())
}

// ── MessagePack internals ───────────────────────────────────────────

fn create_update(
    payload: Vec<(Value, Value)>,
    update_type: i32,
    context_id: Option<&str>,
) -> Vec<u8> {
    use rmpv::Value as V;

    let value = V::Array(vec![
        1.into(),
        V::Map(vec![]),
        V::Nil,
        "ReceiveMessage".into(),
        V::Array(vec![V::Map(vec![
            ("ContextId".into(), convert_option(context_id)),
            ("Type".into(), update_type.into()),
            ("Payload".into(), payload.into()),
        ])]),
    ]);

    serialize(&value)
}

fn create_anonymous_update(
    payload: Vec<(Value, Value)>,
    update_type: i32,
    user_id: &str,
) -> Vec<u8> {
    use rmpv::Value as V;

    let value = V::Array(vec![
        1.into(),
        V::Map(vec![]),
        V::Nil,
        "AuthRequestResponseRecieved".into(),
        V::Array(vec![V::Map(vec![
            ("Type".into(), update_type.into()),
            ("Payload".into(), payload.into()),
            ("UserId".into(), user_id.into()),
        ])]),
    ]);

    serialize(&value)
}

fn serialize(value: &Value) -> Vec<u8> {
    use rmpv::encode::write_value;

    let mut buffer = Vec::new();
    write_value(&mut buffer, value).expect("msgpack encoding should not fail");

    let mut size = buffer.len();
    let mut prefix = Vec::new();

    loop {
        let mut size_part = size & 0x7f;
        size >>= 7;

        if size > 0 {
            size_part |= 0x80;
        }

        prefix.push(size_part as u8);

        if size == 0 {
            break;
        }
    }

    prefix.append(&mut buffer);
    prefix
}

fn serialize_date(date: NaiveDateTime) -> Value {
    let seconds = date.and_utc().timestamp();
    let nanos = i64::from(date.and_utc().timestamp_subsec_nanos());
    let timestamp = (nanos << 34) | seconds;

    Value::Ext(-1, timestamp.to_be_bytes().to_vec())
}

fn convert_option<T: Into<Value>>(option: Option<T>) -> Value {
    match option {
        Some(value) => value.into(),
        None => Value::Nil,
    }
}

fn parse_timestamp(date: &str) -> NaiveDateTime {
    if date.is_empty() {
        return Utc::now().naive_utc();
    }

    DateTime::parse_from_rfc3339(date)
        .map(|value| value.naive_utc())
        .unwrap_or_else(|error| {
            warn!("Failed to parse RFC3339 timestamp '{date}': {error}");
            Utc::now().naive_utc()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::decode::read_value;

    #[test]
    fn update_type_values_match_vaultwarden() {
        assert_eq!(UpdateType::SyncCipherUpdate as i32, 0);
        assert_eq!(UpdateType::SyncCipherCreate as i32, 1);
        assert_eq!(UpdateType::SyncLoginDelete as i32, 2);
        assert_eq!(UpdateType::SyncFolderDelete as i32, 3);
        assert_eq!(UpdateType::SyncCiphers as i32, 4);
        assert_eq!(UpdateType::SyncVault as i32, 5);
        assert_eq!(UpdateType::SyncOrgKeys as i32, 6);
        assert_eq!(UpdateType::SyncFolderCreate as i32, 7);
        assert_eq!(UpdateType::SyncFolderUpdate as i32, 8);
        assert_eq!(UpdateType::SyncSettings as i32, 10);
        assert_eq!(UpdateType::LogOut as i32, 11);
        assert_eq!(UpdateType::SyncSendCreate as i32, 12);
        assert_eq!(UpdateType::SyncSendUpdate as i32, 13);
        assert_eq!(UpdateType::SyncSendDelete as i32, 14);
        assert_eq!(UpdateType::AuthRequest as i32, 15);
        assert_eq!(UpdateType::AuthRequestResponse as i32, 16);
        assert_eq!(UpdateType::None as i32, 100);
    }

    #[test]
    fn selector_tags_are_stable() {
        assert_eq!(user_tag("user-1"), "u:user-1");
        assert_eq!(anonymous_tag("token-1"), "a:token-1");
    }

    #[test]
    fn initial_message_accepts_record_separator() {
        assert!(is_initial_message(
            "{\"protocol\":\"messagepack\",\"version\":1}\u{1e}"
        ));
        assert!(is_initial_message(
            "{\"protocol\":\"messagepack\",\"version\":1}"
        ));
        assert!(!is_initial_message("{\"protocol\":\"json\",\"version\":1}"));
    }

    #[test]
    fn build_user_update_message_targets_receive_message() {
        let bytes = build_user_update_message(
            UpdateType::SyncSettings as i32,
            "user-1",
            "2026-03-14T00:00:00.000Z",
            None,
        );
        let mut slice = &bytes[1..];
        let value = read_value(&mut slice).expect("valid msgpack");

        match value {
            Value::Array(parts) => {
                assert_eq!(parts[3], Value::from("ReceiveMessage"));
                match &parts[4] {
                    Value::Array(args) => match &args[0] {
                        Value::Map(map) => {
                            assert!(map.iter().any(|(key, value)| {
                                key == &Value::from("Type")
                                    && value == &Value::from(UpdateType::SyncSettings as i32)
                            }));
                        }
                        other => panic!("expected map, got {other:?}"),
                    },
                    other => panic!("expected args array, got {other:?}"),
                }
            }
            other => panic!("expected invocation array, got {other:?}"),
        }
    }
}
