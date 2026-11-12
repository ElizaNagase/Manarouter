//! Local admission for self-hosted providers.
//!
//! A hosted provider meters requests; a self-hosted vLLM endpoint meters KV tokens and wall-clock.
//! [`AdmissionPool`] holds reservations for the requests currently in flight so a new request is
//! rejected before its prompt is serialized and scheduled past the configured GPU capacity.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body, Frame};
use mr_core::store::{GpuUsageRecord, Store};

/// One self-hosted endpoint usage settlement. The relay may record a GPU-seconds row only after
/// the upstream returns a successful response; failed admission/upstream attempts release the
/// reservation without fabricating a billable row.
#[derive(Debug)]
pub struct KvBudgetLease {
    pool: Arc<AdmissionPool>,
    key: String,
    request_id: String,
    billing: Option<GpuBilling>,
    settled: bool,
}

/// Metadata for one settled GPU-second charge.
#[derive(Debug, Clone)]
pub struct GpuBilling {
    pub store: Arc<Store>,
    pub provider_id: String,
    pub model: String,
    pub price_per_second: Option<f64>,
    pub started: Instant,
}

impl KvBudgetLease {
    /// Mark the upstream attempt as having started successfully. The drop handler releases the
    /// local reservation and records elapsed request-body lifetime; a client disconnect can therefore
    /// still account for GPU time already spent.
    pub fn mark_success(&mut self) {
        self.settled = true;
    }
}

impl Drop for KvBudgetLease {
    fn drop(&mut self) {
        if self.settled {
            if let Some(billing) = self.billing.take() {
                let duration_ms =
                    billing.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                let gpu_seconds = duration_ms as f64 / 1000.0;
                let record = GpuUsageRecord {
                    usage_id: self.request_id.clone(),
                    provider_id: billing.provider_id.clone(),
                    model: billing.model.clone(),
                    duration_ms,
                    gpu_seconds,
                    cost_usd: billing.price_per_second.map(|price| price * gpu_seconds),
                    created_at: mr_core::ids::Millis::now(),
                };
                if let Err(error) = billing.store.record_gpu_usage(&record) {
                    tracing::warn!(request_id = %self.request_id, error = %error, "could not record self-hosted GPU usage");
                }
            }
        }
        self.pool.release(&self.key, &self.request_id);
    }
}

/// A body wrapper whose whole job is to keep a [`KvBudgetLease`] alive for exactly as long as
/// bytes are being delivered to the client.
pub struct BudgetedBody<B> {
    inner: B,
    _lease: KvBudgetLease,
}

impl<B> BudgetedBody<B> {
    pub fn new(inner: B, lease: KvBudgetLease) -> Self {
        Self {
            inner,
            _lease: lease,
        }
    }
}

impl<B> Body for BudgetedBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }
}

/// Why a self-hosted admission failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvBudgetDenial {
    /// The request exceeds the configured model context window.
    ContextExceeded,
    /// The requested context does not fit in the configured KV-token pool.
    PoolTooSmall,
    /// The pool is already reserved by in-flight requests.
    PoolFull,
    /// The configured max-concurrent slot count is already occupied.
    ConcurrencyFull,
}

/// Admission limits copied from a compiled provider plan.
#[derive(Debug, Clone, Copy)]
pub struct KvBudgetLimits {
    /// Maximum KV tokens held by concurrent requests.
    pub pool_tokens: Option<u64>,
    /// Maximum context length per request, from declared model metadata/provider override.
    pub max_context: Option<u64>,
    /// Maximum simultaneous concurrent requests.
    pub max_concurrent: Option<u32>,
}

/// Process-local reservation ledger for self-hosted endpoint budgets.
#[derive(Debug, Default)]
pub struct AdmissionPool {
    reservations: Mutex<std::collections::HashMap<String, Vec<KvReservation>>>,
}

#[derive(Debug, Clone)]
struct KvReservation {
    request_id: String,
    tokens: u64,
}

