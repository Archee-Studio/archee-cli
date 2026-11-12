//! The wait loop of a device sign-in: what each poll answer means and when to give up.

use std::time::Duration;

use crate::api::ApiError;
use crate::models::LoginPoll;
use crate::output::{CliError, EXIT_AUTH};

/// One answer to "has the person approved this code yet?".
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Poll {
    Pending,
    Approved(String),
    /// The server refuses this sign-in for good (the account is gone or access is withheld).
    Denied(String),
    /// The code ran out, or was already used; a new one is needed.
    Expired,
    /// A network or server hiccup: worth another try, but not forever.
    Transient(String),
}

impl Poll {
    /// Map a reply to a state. Only the server's own words count: a 2xx without a token for an
    /// approved code is treated as a hiccup, not as success.
    pub(crate) fn from_reply(reply: Result<LoginPoll, ApiError>) -> Self {
        match reply {
            Ok(LoginPoll { status, token }) => match (status.as_str(), token) {
                ("pending", _) => Poll::Pending,
                ("approved", Some(token)) if !token.is_empty() => Poll::Approved(token),
                (other, _) => Poll::Transient(format!("unexpected sign-in status `{other}`")),
            },
            Err(ApiError::Status { status: 404, .. }) => Poll::Expired,
            Err(ApiError::Status {
                status: 401 | 403,
                detail,
            }) => Poll::Denied(detail),
            // Rate limits and server errors clear up; any other 4xx will not.
            Err(ApiError::Status { status, detail }) if status == 429 || status >= 500 => {
                Poll::Transient(format!("the server answered {status}: {detail}"))
            }
            Err(ApiError::Status { status, detail }) => {
                Poll::Denied(format!("the server answered {status}: {detail}"))
            }
            Err(ApiError::Network(detail) | ApiError::Decode(detail)) => Poll::Transient(detail),
        }
    }
}

/// Where poll answers come from; a fake stands in for the server in tests.
pub(crate) trait PollSource {
    async fn poll(&self) -> Poll;
}

/// Pacing of the loop. Tests shrink it; sign-in uses [`Pacing::new`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pacing {
    pub(crate) interval: Duration,
    pub(crate) backoff_base: Duration,
    pub(crate) backoff_max: Duration,
    /// Failures in a row before the sign-in stops; a pending answer resets the count.
    pub(crate) max_transient: u32,
}

impl Pacing {
    pub(crate) fn new(interval_s: u64) -> Self {
        Self {
            interval: Duration::from_secs(interval_s.max(1)),
            backoff_base: Duration::from_secs(2),
            backoff_max: Duration::from_secs(30),
            max_transient: 5,
        }
    }

    fn backoff(&self, failures: u32) -> Duration {
        let factor = 2u32.saturating_pow(failures.saturating_sub(1));
        self.backoff_base
            .saturating_mul(factor)
            .min(self.backoff_max)
    }
}

