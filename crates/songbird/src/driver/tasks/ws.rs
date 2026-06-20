use super::message::*;
use crate::{
    driver::tasks::error::DaveReinitError,
    events::CoreContext,
    model::{
        payload::Speaking,
        CloseCode as VoiceCloseCode,
        Event as GatewayEvent,
        FromPrimitive,
        SpeakingState,
    },
    ws::{Error as WsError, WsStream},
    ConnectionInfo,
};
use flume::Receiver;
use rand::{distr::Uniform, Rng};
use serenity_voice_model::{
    id::UserId,
    payload::{
        DaveMlsCommitWelcome,
        DaveMlsInvalidCommitWelcome,
        DaveMlsKeyPackage,
        DaveMlsProposalsOperationType,
        DaveTransitionReady,
    },
};
use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU16,
    sync::{
        atomic::{AtomicBool, AtomicU16, Ordering},
        Arc,
        RwLock,
    },
    time::Duration,
};
use tokio::{
    select,
    time::{sleep_until, Instant},
};
#[cfg(feature = "tungstenite")]
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tracing::{debug, info, instrument, trace, warn};

pub(crate) struct AuxNetwork {
    rx: Receiver<WsMessage>,
    ws_client: WsStream,
    dont_send: bool,

    ssrc: u32,
    heartbeat_interval: Duration,

    speaking: SpeakingState,
    last_heartbeat_nonce: Option<u64>,
    /// Last numbered gateway message (`seq_ack` for v8 heartbeats).
    last_seq: Option<i32>,

    attempt_idx: usize,
    info: ConnectionInfo,

    dave_session: Arc<RwLock<Option<davey::DaveSession>>>,
    dave_protocol_version: Arc<AtomicU16>,
    dave_media_allowed: Arc<AtomicBool>,
    dave_pending_transitions: HashMap<u16, u16>,
    recognized_user_ids: HashSet<UserId>,

    #[cfg(feature = "receive")]
    ssrc_signalling: Arc<SsrcTracker>,
}

impl AuxNetwork {
    pub(crate) fn new(
        evt_rx: Receiver<WsMessage>,
        ws_client: WsStream,
        ssrc: u32,
        heartbeat_interval: f64,
        attempt_idx: usize,
        info: ConnectionInfo,
        dave_session: Arc<RwLock<Option<davey::DaveSession>>>,
        dave_protocol_version: Arc<AtomicU16>,
        dave_media_allowed: Arc<AtomicBool>,
        #[cfg(feature = "receive")] ssrc_signalling: Arc<SsrcTracker>,
    ) -> Self {
        let mut recognized_user_ids = HashSet::new();

        recognized_user_ids.insert(info.user_id.into());

        let last_seq = ws_client.last_seq();

        Self {
            rx: evt_rx,
            ws_client,
            dont_send: false,

            ssrc,
            heartbeat_interval: Duration::from_secs_f64(heartbeat_interval / 1000.0),

            speaking: SpeakingState::empty(),
            last_heartbeat_nonce: None,
            last_seq,

            attempt_idx,
            info,

            dave_session,
            dave_protocol_version,
            dave_media_allowed,
            dave_pending_transitions: HashMap::new(),
            recognized_user_ids,

            #[cfg(feature = "receive")]
            ssrc_signalling,
        }
    }

