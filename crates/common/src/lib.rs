/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::{
    borrow::Cow,
    net::IpAddr,
    sync::{atomic::AtomicU8, Arc},
};

use ahash::AHashMap;
use arc_swap::ArcSwap;
use auth::{roles::RolePermissions, AccessToken};
use config::{
    imap::ImapConfig,
    jmap::settings::JmapConfig,
    scripts::Scripting,
    smtp::{
        auth::{ArcSealer, DkimSigner},
        queue::RelayHost,
        SmtpConfig,
    },
    storage::Storage,
    telemetry::Metrics,
};
use directory::{
    backend::internal::manage::ManageDirectory, core::secret::verify_secret_hash, Directory,
    Principal, QueryBy, Type,
};
use expr::if_block::IfBlock;
use futures::StreamExt;
use listener::{
    blocked::{AllowedIps, BlockedIps},
    tls::TlsManager,
};
use mail_send::Credentials;

use manager::webadmin::Resource;
use parking_lot::Mutex;
use reqwest::Response;
use sieve::Sieve;
use store::{
    write::{QueueClass, ValueClass},
    IterateParams, LookupStore, ValueKey,
};
use tokio::sync::{mpsc, oneshot};
use trc::AddContext;
use utils::{
    map::ttl_dashmap::{ADashMap, TtlDashMap},
    BlobHash,
};

pub mod addresses;
pub mod auth;
pub mod config;
#[cfg(feature = "enterprise")]
pub mod enterprise;
pub mod expr;
pub mod listener;
pub mod manager;
pub mod scripts;
pub mod telemetry;

pub use psl;

pub static USER_AGENT: &str = concat!("Stalwart/", env!("CARGO_PKG_VERSION"),);
pub static DAEMON_NAME: &str = concat!("Stalwart Mail Server v", env!("CARGO_PKG_VERSION"),);

pub const IPC_CHANNEL_BUFFER: usize = 1024;

pub type SharedCore = Arc<ArcSwap<Core>>;

#[derive(Clone, Default)]
pub struct Core {
    pub storage: Storage,
    pub sieve: Scripting,
    pub network: Network,
    pub tls: TlsManager,
    pub smtp: SmtpConfig,
    pub jmap: JmapConfig,
    pub imap: ImapConfig,
    pub metrics: Metrics,
    pub security: Security,
    #[cfg(feature = "enterprise")]
    pub enterprise: Option<enterprise::Enterprise>,
}

//TODO: temporary hack until OIDC is implemented
#[derive(Default)]
pub struct Security {
    pub logos: Mutex<AHashMap<String, Option<Resource<Vec<u8>>>>>,
    pub access_tokens: TtlDashMap<u32, Arc<AccessToken>>,
    pub permissions: ADashMap<u32, Arc<RolePermissions>>,
    pub permissions_version: AtomicU8,
}

#[derive(Clone)]
pub struct Network {
    pub node_id: u64,
    pub blocked_ips: BlockedIps,
    pub allowed_ips: AllowedIps,
    pub http_response_url: IfBlock,
    pub http_allowed_endpoint: IfBlock,
}

#[derive(Debug)]
pub enum DeliveryEvent {
    Ingest {
        message: IngestMessage,
        result_tx: oneshot::Sender<Vec<DeliveryResult>>,
    },
    Stop,
}

pub struct Ipc {
    pub delivery_tx: mpsc::Sender<DeliveryEvent>,
}

#[derive(Debug)]
pub struct IngestMessage {
    pub sender_address: String,
    pub recipients: Vec<String>,
    pub message_blob: BlobHash,
    pub message_size: usize,
    pub session_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryResult {
    Success,
    TemporaryFailure {
        reason: Cow<'static, str>,
    },
    PermanentFailure {
        code: [u8; 3],
        reason: Cow<'static, str>,
    },
}

pub trait IntoString: Sized {
    fn into_string(self) -> String;
}

impl IntoString for Vec<u8> {
    fn into_string(self) -> String {
        String::from_utf8(self)
            .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
    }
}

impl Core {
    pub fn get_directory(&self, name: &str) -> Option<&Arc<Directory>> {
        self.storage.directories.get(name)
    }

