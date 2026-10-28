// Prometheus-compatible metrics + per-request timing.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

const BUCKETS_MS: &[f64] = &[
    1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0,
];

pub struct Metrics {
    pub requests_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub schema_reloads: AtomicU64,
    pub by_status: Mutex<HashMap<u16, u64>>,
    pub buckets: Vec<AtomicU64>,
    pub duration_sum_ms: AtomicU64,
}

impl Metrics {
    pub fn new() -> Metrics {
        Metrics {
            requests_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            schema_reloads: AtomicU64::new(0),
            by_status: Mutex::new(HashMap::new()),
            buckets: BUCKETS_MS.iter().map(|_| AtomicU64::new(0)).collect(),
            duration_sum_ms: AtomicU64::new(0),
        }
    }

    pub fn record(&self, status: u16, duration_ms: f64) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        if status >= 400 {
            self.errors_total.fetch_add(1, Ordering::Relaxed);
        }
        {
            let mut m = self.by_status.lock().unwrap();
            *m.entry(status).or_insert(0) += 1;
        }
        self.duration_sum_ms
            .fetch_add(duration_ms as u64, Ordering::Relaxed);
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            if duration_ms <= *b {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn render(&self, pool_available: u64, pool_size: u64) -> String {
        let rss_kb = resident_kib();
        let mut out = String::with_capacity(2048);
        out.push_str("# TYPE pgrst_requests_total counter\n");
        out.push_str(&format!(
            "pgrst_requests_total {}\n",
            self.requests_total.load(Ordering::Relaxed)
        ));
        out.push_str("# TYPE pgrst_errors_total counter\n");
        out.push_str(&format!(
            "pgrst_errors_total {}\n",
            self.errors_total.load(Ordering::Relaxed)
        ));
        out.push_str("# TYPE pgrst_schema_cache_reloads_total counter\n");
        out.push_str(&format!(
            "pgrst_schema_cache_reloads_total {}\n",
            self.schema_reloads.load(Ordering::Relaxed)
        ));
        {
            let m = self.by_status.lock().unwrap();
            let mut keys: Vec<u16> = m.keys().cloned().collect();
            keys.sort();
            out.push_str("# TYPE pgrst_responses_total counter\n");
            for k in keys {
                out.push_str(&format!(
                    "pgrst_responses_total{{code=\"{}\"}} {}\n",
                    k, m[&k]
                ));
            }
        }
        out.push_str("# TYPE pgrst_request_duration_seconds histogram\n");
        let mut prev: u64 = 0;
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            let cum = self.buckets[i].load(Ordering::Relaxed);
            out.push_str(&format!(
                "pgrst_request_duration_seconds_bucket{{le=\"{}\"}} {}\n",
                b / 1000.0,
                cum
            ));
            prev = cum;
        }
        let _ = prev;
        out.push_str(&format!(
            "pgrst_request_duration_seconds_sum {}\n",
            self.duration_sum_ms.load(Ordering::Relaxed) as f64 / 1000.0
        ));
        out.push_str(&format!(
            "pgrst_request_duration_seconds_count {}\n",
            self.requests_total.load(Ordering::Relaxed)
        ));
        out.push_str("# TYPE pgrst_db_pool gauge\n");
        out.push_str(&format!("pgrst_db_pool_available {}\n", pool_available));
        out.push_str(&format!("pgrst_db_pool_size {}\n", pool_size));
        out.push_str("# TYPE pgrst_process_resident_memory_bytes gauge\n");
        out.push_str(&format!("pgrst_process_resident_memory_bytes {}\n", rss_kb * 1024));
        out
    }
}

pub fn resident_kib() -> u64 {
    // /proc/self/statm: size resident shared text lib data dt (pages)
    if let Ok(s) = std::fs::read_to_string("/proc/self/statm") {
        let mut it = s.split_whitespace();
        let _size = it.next();
        if let Some(res) = it.next().and_then(|v| v.parse::<u64>().ok()) {
            return res * 4; // assume 4 KiB pages
        }
    }
    0
}