    #[instrument(skip(self))]
    async fn run(&mut self, interconnect: &mut Interconnect) {
        let mut next_heartbeat = Instant::now() + self.heartbeat_interval;

        loop {
            let mut ws_error = false;
            let mut should_reconnect = false;
            let mut ws_reason = None;

            let hb = sleep_until(next_heartbeat);

            select! {
                biased;

                () = hb => {
                    ws_error = match self.send_heartbeat().await {
                        Err(e) => {
                            should_reconnect = ws_error_is_not_final(&e);
                            ws_reason = Some((&e).into());
                            true
                        },
                        _ => false,
                    };
                    next_heartbeat = self.next_heartbeat();
                }
                inner_msg = self.rx.recv_async() => {
                    match inner_msg {
                        Ok(WsMessage::Ws(data)) => {
                            self.ws_client = *data;
                            self.last_seq = self.ws_client.last_seq();
                            next_heartbeat = self.next_heartbeat();
                            self.dont_send = false;
                            self.reassert_speaking().await;
                        },
                        Ok(WsMessage::ReplaceInterconnect(i)) => {
                            *interconnect = i;
                        },
                        Ok(WsMessage::SetKeepalive(keepalive)) => {
                            self.heartbeat_interval = Duration::from_secs_f64(keepalive / 1000.0);
                            next_heartbeat = self.next_heartbeat();
                        },
                        Ok(WsMessage::Speaking(is_speaking)) => {
                            if self.speaking.contains(SpeakingState::MICROPHONE) != is_speaking && !self.dont_send {
                                self.speaking.set(SpeakingState::MICROPHONE, is_speaking);
                                if is_speaking
                                    && self.dave_protocol_version.load(Ordering::Relaxed) != 0
                                    && !self.dave_media_allowed.load(Ordering::Acquire)
                                {
                                    continue;
                                }
                                info!("Changing to {:?}", self.speaking);

                                let ssu_status = self.ws_client
                                    .send_json(&GatewayEvent::from(Speaking {
                                        delay: Some(0),
                                        speaking: self.speaking,
                                        ssrc: self.ssrc,
                                        user_id: None,
                                    }))
                                    .await;

                                ws_error |= match ssu_status {
                                    Err(e) => {
                                        should_reconnect = ws_error_is_not_final(&e);
                                        ws_reason = Some((&e).into());
                                        true
                                    },
                                    _ => false,
                                }
                            }
                        },
                        Ok(WsMessage::Deliver(msg)) => {
                            ws_error |= match self.process_ws(interconnect, msg).await {
                                Err(e) => {
                                    should_reconnect = ws_error_is_not_final(&e);
                                    ws_reason = Some((&e).into());
                                    true
                                }
                                _ => false,
                            }
                        },
                        Ok(WsMessage::UpdateInfo(info)) => {
                            debug!(
                                "WS: Connection info refreshed (endpoint={}, session_len={})",
                                info.endpoint,
                                info.session_id.len()
                            );
                            self.info = info;
                        },
                        Err(flume::RecvError::Disconnected) => {
                            break;
                        },
                    }
                }
                ws_msg = self.ws_client.recv_event_with_seq_no_timeout(), if !self.dont_send => {
                    ws_error = match ws_msg {
                        Err(e) => {
                            should_reconnect = ws_error_is_not_final(&e);
                            ws_reason = Some((&e).into());
                            true
                        },
                        Ok((Some(msg), seq)) => {
                            if let Some(s) = seq {
                                self.last_seq = Some(s as i32);
                            }
                            match self.process_ws(interconnect, msg).await {
                                Err(e) => {
                                    should_reconnect = ws_error_is_not_final(&e);
                                    ws_reason = Some((&e).into());
                                    true
                                },
                                _ => false
                            }
                        },
                        _ => false,
                    };
                }
            }

            if ws_error {
                self.dont_send = true;

                if should_reconnect {
                    info!("WS: attempting in-driver reconnect ({:?})", ws_reason);
                    drop(interconnect.core.send(CoreMessage::Reconnect));
                } else {
                    drop(interconnect.core.send(CoreMessage::SignalWsClosure(
                        self.attempt_idx,
                        self.info.clone(),
                        ws_reason,
                    )));
                    break;
                }
            }
        }
    }

    fn next_heartbeat(&self) -> Instant {
        Instant::now() + self.heartbeat_interval
    }

