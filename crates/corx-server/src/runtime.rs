//! Native [`corx::service::Runtime`] over governor (rate limits), moka
//! (shared cache) and tokio (timer).

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use corx::Fetch;
use corx::service::{Admission, Bucket, Cached, Fresh, Runtime};
use governor::clock::{Clock, DefaultClock};
use governor::middleware::NoOpMiddleware;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use http::StatusCode;
use moka::future::Cache;

use crate::app::Limits;

type Limiter = RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock, NoOpMiddleware>;

const SUCCESS_TTL: Duration = Duration::from_hours(24);
const ERROR_TTL: Duration = Duration::from_secs(3600);

/// TTL per stored [`Fresh`]: successes a day, cacheable errors an hour.
struct FreshExpiry;

impl moka::Expiry<String, Fresh> for FreshExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Fresh,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(if value.response.status() == StatusCode::OK {
            SUCCESS_TTL
        } else {
            ERROR_TTL
        })
    }
}

/// Shared runtime state for the request pipeline.
#[derive(Debug)]
pub(crate) struct NativeRuntime<F> {
    fetch: F,
    cache: Cache<String, Fresh>,
    client: Arc<Limiter>,
    host: Arc<Limiter>,
}

impl<F> NativeRuntime<F> {
    /// Builds the runtime and spawns the limiter-key pruning task when a
    /// tokio runtime is active.
    pub(crate) fn new(fetch: F, limits: &Limits) -> Self {
        let nz = |v: u32| std::num::NonZeroU32::new(v).unwrap_or(std::num::NonZeroU32::MIN);
        let client = Arc::new(RateLimiter::keyed(
            Quota::per_minute(nz(limits.ip_rate_per_minute)).allow_burst(nz(limits.ip_burst)),
        ));
        let host = Arc::new(RateLimiter::keyed(Quota::per_minute(nz(
            limits.host_rate_per_minute
        ))));
        spawn_retain(&client, &host);
        Self {
            fetch,
            cache: Cache::builder()
                .max_capacity(limits.cache_capacity)
                .expire_after(FreshExpiry)
                .build(),
            client,
            host,
        }
    }
}

/// Periodically prunes stale limiter keys; spawned when a runtime is active.
fn spawn_retain(client: &Arc<Limiter>, host: &Arc<Limiter>) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let client = Arc::clone(client);
    let host = Arc::clone(host);
    rt.spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            client.retain_recent();
            client.shrink_to_fit();
            host.retain_recent();
            host.shrink_to_fit();
        }
    });
}

impl<F: Fetch> Runtime for NativeRuntime<F> {
    type Fetch = F;

    fn fetcher(&self) -> &F {
        &self.fetch
    }

    fn now_unix_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(duration)
    }

    fn admit(&self, bucket: Bucket, key: &str) -> impl Future<Output = Admission> + Send {
        let limiter = match bucket {
            Bucket::Client => &self.client,
            Bucket::Host => &self.host,
        };
        let verdict = match limiter.check_key(&key.to_owned()) {
            Ok(()) => Admission::Allowed,
            Err(not_until) => Admission::Denied {
                retry_after_secs: not_until.wait_time_from(limiter.clock().now()).as_secs(),
            },
        };
        std::future::ready(verdict)
    }

    async fn cached<M>(&self, key: &str, miss: M) -> Cached
    where
        M: Future<Output = Fresh> + Send,
    {
        let entry = self
            .cache
            .entry(key.to_owned())
            .or_try_insert_with(async move {
                let fresh = miss.await;
                if fresh.cacheable {
                    Ok(fresh)
                } else {
                    Err(fresh)
                }
            })
            .await;
        match entry {
            Ok(entry) if entry.is_fresh() => Cached::Miss(entry.into_value()),
            Ok(entry) => Cached::Hit(entry.into_value().response),
            Err(shared) => Cached::Miss(Arc::unwrap_or_clone(shared)),
        }
    }
}
