use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};
use tokio::time::Instant;

type Destinations = HashMap<String, Vec<SocketAddr>>;

/// One budget for the whole process, including reconnects and task replacements.
pub struct Probes {
    http_targets: Option<Destinations>,
    next_start: Mutex<Instant>,
    dns_slots: Arc<Semaphore>,
}

impl Probes {
    pub fn new(tls: bool, allowlist: &str) -> Self {
        let http_targets = (!tls).then(|| {
            parse_list(allowlist).unwrap_or_else(|| {
                eprintln!("invalid MONITOR_HTTP_PROBE_ALLOWLIST; HTTP latency probes disabled");
                HashMap::new()
            })
        });
        Self { http_targets, next_start: Mutex::new(Instant::now()), dns_slots: Arc::new(Semaphore::new(8)) }
    }

    pub fn allowed(&self, target: &str) -> bool {
        self.http_targets.as_ref().is_none_or(|list| list.contains_key(&key(target)))
    }

    pub async fn pace(&self) {
        // Serialize starts, not probes. An aborted waiter does not reserve a future slot.
        let mut next = self.next_start.lock().await;
        tokio::time::sleep_until(*next).await;
        *next = Instant::now() + Duration::from_millis(60);
    }

    pub async fn resolve(&self, target: &str) -> io::Result<std::vec::IntoIter<SocketAddr>> {
        if !self.allowed(target) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let addresses = if let Ok(addr) = target.parse::<SocketAddr>() {
            vec![addr]
        } else {
            let target = target.to_owned();
            self.blocking_lookup(move || Ok(target.to_socket_addrs()?.take(128).collect())).await?
        };
        self.filter_resolved(target, addresses)
    }

    fn filter_resolved(
        &self,
        target: &str,
        mut addresses: Vec<SocketAddr>,
    ) -> io::Result<std::vec::IntoIter<SocketAddr>> {
        if let Some(list) = &self.http_targets {
            let pins = list.get(&key(target)).ok_or(io::ErrorKind::PermissionDenied)?;
            addresses.retain(|address| pins.contains(address));
        }
        if addresses.is_empty() {
            return Err(io::Error::other("no permitted address resolved"));
        }
        Ok(addresses.into_iter())
    }

    async fn blocking_lookup<T: Send + 'static>(
        &self,
        lookup: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let permit = self.dns_slots.clone().acquire_owned().await.map_err(io::Error::other)?;
        // getaddrinfo can outlive both the deadline and the task. The permit
        // belongs to the OS call and is returned only when that call finishes.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            lookup()
        })
        .await
        .map_err(io::Error::other)?
    }
}

fn key(target: &str) -> String {
    target.parse::<SocketAddr>().map(|a| a.to_string()).unwrap_or_else(|_| target.to_ascii_lowercase())
}

fn parse_list(text: &str) -> Option<Destinations> {
    let mut list = HashMap::new();
    if text.trim().is_empty() {
        return Some(list);
    }
    for item in text.split(',').map(str::trim) {
        if let Ok(address) = item.parse::<SocketAddr>() {
            if address.port() == 0 {
                return None;
            }
            list.insert(address.to_string(), vec![address]);
            continue;
        }
        let (endpoint, pins) = item.split_once('=')?;
        let (host, port) = endpoint.rsplit_once(':')?;
        let port: u16 = port.parse().ok()?;
        if port == 0
            || host.is_empty()
            || host.len() > 253
            || !host.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
        {
            return None;
        }
        let pins: Option<Vec<_>> =
            pins.split('|').map(|ip| Some(SocketAddr::new(ip.parse::<IpAddr>().ok()?, port))).collect();
        list.insert(key(endpoint), pins?);
    }
    Some(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_requires_exact_targets_and_pinned_dns_answers() {
        let target = "example.test:443";
        let blocked = Probes::new(false, "");
        assert!(blocked.resolve(target).await.is_err());
        assert!(!Probes::new(false, "127.0.0.1:80,bad").allowed("127.0.0.1:80"));
        let probes = Probes::new(false, "EXAMPLE.TEST:443=192.0.2.1|2001:db8::1,[::1]:80");
        assert!(probes.allowed(target));
        assert!(!probes.allowed("example.test:80"));
        let approved = "192.0.2.1:443".parse().unwrap();
        let unapproved = "127.0.0.1:443".parse().unwrap();
        assert!(probes.filter_resolved(target, vec![unapproved]).is_err());
        assert_eq!(
            probes.filter_resolved(target, vec![unapproved, approved]).unwrap().collect::<Vec<_>>(),
            [approved]
        );
        assert_eq!(probes.resolve("[::1]:80").await.unwrap().count(), 1);
        assert!(Probes::new(true, "").allowed("internal.example:443"));
    }

    #[tokio::test(start_paused = true)]
    async fn replaced_tasks_share_the_same_start_budget() {
        let probes = Arc::new(Probes::new(true, ""));
        probes.pace().await;
        let start = Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let shared = probes.clone();
            tasks.push(tokio::spawn(async move {
                shared.pace().await;
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert!(start.elapsed() >= Duration::from_millis(240));
    }

    #[tokio::test]
    async fn cancelled_dns_still_counts_toward_the_worker_limit() {
        let probes = Arc::new(Probes::new(true, ""));
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let shared = probes.clone();
        let active = tokio::spawn(async move {
            shared
                .blocking_lookup(move || {
                    let _ = started.send(());
                    let _ = wait.recv_timeout(Duration::from_secs(5));
                    Ok(())
                })
                .await
        });
        ready.await.unwrap();
        active.abort();
        let _ = active.await;
        assert_eq!(probes.dns_slots.available_permits(), 7);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), probes.dns_slots.acquire_many(8))
            .await
            .unwrap()
            .unwrap()
            .forget();
    }
}