    async fn reassert_speaking(&mut self) {
        if !self.speaking.contains(SpeakingState::MICROPHONE) || self.dont_send {
            return;
        }

        if self.dave_protocol_version.load(Ordering::Relaxed) != 0
            && !self.dave_media_allowed.load(Ordering::Acquire)
        {
            return;
        }

        if let Err(e) = self
            .ws_client
            .send_json(&GatewayEvent::from(Speaking {
                delay: Some(0),
                speaking: self.speaking,
                ssrc: self.ssrc,
                user_id: None,
            }))
            .await
        {
            warn!("WS: Failed to re-assert speaking after resume: {:?}", e);
        }
    }

    async fn send_heartbeat(&mut self) -> Result<(), WsError> {
        // Discord have suddenly, mysteriously, started rejecting
        // ints-as-strings. Keep JS happy here, I suppose...
        const JS_MAX_INT: u64 = (1u64 << 53) - 1;
        let nonce_range =
            Uniform::new(0, JS_MAX_INT).expect("uniform range is finite and nonempty");
        let nonce = rand::rng().sample(nonce_range);
        self.last_heartbeat_nonce = Some(nonce);

        trace!(
            "Sent heartbeat speaking={:?} seq_ack={:?}",
            self.speaking,
            self.last_seq
        );

        if !self.dont_send {
            // Voice gateway v8 expects `{ "t": nonce, "seq_ack": last_seq }`, not a bare nonce.
            let seq_ack = self.last_seq.unwrap_or(-1);
            let payload = format!(r#"{{"op":3,"d":{{"t":{nonce},"seq_ack":{seq_ack}}}}}"#);
            self.ws_client.send_text(&payload).await?;
        }

        Ok(())
    }

    async fn process_ws(
        &mut self,
        interconnect: &Interconnect,
        value: GatewayEvent,
    ) -> Result<(), WsError> {
        match value {
            GatewayEvent::Speaking(ev) => {
                #[cfg(feature = "receive")]
                if let Some(user_id) = &ev.user_id {
                    self.ssrc_signalling.user_ssrc_map.insert(*user_id, ev.ssrc);
                    self.ssrc_signalling.ssrc_user_map.insert(ev.ssrc, *user_id);
                }

                drop(interconnect.events.send(EventMessage::FireCoreEvent(
                    CoreContext::SpeakingStateUpdate(ev),
                )));
            },
            GatewayEvent::ClientConnect(ev) => {
                #[cfg(feature = "receive")]
                {
                    self.ssrc_signalling
                        .user_ssrc_map
                        .insert(ev.user_id, ev.audio_ssrc);
                    self.ssrc_signalling
                        .ssrc_user_map
                        .insert(ev.audio_ssrc, ev.user_id);
                    self.recognized_user_ids.insert(ev.user_id);
                }

                drop(interconnect.events.send(EventMessage::FireCoreEvent(
                    CoreContext::ClientConnect(ev),
                )));
            },
            GatewayEvent::ClientDisconnect(ev) => {
                #[cfg(feature = "receive")]
                {
                    if let Some(ssrc) = self.ssrc_signalling.user_ssrc_map.get(&ev.user_id) {
                        self.ssrc_signalling.ssrc_user_map.remove(&*ssrc);
                    }
                    self.ssrc_signalling.user_ssrc_map.remove(&ev.user_id);
                    self.ssrc_signalling.disconnected_users.insert(ev.user_id);
                }

                self.recognized_user_ids.remove(&ev.user_id);

                drop(interconnect.events.send(EventMessage::FireCoreEvent(
                    CoreContext::ClientDisconnect(ev),
                )));
            },
            GatewayEvent::ClientsConnect(ev) => {
                self.recognized_user_ids.extend(&ev.user_ids);
            },
            GatewayEvent::HeartbeatAck(ev) => {
                if let Some(nonce) = self.last_heartbeat_nonce.take() {
                    if ev.nonce == nonce {
                        trace!("Heartbeat ACK received.");
                    } else {
                        warn!(
                            "Heartbeat nonce mismatch! Expected {}, saw {}.",
                            nonce, ev.nonce
                        );
                    }
                }
            },
            GatewayEvent::DavePrepareTransition(ev) => {
                info!(
                    "DAVE: Received DavePrepareTransition (transition_id={}, protocol_version={})",
                    ev.transition_id, ev.protocol_version
                );
                self.dave_pending_transitions
                    .insert(ev.transition_id, ev.protocol_version);

                if ev.transition_id == 0 {
                    self.execute_dave_transition(ev.transition_id).await;
                } else if ev.protocol_version == 0 {
                    if let Some(ref mut dave_session) = *self.dave_session.write().unwrap() {
                        dave_session.set_passthrough_mode(true, Some(120));
                    }

                    self.ws_client
                        .send_json(&GatewayEvent::from(DaveTransitionReady {
                            transition_id: ev.transition_id,
                            protocol_version: ev.protocol_version,
                        }))
                        .await?;
                }
            },
            GatewayEvent::DaveExecuteTransition(ev) => {
                info!(
                    "DAVE: Received DaveExecuteTransition (transition_id={})",
                    ev.transition_id
                );
                self.execute_dave_transition(ev.transition_id).await;
            },
            GatewayEvent::DavePrepareEpoch(ev) if ev.epoch == 1 => {
                info!("DAVE: Received DavePrepareEpoch (protocol_version={})", ev.protocol_version);
                self.dave_protocol_version
                    .store(ev.protocol_version, Ordering::Relaxed);
                match self.reinit_dave_session().await {
                    Err(DaveReinitError::Ws(e)) => return Err(e),
                    Err(e) => {
                        warn!(error = ?e, "failed to reinitialize DAVE session");
                    },
                    _ => {
                        let is_ready = if let Some(ref ds) = *self.dave_session.read().unwrap() {
                            ds.is_ready()
                        } else {
                            false
                        };
                        info!("DAVE: After reinit, is_ready={}", is_ready);
                    },
                }
            },
            GatewayEvent::DaveMlsExternalSender(ev) => {
                info!("DAVE: Received DaveMlsExternalSender ({} bytes)", ev.external_sender.len());
                if let Some(ref mut dave_session) = *self.dave_session.write().unwrap() {
                    if let Err(e) = dave_session.set_external_sender(&ev.external_sender) {
                        warn!(error = ?e, "error setting MLS external sender");
                    }
                }
            },
            GatewayEvent::DaveMlsProposals(ev) => {
                info!("DAVE: Received DaveMlsProposals (operation_type={:?})", ev.operation_type);
                let operation_type = match ev.operation_type {
                    DaveMlsProposalsOperationType::Append => davey::ProposalsOperationType::APPEND,
                    DaveMlsProposalsOperationType::Revoke => davey::ProposalsOperationType::REVOKE,
                };
                let result = if let Some(ref mut dave_session) = *self.dave_session.write().unwrap()
                {
                    match dave_session.process_proposals(
                        operation_type,
                        &ev.proposals,
                        Some(
                            &self
                                .recognized_user_ids
                                .iter()
                                .map(|u| u.0)
                                .collect::<Vec<_>>(),
                        ),
                    ) {
                        Ok(result) => result,
                        Err(e) => {
                            warn!(error = ?e, "error processing MLS proposals");
                            None
                        },
                    }
                } else {
                    None
                };

                if let Some(commit_welcome) = result {
                    info!("DAVE: Sending DaveMlsCommitWelcome");
                    self.ws_client
                        .send_binary(&GatewayEvent::from(DaveMlsCommitWelcome {
                            commit: commit_welcome.commit.clone(),
                            welcome: commit_welcome.welcome.clone(),
                        }))
                        .await?;
                    info!("DAVE: DaveMlsCommitWelcome sent successfully");

                    // As committer, merge our pending MLS commit locally. Discord may not
                    // send AnnounceCommitTransition promptly (or at all for transition_id 0).
                    if let Some(ref mut dave_session) = *self.dave_session.write().unwrap() {
                        match dave_session.process_commit(&commit_welcome.commit) {
                            Ok(()) => info!(
                                "DAVE: Local commit merged after CommitWelcome, is_ready={}",
                                dave_session.is_ready()
                            ),
                            Err(e) => warn!(
                                error = ?e,
                                "DAVE: local process_commit after CommitWelcome failed"
                            ),
                        }
                    }
                    // Wait for DaveExecuteTransition before sending RTP on the new epoch.
                    self.dave_media_allowed.store(false, Ordering::Release);
                } else {
                    info!("DAVE: process_proposals returned None (no commit/welcome to send)");
                }
                let is_ready = if let Some(ref ds) = *self.dave_session.read().unwrap() {
                    ds.is_ready()
                } else {
                    false
                };
                info!("DAVE: After proposals, is_ready={}", is_ready);
            },
            GatewayEvent::DaveMlsAnnounceCommitTransition(ev) => {
                let already_ready = self
                    .dave_session
                    .read()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|s| s.is_ready());

                if already_ready {
                    if ev.transition_id != 0 {
                        info!(
                            "DAVE: AnnounceCommitTransition (transition_id={}) after local merge — registering for execute",
                            ev.transition_id
                        );
                        let protocol_version =
                            self.dave_protocol_version.load(Ordering::Relaxed);
                        self.dave_pending_transitions
                            .insert(ev.transition_id, protocol_version);
                        self.ws_client
                            .send_json(&GatewayEvent::from(DaveTransitionReady {
                                transition_id: ev.transition_id,
                                protocol_version,
                            }))
                            .await?;
                    } else {
                        tracing::debug!(
                            "DAVE: AnnounceCommitTransition (transition_id=0) after local merge — already ready"
                        );
                    }
                } else {
                    info!(
                        "DAVE: Received DaveMlsAnnounceCommitTransition (transition_id={})",
                        ev.transition_id
                    );
                    match self.dave_process_commit(&ev.commit_message) {
                        Some(Ok(())) if ev.transition_id != 0 => {
                            let protocol_version =
                                self.dave_protocol_version.load(Ordering::Relaxed);

                            self.dave_pending_transitions
                                .insert(ev.transition_id, protocol_version);
                            self.ws_client
                                .send_json(&GatewayEvent::from(DaveTransitionReady {
                                    transition_id: ev.transition_id,
                                    protocol_version,
                                }))
                                .await?;
                        },
                        Some(Ok(())) => {
                            info!("DAVE: Commit processed for transition_id=0");
                            self.try_enable_dave_media().await;
                        },
                        Some(Err(e)) if is_stale_dave_commit_error(&e) => {
                            tracing::debug!(
                                "DAVE: Stale commit announcement ignored: {e:?}"
                            );
                            self.try_enable_dave_media().await;
                        },
                        Some(Err(e)) => {
                            warn!("MLS commit errored: {e:?}");
                            self.ws_client
                                .send_json(&GatewayEvent::from(DaveMlsInvalidCommitWelcome {
                                    transition_id: ev.transition_id,
                                }))
                                .await?;
                            match self.reinit_dave_session().await {
                                Err(DaveReinitError::Ws(e)) => return Err(e),
                                Err(e) => {
                                    warn!(error = ?e, "failed to reinitialize DAVE session");
                                },
                                _ => {},
                            }
                        },
                        None => {},
                    }
                }
            },
            GatewayEvent::DaveMlsWelcome(ev) => {
                info!("DAVE: Received DaveMlsWelcome (transition_id={})", ev.transition_id);
                match self.dave_process_welcome(&ev.welcome) {
                    Some(Ok(())) if ev.transition_id != 0 => {
                        let protocol_version = self.dave_protocol_version.load(Ordering::Relaxed);

                        self.dave_pending_transitions
                            .insert(ev.transition_id, protocol_version);
                        self.ws_client
                            .send_json(&GatewayEvent::from(DaveTransitionReady {
                                transition_id: ev.transition_id,
                                protocol_version,
                            }))
                            .await?;
                    },

                    Some(Err(e))
                        if matches!(
                            e,
                            davey::errors::ProcessWelcomeError::AlreadyInGroup
                        ) =>
                    {
                        tracing::debug!("DAVE: Welcome ignored — already in MLS group");
                    },
                    Some(Err(e)) => {
                        warn!("MLS welcome errored: {e:?}");
                        self.ws_client
                            .send_json(&GatewayEvent::from(DaveMlsInvalidCommitWelcome {
                                transition_id: ev.transition_id,
                            }))
                            .await?;
                        match self.reinit_dave_session().await {
                            Err(DaveReinitError::Ws(e)) => return Err(e),
                            Err(e) => {
                                warn!(error = ?e, "failed to reinitialize DAVE session");
                            },
                            _ => {},
                        }
                    },
                    Some(Ok(())) => {
                        info!("DAVE: Welcome processed for transition_id={}", ev.transition_id);
                        self.try_enable_dave_media().await;
                    },
                    None => {},
                }
                let is_ready = if let Some(ref ds) = *self.dave_session.read().unwrap() {
                    ds.is_ready()
                } else {
                    false
                };
                info!("DAVE: After welcome, is_ready={}", is_ready);
                if is_ready {
                    self.try_enable_dave_media().await;
                }
            },
            other => {
                trace!("Received other websocket data: {:?}", other);
            },
        }

        Ok(())
    }

