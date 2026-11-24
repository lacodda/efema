//! In whose name a request to a bucket is signed, and for how long.
//!
//! A device may hold an access key for good, or credentials handed to it for
//! an hour - an access key with a session token, from the service's STS or
//! from the app's own backend. A [`Bucket`](super::Bucket) takes either the
//! same way: a [`CredentialsProvider`] it asks for the credentials to sign
//! with, again before the ones it holds run out and again when the bucket
//! stops taking them.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// How long before the credentials held run out a bucket asks its provider
/// for the next ones. Until those come the ones held go on signing, for as
/// long as they last - so a provider that cannot be reached has this long to
/// come back before a request fails.
pub(crate) const RENEWAL: Duration = Duration::from_secs(300);

/// Why a provider could not hand credentials out.
pub(crate) type ProviderError = Box<dyn Error + Send + Sync>;

/// What a bucket's owner hands out to reach it: an access key's id and its
/// secret, as S3 and every service that speaks its protocol issue them -
/// and, for credentials that last an hour and not for good, the session
/// token that came with them and the moment they run out.
///
/// The secret never leaves the device: a request carries a signature made
/// with it. The session token goes with every request, in the one header
/// S3 reads it from. Nothing prints either - not `Debug`, not an error.
///
/// ```
/// use std::time::{Duration, SystemTime};
///
/// use efema::Credentials;
///
/// let for_good = Credentials::new("AKIDEXAMPLE", "not a real secret");
/// assert_eq!(for_good.expires(), None);
///
/// let in_an_hour = SystemTime::now() + Duration::from_secs(3600);
/// let for_an_hour = Credentials::new("ASIAEXAMPLE", "not a real secret").with_token("not a real token").until(in_an_hour);
/// assert_eq!(for_an_hour.expires(), Some(in_an_hour));
/// assert_eq!(format!("{for_an_hour:?}"), "Credentials(ASIAEXAMPLE, with a session token)");
/// ```
#[derive(Clone)]
pub struct Credentials {
    id: String,
    secret: String,
    token: Option<String>,
    expires: Option<SystemTime>,
}

impl Credentials {
    /// The access key `id` with its `secret`.
    pub fn new(id: impl Into<String>, secret: impl Into<String>) -> Self {
        Self { id: id.into(), secret: secret.into(), token: None, expires: None }
    }

    /// These credentials with the session `token` they were handed out
    /// with: what `AWS_SESSION_TOKEN` holds, and `SessionToken` in an answer
    /// of STS.
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// These credentials, known to run out at `expires` - `Expiration` in
    /// an answer of STS. A bucket asks its provider for the next ones five
    /// minutes before that. Credentials that do not say when they run out
    /// are used until the bucket refuses them.
    #[must_use]
    pub fn until(mut self, expires: SystemTime) -> Self {
        self.expires = Some(expires);
        self
    }

    /// The access key's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// When these credentials run out, if they say.
    pub fn expires(&self) -> Option<SystemTime> {
        self.expires
    }

    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }

    pub(crate) fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Whether a request signed with `other` is signed as with these: the
    /// same key, secret and token, whenever each says it runs out.
    fn sign_as(&self, other: &Self) -> bool {
        self.id == other.id && self.secret == other.secret && self.token == other.token
    }

    /// Whether these are worth asking the provider to replace at `now`:
    /// they run out within [`RENEWAL`].
    fn running_out(&self, now: SystemTime) -> bool {
        self.expires.is_some_and(|expires| now + RENEWAL >= expires)
    }

    /// Whether these have run out at `now`.
    fn ran_out(&self, now: SystemTime) -> bool {
        self.expires.is_some_and(|expires| now >= expires)
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.token {
            Some(_) => write!(f, "Credentials({}, with a session token)", self.id),
            None => write!(f, "Credentials({})", self.id),
        }
    }
}

