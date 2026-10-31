//! The dial-out worker runtime: register, heartbeat, claim, feed, execute, post receipts.
//!
//! Outbound only: the worker polls the backend; the backend never connects in.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use crate::api::{ApiError, Client};
use crate::journal::{Journal, JournalReceipt};
use crate::models::{ClaimedAssignment, OperationEnvelope, OperationResult};
use crate::operations::execute;
use crate::output::{CliError, EXIT_AUTH, EXIT_USAGE};
use crate::rfc3339::epoch_seconds;
use crate::state::{
    StoredCredential, ensure_outside, instance_id, load_credential, save_credential, state_root,
    workdir_id,
};

const POLL: Duration = Duration::from_secs(2);
const IDLE_POLL: Duration = Duration::from_secs(5);
const BACKOFF_CEILING: Duration = Duration::from_secs(30);
const HEARTBEAT: Duration = Duration::from_secs(15);
const IDLE_RELEASE: Duration = Duration::from_secs(30);
const FEED_WAIT_SECONDS: u32 = 10;

fn expired(lease: &ClaimedAssignment) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    // An unreadable expiry counts as expired: never act under a lease we cannot judge.
    epoch_seconds(&lease.expires_at).is_none_or(|at| at <= now)
}

/// What the heartbeat task and the serving loop share.
#[derive(Debug)]
struct Shared {
    client: Client,
    draining: watch::Sender<bool>,
    lease_lost: watch::Sender<bool>,
    lease: Mutex<Option<ClaimedAssignment>>,
    credential_lost: std::sync::atomic::AtomicBool,
}

impl Shared {
    fn set_lease(&self, lease: Option<ClaimedAssignment>) {
        *self.lease.lock().unwrap_or_else(PoisonError::into_inner) = lease;
    }

    fn current_lease(&self) -> Option<ClaimedAssignment> {
        self.lease
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn lose_lease(&self, reason: &str) {
        eprintln!("{reason}: stopping active work");
        self.set_lease(None);
        self.lease_lost.send_replace(true);
    }

    async fn renew(&self) {
        let Some(lease) = self.current_lease() else {
            return;
        };
        if expired(&lease) {
            self.lose_lease("lease expired");
            return;
        }
        match self.client.renew(&lease).await {
            Ok(()) => {}
            Err(ApiError::Status {
                status: 401 | 403 | 404,
                detail,
            }) => self.lose_lease(&format!("lease lost ({detail})")),
            Err(_) if expired(&lease) => self.lose_lease("lease expired"),
            Err(error) => eprintln!("lease renewal failed, retrying: {}", CliError::from(error)),
        }
    }

    async fn heartbeat_loop(self: Arc<Self>) {
        let mut draining = self.draining.subscribe();
        loop {
            match self.client.heartbeat().await {
                Ok(()) => {}
                Err(ApiError::Status { status: 401, .. }) => {
                    eprintln!("worker credential rejected; shutting down");
                    self.credential_lost
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    self.draining.send_replace(true);
                    self.lose_lease("credential rejected");
                }
                // Transient: the next beat retries; the lease expires server-side if it persists.
                Err(error) => eprintln!("heartbeat failed, retrying: {}", CliError::from(error)),
            }
            self.renew().await;
            if tokio::time::timeout(HEARTBEAT, draining.wait_for(|set| *set))
                .await
                .is_ok()
            {
                return;
            }
        }
    }
}

struct Runtime {
    shared: Arc<Shared>,
    folder: PathBuf,
    journals: PathBuf,
    // A mutation whose start survived but whose result did not is never re-run.
    refused: HashSet<String>,
}

impl Runtime {
    fn draining(&self) -> bool {
        *self.shared.draining.borrow()
    }

    async fn nap(&self, length: Duration) {
        let mut draining = self.shared.draining.subscribe();
        // Elapsing is the normal outcome; either way the caller re-checks draining.
        let _waited = tokio::time::timeout(length, draining.wait_for(|set| *set)).await;
    }

    async fn run(&mut self, workdir: &str) -> Result<(), CliError> {
        self.shared.client.register(workdir).await?;
        let beat = tokio::spawn(Arc::clone(&self.shared).heartbeat_loop());
        eprintln!(
            "serving {} (outbound only; Ctrl-C drains)",
            self.folder.display()
        );
        let outcome = self.serve_until_drained().await;
        beat.abort();
        outcome?;
        if self
            .shared
            .credential_lost
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(CliError::new(
                "unauthorized",
                "worker credential was rejected; register again",
            )
            .exit(EXIT_AUTH));
        }
        Ok(())
    }