    fn dave_process_commit(
        &mut self,
        commit_message: &[u8],
    ) -> Option<Result<(), davey::errors::ProcessCommitError>> {
        let mut dave_session_lock = self.dave_session.write().unwrap();
        let dave_session = (*dave_session_lock).as_mut()?;

        Some(dave_session.process_commit(commit_message))
    }

    fn dave_process_welcome(
        &mut self,
        welcome: &[u8],
    ) -> Option<Result<(), davey::errors::ProcessWelcomeError>> {
        let mut dave_session_lock = self.dave_session.write().unwrap();
        let dave_session = (*dave_session_lock).as_mut()?;

        Some(dave_session.process_welcome(welcome))
    }

    async fn reinit_dave_session(&mut self) -> Result<(), DaveReinitError> {
        self.dave_media_allowed.store(false, Ordering::Release);
        let protocol_version = self.dave_protocol_version.load(Ordering::Relaxed);

        if let Some(dave_protocol_version) = NonZeroU16::new(protocol_version) {
            let user_id = self.info.user_id.0.into();
            let channel_id = self.info.channel_id.0.into();

            let key_package =
                if let Some(ref mut dave_session) = *self.dave_session.write().unwrap() {
                    dave_session.reinit(dave_protocol_version, user_id, channel_id, None)?;
                    dave_session.create_key_package()?
                } else {
                    let mut dave_session =
                        davey::DaveSession::new(dave_protocol_version, user_id, channel_id, None)?;
                    let key_package = dave_session.create_key_package()?;

                    *self.dave_session.write().unwrap() = Some(dave_session);

                    key_package
                };

            self.ws_client
                .send_binary(&GatewayEvent::DaveMlsKeyPackage(DaveMlsKeyPackage {
                    key_package,
                }))
                .await?;
        } else if let Some(ref mut dave_session) = *self.dave_session.write().unwrap() {
            dave_session.reset()?;
            dave_session.set_passthrough_mode(true, Some(10));
        }

        Ok(())
    }