/// Where a [`Bucket`](super::Bucket) gets the credentials it signs with.
///
/// [`Credentials`] are their own provider: an access key held for good, or
/// temporary ones that nothing will replace. An app whose devices get
/// credentials for an hour at a time implements this over wherever they
/// come from - its own backend, the service's STS - and the bucket asks it:
///
/// - before its first request;
/// - again once the credentials it holds run out within five minutes, if
///   they say when ([`Credentials::until`]) - before every request, until it
///   is handed ones that last longer. While the ones held still last, a
///   provider that fails does not fail the request;
/// - again when the bucket refuses the credentials it holds as not valid -
///   run out without saying when, or revoked. Handed other credentials, the
///   bucket signs the refused request with them and sends it once more;
///   handed the same ones, it reports the refusal.
///
/// The bucket asks one question at a time, however many requests wait for
/// the answer, and keeps what it is handed: a provider needs no cache of its
/// own.
///
/// ```
/// use std::time::{Duration, SystemTime};
///
/// use efema::{Bucket, Credentials, CredentialsProvider};
///
/// /// Credentials for an hour, from the app's own backend.
/// struct Backend;
///
/// impl CredentialsProvider for Backend {
///     async fn credentials(&self) -> Result<Credentials, Box<dyn std::error::Error + Send + Sync>> {
///         // Ask the backend here; what it answers:
///         let (id, secret, token) = ("ASIAEXAMPLE", "not a real secret", "not a real token");
///         Ok(Credentials::new(id, secret).with_token(token).until(SystemTime::now() + Duration::from_secs(3600)))
///     }
/// }
///
/// let bucket = Bucket::new("https://notes.s3.eu-central-1.amazonaws.com", "eu-central-1", Backend)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub trait CredentialsProvider: Send + Sync + 'static {
    /// The credentials to sign requests with from now on.
    ///
    /// # Errors
    ///
    /// Whatever kept the provider from handing them out. The bucket reports
    /// it as [`TransportError::Credentials`](crate::TransportError::Credentials)
    /// when it holds none that still last.
    fn credentials(&self) -> impl Future<Output = Result<Credentials, Box<dyn Error + Send + Sync>>> + Send;
}

impl CredentialsProvider for Credentials {
    async fn credentials(&self) -> Result<Credentials, ProviderError> {
        Ok(self.clone())
    }
}

/// A provider behind a pointer: the trait as a bucket of one type holds it.
trait Asked: Send + Sync {
    fn ask(&self) -> Pin<Box<dyn Future<Output = Result<Credentials, ProviderError>> + Send + '_>>;
}

impl<P: CredentialsProvider> Asked for P {
    fn ask(&self) -> Pin<Box<dyn Future<Output = Result<Credentials, ProviderError>> + Send + '_>> {
        Box::pin(self.credentials())
    }
}

/// The credentials a request was signed with, and which handing-out of the
/// provider they are: a refusal names the one it refused.
#[derive(Clone)]
pub(crate) struct Held {
    pub(crate) credentials: Arc<Credentials>,
    turn: u64,
}

/// A bucket's provider and what it handed out last. Shared by a bucket and
/// every prefix of it: one question to the provider serves them all.
pub(crate) struct Keeper {
    provider: Box<dyn Asked>,
    held: tokio::sync::Mutex<Option<Held>>,
}

impl Keeper {
    pub(crate) fn new(provider: impl CredentialsProvider) -> Self {
        Self { provider: Box::new(provider), held: tokio::sync::Mutex::new(None) }
    }

    /// The credentials to sign a request with at `now`: the ones held, or
    /// the provider's next ones if none are held or they are running out.
    pub(crate) async fn current(&self, now: SystemTime) -> Result<Held, ProviderError> {
        let mut held = self.held.lock().await;
        if let Some(held) = &*held
            && !held.credentials.running_out(now)
        {
            return Ok(held.clone());
        }
        let turn = held.as_ref().map_or(1, |held| held.turn + 1);
        match self.provider.ask().await {
            Ok(next) => {
                let next = Held { credentials: Arc::new(next), turn };
                *held = Some(next.clone());
                Ok(next)
            }
            Err(e) => match &*held {
                // The ones held still sign: the provider has until they run
                // out.
                Some(before) if !before.credentials.ran_out(now) => Ok(before.clone()),
                _ => Err(e),
            },
        }
    }

    /// Other credentials than `refused`, which the bucket did not take: the
    /// ones another request has got since, or the provider's next ones -
    /// `None` if it hands the same ones out again.
    pub(crate) async fn instead_of(&self, refused: &Held) -> Result<Option<Held>, ProviderError> {
        let mut held = self.held.lock().await;
        if let Some(held) = &*held
            && held.turn != refused.turn
        {
            return Ok(Some(held.clone()));
        }
        let next = self.provider.ask().await?;
        if next.sign_as(&refused.credentials) {
            return Ok(None);
        }
        let next = Held { credentials: Arc::new(next), turn: refused.turn + 1 };
        *held = Some(next.clone());
        Ok(Some(next))
    }
}

impl fmt::Debug for Keeper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.held.try_lock().as_deref() {
            Ok(Some(held)) => held.credentials.fmt(f),
            Ok(None) => f.write_str("no credentials asked for yet"),
            Err(_) => f.write_str("credentials being asked for"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    /// Hands out what it was given, one after another; an `Err` once the
    /// list is empty.
    struct Scripted {
        next: Mutex<Vec<Credentials>>,
        asked: Arc<AtomicUsize>,
    }

    fn scripted(mut list: Vec<Credentials>) -> (Keeper, Arc<AtomicUsize>) {
        list.reverse();
        let asked = Arc::new(AtomicUsize::new(0));
        (Keeper::new(Scripted { next: Mutex::new(list), asked: asked.clone() }), asked)
    }

    impl CredentialsProvider for Scripted {
        async fn credentials(&self) -> Result<Credentials, ProviderError> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.next.lock().unwrap().pop().ok_or_else(|| "the backend is down".into())
        }
    }

