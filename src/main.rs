use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;
use hdrhistogram::Histogram;
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use url::Url;

#[derive(Parser, Debug)]
#[command(name = "wrk-rs", version, about = "HTTP benchmarking tool (Rust rewrite of wrk)")]
struct Args {
    /// total number of HTTP connections to keep open with each thread handling N = connections/threads
    #[arg(short = 'c', long = "connections", default_value_t = 10)]
    connections: usize,

    /// duration of the test, e.g. 2s, 2m, 2h
    #[arg(short = 'd', long = "duration", default_value = "10s")]
    duration: String,

    /// total number of threads to use
    #[arg(short = 't', long = "threads")]
    threads: Option<usize>,

    /// HTTP header to add to request, e.g. "User-Agent: wrk"
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,

    /// print detailed latency statistics
    #[arg(long = "latency")]
    latency: bool,

    /// record a timeout if a response is not received within this amount of time
    #[arg(long = "timeout")]
    timeout: Option<String>,

    /// request HTTP/2 (HTTPS only; negotiates via ALPN)
    #[arg(long = "http2")]
    http2: bool,

    /// target URL
    url: String,
}

struct Metrics {
    requests: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    timeouts: AtomicU64,
    latencies: Mutex<Histogram<u64>>, // microseconds
}

impl Metrics {
    fn new() -> Self {
        let hist = Histogram::new(3).expect("histogram");
        Self {
            requests: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            latencies: Mutex::new(hist),
        }
    }

    fn record_ok(&self, latency: Duration, bytes: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        let micros = latency.as_micros().min(u64::MAX as u128) as u64;
        let mut hist = self.latencies.lock();
        let _ = hist.record(micros);
    }

    fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    fn record_timeout(&self) {
        self.timeouts.fetch_add(1, Ordering::Relaxed);
    }
}

fn parse_duration(input: &str) -> Result<Duration, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("duration is empty".to_string());
    }

    let (num_part, suffix) = input.split_at(input.len() - 1);
    let (number, unit) = if suffix.chars().all(|c| c.is_ascii_digit()) {
        (input, "s")
    } else {
        (num_part, suffix)
    };

    let value: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration number: {number}"))?;

    match unit {
        "s" => Ok(Duration::from_secs(value)),
        "m" => Ok(Duration::from_secs(value * 60)),
        "h" => Ok(Duration::from_secs(value * 60 * 60)),
        _ => Err(format!("invalid duration unit: {unit}")),
    }
}

fn parse_headers(values: &[String]) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for raw in values {
        let parts: Vec<&str> = raw.splitn(2, ':').collect();
        if parts.len() != 2 {
            return Err(format!("invalid header format: {raw}"));
        }
        let name = parts[0].trim();
        let value = parts[1].trim();
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name: {name}"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| format!("invalid header value for {name}"))?;
        headers.append(name, value);
    }
    Ok(headers)
}

fn build_client(http2: bool, url: &Url) -> Result<reqwest::Client, String> {
    let builder = reqwest::Client::builder()
        .tcp_nodelay(true)
        .pool_max_idle_per_host(usize::MAX);

    if http2 {
        if url.scheme() != "https" {
            return Err("--http2 requires https URL (h2c not supported)".to_string());
        }
    }

    builder.build().map_err(|e| format!("client build failed: {e}"))
}

fn main() {
    let args = Args::parse();

    let duration = match parse_duration(&args.duration) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    let timeout = match args.timeout.as_deref() {
        Some(raw) => match parse_duration(raw) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        },
        None => None,
    };

    let url = match Url::parse(&args.url) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("error: invalid url: {e}");
            std::process::exit(2);
        }
    };

    if url.scheme() != "http" && url.scheme() != "https" {
        eprintln!("error: unsupported scheme: {}", url.scheme());
        std::process::exit(2);
    }

    let headers = match parse_headers(&args.headers) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    let threads = args.threads.unwrap_or_else(num_cpus::get).max(1);
    let connections = args.connections.max(1);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime");

    runtime.block_on(async_main(args, url, headers, duration, timeout, threads, connections));
}

