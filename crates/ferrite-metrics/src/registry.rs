//! The metric primitives and the Prometheus text encoder.
//!
//! Everything is lock-free: a counter is one `AtomicU64`, a gauge one
//! `AtomicI64`, a histogram one `AtomicU64` per bucket. Recording a metric
//! on the query path must never contend with another connection doing the
//! same, which rules out a `Mutex<HashMap<..>>` registry.

use std::fmt::Write as _;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// A monotonically increasing count.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn inc(&self) {
        self.add(1);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A value that goes up and down.
#[derive(Debug, Default)]
pub struct Gauge(AtomicI64);

impl Gauge {
    pub const fn new() -> Self {
        Self(AtomicI64::new(0))
    }

    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn set(&self, value: i64) {
        self.0.store(value, Ordering::Relaxed);
    }

    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// The label set of a [`CounterVec`], as a closed enum rather than free
/// strings: an unbounded label space is the classic way to blow up a
/// Prometheus server's memory, and every dimension Ferrite reports is known
/// at compile time.
pub trait Label: Copy {
    /// Label values, indexed by [`Label::index`].
    const VALUES: &'static [&'static str];
    /// The label's name, e.g. `kind`.
    const NAME: &'static str;
    fn index(self) -> usize;
}

/// A family of counters sharing a name and differing by one label.
#[derive(Debug)]
pub struct CounterVec<L: Label> {
    counters: Box<[Counter]>,
    _label: PhantomData<fn() -> L>,
}

impl<L: Label> Default for CounterVec<L> {
    fn default() -> Self {
        Self::new()
    }
}

impl<L: Label> CounterVec<L> {
    pub fn new() -> Self {
        Self {
            counters: L::VALUES.iter().map(|_| Counter::new()).collect(),
            _label: PhantomData,
        }
    }

    pub fn inc(&self, label: L) {
        if let Some(counter) = self.counters.get(label.index()) {
            counter.inc();
        }
    }

    pub fn get(&self, label: L) -> u64 {
        self.counters.get(label.index()).map_or(0, Counter::get)
    }
}

/// Bucket upper bounds, in seconds. The string form is carried alongside so
/// the `le` label is rendered exactly as written rather than through float
/// formatting, which Prometheus matches textually across scrapes.
const BUCKETS: &[(f64, &str)] = &[
    (0.0005, "0.0005"),
    (0.001, "0.001"),
    (0.005, "0.005"),
    (0.01, "0.01"),
    (0.05, "0.05"),
    (0.1, "0.1"),
    (0.5, "0.5"),
    (1.0, "1"),
    (5.0, "5"),
    (10.0, "10"),
];

