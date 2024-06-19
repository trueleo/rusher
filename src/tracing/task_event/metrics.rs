use std::{
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};

use atomic::Atomic;
use ordered_float::OrderedFloat;
use tdigest::TDigest;

use super::Value;

#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum MetricType {
    Counter,
    Gauge,
    Histogram,
}

#[allow(clippy::to_string_trait_impl)]
impl ToString for MetricType {
    fn to_string(&self) -> String {
        match self {
            MetricType::Counter => "counter".to_string(),
            MetricType::Gauge => "gauge".to_string(),
            MetricType::Histogram => "histogram".to_string(),
        }
    }
}

impl FromStr for MetricType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "counter" => Ok(Self::Counter),
            "gauge" => Ok(Self::Gauge),
            "histogram" => Ok(Self::Histogram),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum MetricValue {
    Counter(u64),
    GaugeF64(f64),
    GaugeI64(i64),
    GaugeU64(u64),
    GaugeDuration(Duration),
    /// histogram values ((p50, p90, p95, p99), sum)
    Histogram(((f64, f64, f64, f64), f64)),
    DurationHistogram(((Duration, Duration, Duration, Duration), Duration)),
}

#[allow(clippy::to_string_trait_impl)]
impl ToString for MetricValue {
    fn to_string(&self) -> String {
        match self {
            MetricValue::Counter(x) => x.to_string(),
            MetricValue::GaugeF64(x) => x.to_string(),
            MetricValue::GaugeI64(x) => x.to_string(),
            MetricValue::GaugeU64(x) => x.to_string(),
            MetricValue::GaugeDuration(x) => format!("{:.2?}", x),
            MetricValue::Histogram(x) => format!("{:.2?}", x),
            MetricValue::DurationHistogram(x) => format!("{:.2?}", x),
        }
    }
}

