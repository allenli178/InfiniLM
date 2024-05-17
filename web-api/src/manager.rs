use crate::schemas::{Drop, DropSuccess, Error, Fork, ForkSuccess, Infer, Sentence};
use causal_lm::CausalLM;
use lru::LruCache;
use service::{Service, Session};
use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::{self, UnboundedReceiver};

pub(crate) struct ServiceManager<M: CausalLM> {
    service: Service<M>,
    pending: Mutex<LruCache<String, Option<Session<M>>>>,
}

impl<M: CausalLM> ServiceManager<M> {
    #[inline]
    pub fn new(service: Service<M>, capacity: Option<usize>) -> Self {
        let cap =
            capacity.map(|c| NonZeroUsize::new(c).expect("Session capacity must be non-zero"));
        Self {
            service,
            pending: Mutex::new(cap.map(LruCache::new).unwrap_or_else(LruCache::unbounded)),
        }
    }
}

impl<M> ServiceManager<M>
where
    M: CausalLM + Send + Sync + 'static,
    M::Storage: Send,
{
    pub fn infer(
        self: &Arc<Self>,
        Infer {
            inputs: messages,
            session_id,
            dialog_pos,
            temperature,
            top_k,
            top_p,
        }: Infer,
    ) -> Result<UnboundedReceiver<String>, Error> {
        async fn infer<M: CausalLM>(
            session_id: &str,
            session: &mut Session<M>,
            messages: Vec<Sentence>,
            temperature: Option<f32>,
            top_k: Option<usize>,
            top_p: Option<f32>,
            sender: mpsc::UnboundedSender<String>,
        ) {
            if let Some(temperature) = temperature {
                session.sample.temperature = temperature;
            }
            if let Some(top_k) = top_k {
                session.sample.top_k = top_k;
            }
            if let Some(top_p) = top_p {
                session.sample.top_p = top_p;
            }

            session.extend(messages.iter().map(|s| s.content.as_str()));
            if session.dialog_pos() % 2 == 1 {
                info!("{session_id} inference started");
                let mut busy = session.chat();
                while let Some(s) = busy.decode().await {
                    if let Err(e) = sender.send(s.into_owned()) {
                        warn!("Failed to send piece to {session_id} with error \"{e}\"");
                        break;
                    }
                }
                info!("{session_id} inference stopped");
            } else {
                info!("{session_id} inference skipped");
            }
        }

        match (session_id, dialog_pos.unwrap_or(0)) {
            (Some(session_id), 0) => {
                let mut session = self
                    .pending
                    .lock()
                    .unwrap()
                    .get_or_insert_mut(session_id.clone(), || {
                        info!("{session_id} created");
                        Some(self.service.launch())
                    })
                    .take()
                    .ok_or(Error::SessionBusy)?;

                let (sender, receiver) = mpsc::unbounded_channel();
                let self_ = self.clone();
                tokio::spawn(async move {
                    session.revert(0).unwrap();

                    infer(
                        &session_id,
                        &mut session,
                        messages,
                        temperature,
                        top_k,
                        top_p,
                        sender,
                    )
                    .await;

                    self_.restore(session_id, session);
                });

                Ok(receiver)
            }
            (Some(session_id), p) => {
                let mut session = self
                    .pending
                    .lock()
                    .unwrap()
                    .get_mut(&session_id)
                    .ok_or(Error::SessionNotFound)?
                    .take()
                    .ok_or(Error::SessionBusy)?;

                if session.revert(p).is_err() {
                    let current = session.dialog_pos();
                    warn!("Failed to revert {session_id} from {current} to {p}, session restored");
                    self.restore(session_id, session);
                    return Err(Error::InvalidDialogPos(current));
                }

                let (sender, receiver) = mpsc::unbounded_channel();
                let self_ = self.clone();
                tokio::spawn(async move {
                    info!("{session_id} reverted to {p}");

                    infer(
                        &session_id,
                        &mut session,
                        messages,
                        temperature,
                        top_k,
                        top_p,
                        sender,
                    )
                    .await;

                    self_.restore(session_id, session);
                });

                Ok(receiver)
            }
            (None, 0) => {
                let (sender, receiver) = mpsc::unbounded_channel();
                if messages.len() % 2 == 1 {
                    let self_ = self.clone();
                    tokio::spawn(async move {
                        infer(
                            "Temporary session",
                            &mut self_.service.launch(),
                            messages,
                            temperature,
                            top_k,
                            top_p,
                            sender,
                        )
                        .await;
                    });
                }
                Ok(receiver)
            }
            (None, _) => {
                warn!("Temporary session must be created with zero dialog position");
                Err(Error::InvalidDialogPos(0))
            }
        }
    }

    #[inline]
    fn restore(&self, session_id: String, session: Session<M>) {
        if let Some(option) = self.pending.lock().unwrap().get_mut(&session_id) {
            assert!(option.replace(session).is_none());
        }
    }

    pub fn fork(
        &self,
        Fork {
            session_id,
            new_session_id,
        }: Fork,
    ) -> Result<ForkSuccess, Error> {
        let mut sessions = self.pending.lock().unwrap();
        if !sessions.contains(&new_session_id) {
            let new = sessions
                .get_mut(&session_id)
                .ok_or(Error::SessionNotFound)?
                .as_ref()
                .ok_or(Error::SessionBusy)?
                .fork();

            info!("{new_session_id} is forked from {session_id}");
            if let Some((out, _)) = sessions.push(new_session_id, Some(new)) {
                warn!("{out} dropped because LRU cache is full");
            }
            Ok(ForkSuccess)
        } else {
            warn!("Fork failed because {new_session_id} already exists");
            Err(Error::SessionDuplicate)
        }
    }

    pub fn drop_(&self, Drop { session_id }: Drop) -> Result<DropSuccess, Error> {
        if self.pending.lock().unwrap().pop(&session_id).is_some() {
            info!("{session_id} dropped");
            Ok(DropSuccess)
        } else {
            Err(Error::SessionNotFound)
        }
    }
}