/// A fixed-bucket latency histogram.
///
/// The sum is accumulated in microseconds so it stays an integer add; it is
/// rendered in seconds, which is the unit Prometheus expects.
#[derive(Debug)]
pub struct Histogram {
    buckets: Box<[AtomicU64]>,
    sum_micros: AtomicU64,
    count: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Self {
        Self {
            buckets: BUCKETS.iter().map(|_| AtomicU64::new(0)).collect(),
            sum_micros: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, seconds: f64) {
        self.sum_micros
            .fetch_add((seconds * 1e6) as u64, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        for (i, (bound, _)) in BUCKETS.iter().enumerate() {
            if seconds <= *bound {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

/// Accumulates the Prometheus text exposition format, version 0.0.4.
pub struct Encoder {
    out: String,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            out: String::with_capacity(4096),
        }
    }

    pub fn counter(&mut self, name: &str, help: &str, counter: &Counter) {
        self.header(name, help, "counter");
        let _ = writeln!(self.out, "{name} {}", counter.get());
    }

    pub fn gauge(&mut self, name: &str, help: &str, gauge: &Gauge) {
        self.header(name, help, "gauge");
        let _ = writeln!(self.out, "{name} {}", gauge.get());
    }

    pub fn counter_vec<L: Label>(&mut self, name: &str, help: &str, family: &CounterVec<L>) {
        self.header(name, help, "counter");
        for (i, value) in L::VALUES.iter().enumerate() {
            let count = family.counters.get(i).map_or(0, Counter::get);
            let _ = writeln!(self.out, "{name}{{{}=\"{value}\"}} {count}", L::NAME);
        }
    }

    pub fn histogram(&mut self, name: &str, help: &str, histogram: &Histogram) {
        self.header(name, help, "histogram");
        let mut cumulative = 0u64;
        for (i, (_, label)) in BUCKETS.iter().enumerate() {
            cumulative += histogram.buckets[i].load(Ordering::Relaxed);
            let _ = writeln!(self.out, "{name}_bucket{{le=\"{label}\"}} {cumulative}");
        }
        let count = histogram.count.load(Ordering::Relaxed);
        let _ = writeln!(self.out, "{name}_bucket{{le=\"+Inf\"}} {count}");
        let sum = histogram.sum_micros.load(Ordering::Relaxed) as f64 / 1e6;
        let _ = writeln!(self.out, "{name}_sum {sum}");
        let _ = writeln!(self.out, "{name}_count {count}");
    }

    fn header(&mut self, name: &str, help: &str, kind: &str) {
        let _ = writeln!(self.out, "# HELP {name} {help}");
        let _ = writeln!(self.out, "# TYPE {name} {kind}");
    }

    pub fn finish(self) -> String {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum Kind {
        Read,
        Write,
    }

    impl Label for Kind {
        const VALUES: &'static [&'static str] = &["read", "write"];
        const NAME: &'static str = "kind";
        fn index(self) -> usize {
            match self {
                Kind::Read => 0,
                Kind::Write => 1,
            }
        }
    }

    #[test]
    fn a_counter_only_goes_up() {
        let counter = Counter::new();
        counter.inc();
        counter.add(4);
        assert_eq!(counter.get(), 5);
    }

    #[test]
    fn a_gauge_tracks_a_balance() {
        let gauge = Gauge::new();
        gauge.inc();
        gauge.inc();
        gauge.dec();
        assert_eq!(gauge.get(), 1);
        gauge.set(-3);
        assert_eq!(gauge.get(), -3);
    }

    #[test]
    fn each_label_counts_separately() {
        let family: CounterVec<Kind> = CounterVec::new();
        family.inc(Kind::Read);
        family.inc(Kind::Read);
        family.inc(Kind::Write);
        assert_eq!(family.get(Kind::Read), 2);
        assert_eq!(family.get(Kind::Write), 1);
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_end_at_the_total() {
        let histogram = Histogram::new();
        histogram.observe(0.0001);
        histogram.observe(0.02);
        histogram.observe(60.0);

        let mut encoder = Encoder::new();
        encoder.histogram("t_seconds", "help", &histogram);
        let text = encoder.finish();

        assert!(text.contains("t_seconds_bucket{le=\"0.0005\"} 1"));
        assert!(text.contains("t_seconds_bucket{le=\"0.05\"} 2"));
        // The 60 s observation falls past the last bound, so it only shows
        // up in `+Inf` — which must still equal the total count.
        assert!(text.contains("t_seconds_bucket{le=\"10\"} 2"));
        assert!(text.contains("t_seconds_bucket{le=\"+Inf\"} 3"));
        assert!(text.contains("t_seconds_count 3"));
    }

    #[test]
    fn the_encoder_writes_help_and_type_before_every_family() {
        let mut encoder = Encoder::new();
        encoder.counter("c_total", "a count", &Counter::new());
        encoder.gauge("g", "a level", &Gauge::new());
        encoder.counter_vec::<Kind>("v_total", "by kind", &CounterVec::new());
        let text = encoder.finish();

        assert!(text.contains("# HELP c_total a count\n# TYPE c_total counter\nc_total 0\n"));
        assert!(text.contains("# TYPE g gauge\ng 0\n"));
        assert!(text.contains("v_total{kind=\"read\"} 0"));
        assert!(text.contains("v_total{kind=\"write\"} 0"));
    }
}
