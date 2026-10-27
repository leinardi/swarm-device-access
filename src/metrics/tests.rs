/*
 * Copyright 2026 Roberto Leinardi.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// Adapted from swarm-scheduler-exporter/src/metrics/tests.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

use std::collections::BTreeMap;
use std::sync::Arc;

use super::text::{self, push_float};
use super::{
    Collector, Counter, CounterVec, DEF_BUCKETS, Desc, Family, Gauge, GaugeFunc, GaugeVec,
    Histogram, Kind, Registry, Value,
};

/// A collector of two gauge families with no series.
struct TwoGauges(Vec<Arc<Desc>>);

impl Collector for TwoGauges {
    fn descs(&self) -> Vec<Arc<Desc>> {
        self.0.clone()
    }

    fn collect(&self, _families: &mut Vec<Family>) {}
}

/// Go's `MustRegister` panics on a second registration of a name; `register` returns an error
/// instead, which no caller can reach because each family is registered once at startup, so the
/// difference is not observable.
#[test]
fn registry_registers_each_name_once() {
    let registry = Registry::new();
    let gauge = Arc::new(Gauge::new(
        Desc::new("a_gauge", "A gauge.", Kind::Gauge, &[]).expect("desc"),
    ));
    registry
        .register(Arc::clone(&gauge) as Arc<dyn Collector>)
        .expect("first registration");
    let error = registry
        .register(Arc::clone(&gauge) as Arc<dyn Collector>)
        .expect_err("second registration");
    assert_eq!(
        error.to_string(),
        "duplicate metrics collector registration attempted: \"a_gauge\" is already registered"
    );
    // Another collector with one name taken is refused whole.
    let both = Arc::new(TwoGauges(vec![
        Desc::new("b_gauge", "B.", Kind::Gauge, &[]).expect("desc"),
        Desc::new("a_gauge", "A again.", Kind::Gauge, &[]).expect("desc"),
    ]));
    assert!(registry.register(both).is_err());
    let other = Arc::new(Gauge::new(
        Desc::new("b_gauge", "B.", Kind::Gauge, &[]).expect("desc"),
    ));
    registry
        .register(other)
        .expect("b_gauge was not registered by the refused collector");
}

/// The Prometheus Go client's `NewDesc` and `NewHistogram` checks, as errors.
#[test]
fn desc_refuses_what_the_go_client_refuses() {
    let cases: [(&str, Kind, &[&str], &str); 6] = [
        (
            "1abc",
            Kind::Gauge,
            &[],
            "\"1abc\" is not a valid metric name",
        ),
        (
            "a-b",
            Kind::Gauge,
            &[],
            "\"a-b\" is not a valid metric name",
        ),
        (
            "abc",
            Kind::Gauge,
            &["__x"],
            "\"__x\" is not a valid label name for metric \"abc\"",
        ),
        (
            "abc",
            Kind::Gauge,
            &["a:b"],
            "\"a:b\" is not a valid label name for metric \"abc\"",
        ),
        (
            "abc",
            Kind::Gauge,
            &["x", "x"],
            "duplicate label names in constant and variable labels for metric \"abc\"",
        ),
        (
            "abc",
            Kind::Histogram,
            &["le"],
            "\"le\" is not allowed as label name in histograms (metric \"abc\")",
        ),
    ];
    for (name, kind, labels, want) in cases {
        assert_eq!(
            Desc::new(name, "h", kind, labels)
                .expect_err(name)
                .to_string(),
            want
        );
    }
    assert!(Desc::new("a:b_c", "h", Kind::Gauge, &["le", "_x"]).is_ok());
}

/// The family types behave as the Prometheus Go client's: a vec creates a child on first use and
/// deletes it, a counter ignores a negative add (the Prometheus Go client panics there; the
/// daemon never adds a negative), a histogram counts an observation in the first bucket at or
/// above it, and a gauge func is evaluated at every gather.
#[test]
fn families_behave_as_the_go_client() {
    let vec = GaugeVec::new(Desc::new("v", "h", Kind::Gauge, &["a"]).expect("desc"));
    vec.set(&["x"], 2.0).expect("set");
    assert_eq!(vec.get(&["x"]), Some(2.0));
    assert!(vec.set(&["x", "y"], 1.0).is_err());
    assert!(vec.delete(&["x"]));
    assert!(!vec.delete(&["x"]));
    assert_eq!(vec.get(&["x"]), None);

    let gauge = Gauge::new(Desc::new("g", "h", Kind::Gauge, &[]).expect("desc"));
    gauge.set(1.5);
    gauge.add(-0.5);
    assert!((gauge.get() - 1.0).abs() < f64::EPSILON);

    let counter = Counter::new(Desc::new("c", "h", Kind::Counter, &[]).expect("desc"));
    counter.inc();
    counter.add(2.0);
    counter.add(-5.0);
    assert!((counter.get() - 3.0).abs() < f64::EPSILON);

    let counters = CounterVec::new(Desc::new("cv", "h", Kind::Counter, &["a"]).expect("desc"));
    counters.add(&["x"], 0.0).expect("add");
    assert_eq!(counters.get(&["x"]), Some(0.0));
    counters.inc(&["x"]).expect("inc");
    assert_eq!(counters.get(&["x"]), Some(1.0));
    assert!(counters.inc(&[]).is_err());

    let histogram = Histogram::new(
        Desc::new("h", "h", Kind::Histogram, &[]).expect("desc"),
        &[1.0, 2.0, f64::INFINITY],
    );
    for value in [0.5, 1.0, 1.5, 3.0, f64::NAN] {
        histogram.observe(value);
    }
    let mut families = Vec::new();
    histogram.collect(&mut families);
    let Value::Histogram {
        buckets,
        sum,
        count,
    } = &families[0].series[0].value
    else {
        panic!("not a histogram");
    };
    assert_eq!(buckets, &[(1.0, 2), (2.0, 3)]);
    assert!(sum.is_nan());
    assert_eq!(*count, 5);

    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let in_func = Arc::clone(&calls);
    let func = Arc::new(GaugeFunc::new(
        Desc::new("f", "h", Kind::Gauge, &[]).expect("desc"),
        move || f64::from(in_func.fetch_add(1, std::sync::atomic::Ordering::SeqCst)),
    ));
    let registry = Registry::new();
    registry.register(func).expect("register");
    assert_eq!(registry.gather()[0].series[0].value, Value::Scalar(0.0));
    assert_eq!(registry.gather()[0].series[0].value, Value::Scalar(1.0));
}

/// swarm-scheduler-exporter's S2 family set (`gotools/cmd/families`), encoded byte for byte as the
/// Prometheus Go client v1.24.1 and expfmt (common v0.71.0, whose text encoder is the same file as
/// v0.70.1's) wrote it: families by name, the empty vec dropped, label
/// pairs by name whatever their declared order, series by label values, help and label escaping,
/// and the histogram with `le` written by the float formatter.
#[test]
fn encoding_matches_go_on_the_s2_family_set() {
    let registry = Registry::new();
    let states = Arc::new(GaugeVec::new(
        Desc::new(
            "zz_state",
            "Help with \\ backslash and\nnewline and \"quotes\".",
            Kind::Gauge,
            &["zone", "name"],
        )
        .expect("desc"),
    ));
    let counter = Arc::new(Counter::new(
        Desc::new("aa_events_total", "A plain counter.", Kind::Counter, &[]).expect("desc"),
    ));
    let requests = Arc::new(CounterVec::new(
        Desc::new(
            "mm_requests_total",
            "Requests by code.",
            Kind::Counter,
            &["method", "code"],
        )
        .expect("desc"),
    ));
    let histogram = Arc::new(Histogram::new(
        Desc::new("hh_duration_seconds", "Durations.", Kind::Histogram, &[]).expect("desc"),
        &DEF_BUCKETS,
    ));
    let timestamp = Arc::new(Gauge::new(
        Desc::new("tt_timestamp_seconds", "Unix time.", Kind::Gauge, &[]).expect("desc"),
    ));
    let empty = Arc::new(GaugeVec::new(
        Desc::new("ee_empty", "Vec with no children.", Kind::Gauge, &["x"]).expect("desc"),
    ));
    let collectors: [Arc<dyn Collector>; 6] = [
        Arc::clone(&states) as Arc<dyn Collector>,
        Arc::clone(&counter) as Arc<dyn Collector>,
        Arc::clone(&requests) as Arc<dyn Collector>,
        Arc::clone(&histogram) as Arc<dyn Collector>,
        Arc::clone(&timestamp) as Arc<dyn Collector>,
        empty,
    ];
    for collector in collectors {
        registry.register(collector).expect("register");
    }

    states.set(&["z2", "b"], 1.0).expect("set");
    states.set(&["z1", "a\"b\\c\nd"], 0.5).expect("set");
    states.set(&["z1", "b"], 1_234_567.0).expect("set");
    states.set(&["z1", "a"], -1.0).expect("set");
    states.set(&["", "a"], 0.0).expect("set");
    states.set(&["z1", "ä ünïcode"], 1e-5).expect("set");
    counter.add(42.0);
    requests.inc(&["POST", "500"]).expect("inc");
    requests.add(&["GET", "200"], 10.0).expect("add");
    requests.add(&["GET", "500"], 2.0).expect("add");
    requests.add(&["GET", "1000"], 3.0).expect("add");
    for value in [0.003, 0.7, 12.0, 0.005] {
        histogram.observe(value);
    }
    timestamp.set(1.729_771_234_123_456e9);

    let mut out = String::new();
    text::encode(&mut out, &registry.gather());
    assert_eq!(out, GO_FAMILIES);
}

/// S2's `families/go.txt`.
const GO_FAMILIES: &str = r#"# HELP aa_events_total A plain counter.
# TYPE aa_events_total counter
aa_events_total 42
# HELP hh_duration_seconds Durations.
# TYPE hh_duration_seconds histogram
hh_duration_seconds_bucket{le="0.005"} 2
hh_duration_seconds_bucket{le="0.01"} 2
hh_duration_seconds_bucket{le="0.025"} 2
hh_duration_seconds_bucket{le="0.05"} 2
hh_duration_seconds_bucket{le="0.1"} 2
hh_duration_seconds_bucket{le="0.25"} 2
hh_duration_seconds_bucket{le="0.5"} 2
hh_duration_seconds_bucket{le="1"} 3
hh_duration_seconds_bucket{le="2.5"} 3
hh_duration_seconds_bucket{le="5"} 3
hh_duration_seconds_bucket{le="10"} 3
hh_duration_seconds_bucket{le="+Inf"} 4
hh_duration_seconds_sum 12.708
hh_duration_seconds_count 4
# HELP mm_requests_total Requests by code.
# TYPE mm_requests_total counter
mm_requests_total{code="1000",method="GET"} 3
mm_requests_total{code="200",method="GET"} 10
mm_requests_total{code="500",method="GET"} 2
mm_requests_total{code="500",method="POST"} 1
# HELP tt_timestamp_seconds Unix time.
# TYPE tt_timestamp_seconds gauge
tt_timestamp_seconds 1.729771234123456e+09
# HELP zz_state Help with \\ backslash and\nnewline and "quotes".
# TYPE zz_state gauge
zz_state{name="a",zone=""} 0
zz_state{name="a",zone="z1"} -1
zz_state{name="a\"b\\c\nd",zone="z1"} 0.5
zz_state{name="b",zone="z1"} 1.234567e+06
zz_state{name="b",zone="z2"} 1
zz_state{name="ä ünïcode",zone="z1"} 1e-05
"#;

/// Series sort by their label values in label-name order, compared bytewise, as the Prometheus
/// Go client's `metricSorter` does, whatever order the labels were declared in.
#[test]
fn gather_sorts_series_by_label_values() {
    let vec = Arc::new(GaugeVec::new(
        Desc::new("s", "h", Kind::Gauge, &["b", "a"]).expect("desc"),
    ));
    for (second, first) in [("2", "1"), ("1", "2"), ("1", "10"), ("0", "1")] {
        vec.set(&[second, first], 1.0).expect("set");
    }
    let registry = Registry::new();
    registry.register(vec).expect("register");
    let order: Vec<Vec<(String, String)>> = registry.gather()[0]
        .series
        .iter()
        .map(|series| series.labels.clone())
        .collect();
    let pairs = |first: &str, second: &str| {
        vec![
            ("a".to_owned(), first.to_owned()),
            ("b".to_owned(), second.to_owned()),
        ]
    };
    assert_eq!(
        order,
        vec![
            pairs("1", "0"),
            pairs("1", "2"),
            pairs("10", "1"),
            pairs("2", "1")
        ]
    );
}

/// expfmt's `writeFloat` on the 34 edge values of swarm-scheduler-exporter's S2
/// (`floats/go-corpus.tsv`, column 3, as Go printed them): the bits, Go's text, and the input as
/// the Go program wrote it.
#[test]
fn write_float_matches_go_on_the_s2_corpus() {
    const GO_WRITE_FLOAT: [(u64, &str); 34] = [
        (0x0000_0000_0000_0000, "0"),                       // 0
        (0x8000_0000_0000_0000, "0"),                       // -0
        (0x3ff0_0000_0000_0000, "1"),                       // 1
        (0xbff0_0000_0000_0000, "-1"),                      // -1
        (0x7ff8_0000_0000_0001, "NaN"),                     // NaN
        (0x7ff0_0000_0000_0000, "+Inf"),                    // +Inf
        (0xfff0_0000_0000_0000, "-Inf"),                    // -Inf
        (0x444b_1ae4_d6e2_ef50, "1e+21"),                   // 1e21
        (0x4415_af1d_78b5_8c40, "1e+20"),                   // 1e20
        (0x40fe_2400_0000_0000, "123456"),                  // 123456
        (0x4132_d687_0000_0000, "1.234567e+06"),            // 1234567
        (0x3f1a_36e2_eb1c_432d, "0.0001"),                  // 1e-4
        (0x3ee4_f8b5_88e3_68f1, "1e-05"),                   // 1e-5
        (0x3fd3_3333_3333_3333, "0.3"),                     // 0.1+0.2
        (0x41d9_c68d_b880_0000, "1.729771234e+09"),         // 1.729771234e9
        (0x41d9_c68d_b887_e6b7, "1.7297712341234567e+09"),  // 1729771234.123456789
        (0x41d9_c68d_b887_e6b7, "1.7297712341234567e+09"),  // unixnano/1e9
        (0x7fef_ffff_ffff_ffff, "1.7976931348623157e+308"), // MaxFloat64
        (0x0000_0000_0000_0001, "5e-324"),                  // SmallestNonzero
        (0x4004_0000_0000_0000, "2.5"),                     // 2.5
        (0x3f74_7ae1_47ae_147b, "0.005"),                   // 0.005
        (0x412e_8480_0000_0000, "1e+06"),                   // 1e6
        (0x40f8_6a00_0000_0000, "100000"),                  // 100000
        (0x412e_847e_0000_0000, "999999"),                  // 999999
        (0x3f20_2c9d_edbc_309d, "0.0001234"),               // 0.0001234
        (0xbe90_c6f7_a0b5_ed8d, "-2.5e-07"),                // -2.5e-7
        (0x4340_0000_0000_0000, "9.007199254740992e+15"),   // 2^53
        (0x0000_0000_0000_0001, "5e-324"),                  // 5e-324
        (0x7fef_ffff_ffff_ffff, "1.7976931348623157e+308"), // 1.7976931348623157e308
        (0x3fd0_0000_0000_0000, "0.25"),                    // 0.25
        (0x4024_0000_0000_0000, "10"),                      // 10
        (0x4008_0000_0000_0000, "3"),                       // 3
        (0x7ff8_0000_0000_0001, "NaN"),                     // NaN-payload
        (0xfff8_0000_0000_0000, "NaN"),                     // -NaN
    ];
    for (bits, want) in GO_WRITE_FLOAT {
        let mut got = String::new();
        push_float(&mut got, f64::from_bits(bits));
        assert_eq!(got, want, "{bits:016x}");
    }
}

/// The Prometheus Go client's `With(prometheus.Labels)` and `Delete(labels)`: by label name,
/// refusing a map that does not name exactly the family's labels.
#[test]
fn gauge_vec_by_label_map() {
    let vec = GaugeVec::new(Desc::new("by_name", "help", Kind::Gauge, &["b", "a"]).unwrap());
    let labels = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|&(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    };
    vec.set_with(&labels(&[("a", "1"), ("b", "2")]), 7.0)
        .unwrap();
    assert_eq!(
        vec.get(&["2", "1"]),
        Some(7.0),
        "values in label-name order"
    );
    assert_eq!(vec.get_with(&labels(&[("b", "2"), ("a", "1")])), Some(7.0));
    let missing = vec
        .set_with(&labels(&[("a", "1"), ("c", "2")]), 1.0)
        .unwrap_err();
    assert_eq!(
        missing.to_string(),
        "label name \"b\" missing in label map for by_name"
    );
    let extra = vec
        .set_with(&labels(&[("a", "1"), ("b", "2"), ("c", "3")]), 1.0)
        .unwrap_err();
    assert_eq!(
        extra.to_string(),
        "inconsistent label cardinality for by_name: expected 2 label values but got 3"
    );
    assert!(!vec.delete_with(&labels(&[("a", "1")])));
    assert!(vec.delete_with(&labels(&[("a", "1"), ("b", "2")])));
    assert_eq!(vec.get_with(&labels(&[("a", "1"), ("b", "2")])), None);
}

/// The described names include a vec with no series, which a gather leaves out.
#[test]
fn described_names_include_families_without_series() {
    let registry = Registry::new();
    let empty = CounterVec::new(Desc::new("empty_total", "Empty.", Kind::Counter, &["a"]).unwrap());
    registry.register(Arc::new(empty)).unwrap();
    assert!(registry.gather().is_empty());
    assert_eq!(
        registry.described_names().into_iter().collect::<Vec<_>>(),
        ["empty_total"]
    );
}