    async fn serve_until_drained(&mut self) -> Result<(), CliError> {
        let mut backoff = Duration::from_secs(1);
        while !self.draining() {
            let served = match self.serve_available().await {
                Ok(served) => {
                    backoff = Duration::from_secs(1);
                    served
                }
                Err(Failure::Local(error)) => return Err(error),
                // A lost network or a 5xx is waited out, not fatal.
                Err(Failure::Api(ApiError::Network(detail))) => {
                    eprintln!("connection lost ({detail}), retrying");
                    self.nap(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_CEILING);
                    true
                }
                Err(Failure::Api(ApiError::Status {
                    status: 500 | 502 | 503 | 504,
                    ..
                })) => {
                    self.nap(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_CEILING);
                    true
                }
                Err(Failure::Api(ApiError::Status {
                    status: 403 | 404,
                    detail,
                })) => {
                    eprintln!("lease moved on: {detail}");
                    false
                }
                Err(Failure::Api(error)) => return Err(error.into()),
            };
            if !served {
                self.nap(IDLE_POLL).await;
            }
        }
        Ok(())
    }

    async fn serve_available(&mut self) -> Result<bool, Failure> {
        let pending = self.shared.client.assignments().await?;
        let mut served = false;
        for listing in pending {
            if self.draining() {
                break;
            }
            let lease = self.shared.client.claim(&listing.assignment_id).await?;
            eprintln!(
                "claimed assignment {} (fence {})",
                lease.assignment_id, lease.fence_token
            );
            self.shared.lease_lost.send_replace(false);
            self.shared.set_lease(Some(lease.clone()));
            let outcome = self.serve_lease(&lease).await;
            self.shared.set_lease(None);
            outcome?;
            served = true;
        }
        Ok(served)
    }

    async fn serve_lease(&mut self, lease: &ClaimedAssignment) -> Result<(), Failure> {
        let journal = Journal::open(&self.journals, &lease.assignment_id)?;
        for record in journal.unacknowledged_results()? {
            self.repost(&journal, lease, &record).await?;
        }
        for operation_id in journal.interrupted()? {
            eprintln!(
                "operation {operation_id} may already have executed; refusing to re-run it (needs an UNKNOWN resolution on the server)"
            );
            self.refused.insert(operation_id);
        }
        let mut after = 0;
        let mut idle = Duration::ZERO;
        while !self.draining() {
            if *self.shared.lease_lost.borrow() {
                eprintln!(
                    "assignment {} lease lost; returning it",
                    lease.assignment_id
                );
                return Ok(());
            }
            let batch = self
                .shared
                .client
                .feed(lease, after, FEED_WAIT_SECONDS)
                .await?;
            if let Some(last) = batch.last() {
                idle = Duration::ZERO;
                after = last.seq;
                for envelope in &batch {
                    self.execute(lease, &journal, envelope).await?;
                }
                continue;
            }
            self.nap(POLL.min(IDLE_RELEASE.saturating_sub(idle))).await;
            idle += POLL;
            if idle >= IDLE_RELEASE && !self.draining() {
                self.shared.client.release(lease).await?;
                eprintln!(
                    "released assignment {} after {} s idle",
                    lease.assignment_id,
                    idle.as_secs()
                );
                return Ok(());
            }
        }
        if let Err(error) = self.shared.client.release(lease).await {
            eprintln!(
                "could not release the lease while draining: {}",
                CliError::from(error)
            );
        }
        Ok(())
    }

    async fn execute(
        &mut self,
        lease: &ClaimedAssignment,
        journal: &Journal,
        envelope: &OperationEnvelope,
    ) -> Result<(), Failure> {
        let id = envelope.operation_id.as_str();
        if self.refused.contains(id) {
            return Ok(());
        }
        journal.received(id, envelope.seq)?;
        self.shared.client.start(lease, id).await?;
        journal.started(id)?;
        eprintln!("op {id} seq={}", envelope.seq);
        let cancel = self.shared.lease_lost.subscribe();
        let outcome = execute(&self.folder, &envelope.payload, cancel).await;
        // While draining, a second Ctrl-C killed the command: report that, so the turn is not left
        // waiting. If the lease really moved on, the server refuses this receipt.
        if *self.shared.lease_lost.borrow() && !self.draining() {
            // The lease moved on mid-run: the subprocess is dead and the receipt stays UNKNOWN.
            eprintln!("op {id} aborted: assignment lease lost");
            return Ok(());
        }
        let (status, result) = match outcome {
            Ok(result) => ("succeeded", result),
            Err(failure) => {
                eprintln!("op {id} failed: {}", failure.0);
                ("failed", OperationResult::Failure { error: failure.0 })
            }
        };
        journal.terminal(id, status, &result)?;
        self.shared
            .client
            .result(lease, id, status, &result)
            .await?;
        journal.acknowledged(id)?;
        Ok(())
    }