impl AdmissionPool {
    /// Construct an empty local admission pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve space for one request, returning a lease released on drop.
    pub fn reserve(
        self: &Arc<Self>,
        key: String,
        request_id: String,
        estimated_tokens: u64,
        limits: KvBudgetLimits,
        billing: Option<GpuBilling>,
    ) -> Result<KvBudgetLease, KvBudgetDenial> {
        let mut reservations = self
            .reservations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let booked = reservations.get(&key);
        let count = booked.map_or(0, Vec::len) as u32;
        let tokens: u64 = booked.map_or(0, |list| {
            list.iter().fold(0u64, |total, reservation| {
                total.saturating_add(reservation.tokens)
            })
        });

        if let Some(max_context) = limits.max_context {
            if estimated_tokens > max_context {
                return Err(KvBudgetDenial::ContextExceeded);
            }
        }
        if let Some(pool_tokens) = limits.pool_tokens {
            if estimated_tokens > pool_tokens {
                return Err(KvBudgetDenial::PoolTooSmall);
            }
            if tokens.saturating_add(estimated_tokens) > pool_tokens {
                return Err(KvBudgetDenial::PoolFull);
            }
        }
        if let Some(max_concurrent) = limits.max_concurrent {
            if count >= max_concurrent {
                return Err(KvBudgetDenial::ConcurrencyFull);
            }
        }

        reservations
            .entry(key.clone())
            .or_default()
            .push(KvReservation {
                request_id: request_id.clone(),
                tokens: estimated_tokens,
            });
        Ok(KvBudgetLease {
            pool: Arc::clone(self),
            key,
            request_id,
            billing,
            settled: false,
        })
    }

    /// Release a reservation if it is still present.
    fn release(&self, key: &str, request_id: &str) {
        let mut reservations = self
            .reservations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let empty = if let Some(list) = reservations.get_mut(key) {
            list.retain(|reservation| reservation.request_id != request_id);
            list.is_empty()
        } else {
            false
        };
        if empty {
            reservations.remove(key);
        }
    }

    /// Number of requests currently admitted for this key.
    pub fn count(&self, key: &str) -> usize {
        self.reservations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .map_or(0, Vec::len)
    }

    #[cfg(test)]
    fn pooled_tokens(&self, key: &str) -> u64 {
        self.reservations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .map_or(0, |list| {
                list.iter().map(|reservation| reservation.tokens).sum()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_pool_reservations_release_when_the_lease_drops() {
        let pool = Arc::new(AdmissionPool::new());
        let limits = KvBudgetLimits {
            pool_tokens: Some(100),
            max_context: None,
            max_concurrent: None,
        };
        let lease = pool
            .reserve("a".to_string(), "r1".to_string(), 80, limits, None)
            .unwrap();
        assert!(pool
            .reserve("a".to_string(), "r2".to_string(), 21, limits, None)
            .is_err());
        drop(lease);
        assert!(pool
            .reserve("a".to_string(), "r3".to_string(), 21, limits, None)
            .is_ok());
    }

    #[test]
    fn prompt_larger_than_context_is_refused_before_pool_admission() {
        let pool = Arc::new(AdmissionPool::new());
        let limits = KvBudgetLimits {
            pool_tokens: Some(100),
            max_context: Some(50),
            max_concurrent: Some(4),
        };
        assert_eq!(
            pool.reserve("a".into(), "r1".into(), 51, limits, None)
                .unwrap_err(),
            KvBudgetDenial::ContextExceeded
        );
        assert_eq!(pool.count("a"), 0);
    }

    #[test]
    fn max_concurrent_is_enforced() {
        let pool = Arc::new(AdmissionPool::new());
        let limits = KvBudgetLimits {
            pool_tokens: None,
            max_context: None,
            max_concurrent: Some(1),
        };
        let _lease = pool
            .reserve("a".to_string(), "r1".to_string(), 1, limits, None)
            .unwrap();
        assert_eq!(
            pool.reserve("a".to_string(), "r2".to_string(), 1, limits, None)
                .unwrap_err(),
            KvBudgetDenial::ConcurrencyFull
        );
    }

    #[test]
    fn billing_is_success_and_response_lifetime_scoped() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let pool = Arc::new(AdmissionPool::new());
        let limits = KvBudgetLimits {
            pool_tokens: None,
            max_context: None,
            max_concurrent: None,
        };
        let billing = GpuBilling {
            store: Arc::clone(&store),
            provider_id: "modal-a".to_string(),
            model: "qwen".to_string(),
            price_per_second: Some(2.0),
            started: Instant::now(),
        };
        let request_id = "r1".to_string();
        let mut lease = pool
            .reserve(
                "a".to_string(),
                request_id.clone(),
                1,
                limits,
                Some(billing),
            )
            .unwrap();
        assert_eq!(pool.count("a"), 1);
        assert_eq!(pool.pooled_tokens("a"), 1);
        lease.mark_success();
        std::thread::sleep(std::time::Duration::from_millis(12));
        drop(lease);
        assert_eq!(pool.count("a"), 0);
        let totals = store
            .gpu_usage_totals(
                mr_core::ids::Millis(0),
                mr_core::ids::Millis::now().plus_secs(1),
            )
            .unwrap();
        assert_eq!(totals.requests, 1);
        assert!(
            totals.gpu_seconds > 0.0,
            "GPU seconds were not settled: {totals:?}"
        );
        assert!(totals.cost_usd > 0.0, "cost was not derived: {totals:?}");
    }
}