async fn async_main(
    args: Args,
    url: Url,
    headers: HeaderMap,
    duration: Duration,
    timeout: Option<Duration>,
    threads: usize,
    connections: usize,
) {
    let client = match build_client(args.http2, &url) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    let metrics = std::sync::Arc::new(Metrics::new());
    let deadline = Instant::now() + duration;

    println!(
        "Running {}s test @ {}",
        duration.as_secs_f64(),
        url.as_str()
    );
    println!("  {} threads and {} connections", threads, connections);

    let report_metrics = metrics.clone();
    let report_deadline = deadline;
    let report_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut last_reqs = 0u64;
        let mut last_bytes = 0u64;
        let mut last_time = Instant::now();
        loop {
            interval.tick().await;
            if Instant::now() > report_deadline {
                break;
            }
            let now = Instant::now();
            let total_reqs = report_metrics.requests.load(Ordering::Relaxed);
            let total_bytes = report_metrics.bytes.load(Ordering::Relaxed);
            let dt = now.duration_since(last_time).as_secs_f64().max(0.001);
            let dreqs = total_reqs.saturating_sub(last_reqs);
            let dbytes = total_bytes.saturating_sub(last_bytes);

            let rps = dreqs as f64 / dt;
            let mbps = (dbytes as f64 / dt) / (1024.0 * 1024.0);

            let (mean_ms, max_ms, stdev_ms) = {
                let hist = report_metrics.latencies.lock();
                if hist.len() == 0 {
                    (0.0, 0.0, 0.0)
                } else {
                    (
                        hist.mean() / 1000.0,
                        hist.max() as f64 / 1000.0,
                        hist.stdev() / 1000.0,
                    )
                }
            };

            println!(
                "Live (5s): {:.2} req/s, {:.2} MB/s, avg {:.2} ms, stdev {:.2} ms, max {:.2} ms",
                rps, mbps, mean_ms, stdev_ms, max_ms
            );

            last_reqs = total_reqs;
            last_bytes = total_bytes;
            last_time = now;
        }
    });

    let mut handles = Vec::with_capacity(connections);
    for _ in 0..connections {
        let client = client.clone();
        let metrics = metrics.clone();
        let url = url.clone();
        let headers = headers.clone();
        let timeout = timeout;
        let deadline = deadline;

        let handle = tokio::spawn(async move {
            let base = client.get(url.as_str()).headers(headers);
            loop {
                if Instant::now() >= deadline {
                    break;
                }

                let req = match base.try_clone().and_then(|b| b.build().ok()) {
                    Some(r) => r,
                    None => {
                        metrics.record_error();
                        continue;
                    }
                };

                let start = Instant::now();
                let result = if let Some(t) = timeout {
                    tokio::time::timeout(t, client.execute(req)).await
                } else {
                    Ok(client.execute(req).await)
                };

                match result {
                    Ok(Ok(resp)) => {
                        let bytes = match resp.bytes().await {
                            Ok(b) => b.len() as u64,
                            Err(_) => 0,
                        };
                        metrics.record_ok(start.elapsed(), bytes);
                    }
                    Ok(Err(_)) => {
                        metrics.record_error();
                    }
                    Err(_) => {
                        metrics.record_timeout();
                    }
                }
            }
        });

        handles.push(handle);
    }

    for h in handles {
        let _ = h.await;
    }
    let _ = report_handle.await;

    let total_reqs = metrics.requests.load(Ordering::Relaxed);
    let total_bytes = metrics.bytes.load(Ordering::Relaxed);
    let total_errors = metrics.errors.load(Ordering::Relaxed);
    let total_timeouts = metrics.timeouts.load(Ordering::Relaxed);
    let secs = duration.as_secs_f64().max(0.001);

    let (mean_ms, stdev_ms, max_ms, p50_ms, p75_ms, p90_ms, p99_ms) = {
        let hist = metrics.latencies.lock();
        if hist.len() == 0 {
            (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0)
        } else {
            (
                hist.mean() / 1000.0,
                hist.stdev() / 1000.0,
                hist.max() as f64 / 1000.0,
                hist.value_at_quantile(0.50) as f64 / 1000.0,
                hist.value_at_quantile(0.75) as f64 / 1000.0,
                hist.value_at_quantile(0.90) as f64 / 1000.0,
                hist.value_at_quantile(0.99) as f64 / 1000.0,
            )
        }
    };

    println!(
        "{} requests in {:.2}s, {:.2}GB read",
        total_reqs,
        secs,
        total_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    );
    println!("Requests/sec: {:.2}", total_reqs as f64 / secs);
    println!(
        "Transfer/sec: {:.2}MB",
        (total_bytes as f64 / secs) / (1024.0 * 1024.0)
    );
    if total_errors > 0 || total_timeouts > 0 {
        println!("Errors: {}, Timeouts: {}", total_errors, total_timeouts);
    }
    if args.latency {
        println!(
            "Latency (ms): avg {:.2}, stdev {:.2}, max {:.2}",
            mean_ms, stdev_ms, max_ms
        );
        println!(
            "Latency percentiles (ms): p50 {:.2}, p75 {:.2}, p90 {:.2}, p99 {:.2}",
            p50_ms, p75_ms, p90_ms, p99_ms
        );
    }
}