    /// Unblock RTP once the MLS session can encrypt and Discord allows media.
    async fn try_enable_dave_media(&mut self) {
        let is_ready = self
            .dave_session
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.is_ready());
        if !is_ready {
            return;
        }
        if self.dave_media_allowed.load(Ordering::Acquire) {
            return;
        }
        let protocol_version = self.dave_protocol_version.load(Ordering::Relaxed);
        self.dave_pending_transitions.insert(0, protocol_version);
        self.execute_dave_transition(0).await;
        info!("DAVE: Media enabled (session is_ready=true)");
    }

    async fn reassert_speaking_after_dave(&mut self) {
        if self.speaking.contains(SpeakingState::MICROPHONE) && !self.dont_send {
            if let Err(e) = self
                .ws_client
                .send_json(&GatewayEvent::from(Speaking {
                    delay: Some(0),
                    speaking: self.speaking,
                    ssrc: self.ssrc,
                    user_id: None,
                }))
                .await
            {
                warn!("WS: Failed to re-assert speaking after DAVE transition: {:?}", e);
            }
        }
    }

    async fn execute_dave_transition(&mut self, transition_id: u16) {
        let Some(new_version) = self.dave_pending_transitions.get(&transition_id).copied() else {
            warn!("Received DaveExecuteTransition for unknown transition ID {transition_id}");
            let is_ready = self
                .dave_session
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.is_ready());
            if is_ready {
                if !self.dave_media_allowed.load(Ordering::Acquire) {
                    self.dave_media_allowed.store(true, Ordering::Release);
                    info!(
                        "DAVE: RTP media allowed (fallback, unknown transition_id={transition_id})"
                    );
                }
                self.reassert_speaking_after_dave().await;
            }
            return;
        };
        let old_version = self.dave_protocol_version.load(Ordering::Relaxed);

        self.dave_protocol_version
            .store(new_version, Ordering::Relaxed);

        // Upgraded from transport-only encryption
        if transition_id > 0 && old_version == 0 && new_version != 0 {
            if let Some(ref mut dave_session) = *self.dave_session.write().unwrap() {
                dave_session.set_passthrough_mode(true, Some(10));
            }
        }

        self.dave_pending_transitions.remove(&transition_id);
        self.dave_media_allowed.store(true, Ordering::Release);
        info!("DAVE: RTP media allowed (transition_id={transition_id})");

        // Re-assert speaking now that DAVE allows media — the earlier speaking
        // packet was deferred because dave_media_allowed was false.
        self.reassert_speaking_after_dave().await;
    }
}

