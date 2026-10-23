//! The SSH listener: the accept loop, one [`Conn`] handler per connection, and the channel
//! policy (SPEC §25.11.1, §25.11.4, §25.11.7, §25.11.11).
//!
//! What a connection can do, in order:
//!
//! 1. Admission ([`SshLimits::admit`]) runs on `accept`, before any key-exchange work. A refused
//!    connection gets `SSH_MSG_DISCONNECT` (`too many connections`) and is closed.
//! 2. Key exchange with the fixed algorithm set of [`server_config`], then `publickey` login
//!    ([`super::auth`]). `auth_timeout` bounds the whole of this, and a stalled peer's socket is
//!    shut down, since the SSH library does not read its queue during key exchange.
//! 3. After login, up to `max_channels` `session` channels. On each one, exactly one `exec` of
//!    `git-upload-pack` or `git-receive-pack` ([`super::policy::parse_command`]) is handed to the
//!    [`PackExec`]; every other request is refused, audited and counted, and a connection that
//!    makes [`MAX_REFUSED`] refused requests is disconnected.
//!
//! The listener has no key list, no ACL and no shell. A command's rights are decided by the
//! `PackExec` through [`Authorizer::authorize_ssh_key`], once per command, so a multiplexed
//! connection is re-authorised for every command.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Shutdown, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use asc_git::compat::GitProtocol;
use asc_rpc::auth::{Authorizer, Principal, SshFingerprint, SshIdentity, SshKey};
use asc_rpc::limits::CounterGuard;
use russh::keys::PublicKey;
use russh::server::{Auth, Config, Handler, Msg, Session};
use russh::{
    Channel, ChannelId, ChannelOpenFailure, Disconnect, MethodKind, MethodSet, Preferred, Pty,
    SshId, cipher, compression, kex, mac,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};

use super::audit::{Audit, CommandRecord, LoginOutcome, LoginRecord, RefusedRecord};
use super::auth::{self, Identified};
use super::host_key::{HostKeyError, HostKeys};
use super::limits::{ConnPermit, SshLimits};
use super::policy::{self, Command, Request, Verb};
use crate::config::{SshConfig, SshLimitsConfig};

/// The identification string sent first. It names no library and no version.
pub const SERVER_ID: &str = "SSH-2.0-asc";
/// Refused channel requests one connection may make before it is disconnected.
pub const MAX_REFUSED: u32 = 64;
/// Chunks of client data queued for a running command.
const INPUT_QUEUE: usize = 4;
/// Chunks of command output queued for the channel.
const OUTPUT_QUEUE: usize = 8;
/// How long a connection that was told to go may take to close before its socket is shut.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// The `SSH_MSG_DISCONNECT` reason for a refused connection.
const TOO_MANY_CONNECTIONS: u32 = 12;

// ------------------------------------------------------------------------------ the exec seam

/// A chunk of command output.
pub enum Out {
    Stdout(Bytes),
    /// Extended data type 1: the channel's stderr.
    Stderr(Bytes),
}

/// The write side of a command's channel. Dropping every clone ends the output.
#[derive(Clone)]
pub struct PackOut {
    tx: mpsc::Sender<Out>,
}

impl PackOut {
    /// Sends stdout bytes; `false` when the channel is gone.
    pub async fn stdout(&self, data: Bytes) -> bool {
        self.tx.send(Out::Stdout(data)).await.is_ok()
    }

    /// Sends stderr bytes; `false` when the channel is gone.
    pub async fn stderr(&self, data: Bytes) -> bool {
        self.tx.send(Out::Stderr(data)).await.is_ok()
    }
}

/// A command's channel: what the client sends, and where the output goes. The input ends when
/// the client sends EOF or closes the channel.
pub struct PackIo {
    pub input: mpsc::Receiver<Bytes>,
    pub output: PackOut,
}

/// One validated pack command and who runs it.
pub struct PackRequest {
    pub verb: Verb,
    /// `owner/name`, validated by [`policy::parse_command`]; still to be resolved by registry
    /// lookup.
    pub repo: String,
    /// The validated `GIT_PROTOCOL` of the channel's `env` request, if any.
    pub git_protocol: Option<GitProtocol>,
    pub key: SshKey,
    pub identity: SshIdentity,
    /// The principal the key was recognised as at login.
    pub principal: Principal,
    /// The one authorizer: call `authorize_ssh_key(&key, repo, verb.access())` once per command.
    pub authorizer: Arc<dyn Authorizer>,
    pub peer: Option<IpAddr>,
}