    /// A stored result the server never confirmed is sent again, under the current fence.
    async fn repost(
        &mut self,
        journal: &Journal,
        lease: &ClaimedAssignment,
        record: &JournalReceipt,
    ) -> Result<(), Failure> {
        let id = record.operation_id.as_str();
        match self
            .shared
            .client
            .result(lease, id, &record.receipt_status, &record.result)
            .await
        {
            Ok(ack) => {
                eprintln!("op {id} receipt {} acknowledged", ack.receipt_status);
                journal.acknowledged(id)?;
            }
            Err(ApiError::Status { detail, .. }) => {
                eprintln!("reposting {id} refused: {detail}");
                journal.acknowledged(id)?; // the server moved on; never re-run
                self.refused.insert(id.to_owned());
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

#[derive(Debug)]
enum Failure {
    Api(ApiError),
    Local(CliError),
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl From<CliError> for Failure {
    fn from(error: CliError) -> Self {
        Self::Local(error)
    }
}

/// SIGTERM or SIGINT: finish the running operation, then drain. A second one kills it.
fn drain_on_signals(
    draining: watch::Sender<bool>,
    lease_lost: watch::Sender<bool>,
) -> Result<(), CliError> {
    let fail = |e: std::io::Error| CliError::io("could not watch for signals", &e);
    let mut terminate = signal(SignalKind::terminate()).map_err(fail)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(fail)?;
    tokio::spawn(async move {
        for presses in 0.. {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
            if presses == 0 {
                eprintln!("disconnecting after the running command; press Ctrl-C again to stop it");
                draining.send_replace(true);
            } else if presses == 1 {
                lease_lost.send_replace(true);
            } else {
                std::process::exit(130);
            }
        }
    });
    Ok(())
}

/// The folder to serve, checked: it exists and the worker's own state is not inside it.
pub(crate) fn served_folder(workdir: &str) -> Result<PathBuf, CliError> {
    let folder = Path::new(workdir)
        .canonicalize()
        .ok()
        .filter(|path| path.is_dir())
        .ok_or_else(|| {
            CliError::new("usage", format!("{workdir} is not a directory")).exit(EXIT_USAGE)
        })?;
    ensure_outside(&folder, &state_root()?)?;
    Ok(folder)
}

/// Trade a one-use reference for the worker credential and keep it outside the folder.
pub(crate) async fn enroll(
    url: &str,
    outpost: &str,
    reference: &str,
    folder: &Path,
) -> Result<StoredCredential, CliError> {
    let exchange = Client::new(url, None)?
        .exchange(reference, &instance_id(outpost)?, &workdir_id(folder)?)
        .await?;
    let credential = StoredCredential {
        base_url: url.to_owned(),
        worker_token: exchange.worker_token,
        outpost_id: exchange.outpost_id,
        folder: Some(folder.to_string_lossy().into_owned()),
    };
    save_credential(&exchange.outpost_name, &credential)?;
    eprintln!("registered outpost {}", exchange.outpost_name);
    Ok(credential)
}

pub(crate) async fn start(outpost: &str, workdir: &str) -> Result<(), CliError> {
    let folder = served_folder(workdir)?;
    let state = state_root()?;
    let identity = workdir_id(&folder)?;
    let stored = load_credential(outpost)?;
    stored.ensure_serves(outpost, &folder)?;
    let client = Client::new(&stored.base_url, Some(&stored.worker_token))?;
    let (draining, _) = watch::channel(false);
    let (lease_lost, _) = watch::channel(false);
    drain_on_signals(draining.clone(), lease_lost.clone())?;
    let shared = Arc::new(Shared {
        client,
        draining,
        lease_lost,
        lease: Mutex::new(None),
        credential_lost: std::sync::atomic::AtomicBool::new(false),
    });
    let mut runtime = Runtime {
        shared,
        folder,
        journals: state.join("journal"),
        refused: HashSet::new(),
    };
    runtime.run(&identity).await
}
