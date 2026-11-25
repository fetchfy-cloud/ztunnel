// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::config;
use crate::config::ProxyMode;
use crate::identity::Priority::Warmup;
use crate::identity::{Identity, SecretManager};
use crate::state::WorkloadInfo;
use crate::state::workload::{InboundProtocol, Workload};
use crate::strng::Strng;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

/// Responsible for pre-fetching certs for workloads.
pub trait CertFetcher: Send + Sync {
    fn prefetch_cert(&self, w: &Workload);
    /// Called on every real removal of a workload. last_identity_on_node tells
    /// whether no other workload on the node still uses its identity.
    fn clear_cert(&self, w: &Workload, last_identity_on_node: bool);
}

/// A no-op implementation of [CertFetcher].
pub struct NoCertFetcher();

impl CertFetcher for NoCertFetcher {
    fn prefetch_cert(&self, _: &Workload) {}
    fn clear_cert(&self, _: &Workload, _: bool) {}
}

/// Constructs an appropriate [CertFetcher] for the proxy config.
pub fn new(cfg: &config::Config, cert_manager: Arc<SecretManager>) -> Arc<dyn CertFetcher> {
    match cfg.proxy_mode {
        ProxyMode::Dedicated => Arc::new(NoCertFetcher()),
        ProxyMode::Shared => Arc::new(CertFetcherImpl::new(cfg, cert_manager)),
    }
}

/// Internal request type for the pre-fetch channel. Fetch runs at warmup
/// priority. Forget removes the identity-keyed entry. ForgetWorkload removes
/// the workload-keyed entry that SPIRE mode creates for each pod.
enum PrefetchRequest {
    Fetch(Identity),
    Forget(Identity),
    ForgetWorkload {
        id: Identity,
        uid: Strng,
        workload: Arc<WorkloadInfo>,
    },
}

/// A real [CertFetcher] that asynchronously forwards cert pre-fetch requests to a [SecretManager].
struct CertFetcherImpl {
    proxy_mode: ProxyMode,
    local_node: Option<String>,
    tx: mpsc::Sender<PrefetchRequest>,
    spire_enabled: bool,
}

impl CertFetcherImpl {
    fn new(cfg: &config::Config, cert_manager: Arc<SecretManager>) -> Self {
        let (tx, mut rx) = mpsc::channel::<PrefetchRequest>(256);

        // Spawn a task for handling the pre-fetch requests asynchronously.
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                match req {
                    PrefetchRequest::Fetch(workload_identity) => {
                        match cert_manager
                            .fetch_certificate_pri(&workload_identity, Warmup)
                            .await
                        {
                            Ok(_) => {
                                debug!("prefetched cert for {:?}", workload_identity.to_string())
                            }
                            Err(e) => error!(
                                "unable to prefetch cert for {:?}, skipping, {:?}",
                                workload_identity.to_string(),
                                e
                            ),
                        }
                    }
                    PrefetchRequest::Forget(workload_identity) => {
                        cert_manager.forget_certificate(&workload_identity).await;
                    }
                    PrefetchRequest::ForgetWorkload { id, uid, workload } => {
                        cert_manager
                            .forget_certificate_by_workload(&id, workload, uid)
                            .await;
                    }
                }
            }
        });

        Self {
            proxy_mode: cfg.proxy_mode,
            local_node: cfg.local_node.clone(),
            tx,
            spire_enabled: cfg.spire_enabled,
        }
    }

    // Determine if we should prefetch a certificate for this workload. Being "wrong" is not
    // too bad; a missing cert will be fetched on-demand when we get a request, so will just
    // result in some extra latency.
    fn should_prefetch_certificate(&self, w: &Workload) -> bool {
        // Only shared mode fetches other workloads's certs
        self.proxy_mode == ProxyMode::Shared &&
            // We only get certs for our own node
            Some(w.node.as_ref()) == self.local_node.as_deref() &&
            // If it doesn't support HBONE it *probably* doesn't need a cert.
            (w.native_tunnel || w.protocol == InboundProtocol::HBONE) &&
            // SPIRE attests the pod by PID, so its cert can only be fetched on demand.
            !self.spire_enabled
    }
}

impl CertFetcher for CertFetcherImpl {
    fn prefetch_cert(&self, w: &Workload) {
        if self.should_prefetch_certificate(w)
            && let Err(e) = self.tx.try_send(PrefetchRequest::Fetch(w.identity()))
        {
            info!("couldn't prefetch: {:?}", e)
        }
    }

    fn clear_cert(&self, w: &Workload, last_identity_on_node: bool) {
        // A workload-keyed entry belongs to this pod alone, so SPIRE mode drops
        // it on every removal. The identity-keyed entry is shared by the pods
        // of the identity, so it goes only with the last of them.
        let req = if self.spire_enabled {
            PrefetchRequest::ForgetWorkload {
                id: w.identity(),
                uid: w.uid.clone(),
                workload: Arc::new(WorkloadInfo::new(
                    w.name.to_string(),
                    w.namespace.to_string(),
                    w.service_account.to_string(),
                )),
            }
        } else if last_identity_on_node {
            PrefetchRequest::Forget(w.identity())
        } else {
            return;
        };
        if let Err(e) = self.tx.try_send(req) {
            info!("couldn't clear identity: {:?}", e)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::identity::mock::new_secret_manager;
    use crate::strng;
    use crate::test_helpers::{test_config, test_default_workload};

    fn workload(uid: &str) -> Workload {
        Workload {
            uid: strng::new(uid),
            ..test_default_workload()
        }
    }

    fn info(w: &Workload) -> Arc<WorkloadInfo> {
        Arc::new(WorkloadInfo::new(
            w.name.to_string(),
            w.namespace.to_string(),
            w.service_account.to_string(),
        ))
    }

    // The fetcher forgets through a channel, so wait for the cache to settle.
    async fn wait_for_len(cm: &SecretManager, want: usize) {
        for _ in 0..100 {
            if cm.cache_len().await == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(cm.cache_len().await, want);
    }

    #[tokio::test]
    async fn spire_clears_the_cert_of_each_removed_pod() {
        let mut cfg = test_config();
        cfg.proxy_mode = ProxyMode::Shared;
        cfg.spire_enabled = true;
        let cm = new_secret_manager(Duration::from_secs(10));
        let fetcher = CertFetcherImpl::new(&cfg, cm.clone());

        let (a, b) = (workload("pod-a"), workload("pod-b"));
        for w in [&a, &b] {
            cm.fetch_certificate_by_workload(&w.identity(), info(w), w.uid.clone())
                .await
                .unwrap();
        }
        assert_eq!(cm.cache_len().await, 2);

        // Pod b still uses the identity, but the cert of pod a is its own.
        fetcher.clear_cert(&a, false);
        wait_for_len(&cm, 1).await;
        let left = cm.collect_certs(|k, _| k.to_string()).await;
        assert!(left[0].contains("pod-b"), "{left:?}");
    }

    #[tokio::test]
    async fn istio_ca_keeps_the_cert_while_the_identity_is_in_use() {
        let mut cfg = test_config();
        cfg.proxy_mode = ProxyMode::Shared;
        let cm = new_secret_manager(Duration::from_secs(10));
        let fetcher = CertFetcherImpl::new(&cfg, cm.clone());

        let w = workload("pod-a");
        cm.fetch_certificate(&w.identity()).await.unwrap();

        fetcher.clear_cert(&w, false);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(cm.cache_len().await, 1);

        fetcher.clear_cert(&w, true);
        wait_for_len(&cm, 0).await;
    }
}