/// Runs a pack command on a channel (U87 implements it). The server enforces
/// `limits.ssh.request_timeout` by dropping the future, so an implementation must stop its
/// children when dropped, and must not keep [`PackOut`] clones after it returns.
#[async_trait]
pub trait PackExec: Send + Sync {
    /// Runs the command and returns the exit status the client sees.
    async fn run(&self, request: PackRequest, io: PackIo) -> u32;
}

/// The default [`PackExec`]: git over SSH is not wired in this build.
pub struct Unavailable;

#[async_trait]
impl PackExec for Unavailable {
    async fn run(&self, _request: PackRequest, io: PackIo) -> u32 {
        io.output
            .stderr(Bytes::from_static(b"asc: git over ssh is not available\n"))
            .await;
        1
    }
}

// ------------------------------------------------------------------------------ shared state

/// What every connection of one listener shares.
pub struct Shared {
    pub user: String,
    pub min_rsa_bits: u32,
    pub limits: Arc<SshLimits>,
    pub authorizer: Arc<dyn Authorizer>,
    pub exec: Arc<dyn PackExec>,
    pub audit: Audit,
}

/// Decrements `asc_ssh_sessions` when an authenticated connection ends.
struct SessionGuard(Audit);

impl SessionGuard {
    fn open(audit: &Audit) -> Self {
        audit.metrics().session_opened();
        Self(audit.clone())
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.metrics().session_closed();
    }
}

/// A logged-in connection.
struct Login {
    identified: Identified,
    principal: Principal,
    _slot: CounterGuard<String>,
    _session: SessionGuard,
}

enum Chan {
    /// Open, no command yet.
    Idle { git_protocol: Option<GitProtocol> },
    /// The one `exec` was taken.
    Taken {
        /// Client data for the command; `None` after EOF.
        input: Option<mpsc::Sender<Bytes>>,
        task: Option<AbortHandle>,
    },
}

/// The per-connection handler.
pub struct Conn {
    shared: Arc<Shared>,
    peer: Option<SocketAddr>,
    permit: ConnPermit,
    authenticated: Option<oneshot::Sender<()>>,
    /// A key accepted at the offer whose signature has not led to a login yet.
    offered: Option<(Identified, String)>,
    login: Option<Login>,
    channels: HashMap<ChannelId, Chan>,
    running: Arc<Semaphore>,
    refused: u32,
    /// Channels a command closed by itself; their slots are freed at the next channel open.
    /// (The library forgets a channel the server closed first, so no `channel_close` follows.)
    closed_tx: mpsc::UnboundedSender<ChannelId>,
    closed_rx: mpsc::UnboundedReceiver<ChannelId>,
}

impl Conn {
    fn new(
        shared: Arc<Shared>,
        peer: Option<SocketAddr>,
        permit: ConnPermit,
        authenticated: oneshot::Sender<()>,
    ) -> Self {
        let running = Arc::new(Semaphore::new(
            shared.limits.config().max_running_commands as usize,
        ));
        let (closed_tx, closed_rx) = mpsc::unbounded_channel();
        Self {
            shared,
            peer,
            permit,
            authenticated: Some(authenticated),
            offered: None,
            login: None,
            channels: HashMap::new(),
            running,
            refused: 0,
            closed_tx,
            closed_rx,
        }
    }

    fn ip(&self) -> Option<IpAddr> {
        self.peer.map(|p| p.ip())
    }

    /// Records a failed login and charges the peer.
    fn failed(&self, user: &str, key: Option<&PublicKey>, outcome: LoginOutcome) {
        let mut record = LoginRecord::new(self.ip(), user, outcome);
        if let Some(key) = key {
            record.key_type = key.algorithm().as_str().to_owned();
            record.fingerprint = key
                .to_bytes()
                .ok()
                .map(|blob| SshFingerprint::of_blob(&blob));
        }
        self.shared.audit.login(record);
        if let Some(ip) = self.ip() {
            self.shared.limits.failed_login(ip);
        }
    }

    /// A key accepted at the offer that never produced a valid signature.
    fn flush_offer(&mut self) {
        if let Some((id, user)) = self.offered.take() {
            let mut record = LoginRecord::new(self.ip(), &user, LoginOutcome::BadSignature);
            record.key_type = id.key.algo().to_owned();
            record.fingerprint = Some(id.fingerprint);
            self.shared.audit.login(record);
            if let Some(ip) = self.ip() {
                self.shared.limits.failed_login(ip);
            }
        }
    }

    /// The user name check, the key policy and the authorizer's answer.
    async fn evaluate(&self, user: &str, key: &PublicKey) -> Result<Identified, LoginOutcome> {
        if !auth::user_ok(&self.shared.user, user) {
            return Err(LoginOutcome::User);
        }
        auth::identify(&*self.shared.authorizer, key, self.shared.min_rsa_bits).await
    }