/// Poll until approved, refused, expired or too many hiccups in a row.
pub(crate) async fn wait_for_approval(
    source: &impl PollSource,
    pacing: Pacing,
) -> Result<String, CliError> {
    let mut failures = 0;
    let mut delay = pacing.interval;
    loop {
        tokio::time::sleep(delay).await;
        match source.poll().await {
            Poll::Approved(token) => return Ok(token),
            Poll::Pending => {
                failures = 0;
                delay = pacing.interval;
            }
            Poll::Transient(reason) => {
                failures += 1;
                if failures >= pacing.max_transient {
                    return Err(CliError::new(
                        "network",
                        format!("sign-in stopped after {failures} failed checks: {reason}"),
                    )
                    .next("check the connection and ARCHEE_URL, then run `archee login`"));
                }
                delay = pacing.backoff(failures).max(pacing.interval);
            }
            Poll::Denied(reason) => {
                return Err(
                    CliError::new("unauthorized", format!("sign-in refused: {reason}"))
                        .next("sign in on the website, then run `archee login` again")
                        .exit(EXIT_AUTH),
                );
            }
            Poll::Expired => {
                return Err(CliError::new(
                    "expired",
                    "the sign-in code expired or was already used",
                )
                .next("run `archee login` for a new code"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Script(Mutex<Vec<Poll>>);

    impl PollSource for Script {
        async fn poll(&self) -> Poll {
            let mut answers = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if answers.is_empty() {
                return Poll::Pending;
            }
            answers.remove(0)
        }
    }

    fn fast() -> Pacing {
        Pacing {
            interval: Duration::from_millis(1),
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(4),
            max_transient: 3,
        }
    }

    async fn run(answers: Vec<Poll>) -> Result<String, CliError> {
        wait_for_approval(&Script(Mutex::new(answers)), fast()).await
    }

    fn status(status: u16, detail: &str) -> Result<LoginPoll, ApiError> {
        Err(ApiError::Status {
            status,
            detail: detail.to_owned(),
        })
    }

    #[tokio::test]
    async fn pending_then_approved_returns_the_token() {
        let token = run(vec![
            Poll::Pending,
            Poll::Pending,
            Poll::Approved("t".into()),
        ])
        .await;
        assert_eq!(token.ok().as_deref(), Some("t"));
    }

    #[tokio::test]
    async fn a_hiccup_between_pending_answers_is_retried() {
        let token = run(vec![
            Poll::Transient("reset".into()),
            Poll::Transient("reset".into()),
            Poll::Pending,
            Poll::Transient("reset".into()),
            Poll::Transient("reset".into()),
            Poll::Approved("t".into()),
        ])
        .await;
        assert_eq!(token.ok().as_deref(), Some("t"));
    }

    #[tokio::test]
    async fn too_many_hiccups_in_a_row_stop_with_a_next_step() {
        let error = run((0..3).map(|_| Poll::Transient("down".into())).collect())
            .await
            .unwrap_err();
        assert_eq!(error.code, "network");
        assert!(error.message.contains("down"));
        assert!(error.next.is_some());
    }

    #[tokio::test]
    async fn denied_and_expired_stop_at_once() {
        let denied = run(vec![
            Poll::Denied("gone".into()),
            Poll::Approved("t".into()),
        ])
        .await
        .unwrap_err();
        assert_eq!(denied.exit, EXIT_AUTH);
        let expired = run(vec![Poll::Expired, Poll::Approved("t".into())])
            .await
            .unwrap_err();
        assert_eq!(expired.code, "expired");
    }

    #[test]
    fn replies_map_to_states() {
        let reply = |s: &str, t: Option<&str>| {
            Ok(LoginPoll {
                status: s.into(),
                token: t.map(Into::into),
            })
        };
        assert_eq!(Poll::from_reply(reply("pending", None)), Poll::Pending);
        assert_eq!(
            Poll::from_reply(reply("approved", Some("t"))),
            Poll::Approved("t".into())
        );
        assert!(matches!(
            Poll::from_reply(reply("approved", None)),
            Poll::Transient(_)
        ));
        assert_eq!(Poll::from_reply(status(404, "x")), Poll::Expired);
        assert!(matches!(
            Poll::from_reply(status(401, "x")),
            Poll::Denied(_)
        ));
        assert!(matches!(
            Poll::from_reply(status(422, "x")),
            Poll::Denied(_)
        ));
        assert!(matches!(
            Poll::from_reply(status(503, "x")),
            Poll::Transient(_)
        ));
        assert!(matches!(
            Poll::from_reply(status(429, "x")),
            Poll::Transient(_)
        ));
        let network = Poll::from_reply(Err(ApiError::Network("reset".into())));
        assert!(matches!(network, Poll::Transient(_)));
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let pacing = Pacing::new(5);
        let secs: Vec<u64> = (1..=6).map(|n| pacing.backoff(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 30, 30]);
    }
}