    pub fn get_directory_or_default(&self, name: &str, session_id: u64) -> &Arc<Directory> {
        self.storage.directories.get(name).unwrap_or_else(|| {
            if !name.is_empty() {
                trc::event!(
                    Eval(trc::EvalEvent::DirectoryNotFound),
                    Id = name.to_string(),
                    SpanId = session_id,
                );
            }

            &self.storage.directory
        })
    }

    pub fn get_lookup_store(&self, name: &str, session_id: u64) -> &LookupStore {
        self.storage.lookups.get(name).unwrap_or_else(|| {
            if !name.is_empty() {
                trc::event!(
                    Eval(trc::EvalEvent::StoreNotFound),
                    Id = name.to_string(),
                    SpanId = session_id,
                );
            }

            &self.storage.lookup
        })
    }

    pub fn get_arc_sealer(&self, name: &str, session_id: u64) -> Option<&ArcSealer> {
        self.smtp
            .mail_auth
            .sealers
            .get(name)
            .map(|s| s.as_ref())
            .or_else(|| {
                trc::event!(
                    Arc(trc::ArcEvent::SealerNotFound),
                    Id = name.to_string(),
                    SpanId = session_id,
                );

                None
            })
    }

    pub fn get_dkim_signer(&self, name: &str, session_id: u64) -> Option<&DkimSigner> {
        self.smtp
            .mail_auth
            .signers
            .get(name)
            .map(|s| s.as_ref())
            .or_else(|| {
                trc::event!(
                    Dkim(trc::DkimEvent::SignerNotFound),
                    Id = name.to_string(),
                    SpanId = session_id,
                );

                None
            })
    }

    pub fn get_trusted_sieve_script(&self, name: &str, session_id: u64) -> Option<&Arc<Sieve>> {
        self.sieve.trusted_scripts.get(name).or_else(|| {
            trc::event!(
                Sieve(trc::SieveEvent::ScriptNotFound),
                Id = name.to_string(),
                SpanId = session_id,
            );

            None
        })
    }

    pub fn get_untrusted_sieve_script(&self, name: &str, session_id: u64) -> Option<&Arc<Sieve>> {
        self.sieve.untrusted_scripts.get(name).or_else(|| {
            trc::event!(
                Sieve(trc::SieveEvent::ScriptNotFound),
                Id = name.to_string(),
                SpanId = session_id,
            );

            None
        })
    }

    pub fn get_relay_host(&self, name: &str, session_id: u64) -> Option<&RelayHost> {
        self.smtp.queue.relay_hosts.get(name).or_else(|| {
            trc::event!(
                Smtp(trc::SmtpEvent::RemoteIdNotFound),
                Id = name.to_string(),
                SpanId = session_id,
            );

            None
        })
    }