    /// Audits a refused request. Past [`MAX_REFUSED`] of them the connection is dropped.
    fn refuse(&mut self, request: Request, session: &mut Session) {
        if let Some(login) = &self.login {
            self.shared.audit.refused(RefusedRecord {
                peer: self.ip(),
                fingerprint: login.identified.fingerprint,
                principal: login.principal.id.clone(),
                request: request.name(),
            });
        }
        self.refused = self.refused.saturating_add(1);
        if self.refused == MAX_REFUSED {
            let _ = session.disconnect(Disconnect::ByApplication, "too many refused requests", "");
        }
    }

    /// Writes `text` on stderr, `code` as the exit status, then EOF and close.
    fn finish(session: &mut Session, channel: ChannelId, code: u32, text: &str) {
        let _ = session.extended_data(channel, 1, Bytes::copy_from_slice(text.as_bytes()));
        let _ = session.exit_status_request(channel, code);
        let _ = session.eof(channel);
        let _ = session.close(channel);
    }

    /// Starts the one command of a channel.
    fn start(
        &mut self,
        channel: ChannelId,
        command: Command,
        permit: OwnedSemaphorePermit,
        session: &mut Session,
    ) {
        let Some(login) = &self.login else { return };
        let git_protocol = match self.channels.get(&channel) {
            Some(Chan::Idle { git_protocol }) => git_protocol.clone(),
            _ => None,
        };
        let request = PackRequest {
            verb: command.verb,
            repo: command.repo.clone(),
            git_protocol,
            key: login.identified.key.clone(),
            identity: login.identified.identity.clone(),
            principal: login.principal.clone(),
            authorizer: self.shared.authorizer.clone(),
            peer: self.ip(),
        };
        let (in_tx, in_rx) = mpsc::channel::<Bytes>(INPUT_QUEUE);
        let (out_tx, out_rx) = mpsc::channel::<Out>(OUTPUT_QUEUE);
        let io = PackIo {
            input: in_rx,
            output: PackOut { tx: out_tx },
        };
        let job = Job {
            exec: self.shared.exec.clone(),
            audit: self.shared.audit.clone(),
            handle: session.handle(),
            channel,
            closed: self.closed_tx.clone(),
            timeout: Duration::from_secs(self.shared.limits.config().request_timeout.0),
            record: CommandRecord {
                peer: self.ip(),
                fingerprint: login.identified.fingerprint,
                principal: login.principal.id.clone(),
                verb: command.verb.as_str(),
                repo: command.repo,
                outcome: String::new(),
                exit_status: None,
                millis: 0,
            },
        };
        let task = tokio::spawn(async move {
            job.run(request, io, out_rx).await;
            drop(permit);
        });
        self.channels.insert(
            channel,
            Chan::Taken {
                input: Some(in_tx),
                task: Some(task.abort_handle()),
            },
        );
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.flush_offer();
        // The connection is gone: stop its commands.
        for chan in self.channels.values() {
            if let Chan::Taken {
                task: Some(task), ..
            } = chan
            {
                task.abort();
            }
        }
    }
}

/// One running command: runs it, pumps its output to the channel, reports how it ended.
struct Job {
    exec: Arc<dyn PackExec>,
    audit: Audit,
    handle: russh::server::Handle,
    channel: ChannelId,
    closed: mpsc::UnboundedSender<ChannelId>,
    timeout: Duration,
    record: CommandRecord,
}

impl Job {
    async fn run(mut self, request: PackRequest, io: PackIo, mut out: mpsc::Receiver<Out>) {
        let started = Instant::now();
        let mut run = Box::pin(tokio::time::timeout(
            self.timeout,
            self.exec.run(request, io),
        ));
        let mut result = None;
        let mut gone = false;
        loop {
            if result.is_some() {
                // Output the command queued before it ended still goes out.
                out.close();
                while let Some(chunk) = out.recv().await {
                    if !self.send(chunk).await {
                        gone = true;
                        break;
                    }
                }
                break;
            }
            tokio::select! {
                biased;
                r = &mut run => result = Some(r),
                chunk = out.recv() => match chunk {
                    Some(chunk) => {
                        if !self.send(chunk).await {
                            gone = true;
                            break;
                        }
                    }
                    // Every sender is gone: only the command's own end is left to wait for.
                    None => result = Some((&mut run).await),
                },
            }
        }
        drop(run);
        let (code, outcome) = match result {
            _ if gone => (1, "killed".to_owned()),
            Some(Ok(0)) => (0, "ok".to_owned()),
            Some(Ok(code)) => (code, format!("exit:{code}")),
            Some(Err(_)) => {
                let _ = self
                    .handle
                    .extended_data(
                        self.channel,
                        1,
                        Bytes::from_static(policy::TIMED_OUT.as_bytes()),
                    )
                    .await;
                (1, "limit:request_timeout".to_owned())
            }
            None => (1, "killed".to_owned()),
        };
        if !gone {
            let _ = self.handle.exit_status_request(self.channel, code).await;
            let _ = self.handle.eof(self.channel).await;
            let _ = self.handle.close(self.channel).await;
        }
        let _ = self.closed.send(self.channel);
        self.record.outcome = outcome;
        self.record.exit_status = Some(code);
        self.record.millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.audit.command(self.record.clone());
    }