    fn temporary(id: &str, expires: SystemTime) -> Credentials {
        Credentials::new(id, "secret").with_token("token").until(expires)
    }

    #[tokio::test]
    async fn credentials_held_for_good_are_asked_for_once() {
        let (keeper, asked) = scripted(vec![Credentials::new("first", "secret")]);
        let now = SystemTime::now();
        for later in [Duration::ZERO, HOUR, 1000 * HOUR] {
            assert_eq!(keeper.current(now + later).await.unwrap().credentials.id(), "first");
        }
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn credentials_are_replaced_before_they_run_out_and_not_earlier() {
        let start = SystemTime::now();
        let (keeper, asked) = scripted(vec![temporary("first", start + HOUR), temporary("second", start + 2 * HOUR)]);
        assert_eq!(keeper.current(start).await.unwrap().credentials.id(), "first");
        let just_before = start + HOUR - RENEWAL - Duration::from_secs(1);
        assert_eq!(keeper.current(just_before).await.unwrap().credentials.id(), "first");
        assert_eq!(asked.load(Ordering::SeqCst), 1, "asked while the first ones had more than the margin left");
        assert_eq!(keeper.current(start + HOUR - RENEWAL).await.unwrap().credentials.id(), "second");
        assert_eq!(keeper.current(start + HOUR).await.unwrap().credentials.id(), "second");
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_provider_that_fails_does_not_fail_while_the_credentials_last() {
        let start = SystemTime::now();
        let (keeper, asked) = scripted(vec![temporary("first", start + HOUR)]);
        keeper.current(start).await.unwrap();
        let running_out = start + HOUR - Duration::from_secs(1);
        assert_eq!(keeper.current(running_out).await.unwrap().credentials.id(), "first");
        assert_eq!(asked.load(Ordering::SeqCst), 2, "the provider was not asked again");
        let ran_out = keeper.current(start + HOUR).await.err().expect("credentials that ran out signed");
        assert_eq!(ran_out.to_string(), "the backend is down");
    }

    #[tokio::test]
    async fn a_provider_that_fails_at_first_fails() {
        let (keeper, _) = scripted(Vec::new());
        assert!(keeper.current(SystemTime::now()).await.is_err());
    }

    #[tokio::test]
    async fn refused_credentials_are_replaced_by_other_ones_only() {
        let now = SystemTime::now();
        let same = Credentials::new("first", "secret").with_token("token");
        let (keeper, asked) =
            scripted(vec![same.clone(), same.clone(), Credentials::new("second", "secret").with_token("token")]);
        let first = keeper.current(now).await.unwrap();
        assert!(keeper.instead_of(&first).await.unwrap().is_none(), "the same credentials were handed out as others");
        let second = keeper.instead_of(&first).await.unwrap().expect("other credentials");
        assert_eq!(second.credentials.id(), "second");
        assert_eq!(keeper.current(now).await.unwrap().credentials.id(), "second");
        // A request refused with the first ones after another has replaced
        // them takes what that one got, and does not ask.
        let asked_before = asked.load(Ordering::SeqCst);
        assert_eq!(keeper.instead_of(&first).await.unwrap().unwrap().credentials.id(), "second");
        assert_eq!(asked.load(Ordering::SeqCst), asked_before);
        // Nothing left to hand out: the refusal is the provider's failure.
        assert!(keeper.instead_of(&second).await.is_err());
    }

    #[tokio::test]
    async fn a_token_alone_makes_credentials_other_ones() {
        let now = SystemTime::now();
        let (keeper, _) = scripted(vec![
            Credentials::new("id", "secret").with_token("one"),
            Credentials::new("id", "secret").with_token("two"),
        ]);
        let first = keeper.current(now).await.unwrap();
        let second = keeper.instead_of(&first).await.unwrap().expect("another token is other credentials");
        assert_eq!(second.credentials.token(), Some("two"));
    }

    #[tokio::test]
    async fn neither_the_secret_nor_the_token_is_printed() {
        let credentials = Credentials::new("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        assert_eq!(format!("{credentials:?}"), "Credentials(AKIAIOSFODNN7EXAMPLE)");
        let temporary = credentials.with_token("AQoDYXdzEJr-session-token").until(SystemTime::now());
        assert_eq!(format!("{temporary:?}"), "Credentials(AKIAIOSFODNN7EXAMPLE, with a session token)");

        let keeper = Keeper::new(temporary);
        assert_eq!(format!("{keeper:?}"), "no credentials asked for yet");
        keeper.current(SystemTime::UNIX_EPOCH).await.unwrap();
        assert_eq!(format!("{keeper:?}"), "Credentials(AKIAIOSFODNN7EXAMPLE, with a session token)");
    }
}