    pub async fn authenticate(
        &self,
        directory: &Directory,
        session_id: u64,
        credentials: &Credentials<String>,
        remote_ip: IpAddr,
        return_member_of: bool,
    ) -> trc::Result<Principal> {
        // First try to authenticate the user against the default directory
        let result = match directory
            .query(QueryBy::Credentials(credentials), return_member_of)
            .await
        {
            Ok(Some(principal)) => {
                trc::event!(
                    Auth(trc::AuthEvent::Success),
                    AccountName = credentials.login().to_string(),
                    AccountId = principal.id(),
                    SpanId = session_id,
                    Type = principal.typ().as_str(),
                );

                return Ok(principal);
            }
            Ok(None) => Ok(()),
            Err(err) => {
                if err.matches(trc::EventType::Auth(trc::AuthEvent::MissingTotp)) {
                    return Err(err);
                } else {
                    Err(err)
                }
            }
        };

        // Then check if the credentials match the fallback admin or master user
        match (
            &self.jmap.fallback_admin,
            &self.jmap.master_user,
            credentials,
        ) {
            (Some((fallback_admin, fallback_pass)), _, Credentials::Plain { username, secret })
                if username == fallback_admin =>
            {
                if verify_secret_hash(fallback_pass, secret).await? {
                    trc::event!(
                        Auth(trc::AuthEvent::Success),
                        AccountName = username.clone(),
                        SpanId = session_id,
                    );

                    return Ok(Principal::fallback_admin(fallback_pass));
                }
            }
            (_, Some((master_user, master_pass)), Credentials::Plain { username, secret })
                if username.ends_with(master_user) =>
            {
                if verify_secret_hash(master_pass, secret).await? {
                    let username = username.strip_suffix(master_user).unwrap();
                    let username = username.strip_suffix('%').unwrap_or(username);

                    if let Some(principal) = directory
                        .query(QueryBy::Name(username), return_member_of)
                        .await?
                    {
                        trc::event!(
                            Auth(trc::AuthEvent::Success),
                            AccountName = username.to_string(),
                            SpanId = session_id,
                            AccountId = principal.id(),
                            Type = principal.typ().as_str(),
                        );

                        return Ok(principal);
                    }
                }
            }
            _ => {}
        }

        if let Err(err) = result {
            Err(err)
        } else if self.has_auth_fail2ban() {
            let login = credentials.login();
            if self.is_auth_fail2banned(remote_ip, login).await? {
                Err(trc::SecurityEvent::AuthenticationBan
                    .into_err()
                    .ctx(trc::Key::RemoteIp, remote_ip)
                    .ctx(trc::Key::AccountName, login.to_string()))
            } else {
                Err(trc::AuthEvent::Failed
                    .ctx(trc::Key::RemoteIp, remote_ip)
                    .ctx(trc::Key::AccountName, login.to_string()))
            }
        } else {
            Err(trc::AuthEvent::Failed
                .ctx(trc::Key::RemoteIp, remote_ip)
                .ctx(trc::Key::AccountName, credentials.login().to_string()))
        }
    }

    pub async fn total_queued_messages(&self) -> trc::Result<u64> {
        let mut total = 0;
        self.storage
            .data
            .iterate(
                IterateParams::new(
                    ValueKey::from(ValueClass::Queue(QueueClass::Message(0))),
                    ValueKey::from(ValueClass::Queue(QueueClass::Message(u64::MAX))),
                )
                .no_values(),
                |_, _| {
                    total += 1;

                    Ok(true)
                },
            )
            .await
            .map(|_| total)
    }

    pub async fn total_accounts(&self) -> trc::Result<u64> {
        self.storage
            .data
            .count_principals(None, Type::Individual.into(), None)
            .await
            .caused_by(trc::location!())
    }

    pub async fn total_domains(&self) -> trc::Result<u64> {
        self.storage
            .data
            .count_principals(None, Type::Domain.into(), None)
            .await
            .caused_by(trc::location!())
    }
}

trait CredentialsUsername {
    fn login(&self) -> &str;
}

impl CredentialsUsername for Credentials<String> {
    fn login(&self) -> &str {
        match self {
            Credentials::Plain { username, .. }
            | Credentials::XOauth2 { username, .. }
            | Credentials::OAuthBearer { token: username } => username,
        }
    }
}

impl Clone for Security {
    fn clone(&self) -> Self {
        Self {
            access_tokens: self.access_tokens.clone(),
            permissions: self.permissions.clone(),
            permissions_version: AtomicU8::new(
                self.permissions_version
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            logos: Mutex::new(self.logos.lock().clone()),
        }
    }
}

pub trait HttpLimitResponse: Sync + Send {
    fn bytes_with_limit(
        self,
        limit: usize,
    ) -> impl std::future::Future<Output = reqwest::Result<Option<Vec<u8>>>> + Send;
}

impl HttpLimitResponse for Response {
    async fn bytes_with_limit(self, limit: usize) -> reqwest::Result<Option<Vec<u8>>> {
        if self
            .content_length()
            .map_or(false, |len| len as usize > limit)
        {
            return Ok(None);
        }

        let mut bytes = Vec::with_capacity(std::cmp::min(limit, 1024));
        let mut stream = self.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len() + chunk.len() > limit {
                return Ok(None);
            }
            bytes.extend_from_slice(&chunk);
        }

        Ok(Some(bytes))
    }
}