    async fn send(&self, chunk: Out) -> bool {
        match chunk {
            Out::Stdout(b) => self.handle.data(self.channel, b).await.is_ok(),
            Out::Stderr(b) => self.handle.extended_data(self.channel, 1, b).await.is_ok(),
        }
    }
}

// ------------------------------------------------------------------------------ the handler

impl Handler for Conn {
    type Error = russh::Error;

    /// `none` is the client asking which methods exist: answered with the method list.
    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::reject())
    }

    async fn auth_password(&mut self, user: &str, _password: &str) -> Result<Auth, Self::Error> {
        self.failed(user, None, LoginOutcome::Method);
        Ok(Auth::reject())
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        _response: Option<russh::server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        self.failed(user, None, LoginOutcome::Method);
        Ok(Auth::reject())
    }

    async fn auth_openssh_certificate(
        &mut self,
        user: &str,
        certificate: &russh::keys::Certificate,
    ) -> Result<Auth, Self::Error> {
        let key = PublicKey::new(certificate.public_key().clone(), "");
        self.offered = None;
        self.failed(user, Some(&key), LoginOutcome::KeyType);
        Ok(Auth::reject())
    }

    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.flush_offer();
        match self.evaluate(user, key).await {
            Ok(identified) => {
                self.offered = Some((identified, user.to_owned()));
                Ok(Auth::Accept)
            }
            Err(outcome) => {
                self.failed(user, Some(key), outcome);
                Ok(Auth::reject())
            }
        }
    }

    /// Called after the library verified the signature. The key is looked up again, so a key
    /// revoked between the offer and the signature does not log in.
    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        self.offered = None;
        let identified = match self.evaluate(user, key).await {
            Ok(identified) => identified,
            Err(outcome) => {
                self.failed(user, Some(key), outcome);
                return Ok(Auth::reject());
            }
        };
        let principal = identified.identity.principal.clone();
        let Some(slot) = self.shared.limits.principal_slot(&principal.id) else {
            self.failed(
                user,
                Some(key),
                LoginOutcome::Limit("principal_connections"),
            );
            return Ok(Auth::reject());
        };
        let mut record = LoginRecord::new(self.ip(), user, LoginOutcome::Ok);
        record.key_type = identified.key.algo().to_owned();
        record.fingerprint = Some(identified.fingerprint);
        record.principal = Some(principal.id.clone());
        record.key_kind = Some(identified.identity.kind.as_str());
        record.key_id = Some(identified.identity.key_id.clone());
        self.shared.audit.login(record);
        self.permit.authenticated();
        self.login = Some(Login {
            identified,
            principal,
            _slot: slot,
            _session: SessionGuard::open(&self.shared.audit),
        });
        if let Some(done) = self.authenticated.take() {
            let _ = done.send(());
        }
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let id = channel.id();
        // The library blocks on a channel's queue that nobody reads, so it is dropped here;
        // everything is handled through this handler and the session handle.
        drop(channel);
        if self.login.is_none() {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        while let Ok(done) = self.closed_rx.try_recv() {
            self.channels.remove(&done);
        }
        if self.channels.len() >= self.shared.limits.config().max_channels as usize {
            self.refuse(Request::SessionOpen, session);
            reply.reject(ChannelOpenFailure::ResourceShortage).await;
            return Ok(());
        }
        self.channels.insert(id, Chan::Idle { git_protocol: None });
        reply.accept().await;
        Ok(())
    }

    async fn channel_open_x11(
        &mut self,
        channel: Channel<Msg>,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::server::ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        drop(channel);
        self.refuse(Request::X11Open, session);
        reply
            .reject(ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::server::ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        drop(channel);
        self.refuse(Request::DirectTcpip, session);
        reply
            .reject(ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        Ok(())
    }

    async fn channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::server::ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        drop(channel);
        self.refuse(Request::ForwardedTcpip, session);
        reply
            .reject(ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        Ok(())
    }

    async fn channel_open_direct_streamlocal(
        &mut self,
        channel: Channel<Msg>,
        _socket_path: &str,
        reply: russh::server::ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        drop(channel);
        self.refuse(Request::DirectStreamlocal, session);
        reply
            .reject(ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.channels.contains_key(&channel) {
            session.channel_failure(channel)?;
            return Ok(());
        }
        let who = self
            .login
            .as_ref()
            .map(|l| l.principal.id.clone())
            .unwrap_or_default();
        self.refuse(Request::Shell, session);
        session.channel_success(channel)?;
        Self::finish(session, channel, 1, &policy::shell_greeting(&who));
        // The server closed it: the library has forgotten the channel, and so do we.
        self.channels.remove(&channel);
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let taken = match self.channels.get(&channel) {
            Some(Chan::Idle { .. }) => false,
            Some(Chan::Taken { .. }) => true,
            None => {
                session.channel_failure(channel)?;
                return Ok(());
            }
        };
        if taken {
            // One `exec` per channel.
            self.refuse(Request::Exec, session);
            session.channel_failure(channel)?;
            return Ok(());
        }
        let command = match policy::parse_command(data) {
            Ok(command) => command,
            Err(_) => {
                self.refuse(Request::Exec, session);
                session.channel_success(channel)?;
                Self::finish(session, channel, 1, policy::NOT_ALLOWED);
                self.channels.remove(&channel);
                return Ok(());
            }
        };
        let Ok(permit) = self.running.clone().try_acquire_owned() else {
            if let Some(login) = &self.login {
                self.shared.audit.command(CommandRecord {
                    peer: self.ip(),
                    fingerprint: login.identified.fingerprint,
                    principal: login.principal.id.clone(),
                    verb: command.verb.as_str(),
                    repo: command.repo,
                    outcome: "limit:commands".into(),
                    exit_status: Some(1),
                    millis: 0,
                });
            }
            session.channel_success(channel)?;
            Self::finish(session, channel, 1, policy::BUSY);
            self.channels.remove(&channel);
            return Ok(());
        };
        session.channel_success(channel)?;
        self.start(channel, command, permit, session);
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(Chan::Taken {
            input: Some(input), ..
        }) = self.channels.get(&channel)
        else {
            // No command is reading: the bytes are dropped.
            return Ok(());
        };
        let input = input.clone();
        let idle = Duration::from_secs(self.shared.limits.config().idle_timeout.0);
        match tokio::time::timeout(idle, input.send(Bytes::copy_from_slice(data))).await {
            Ok(Ok(())) => {}
            // The command stopped reading: nothing more to do with the bytes.
            Ok(Err(_)) => {}
            // The command did not read for `idle_timeout` while the client kept sending.
            Err(_) => {
                if let Some(Chan::Taken { task, input }) = self.channels.get_mut(&channel) {
                    *input = None;
                    if let Some(task) = task.take() {
                        task.abort();
                    }
                }
                let _ = session.close(channel);
            }
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(Chan::Taken { input, .. }) = self.channels.get_mut(&channel) {
            *input = None;
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(Chan::Taken {
            task: Some(task), ..
        }) = self.channels.remove(&channel)
        {
            task.abort();
        }
        Ok(())
    }

    /// `GIT_PROTOCOL` is the only variable, and only with a valid value, before the `exec`.
    async fn env_request(
        &mut self,
        channel: ChannelId,
        variable_name: &str,
        variable_value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let (Some(Chan::Idle { git_protocol }), Some(value)) = (
            self.channels.get_mut(&channel),
            policy::env_git_protocol(variable_name, variable_value),
        ) {
            *git_protocol = Some(value);
            session.channel_success(channel)?;
        } else {
            self.refuse(Request::Env, session);
            session.channel_failure(channel)?;
        }
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.refuse(Request::Subsystem, session);
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.refuse(Request::PtyReq, session);
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn x11_request(
        &mut self,
        channel: ChannelId,
        _single_connection: bool,
        _x11_auth_protocol: &str,
        _x11_auth_cookie: &str,
        _x11_screen_number: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.refuse(Request::X11Req, session);
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn agent_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.refuse(Request::AuthAgentReq, session);
        let _ = channel;
        Ok(false)
    }

    async fn tcpip_forward(
        &mut self,
        _address: &str,
        _port: &mut u32,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.refuse(Request::TcpipForward, session);
        Ok(false)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        _address: &str,
        _port: u32,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.refuse(Request::CancelTcpipForward, session);
        Ok(false)
    }

    async fn streamlocal_forward(
        &mut self,
        _socket_path: &str,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.refuse(Request::StreamlocalForward, session);
        Ok(false)
    }

    async fn cancel_streamlocal_forward(
        &mut self,
        _socket_path: &str,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.refuse(Request::CancelStreamlocalForward, session);
        Ok(false)
    }
}

// ------------------------------------------------------------------------------ configuration

/// The library configuration for the listener (SPEC §25.11.2, §25.11.7).
///
/// Offers nothing but: key exchange `curve25519-sha256` (and its `@libssh.org` name) with the
/// strict key-exchange extension of CVE-2023-48795; the AEAD ciphers `chacha20-poly1305`,
/// `aes256-gcm` and `aes128-gcm` (so no separate MAC); no compression; host keys `ssh-ed25519`,
/// plus `rsa-sha2-512` and `rsa-sha2-256` when an RSA host key is configured. No CBC, no SHA-1
/// key exchange, no `ssh-rsa` host key.
pub fn server_config(limits: &SshLimitsConfig, host_keys: &HostKeys) -> Config {
    use russh::keys::{Algorithm, EcdsaCurve, HashAlg};
    // The library offers this list both as the host key algorithms (filtered to the keys held)
    // and, unfiltered, as `server-sig-algs` (RFC 8308): the signature algorithms a *client key*
    // may use. An OpenSSH client with an RSA key signs with what this list names, so it names
    // the SHA-2 RSA algorithms and never `ssh-rsa` (SHA-1), whether or not the host key is RSA.
    let key_algos = vec![
        Algorithm::Ed25519,
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256,
        },
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP384,
        },
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP521,
        },
        Algorithm::Rsa {
            hash: Some(HashAlg::Sha512),
        },
        Algorithm::Rsa {
            hash: Some(HashAlg::Sha256),
        },
    ];
    let keepalive = Duration::from_secs(limits.keepalive.0);
    Config {
        server_id: SshId::Standard(SERVER_ID.into()),
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        auth_rejection_time: Duration::from_millis(limits.reject_delay_ms),
        // The client's first `none` probe is not a failed attempt: no delay.
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: host_keys.private_keys(),
        window_size: u32::try_from(limits.window_size.0).unwrap_or(2 * 1024 * 1024),
        maximum_packet_size: u32::try_from(limits.packet_size.0).unwrap_or(32 * 1024),
        channel_buffer_size: OUTPUT_QUEUE,
        preferred: Preferred {
            kex: vec![
                kex::CURVE25519,
                kex::CURVE25519_PRE_RFC_8731,
                kex::EXTENSION_SUPPORT_AS_CLIENT,
                kex::EXTENSION_SUPPORT_AS_SERVER,
                kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
                kex::EXTENSION_OPENSSH_STRICT_KEX_AS_SERVER,
            ]
            .into(),
            key: key_algos.into(),
            cipher: vec![
                cipher::CHACHA20_POLY1305,
                cipher::AES_256_GCM,
                cipher::AES_128_GCM,
            ]
            .into(),
            mac: vec![mac::NONE].into(),
            compression: vec![compression::NONE].into(),
            ..Preferred::DEFAULT
        },
        max_auth_attempts: limits.max_auth_tries as usize,
        // A silent peer is closed by keep-alives (the library's inactivity timer is reset by
        // their answers); this bound also covers the wait for the client's identification.
        inactivity_timeout: Some(keepalive * (limits.keepalive_max + 1) + Duration::from_secs(30)),
        keepalive_interval: Some(keepalive),
        keepalive_max: limits.keepalive_max as usize,
        nodelay: true,
        ..Config::default()
    }
}

// ------------------------------------------------------------------------------ the listener

/// Why the listener could not start.
#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error(transparent)]
    HostKey(#[from] HostKeyError),
    #[error("ssh listen address {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("loading the ssh host keys: {0}")]
    Task(String),
}

/// What the listener is built from.
pub struct SshDeps {
    pub authorizer: Arc<dyn Authorizer>,
    /// The server's admission controller, shared with the HTTP surfaces (SPEC §15.6.2).
    pub admission: Option<Arc<crate::limits::Admission>>,
    pub audit: Audit,
    pub exec: Arc<dyn PackExec>,
}

/// A bound listener. [`run`](Self::run) serves it.
pub struct SshServer {
    listener: TcpListener,
    config: Arc<Config>,
    shared: Arc<Shared>,
    host_keys: HostKeys,
}

impl SshServer {
    /// Loads (or creates) the host keys and binds `cfg.listen`.
    ///
    /// # Errors
    /// A host key that fails the checks of SPEC §25.11.6, or an address that cannot be bound.
    pub async fn bind(
        cfg: &SshConfig,
        limits: &SshLimitsConfig,
        deps: SshDeps,
    ) -> Result<Self, SshError> {
        let files = cfg.host_key_files.clone();
        let host_keys = tokio::task::spawn_blocking(move || HostKeys::load_or_create(&files))
            .await
            .map_err(|e| SshError::Task(e.to_string()))??;
        let listener = TcpListener::bind(cfg.listen)
            .await
            .map_err(|source| SshError::Bind {
                addr: cfg.listen,
                source,
            })?;
        let config = Arc::new(server_config(limits, &host_keys));
        let shared = Arc::new(Shared {
            user: cfg.user.clone(),
            min_rsa_bits: cfg.min_rsa_bits,
            limits: Arc::new(SshLimits::new(limits, deps.admission)),
            authorizer: deps.authorizer,
            exec: deps.exec,
            audit: deps.audit,
        });
        Ok(Self {
            listener,
            config,
            shared,
            host_keys,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// The host keys presented, for the start-up log.
    pub fn host_keys(&self) -> &HostKeys {
        &self.host_keys
    }

    /// The limits in force (for tests and the admin surface).
    pub fn limits(&self) -> &Arc<SshLimits> {
        &self.shared.limits
    }

    /// Accepts connections until `shutdown` resolves. Dropping the future (or `shutdown`) ends
    /// every connection.
    pub async fn run(self, shutdown: impl Future<Output = ()>) {
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, peer)) => self.accept(&mut connections, stream, peer),
                    Err(e) => {
                        // Out of descriptors or a reset before accept: back off, keep serving.
                        tracing::warn!(error = %e, "ssh accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
            }
            while connections.try_join_next().is_some() {}
        }
        connections.abort_all();
    }

    fn accept(&self, connections: &mut JoinSet<()>, stream: TcpStream, peer: SocketAddr) {
        let shared = &self.shared;
        match shared.limits.admit(peer.ip()) {
            Ok(permit) => {
                shared.audit.metrics().connection("accepted");
                let (shared, config) = (shared.clone(), self.config.clone());
                connections.spawn(serve_connection(shared, config, stream, peer, permit));
            }
            Err(why) => {
                let which = why.which();
                shared.audit.metrics().connection(&format!("limit:{which}"));
                shared.audit.login(LoginRecord::new(
                    Some(peer.ip()),
                    "",
                    LoginOutcome::Limit(which),
                ));
                refuse_connection(&stream);
            }
        }
    }
}

/// Tells a refused peer `too many connections` and closes: the identification string, then one
/// unencrypted `SSH_MSG_DISCONNECT`. One non-blocking write; if the socket cannot take it the
/// peer gets a plain close.
fn refuse_connection(stream: &TcpStream) {
    let mut out = format!("{SERVER_ID}\r\n").into_bytes();
    out.extend_from_slice(&disconnect_packet(
        TOO_MANY_CONNECTIONS,
        "too many connections",
    ));
    let _ = stream.try_write(&out);
}

/// An unencrypted SSH binary packet carrying `SSH_MSG_DISCONNECT` (RFC 4253 sections 6, 11.1).
fn disconnect_packet(reason: u32, description: &str) -> Vec<u8> {
    let mut payload = vec![1u8];
    payload.extend_from_slice(&reason.to_be_bytes());
    payload.extend_from_slice(&(description.len() as u32).to_be_bytes());
    payload.extend_from_slice(description.as_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes()); // empty language tag
    // padding_length (1) + payload + padding: a multiple of 8, at least 4 bytes of padding.
    let mut padding = 8 - ((payload.len() + 5) % 8);
    if padding < 4 {
        padding += 8;
    }
    let mut packet = Vec::with_capacity(payload.len() + padding + 5);
    packet.extend_from_slice(&((payload.len() + padding + 1) as u32).to_be_bytes());
    packet.push(padding as u8);
    packet.extend_from_slice(&payload);
    packet.resize(packet.len() + padding, 0);
    packet
}

/// Runs one admitted connection to its end.
async fn serve_connection(
    shared: Arc<Shared>,
    config: Arc<Config>,
    stream: TcpStream,
    peer: SocketAddr,
    permit: ConnPermit,
) {
    // A second handle on the socket, to cut it when the peer stalls somewhere the SSH library
    // does not look (it does not read its command queue during key exchange).
    let (stream, killer) = match split_killer(stream) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::debug!(error = %e, "ssh socket setup failed");
            return;
        }
    };
    let (authenticated_tx, authenticated_rx) = oneshot::channel();
    let conn = Conn::new(shared.clone(), Some(peer), permit, authenticated_tx);
    let deadline = tokio::time::Instant::now() + shared.limits.auth_timeout();
    let started =
        tokio::time::timeout_at(deadline, russh::server::run_stream(config, stream, conn));
    let mut session = match started.await {
        Ok(Ok(session)) => session,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, peer = %peer.ip(), "ssh connection setup failed");
            return;
        }
        Err(_) => {
            auth_timed_out(&shared, peer);
            let _ = killer.shutdown(Shutdown::Both);
            return;
        }
    };
    let handle = session.handle();
    let mut watchdog = Box::pin(tokio::time::timeout_at(deadline, authenticated_rx));
    tokio::select! {
        ended = &mut session => log_end(peer, ended),
        logged_in = &mut watchdog => {
            if logged_in.is_ok() {
                // Authenticated: the connection lives on until the client or the keep-alives end
                // it. (A dropped sender means the handler is gone: the connection already ended.)
                log_end(peer, (&mut session).await);
            } else {
                auth_timed_out(&shared, peer);
                let _ = handle
                    .disconnect(Disconnect::ByApplication, "authentication timeout".into(), String::new())
                    .await;
                let _ = killer.shutdown(Shutdown::Both);
                let _ = tokio::time::timeout(CLOSE_GRACE, &mut session).await;
            }
        }
    }
}