#[instrument(skip(interconnect, aux))]
pub(crate) async fn runner(mut interconnect: Interconnect, mut aux: AuxNetwork) {
    trace!("WS thread started.");
    aux.run(&mut interconnect).await;
    trace!("WS thread finished.");
}

fn is_stale_dave_commit_error(err: &davey::errors::ProcessCommitError) -> bool {
    format!("{err:?}").contains("WrongEpoch")
}

fn ws_error_is_not_final(err: &WsError) -> bool {
    match err {
        #[cfg(feature = "tungstenite")]
        WsError::WsClosed(Some(frame)) => match frame.code {
            CloseCode::Library(l) =>
                if let Some(code) = VoiceCloseCode::from_u16(l) {
                    // SessionInvalid is expected during DAVE; resume instead of dying.
                    if code == VoiceCloseCode::SessionInvalid {
                        return true;
                    }
                    code.should_resume()
                } else {
                    true
                },
            _ => true,
        },
        #[cfg(feature = "tws")]
        WsError::WsClosed(Some(code)) => match (*code).into() {
            code @ 4000..=4999_u16 =>
                if let Some(voice_code) = VoiceCloseCode::from_u16(code) {
                    if voice_code == VoiceCloseCode::SessionInvalid {
                        return true;
                    }
                    voice_code.should_resume()
                } else {
                    true
                },
            _ => true,
        },
        e => {
            debug!("Error sending/receiving ws {:?}.", e);
            true
        },
    }
}