impl MetricValue {
    pub fn min_gauge<'a>(&'a self, other: &'a Self) -> &'a Self {
        match (self, other) {
            (&Self::GaugeF64(x), &Self::GaugeF64(y)) => {
                if x < y {
                    self
                } else {
                    other
                }
            }
            (&Self::GaugeU64(x), &Self::GaugeU64(y)) => {
                if x < y {
                    self
                } else {
                    other
                }
            }
            (&Self::GaugeI64(x), &Self::GaugeI64(y)) => {
                if x < y {
                    self
                } else {
                    other
                }
            }
            (&Self::GaugeDuration(x), &Self::GaugeDuration(y)) => {
                if x < y {
                    self
                } else {
                    other
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn max_gauge<'a>(&'a self, other: &'a Self) -> &'a Self {
        match (self, other) {
            (&Self::GaugeF64(x), &Self::GaugeF64(y)) => {
                if x > y {
                    self
                } else {
                    other
                }
            }
            (&Self::GaugeU64(x), &Self::GaugeU64(y)) => {
                if x > y {
                    self
                } else {
                    other
                }
            }
            (&Self::GaugeI64(x), &Self::GaugeI64(y)) => {
                if x > y {
                    self
                } else {
                    other
                }
            }
            (&Self::GaugeDuration(x), &Self::GaugeDuration(y)) => {
                if x > y {
                    self
                } else {
                    other
                }
            }
            _ => unreachable!(),
        }
    }
    pub fn mid<'a>(&'a self, other: &'a Self) -> Self {
        match (self, other) {
            (&Self::GaugeF64(x), &Self::GaugeF64(y)) => Self::GaugeF64((x + y) / 2.),
            (&Self::GaugeU64(x), &Self::GaugeU64(y)) => Self::GaugeU64((x + y) / 2),
            (&Self::GaugeI64(x), &Self::GaugeI64(y)) => Self::GaugeI64((x + y) / 2),
            (&Self::GaugeDuration(x), &Self::GaugeDuration(y)) => Self::GaugeDuration((x + y) / 2),
            _ => unreachable!(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum Metric {
    Counter(Counter),
    GaugeF64(Gauge<f64>),
    GaugeI64(Gauge<i64>),
    GaugeU64(Gauge<u64>),
    GaugeDuration((Gauge<u64>, Gauge<u32>)),
    Histogram(Histogram),
    Duration(Histogram),
}

impl Metric {
    pub fn new(ty: MetricType, value: &Value) -> Self {
        match (ty, value) {
            (MetricType::Counter, Value::UnsignedNumber(_)) => Self::Counter(Counter::new()),
            (MetricType::Gauge, Value::Float(_)) => Self::GaugeF64(Gauge::new()),
            (MetricType::Gauge, Value::Number(_)) => Self::GaugeI64(Gauge::new()),
            (MetricType::Gauge, Value::UnsignedNumber(_)) => Self::GaugeU64(Gauge::new()),
            (MetricType::Gauge, Value::Duration(_)) => {
                Self::GaugeDuration((Gauge::new(), Gauge::new()))
            }
            (MetricType::Histogram, Value::Float(_)) => Self::Histogram(Histogram::new()),
            (MetricType::Histogram, Value::Duration(_)) => Self::Duration(Histogram::new()),
            _ => panic!("Unsupported value type for metric"),
        }
    }

    pub(crate) fn update(&self, value: Value) {
        match (self, value) {
            (Metric::Counter(x), Value::UnsignedNumber(val)) => x.add(val),
            (Metric::GaugeF64(x), Value::Float(f)) => x.set(f.0),
            (Metric::GaugeI64(x), Value::Number(f)) => x.set(f),
            (Metric::GaugeU64(x), Value::UnsignedNumber(f)) => x.set(f),
            (Metric::GaugeDuration((sec, nanos)), Value::Duration(f)) => {
                sec.set(f.as_secs());
                nanos.set(f.subsec_nanos())
            }
            (Metric::Histogram(x), Value::Float(val)) => x.observe(val.0),
            (Metric::Duration(x), Value::Duration(f)) => {
                let val = f.as_nanos() as u64;
                x.observe(val as f64)
            }
            _ => {}
        }
    }

    pub fn value(&self) -> MetricValue {
        match self {
            Metric::Counter(x) => MetricValue::Counter(x.get()),
            Metric::GaugeF64(x) => MetricValue::GaugeF64(x.get()),
            Metric::GaugeI64(x) => MetricValue::GaugeI64(x.get()),
            Metric::GaugeU64(x) => MetricValue::GaugeU64(x.get()),
            Metric::GaugeDuration(x) => {
                MetricValue::GaugeDuration(Duration::new(x.0.get(), x.1.get()))
            }
            Metric::Histogram(x) => MetricValue::Histogram((x.get_percentiles(), x.get_sum())),
            Metric::Duration(x) => {
                let f = |f: f64| -> u64 {
                    if f.is_nan() {
                        return 0;
                    }
                    unsafe { f.to_int_unchecked() }
                };
                let (p50, p90, p95, p99) = x.get_percentiles();
                let p50 = Duration::from_nanos(f(p50));
                let p90 = Duration::from_nanos(f(p90));
                let p95 = Duration::from_nanos(f(p95));
                let p99 = Duration::from_nanos(f(p99));
                MetricValue::DurationHistogram((
                    (p50, p90, p95, p99),
                    Duration::from_nanos(f(x.get_sum())),
                ))
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct Counter {
    pub(crate) value: AtomicU64,
}

impl Counter {
    pub(crate) fn new() -> Self {
        Counter {
            value: AtomicU64::new(0),
        }
    }

    pub(crate) fn add(&self, amount: u64) {
        self.value.fetch_add(amount, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub(crate) struct Gauge<T: bytemuck::NoUninit> {
    pub(crate) value: Atomic<T>,
}

impl<T: bytemuck::NoUninit + Default> Gauge<T> {
    pub(crate) fn new() -> Self {
        Gauge {
            value: Atomic::new(T::default()),
        }
    }

    pub(crate) fn set(&self, value: T) {
        self.value.swap(value, Ordering::Relaxed);
    }

    pub(crate) fn get(&self) -> T {
        self.value.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub(crate) struct Histogram {
    inner: Mutex<(Option<TDigest>, Vec<OrderedFloat<f64>>, f64)>,
}

impl Histogram {
    fn new() -> Self {
        Self {
            inner: Mutex::new((None, Vec::default(), 0.)),
        }
    }

    fn observe(&self, value: f64) {
        let mut inner = self.inner.lock().unwrap();
        inner.1.push(OrderedFloat(value));
        if inner.1.len() >= 4096 {
            let values = std::mem::take(&mut inner.1);
            let values = values.into_iter().map(|x| x.0).collect();
            if let Some(tdigest) = inner.0.as_mut() {
                tdigest.merge_unsorted(values);
            } else {
                let tdigest = TDigest::default();
                tdigest.merge_unsorted(values);
                inner.0 = Some(tdigest)
            }
        }
        inner.2 += value;
    }

    fn get_percentile(&self, u: usize, l: usize) -> f64 {
        let mut lock = self.inner.lock().unwrap();
        if let Some(tdigest) = &lock.0 {
            let quantile = u as f64 / l as f64;
            tdigest.estimate_quantile(quantile)
        } else {
            let index = (lock.1.len() * u) / l;
            lock.1.sort_unstable();
            lock.1[index].0
        }
    }

    fn get_percentiles(&self) -> (f64, f64, f64, f64) {
        (
            self.get_percentile(1, 2),
            self.get_percentile(9, 10),
            self.get_percentile(95, 100),
            self.get_percentile(99, 100),
        )
    }

    fn get_sum(&self) -> f64 {
        self.inner.lock().unwrap().2
    }
}