fn log_end(peer: SocketAddr, ended: Result<(), russh::Error>) {
    if let Err(e) = ended {
        tracing::debug!(error = %e, peer = %peer.ip(), "ssh connection ended");
    }
}

/// `auth_timeout` ran out before a login: record it and charge the peer.
fn auth_timed_out(shared: &Shared, peer: SocketAddr) {
    shared.audit.login(LoginRecord::new(
        Some(peer.ip()),
        "",
        LoginOutcome::Limit("auth_timeout"),
    ));
    shared.limits.failed_login(peer.ip());
}

/// The stream, and a second handle on its socket that can shut it down from outside.
fn split_killer(stream: TcpStream) -> std::io::Result<(TcpStream, std::net::TcpStream)> {
    let std = stream.into_std()?;
    let killer = std.try_clone()?;
    Ok((TcpStream::from_std(std)?, killer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disconnect_packet_is_well_formed() {
        for text in ["too many connections", "", "x", "abcdefg"] {
            let p = disconnect_packet(TOO_MANY_CONNECTIONS, text);
            assert_eq!((p.len()) % 8, 0, "{text:?}");
            let len = u32::from_be_bytes(p[0..4].try_into().unwrap()) as usize;
            assert_eq!(len + 4, p.len());
            let padding = p[4] as usize;
            assert!(padding >= 4);
            assert_eq!(p[5], 1, "SSH_MSG_DISCONNECT");
            assert_eq!(u32::from_be_bytes(p[6..10].try_into().unwrap()), 12);
            let dlen = u32::from_be_bytes(p[10..14].try_into().unwrap()) as usize;
            assert_eq!(&p[14..14 + dlen], text.as_bytes());
        }
    }

    #[test]
    fn the_offered_algorithms_are_the_spec_set() {
        let limits = SshLimitsConfig::default();
        let dir = tempfile::tempdir().unwrap();
        let keys = HostKeys::load_or_create(&[dir.path().join("k")]).unwrap();
        let c = server_config(&limits, &keys);
        let kex: Vec<_> = c
            .preferred
            .kex
            .iter()
            .map(|k| k.as_ref().to_owned())
            .collect();
        for want in [
            "curve25519-sha256",
            "curve25519-sha256@libssh.org",
            "kex-strict-s-v00@openssh.com",
        ] {
            assert!(kex.iter().any(|k| k == want), "{want} in {kex:?}");
        }
        assert!(
            kex.iter()
                .all(|k| !k.contains("sha1") && !k.contains("group1-")),
            "{kex:?}"
        );
        let ciphers: Vec<_> = c
            .preferred
            .cipher
            .iter()
            .map(|k| k.as_ref().to_owned())
            .collect();
        assert_eq!(
            ciphers,
            [
                "chacha20-poly1305@openssh.com",
                "aes256-gcm@openssh.com",
                "aes128-gcm@openssh.com"
            ]
        );
        assert_eq!(c.preferred.compression.len(), 1);
        assert_eq!(c.preferred.compression[0].as_ref(), "none");
        // Doubles as `server-sig-algs`: the SHA-2 RSA algorithms, and never `ssh-rsa`.
        let sigalgs: Vec<_> = c.preferred.key.iter().map(|a| a.to_string()).collect();
        assert!(sigalgs.contains(&"ssh-ed25519".to_owned()));
        assert!(sigalgs.contains(&"rsa-sha2-512".to_owned()));
        assert!(sigalgs.contains(&"rsa-sha2-256".to_owned()));
        assert!(!sigalgs.contains(&"ssh-rsa".to_owned()), "{sigalgs:?}");
        assert_eq!(c.methods.len(), 1);
        assert_eq!(c.max_auth_attempts, 6);
        assert_eq!(c.window_size, 2 * 1024 * 1024);
        assert_eq!(c.maximum_packet_size, 32 * 1024);
        assert_eq!(c.keepalive_interval, Some(Duration::from_secs(30)));
        assert_eq!(c.keepalive_max, 3);
    }
}
