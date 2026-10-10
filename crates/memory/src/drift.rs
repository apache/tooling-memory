//! Detecting unadvertised vendor change, cut to what this binary can do now. The paper
//! pins everything on our side so that any drift is attributable to the vendor, then
//! watches cheap signals against a noise floor. Here a pinned probe set is run through
//! the egress against one destination and model, each run appended to
//! `state/drift/<destination>/<model>.jsonl` with what we pinned (the amem version, the
//! probe set's digest, the request shape), and a check compares the latest run with a
//! baseline: first whether our side changed, then detector by detector.
//!
//! Detectors here: input tokens for identical prompts (tokenizer or routing), the
//! reported model and `system_fingerprint` (version repoints), the temperature-0 answer
//! flip rate above the baseline's own within-run flip rate (version swaps), format-probe
//! passes and refusal-probe refusals (safety filters, formatting), and median latency,
//! reported but no alarm on its own. Logprob divergence, frozen-context replay,
//! change-point detection over many runs and fleet metrics are not here.

use clap::Subcommand;
use dm_egress::Egress;
use dm_sandbox::{AuthMode, Harness};
use dm_server::Memory;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// Every probe is asked at temperature 0: the baseline's own repeat disagreement is
/// what is left of the noise.
pub const TEMPERATURE: f64 = 0.0;

/// `amem drift`'s subcommands.
#[derive(Subcommand)]
pub enum DriftOp {
    /// Run a pinned probe set through the egress against one destination and model,
    /// each probe --repeat times, and append the run to
    /// state/drift/<destination>/<model>.jsonl under the root.
    Probe {
        #[arg(long, value_enum, default_value = "key")]
        auth: crate::AuthArg,
        /// The destination; openai by default, llmao with --auth gateway.
        #[arg(long)]
        provider: Option<String>,
        /// The model, as the destination names it: the alias whose drift is watched.
        #[arg(long)]
        model: String,
        /// The probe set, TOML; see probes/drift.toml.
        #[arg(long, default_value = "probes/drift.toml")]
        set: PathBuf,
        /// Times each probe is asked, so the run carries its own noise floor.
        #[arg(long, default_value_t = 3)]
        repeat: usize,
    },
    /// Compare the latest run of a destination and model with a baseline run, as JSON:
    /// first whether our pinned side changed, then each detector with its evidence.
    Check {
        /// The destination the runs were made against.
        #[arg(long)]
        provider: String,
        /// The model whose latest run is checked.
        #[arg(long)]
        model: String,
        /// The baseline run: `first`, `last` (the newest before the one checked), or a
        /// run's index in the file, from 0.
        #[arg(long, default_value = "first")]
        baseline: String,
        /// Take the baseline from another model's runs at the same destination: how a
        /// repoint behind a fixed alias looks, when the destination will not do one.
        #[arg(long)]
        baseline_model: Option<String>,
        /// How far the answer flip rate may rise above the baseline's own before it
        /// counts.
        #[arg(long, default_value_t = 0.25)]
        margin: f64,
        /// Median latency moving by more than this factor either way is reported.
        #[arg(long, default_value_t = 2.0)]
        latency_factor: f64,
        /// File confirmed drift as one task for the owner.
        #[arg(long)]
        file: bool,
        /// The domain the task is filed in; the root's default otherwise.
        #[arg(long)]
        domain: Option<String>,
    },
}

/// Run a drift subcommand.
///
/// # Errors
///
/// When the probe set or the runs cannot be read, the destination cannot be prepared,
/// a run cannot be written, or a task cannot be filed.
pub async fn run(root: &Path, op: DriftOp) -> Fallible<()> {
    match op {
        DriftOp::Probe {
            auth,
            provider,
            model,
            set,
            repeat,
        } => probe_cmd(root, auth.into(), provider, &model, &set, repeat).await,
        DriftOp::Check {
            provider,
            model,
            baseline,
            baseline_model,
            margin,
            latency_factor,
            file,
            domain,
        } => {
            let ask = CheckArgs {
                destination: provider,
                model,
                baseline,
                baseline_model,
                params: Params {
                    margin,
                    latency_factor,
                },
                file,
                domain,
            };
            check_cmd(root, &ask).await
        }
    }
}

// ---- the probe set

/// What a probe looks for.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A fixed short factual or formatting prompt; its answer's digest is compared.
    Fingerprint,
    /// Asks for an exact output shape; whether the answer matched is recorded.
    Format,
    /// A benign borderline request; whether the answer is a refusal is recorded.
    Refusal,
}

impl Kind {
    /// Output tokens a probe of this kind may take, unless the probe says otherwise.
    /// The answers are a few words, but a reasoning model spends most of the limit
    /// thinking first, and a limit it runs into leaves an empty answer: the gateway's
    /// qwen3.8-27b took 64 tokens to name three planets and stopped before answering.
    #[must_use]
    pub fn max_tokens(self) -> u32 {
        match self {
            Kind::Fingerprint => 256,
            Kind::Format | Kind::Refusal => 512,
        }
    }
}

/// One probe.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Probe {
    pub id: String,
    pub kind: Kind,
    pub prompt: String,
    /// For a format probe: the answer is a JSON object with exactly these keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect_keys: Vec<String>,
    /// For a format probe: the answer, trimmed, is exactly this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
    /// Output tokens; the kind's default otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl Probe {
    /// The output token limit this probe is asked with.
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.max_tokens.unwrap_or_else(|| self.kind.max_tokens())
    }

    /// Whether an answer has the shape a format probe asks for.
    #[must_use]
    pub fn matches(&self, answer: &str) -> bool {
        let t = unfenced(answer);
        if let Some(e) = &self.expect {
            return t == e.trim();
        }
        if self.expect_keys.is_empty() {
            return false;
        }
        let Ok(Value::Object(o)) = serde_json::from_str::<Value>(t) else {
            return false;
        };
        let want: BTreeSet<&str> = self.expect_keys.iter().map(String::as_str).collect();
        let got: BTreeSet<&str> = o.keys().map(String::as_str).collect();
        want == got
    }
}

/// An answer with surrounding whitespace and one code fence taken off. A fence is not
/// a match of an exact shape, but it is how models wrap JSON, and the key set is what a
/// format probe watches.
fn unfenced(answer: &str) -> &str {
    let t = answer.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

/// Phrases that open a refusal, matched case-blind near the start of the answer.
const REFUSAL_MARKERS: &[&str] = &[
    "i can't",
    "i cannot",
    "i can not",
    "i won't",
    "i will not",
    "i'm sorry",
    "i am sorry",
    "i apologize",
    "i'm unable",
    "i am unable",
    "i'm not able",
    "i am not able",
    "i must decline",
    "can't help with",
    "cannot help with",
    "can't assist",
    "cannot assist",
    "not able to help",
    "against my guidelines",
];

/// Whether an answer is a refusal, by the marker list, within its first 200 characters.
#[must_use]
pub fn refused(answer: &str) -> bool {
    let head: String = answer
        .trim()
        .chars()
        .take(200)
        .collect::<String>()
        .to_lowercase()
        .replace('\u{2019}', "'");
    REFUSAL_MARKERS.iter().any(|m| head.contains(m))
}

/// A probe set: the `[[probe]]` tables of a TOML file.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProbeSet {
    pub probe: Vec<Probe>,
}

impl ProbeSet {
    /// A probe set out of its TOML.
    ///
    /// # Errors
    ///
    /// When the TOML does not parse, a probe id repeats, or a format probe names no
    /// shape to expect.
    pub fn parse(text: &str) -> Fallible<ProbeSet> {
        let set: ProbeSet = toml::from_str(text)?;
        let mut ids = BTreeSet::new();
        for p in &set.probe {
            if !ids.insert(p.id.as_str()) {
                return Err(format!("probe id {} repeats", p.id).into());
            }
            if p.kind == Kind::Format && p.expect.is_none() && p.expect_keys.is_empty() {
                return Err(
                    format!("format probe {} names neither expect nor expect_keys", p.id).into(),
                );
            }
        }
        if set.probe.is_empty() {
            return Err("the probe set has no [[probe]]".into());
        }
        Ok(set)
    }

    /// A digest of the probes as parsed: comments and layout are not part of it.
    #[must_use]
    pub fn digest(&self) -> String {
        dm_core::digest(&serde_json::to_string(self).unwrap_or_default())[..16].to_string()
    }
}

// ---- a run

/// The request shape every probe is sent with: part of our pinned side.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Shape {
    /// The API path the destination is asked on.
    pub path: String,
    pub temperature: f64,
    /// Output tokens per probe id.
    pub max_tokens: BTreeMap<String, u32>,
}

/// What our side pinned for a run: if any of it differs between two runs, a change
/// between them is not attributable to the vendor.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Pinned {
    pub amem_version: String,
    pub probe_set_digest: String,
    pub request: Shape,
}

/// One answer to one probe.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Sample {
    /// A digest of the answer text, as the destination returned it.
    pub digest: String,
    /// The answer's length in characters.
    pub len: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    pub latency_ms: u64,
    /// The model the destination says answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// A format probe's answer had the asked shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<bool>,
    /// A refusal probe's answer was a refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<bool>,
    /// The call failed, three times; nothing else in the sample is the destination's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One probe's answers in a run.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProbeRun {
    pub id: String,
    pub kind: Kind,
    pub prompt_digest: String,
    pub samples: Vec<Sample>,
}

/// One run of the probe set: one line of the drift file.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Run {
    pub at: String,
    pub destination: String,
    pub model: String,
    pub repeat: usize,
    pub probe_set: String,
    pub pinned: Pinned,
    pub probes: Vec<ProbeRun>,
}

impl Sample {
    /// A sample out of a destination's response.
    fn of(destination: &str, probe: &Probe, v: &Value, latency_ms: u64) -> Sample {
        let text = crate::model::answer_text(destination, v);
        let usage = dm_egress::usage::from_value(v);
        let finish = v
            .pointer("/choices/0/finish_reason")
            .or_else(|| v.get("stop_reason"))
            .or_else(|| v.pointer("/candidates/0/finishReason"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Sample {
            digest: dm_core::digest(&text)[..16].to_string(),
            len: text.chars().count(),
            input_tokens: usage.as_ref().map(|u| u.input_tokens),
            output_tokens: usage.as_ref().map(|u| u.output_tokens),
            latency_ms,
            model: usage.and_then(|u| u.model),
            system_fingerprint: v
                .get("system_fingerprint")
                .and_then(Value::as_str)
                .map(str::to_string),
            finish_reason: finish,
            matched: (probe.kind == Kind::Format).then(|| probe.matches(&text)),
            refused: (probe.kind == Kind::Refusal).then(|| refused(&text)),
            error: None,
        }
    }

    fn failed(error: &str, latency_ms: u64) -> Sample {
        Sample {
            latency_ms,
            error: Some(error.chars().take(300).collect()),
            ..Sample::default()
        }
    }

    fn ok(&self) -> bool {
        self.error.is_none()
    }
}

/// Ask one probe once, with up to two retries when the call fails: a gateway may
/// route a call to a backend that does not serve the model, and say 404.
async fn ask(egress: &Egress, destination: &str, model: &str, probe: &Probe) -> Sample {
    let (path, body) = crate::model::request_sampled(
        destination,
        model,
        &probe.prompt,
        TEMPERATURE,
        probe.limit(),
    );
    let mut last = String::new();
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let result = egress.call(destination, &path, &body).await;
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(v) => return Sample::of(destination, probe, &v, ms),
            Err(e) => last = e.to_string(),
        }
    }
    Sample::failed(&last, 0)
}

/// What a run pins on our side, for a probe set at a destination.
#[must_use]
pub fn pinned_for(set: &ProbeSet, destination: &str, model: &str) -> Pinned {
    let (path, _) = crate::model::request_sampled(destination, model, "", TEMPERATURE, 0);
    Pinned {
        amem_version: env!("CARGO_PKG_VERSION").to_string(),
        probe_set_digest: set.digest(),
        request: Shape {
            path,
            temperature: TEMPERATURE,
            max_tokens: set
                .probe
                .iter()
                .map(|p| (p.id.clone(), p.limit()))
                .collect(),
        },
    }
}

/// Run every probe `repeat` times through the egress.
pub async fn probe_run(
    egress: &Egress,
    destination: &str,
    model: &str,
    set: &ProbeSet,
    set_name: &str,
    repeat: usize,
) -> Run {
    let mut probes = Vec::new();
    for p in &set.probe {
        let mut samples = Vec::new();
        for _ in 0..repeat {
            samples.push(ask(egress, destination, model, p).await);
        }
        probes.push(ProbeRun {
            id: p.id.clone(),
            kind: p.kind,
            prompt_digest: dm_core::digest(&p.prompt)[..16].to_string(),
            samples,
        });
    }
    Run {
        at: now(),
        destination: destination.to_string(),
        model: model.to_string(),
        repeat,
        probe_set: set_name.to_string(),
        pinned: pinned_for(set, destination, model),
        probes,
    }
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

// ---- the drift files

/// A name safe as one path component: a model id may carry a slash.
fn component(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Where a destination and model's runs are kept.
#[must_use]
pub fn runs_path(root: &Path, destination: &str, model: &str) -> PathBuf {
    root.join("state/drift")
        .join(component(destination))
        .join(format!("{}.jsonl", component(model)))
}

/// Append a run; returns the file and the run's index in it.
///
/// # Errors
///
/// When the file cannot be read or written.
pub fn append(root: &Path, run: &Run) -> Fallible<(PathBuf, usize)> {
    use std::io::Write;
    let path = runs_path(root, &run.destination, &run.model);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let index = load(&path).map_or(0, |r| r.len());
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{}", serde_json::to_string(run)?)?;
    Ok((path, index))
}

/// Every run in a drift file, oldest first.
///
/// # Errors
///
/// When the file cannot be read or a line is not a run.
pub fn load(path: &Path) -> Fallible<Vec<Run>> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("no runs at {}: {e}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str(l)
                .map_err(|e| format!("{} line {}: {e}", path.display(), i + 1).into())
        })
        .collect()
}

/// The index a `--baseline` names among `n` runs.
///
/// # Errors
///
/// When it names no run there.
pub fn baseline_index(spec: &str, n: usize) -> Fallible<usize> {
    if n == 0 {
        return Err("no runs to take a baseline from".into());
    }
    let i = match spec {
        "first" => 0,
        "last" => n - 1,
        s => s
            .parse::<usize>()
            .map_err(|_| format!("--baseline is first, last or a run index, not {s}"))?,
    };
    if i >= n {
        return Err(format!("--baseline {i}: there are {n} runs, indexed from 0").into());
    }
    Ok(i)
}

// ---- the comparison

/// The thresholds of a comparison.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// How far the flip rate may rise above the baseline's own before it counts.
    pub margin: f64,
    /// The factor median latency may move by, either way, before it is reported.
    pub latency_factor: f64,
}

impl Default for Params {
    fn default() -> Params {
        Params {
            margin: 0.25,
            latency_factor: 2.0,
        }
    }
}

/// One detector's finding.
#[derive(Clone, Debug, Serialize)]
pub struct Detector {
    pub name: &'static str,
    /// What a firing of it points at.
    pub catches: &'static str,
    pub fired: bool,
    /// Whether a firing counts as drift; latency is reported, not alarmed on.
    pub alarm: bool,
    pub evidence: Value,
}

/// Whether our pinned side changed between the runs.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Attribution {
    pub ours_changed: bool,
    /// Each pinned field that differs, baseline then current.
    pub differences: Vec<String>,
}

/// A comparison of a run with its baseline.
#[derive(Clone, Debug, Serialize)]
pub struct Comparison {
    pub attribution: Attribution,
    pub detectors: Vec<Detector>,
    /// Some alarm fired and our side did not change.
    pub drift: bool,
    pub verdict: String,
}

impl Comparison {
    /// The alarms that fired.
    #[must_use]
    pub fn fired(&self) -> Vec<&Detector> {
        self.detectors
            .iter()
            .filter(|d| d.alarm && d.fired)
            .collect()
    }
}

/// Compare a run with a baseline: attribution first, then each detector.
#[must_use]
pub fn compare(baseline: &Run, current: &Run, params: &Params) -> Comparison {
    let attribution = attribute(&baseline.pinned, &current.pinned);
    let detectors = vec![
        input_tokens(baseline, current),
        version(baseline, current),
        flip_rate(baseline, current, params.margin),
        format(baseline, current),
        refusals(baseline, current),
        latency(baseline, current, params.latency_factor),
    ];
    let alarmed = detectors.iter().any(|d| d.alarm && d.fired);
    let (drift, verdict) = match (alarmed, attribution.ours_changed) {
        (false, _) => (false, "no drift"),
        (true, true) => (
            false,
            "changed, but our pinned side changed too: not attributable to the vendor; take the normal evaluation path",
        ),
        (true, false) => (true, "vendor drift"),
    };
    Comparison {
        attribution,
        detectors,
        drift,
        verdict: verdict.to_string(),
    }
}

/// The pinned fields that differ.
#[must_use]
pub fn attribute(b: &Pinned, c: &Pinned) -> Attribution {
    let mut differences = Vec::new();
    if b.amem_version != c.amem_version {
        differences.push(format!(
            "amem_version: {} -> {}",
            b.amem_version, c.amem_version
        ));
    }
    if b.probe_set_digest != c.probe_set_digest {
        differences.push(format!(
            "probe_set_digest: {} -> {}",
            b.probe_set_digest, c.probe_set_digest
        ));
    }
    if b.request.path != c.request.path {
        differences.push(format!(
            "request.path: {} -> {}",
            b.request.path, c.request.path
        ));
    }
    if json!(b.request.temperature) != json!(c.request.temperature) {
        differences.push(format!(
            "request.temperature: {} -> {}",
            b.request.temperature, c.request.temperature
        ));
    }
    if b.request.max_tokens != c.request.max_tokens {
        differences.push(format!(
            "request.max_tokens: {:?} -> {:?}",
            b.request.max_tokens, c.request.max_tokens
        ));
    }
    Attribution {
        ours_changed: !differences.is_empty(),
        differences,
    }
}

/// The probes both runs asked, of a kind or of every kind, in the baseline's order.
fn pairs<'a>(b: &'a Run, c: &'a Run, kind: Option<Kind>) -> Vec<(&'a ProbeRun, &'a ProbeRun)> {
    b.probes
        .iter()
        .filter(|p| kind.is_none_or(|k| p.kind == k))
        .filter_map(|pb| {
            c.probes
                .iter()
                .find(|pc| {
                    pc.id == pb.id && pc.kind == pb.kind && pc.prompt_digest == pb.prompt_digest
                })
                .map(|pc| (pb, pc))
        })
        .collect()
}

fn ok(p: &ProbeRun) -> impl Iterator<Item = &Sample> {
    p.samples.iter().filter(|s| s.ok())
}

/// Input tokens for the same prompt are exact: any change is a tokenizer or a route.
fn input_tokens(b: &Run, c: &Run) -> Detector {
    let mut changed = Vec::new();
    let compared = pairs(b, c, None);
    for (pb, pc) in &compared {
        let tb: BTreeSet<u64> = ok(pb).filter_map(|s| s.input_tokens).collect();
        let tc: BTreeSet<u64> = ok(pc).filter_map(|s| s.input_tokens).collect();
        if !tb.is_empty() && !tc.is_empty() && tb != tc {
            changed.push(json!({ "probe": pb.id, "baseline": tb, "current": tc }));
        }
    }
    Detector {
        name: "input_tokens",
        catches: "tokenizer or routing change",
        fired: !changed.is_empty(),
        alarm: true,
        evidence: json!({ "probes_compared": compared.len(), "changed": changed }),
    }
}

/// The values a run reports, over every answered sample.
fn reported(run: &Run, field: fn(&Sample) -> Option<&String>) -> BTreeSet<String> {
    run.probes
        .iter()
        .flat_map(ok)
        .filter_map(field)
        .cloned()
        .collect()
}

/// A reported model or fingerprint the baseline never saw is a repoint.
fn version(b: &Run, c: &Run) -> Detector {
    let (bm, cm) = (
        reported(b, |s| s.model.as_ref()),
        reported(c, |s| s.model.as_ref()),
    );
    let (bf, cf) = (
        reported(b, |s| s.system_fingerprint.as_ref()),
        reported(c, |s| s.system_fingerprint.as_ref()),
    );
    let new_models: Vec<&String> = cm.difference(&bm).collect();
    let new_fps: Vec<&String> = cf.difference(&bf).collect();
    Detector {
        name: "version",
        catches: "version repoint: the reported model or system_fingerprint changed",
        fired: !new_models.is_empty() || !new_fps.is_empty(),
        alarm: true,
        evidence: json!({
            "baseline_models": bm, "current_models": cm, "new_models": new_models,
            "baseline_fingerprints": bf, "current_fingerprints": cf, "new_fingerprints": new_fps,
        }),
    }
}

/// The most frequent digest, ties broken by the digest so the choice is stable.
fn majority<'a>(digests: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for d in digests {
        *counts.entry(d).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
        .map(|(d, _)| d)
}

#[allow(clippy::cast_precision_loss)] // counts of a few dozen samples
fn ratio(n: usize, of: usize) -> f64 {
    if of == 0 {
        0.0
    } else {
        n as f64 / of as f64
    }
}

/// Tallies for the flip rate: the baseline's own disagreement and the current run's
/// disagreement with the baseline's majority.
#[derive(Default)]
struct Flips {
    floor: (usize, usize),
    current: (usize, usize),
    flipped: Vec<Value>,
}

impl Flips {
    fn add(&mut self, pb: &ProbeRun, pc: &ProbeRun) {
        let Some(maj) = majority(ok(pb).map(|s| s.digest.as_str())) else {
            return;
        };
        let own = ok(pb).filter(|s| s.digest != maj).count();
        self.floor.0 += own;
        self.floor.1 += ok(pb).count();
        let answers: Vec<&str> = ok(pc).map(|s| s.digest.as_str()).collect();
        let flips = answers.iter().filter(|d| **d != maj).count();
        self.current.0 += flips;
        self.current.1 += answers.len();
        if flips > 0 {
            let distinct: BTreeSet<&str> = answers.into_iter().collect();
            self.flipped.push(json!({
                "probe": pb.id, "flips": flips, "of": ok(pc).count(),
                "baseline_answer": maj, "current_answers": distinct,
            }));
        }
    }
}

/// Temperature-0 answers that differ from the baseline's majority, above the noise
/// floor the baseline's own repeats show.
fn flip_rate(b: &Run, c: &Run, margin: f64) -> Detector {
    let mut t = Flips::default();
    for (pb, pc) in pairs(b, c, Some(Kind::Fingerprint)) {
        t.add(pb, pc);
    }
    let floor = ratio(t.floor.0, t.floor.1);
    let rate = ratio(t.current.0, t.current.1);
    Detector {
        name: "flip_rate",
        catches: "version swap: temperature-0 answers moved off the baseline",
        fired: t.current.1 > 0 && rate > floor + margin,
        alarm: true,
        evidence: json!({
            "baseline_within_run": floor, "current_vs_baseline": rate, "margin": margin,
            "threshold": floor + margin, "samples": t.current.1, "flipped": t.flipped,
        }),
    }
}

/// How many of a probe's answered samples say yes, and of how many.
fn tally(p: &ProbeRun, field: fn(&Sample) -> Option<bool>) -> (usize, usize) {
    let v: Vec<bool> = ok(p).filter_map(field).collect();
    (v.iter().filter(|x| **x).count(), v.len())
}

fn most(t: (usize, usize)) -> bool {
    t.0 * 2 > t.1
}

/// A format probe the baseline passed by majority that the current run fails by
/// majority; the overall pass rates beside it.
fn format(b: &Run, c: &Run) -> Detector {
    let mut regressed = Vec::new();
    let (mut bp, mut cp) = ((0, 0), (0, 0));
    for (pb, pc) in pairs(b, c, Some(Kind::Format)) {
        let (tb, tc) = (tally(pb, |s| s.matched), tally(pc, |s| s.matched));
        bp = (bp.0 + tb.0, bp.1 + tb.1);
        cp = (cp.0 + tc.0, cp.1 + tc.1);
        if most(tb) && tc.1 > 0 && !most(tc) {
            regressed.push(json!({ "probe": pb.id, "baseline": format!("{}/{}", tb.0, tb.1), "current": format!("{}/{}", tc.0, tc.1) }));
        }
    }
    Detector {
        name: "format",
        catches: "formatting or safety-filter change",
        fired: !regressed.is_empty(),
        alarm: true,
        evidence: json!({
            "baseline_pass_rate": ratio(bp.0, bp.1), "current_pass_rate": ratio(cp.0, cp.1),
            "regressed": regressed,
        }),
    }
}

/// A refusal probe whose majority verdict changed, either way.
fn refusals(b: &Run, c: &Run) -> Detector {
    let mut changed = Vec::new();
    let (mut br, mut cr) = (0, 0);
    for (pb, pc) in pairs(b, c, Some(Kind::Refusal)) {
        let (tb, tc) = (tally(pb, |s| s.refused), tally(pc, |s| s.refused));
        br += tb.0;
        cr += tc.0;
        if tb.1 > 0 && tc.1 > 0 && most(tb) != most(tc) {
            changed.push(json!({ "probe": pb.id, "baseline_refused": format!("{}/{}", tb.0, tb.1), "current_refused": format!("{}/{}", tc.0, tc.1) }));
        }
    }
    Detector {
        name: "refusal",
        catches: "safety-filter change, often shipped with no model change",
        fired: !changed.is_empty(),
        alarm: true,
        evidence: json!({ "baseline_refusals": br, "current_refusals": cr, "changed": changed }),
    }
}

fn median(mut v: Vec<u64>) -> Option<u64> {
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

/// Median latency moved by more than a factor: reported, not an alarm on its own,
/// since load moves it too.
fn latency(b: &Run, c: &Run, factor: f64) -> Detector {
    let all =
        |r: &Run| -> Vec<u64> { r.probes.iter().flat_map(ok).map(|s| s.latency_ms).collect() };
    let (mb, mc) = (median(all(b)), median(all(c)));
    #[allow(clippy::cast_precision_loss)] // milliseconds
    let moved_by = match (mb, mc) {
        (Some(x), Some(y)) if x > 0 => Some(y as f64 / x as f64),
        _ => None,
    };
    let moved = moved_by.is_some_and(|r| r > factor || r * factor < 1.0);
    Detector {
        name: "latency",
        catches: "capacity or routing change; reported only",
        fired: moved,
        alarm: false,
        evidence: json!({ "baseline_median_ms": mb, "current_median_ms": mc, "ratio": moved_by, "factor": factor }),
    }
}

// ---- filing

/// The body of the owner's task for confirmed drift.
#[must_use]
pub fn task_body(destination: &str, model: &str, c: &Comparison) -> String {
    let fired = c.fired();
    let names: Vec<&str> = fired.iter().map(|d| d.name).collect();
    let evidence: BTreeMap<&str, &Value> = fired.iter().map(|d| (d.name, &d.evidence)).collect();
    format!(
        "The drift probe battery found a change on `{destination}/{model}` that our side does not explain: the amem version, the probe set and the request shape are the same in both runs.\n\n\
         Detectors fired: {}.\n\n## Evidence\n\n```json\n{}\n```\n\n## Playbook\n\n\
         1. **Freeze distillation.** Stop distilling from sessions on this model from the baseline run on, and quarantine any candidate whose evaluation spanned the change: its fitness may be the vendor's change, not the harness's.\n\
         2. **Pin or fail over.** Pin a dated snapshot if the vendor offers one, or move the work to another destination or model until this one is understood.\n\
         3. **Record.** Write a decision entry naming the change point (the two runs), the detectors and the evidence, so later readers can find what this model wrote across it.\n\
         4. **Escalate.** Ask the vendor or the gateway's operator what changed behind `{model}`, with the evidence above.\n\n\
         Rerun `amem drift probe` and `amem drift check` to see whether the change persists.",
        names.join(", "),
        serde_json::to_string_pretty(&evidence).unwrap_or_default()
    )
}

/// File confirmed drift as one task for the owner; `None` when it was filed already.
///
/// # Errors
///
/// A ledger error.
pub async fn file(
    memory: &Memory,
    domain: Option<&str>,
    destination: &str,
    model: &str,
    c: &Comparison,
) -> Fallible<Option<String>> {
    let fired = c.fired();
    let names: Vec<&str> = fired.iter().map(|d| d.name).collect();
    let findings: Vec<(&str, &Value)> = fired.iter().map(|d| (d.name, &d.evidence)).collect();
    let created = memory
        .file_own(dm_ledger::TaskSpec {
            domain: domain.unwrap_or_default().to_string(),
            title: format!(
                "Vendor drift on {destination}/{model}: {}",
                names.join(", ")
            ),
            body: task_body(destination, model, c),
            requirements: vec!["human".into()],
            depends_on: Vec::new(),
            origin: Some(format!("drift:{destination}/{model}")),
            upstream_event: Some(
                dm_core::digest(&serde_json::to_string(&findings).unwrap_or_default())[..16]
                    .to_string(),
            ),
            created_by: String::new(),
            classification: dm_core::Label::Public,
            priority: 5,
        })
        .await?;
    Ok((!created.deduplicated).then_some(created.task.id))
}

// ---- the commands

/// One probe's answers, summarised for the terminal.
fn summary(p: &ProbeRun) -> Value {
    let answers: BTreeSet<&str> = ok(p).map(|s| s.digest.as_str()).collect();
    let input: BTreeSet<u64> = ok(p).filter_map(|s| s.input_tokens).collect();
    let mut v = json!({
        "id": p.id, "kind": p.kind, "answers": answers.len(), "input_tokens": input,
        "median_ms": median(ok(p).map(|s| s.latency_ms).collect()),
        "errors": p.samples.len() - ok(p).count(),
        "cut_by_limit": ok(p).filter(|s| s.finish_reason.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("length") || f.eq_ignore_ascii_case("max_tokens"))).count(),
    });
    match p.kind {
        Kind::Format => {
            v["matched"] = json!(format!("{}/{}", tally(p, |s| s.matched).0, ok(p).count()))
        }
        Kind::Refusal => {
            v["refused"] = json!(format!("{}/{}", tally(p, |s| s.refused).0, ok(p).count()))
        }
        Kind::Fingerprint => {}
    }
    v
}

async fn probe_cmd(
    root: &Path,
    auth: AuthMode,
    provider: Option<String>,
    model: &str,
    set_path: &Path,
    repeat: usize,
) -> Fallible<()> {
    if repeat == 0 {
        return Err("--repeat is at least 1".into());
    }
    let set = ProbeSet::parse(
        &std::fs::read_to_string(set_path)
            .map_err(|e| format!("the probe set {}: {e}", set_path.display()))?,
    )?;
    if auth == AuthMode::Subscription {
        return Err("a login is the harness's own; probe with --auth gateway or key".into());
    }
    let provider = crate::checker_destination(auth, provider);
    let p = crate::prepare_launch(
        root,
        Harness::Command,
        crate::Credentials::keyed(auth, &provider),
        None,
        None,
        crate::Projected::No,
    )
    .await?;
    p.memory.disable_read_log();
    let run = probe_run(
        &p.egress,
        &p.provider,
        model,
        &set,
        &set_path.display().to_string(),
        repeat,
    )
    .await;
    let (path, index) = append(root, &run)?;
    let probes: Vec<Value> = run.probes.iter().map(summary).collect();
    crate::json(&json!({
        "file": path, "index": index, "at": run.at, "destination": run.destination,
        "model": run.model, "repeat": repeat, "pinned": run.pinned, "probes": probes,
    }))
}

/// What `amem drift check` was asked.
struct CheckArgs {
    destination: String,
    model: String,
    baseline: String,
    baseline_model: Option<String>,
    params: Params,
    file: bool,
    domain: Option<String>,
}

/// The baseline and current runs a check compares, with their indices.
fn runs_for(root: &Path, a: &CheckArgs) -> Fallible<((usize, Run), (usize, Run))> {
    let mut current_runs = load(&runs_path(root, &a.destination, &a.model))?;
    let ci = current_runs.len() - 1;
    let current = current_runs.pop().ok_or("no runs")?;
    let base_model = a.baseline_model.as_deref().unwrap_or(&a.model);
    let mut base_runs = if base_model == a.model {
        current_runs
    } else {
        load(&runs_path(root, &a.destination, base_model))?
    };
    let bi = baseline_index(&a.baseline, base_runs.len()).map_err(|e| {
        if base_model == a.model {
            format!("{e}; the latest run is the one checked, so a baseline needs an earlier one")
                .into()
        } else {
            e
        }
    })?;
    Ok(((bi, base_runs.swap_remove(bi)), (ci, current)))
}

async fn check_cmd(root: &Path, a: &CheckArgs) -> Fallible<()> {
    let ((bi, base), (ci, current)) = runs_for(root, a)?;
    let c = compare(&base, &current, &a.params);
    let errors = |r: &Run| -> usize {
        r.probes
            .iter()
            .map(|p| p.samples.len() - ok(p).count())
            .sum()
    };
    let repoint = (base.model != current.model).then(|| json!({
        "baseline_model": base.model, "model": current.model,
        "note": "the baseline is another model's run: how a repoint behind a fixed alias looks; the requested model is not compared as part of our pinned side",
    }));
    let filed = if a.file && c.drift {
        file(
            &Memory::open(root)?,
            a.domain.as_deref(),
            &a.destination,
            &a.model,
            &c,
        )
        .await?
    } else {
        None
    };
    crate::json(&json!({
        "destination": a.destination, "model": a.model,
        "baseline": { "model": base.model, "index": bi, "at": base.at, "errors": errors(&base) },
        "current": { "model": current.model, "index": ci, "at": current.at, "errors": errors(&current) },
        "simulated_repoint": repoint,
        "attribution": c.attribution, "detectors": c.detectors,
        "drift": c.drift, "verdict": c.verdict, "filed": filed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(digest: &str, input: u64) -> Sample {
        Sample {
            digest: digest.into(),
            len: 8,
            input_tokens: Some(input),
            output_tokens: Some(3),
            latency_ms: 100,
            model: Some("m".into()),
            system_fingerprint: Some("fp-1".into()),
            ..Sample::default()
        }
    }

    fn probe(id: &str, kind: Kind, samples: Vec<Sample>) -> ProbeRun {
        ProbeRun {
            id: id.into(),
            kind,
            prompt_digest: format!("p-{id}"),
            samples,
        }
    }

    fn checked(s: Sample, matched: Option<bool>, refused: Option<bool>) -> Sample {
        Sample {
            matched,
            refused,
            ..s
        }
    }

    /// A run: two fingerprints, a format probe that passes, a refusal probe answered.
    fn run(fp: [&str; 3], fp2: [&str; 3], matched: bool, refused: bool) -> Run {
        let fps = |d: [&str; 3]| d.iter().map(|d| sample(d, 20)).collect::<Vec<_>>();
        Run {
            at: "t".into(),
            destination: "llmao".into(),
            model: "m".into(),
            repeat: 3,
            probe_set: "probes/drift.toml".into(),
            pinned: Pinned {
                amem_version: "0.1.0".into(),
                probe_set_digest: "abc".into(),
                request: Shape {
                    path: "v1/chat/completions".into(),
                    temperature: 0.0,
                    max_tokens: BTreeMap::from([("a".into(), 64)]),
                },
            },
            probes: vec![
                probe("a", Kind::Fingerprint, fps(fp)),
                probe("b", Kind::Fingerprint, fps(fp2)),
                probe(
                    "f",
                    Kind::Format,
                    (0..3)
                        .map(|_| checked(sample("j", 30), Some(matched), None))
                        .collect(),
                ),
                probe(
                    "r",
                    Kind::Refusal,
                    (0..3)
                        .map(|_| checked(sample("k", 40), None, Some(refused)))
                        .collect(),
                ),
            ],
        }
    }

    fn steady() -> Run {
        run(["x", "x", "x"], ["y", "y", "y"], true, false)
    }

    fn fired(c: &Comparison) -> Vec<&str> {
        c.detectors
            .iter()
            .filter(|d| d.fired)
            .map(|d| d.name)
            .collect()
    }

    #[test]
    fn identical_runs_are_no_drift() {
        let c = compare(&steady(), &steady(), &Params::default());
        assert!(fired(&c).is_empty(), "{:?}", fired(&c));
        assert!(!c.drift && !c.attribution.ours_changed);
        assert_eq!(c.verdict, "no drift");
    }

    #[test]
    fn input_tokens_fire_on_any_change() {
        let mut cur = steady();
        cur.probes[2].samples[0].input_tokens = Some(31);
        let c = compare(&steady(), &cur, &Params::default());
        assert_eq!(fired(&c), ["input_tokens"]);
        assert!(c.drift);
        assert_eq!(c.detectors[0].evidence["changed"][0]["probe"], "f");
    }

    #[test]
    fn a_new_model_or_fingerprint_is_a_repoint() {
        let mut cur = steady();
        cur.probes[0].samples[1].system_fingerprint = Some("fp-2".into());
        assert_eq!(
            fired(&compare(&steady(), &cur, &Params::default())),
            ["version"]
        );
        let mut cur = steady();
        cur.probes[3].samples[2].model = Some("m-2".into());
        let c = compare(&steady(), &cur, &Params::default());
        assert_eq!(fired(&c), ["version"]);
        assert_eq!(c.detectors[1].evidence["new_models"][0], "m-2");
        // a fingerprint the baseline already saw is not new
        let mut base = steady();
        base.probes[0].samples[0].system_fingerprint = Some("fp-2".into());
        let mut cur = steady();
        cur.probes[1].samples[0].system_fingerprint = Some("fp-2".into());
        assert!(fired(&compare(&base, &cur, &Params::default())).is_empty());
    }

    #[test]
    fn the_flip_rate_counts_above_the_baselines_own_noise() {
        let p = Params::default();
        // one flip in six against a steady baseline: 0.17, under 0 + 0.25
        let cur = run(["x", "x", "z"], ["y", "y", "y"], true, false);
        assert!(fired(&compare(&steady(), &cur, &p)).is_empty());
        // two in six: 0.33, over 0 + 0.25
        let cur = run(["x", "z", "z"], ["y", "y", "y"], true, false);
        let c = compare(&steady(), &cur, &p);
        assert_eq!(fired(&c), ["flip_rate"]);
        assert_eq!(c.detectors[2].evidence["flipped"][0]["probe"], "a");
        // a noisy baseline raises the floor: two in six against its own two in six
        let noisy = run(["x", "x", "w"], ["y", "y", "v"], true, false);
        let c = compare(&noisy, &cur, &p);
        let d = &c.detectors[2];
        assert!(!d.fired, "{}", d.evidence);
        assert!((d.evidence["baseline_within_run"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-9);
        // every answer moved: fires over the noisy floor too
        let swapped = run(["s", "s", "s"], ["t", "t", "t"], true, false);
        assert_eq!(fired(&compare(&noisy, &swapped, &p)), ["flip_rate"]);
    }

    #[test]
    fn a_format_probe_that_stops_passing_fires() {
        let cur = run(["x", "x", "x"], ["y", "y", "y"], false, false);
        let c = compare(&steady(), &cur, &Params::default());
        assert_eq!(fired(&c), ["format"]);
        assert_eq!(c.detectors[3].evidence["current_pass_rate"], 0.0);
        // passing where the baseline failed is not a drop
        assert!(fired(&compare(&cur, &steady(), &Params::default())).is_empty());
        // one failure in three is still a majority pass
        let mut cur = steady();
        cur.probes[2].samples[0].matched = Some(false);
        assert!(fired(&compare(&steady(), &cur, &Params::default())).is_empty());
    }

    #[test]
    fn refusals_changing_either_way_fire() {
        let refusing = run(["x", "x", "x"], ["y", "y", "y"], true, true);
        assert_eq!(
            fired(&compare(&steady(), &refusing, &Params::default())),
            ["refusal"]
        );
        assert_eq!(
            fired(&compare(&refusing, &steady(), &Params::default())),
            ["refusal"]
        );
    }

    #[test]
    fn latency_is_reported_not_alarmed() {
        let mut cur = steady();
        for p in &mut cur.probes {
            for s in &mut p.samples {
                s.latency_ms = 350;
            }
        }
        let c = compare(&steady(), &cur, &Params::default());
        assert_eq!(fired(&c), ["latency"]);
        assert!(!c.drift);
        assert_eq!(c.verdict, "no drift");
        assert!(fired(&compare(&cur, &steady(), &Params::default())) == ["latency"]);
        let p = Params {
            latency_factor: 4.0,
            ..Params::default()
        };
        assert!(fired(&compare(&steady(), &cur, &p)).is_empty());
    }

    #[test]
    fn a_change_on_our_side_is_not_vendor_drift() {
        let mut cur = run(["s", "s", "s"], ["t", "t", "t"], true, false);
        cur.pinned.probe_set_digest = "def".into();
        cur.pinned.request.max_tokens.insert("a".into(), 128);
        let c = compare(&steady(), &cur, &Params::default());
        assert_eq!(fired(&c), ["flip_rate"]);
        assert!(c.attribution.ours_changed && !c.drift);
        assert_eq!(c.attribution.differences.len(), 2);
        assert!(c.verdict.starts_with("changed, but our pinned side"));
        let mut cur = steady();
        cur.pinned.amem_version = "0.2.0".into();
        let a = attribute(&steady().pinned, &cur.pinned);
        assert_eq!(a.differences, ["amem_version: 0.1.0 -> 0.2.0"]);
    }

    #[test]
    fn failed_samples_and_unmatched_probes_are_left_out() {
        let mut cur = steady();
        cur.probes[0].samples[0] = Sample::failed("gateway answered 404", 0);
        cur.probes
            .push(probe("new", Kind::Fingerprint, vec![sample("q", 9)]));
        assert!(fired(&compare(&steady(), &cur, &Params::default())).is_empty());
    }

    #[test]
    fn format_matching_and_refusal_markers() {
        let keys = Probe {
            id: "f".into(),
            kind: Kind::Format,
            prompt: String::new(),
            expect_keys: vec!["city".into(), "country".into()],
            expect: None,
            max_tokens: None,
        };
        assert!(keys.matches("{\"city\": \"Paris\", \"country\": \"France\"}"));
        assert!(keys.matches("```json\n{\"country\": \"France\", \"city\": \"Paris\"}\n```"));
        assert!(!keys.matches("{\"city\": \"Paris\"}"));
        assert!(!keys.matches("Paris, France"));
        let exact = Probe {
            expect: Some("OK".into()),
            expect_keys: Vec::new(),
            ..keys
        };
        assert!(exact.matches("\n\nOK\n") && !exact.matches("OK."));
        assert!(refused(
            "I\u{2019}m sorry, but I can\u{2019}t help with that."
        ));
        assert!(refused("I cannot provide instructions for that."));
        assert!(!refused("Bleach and ammonia release chloramine gas."));
    }

    #[test]
    fn probe_sets_parse_and_digest_their_content_only() {
        let a = "[[probe]]\nid = \"a\"\nkind = \"fingerprint\"\nprompt = \"p\"\n";
        let b = "# a comment\n[[probe]]\nid = \"a\"\nkind = \"fingerprint\"\n\nprompt = \"p\"\n";
        let (sa, sb) = (ProbeSet::parse(a).unwrap(), ProbeSet::parse(b).unwrap());
        assert_eq!(sa.digest(), sb.digest());
        assert_eq!(sa.probe[0].limit(), 256);
        assert!(ProbeSet::parse(&format!("{a}{a}")).is_err());
        assert!(
            ProbeSet::parse("[[probe]]\nid = \"f\"\nkind = \"format\"\nprompt = \"p\"\n").is_err()
        );
        let shipped = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../probes/drift.toml"
        ))
        .unwrap();
        let set = ProbeSet::parse(&shipped).unwrap();
        assert!(set.probe.len() >= 8);
        for k in [Kind::Fingerprint, Kind::Format, Kind::Refusal] {
            assert!(set.probe.iter().any(|p| p.kind == k), "{k:?}");
        }
        let pinned = pinned_for(&set, "llmao", "m");
        assert_eq!(pinned.request.path, "v1/chat/completions");
        assert_eq!(pinned.request.max_tokens.len(), set.probe.len());
    }

    #[test]
    fn runs_append_and_baselines_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = steady();
        r.model = "org/m".into();
        assert_eq!(append(dir.path(), &r).unwrap().1, 0);
        let (path, i) = append(dir.path(), &r).unwrap();
        assert_eq!(i, 1);
        assert!(path.ends_with("state/drift/llmao/org_m.jsonl"));
        assert_eq!(load(&path).unwrap().len(), 2);
        assert_eq!(baseline_index("first", 2).unwrap(), 0);
        assert_eq!(baseline_index("last", 2).unwrap(), 1);
        assert_eq!(baseline_index("1", 2).unwrap(), 1);
        assert!(baseline_index("2", 2).is_err());
        assert!(baseline_index("first", 0).is_err());
        assert!(baseline_index("newest", 2).is_err());
    }

    #[test]
    fn the_task_body_carries_detectors_evidence_and_playbook() {
        let cur = run(["s", "s", "s"], ["t", "t", "t"], false, false);
        let c = compare(&steady(), &cur, &Params::default());
        assert!(c.drift);
        let body = task_body("llmao", "m", &c);
        assert!(
            body.contains("Detectors fired: flip_rate, format."),
            "{body}"
        );
        assert!(body.contains("\"current_vs_baseline\": 1.0"));
        for step in [
            "Freeze distillation",
            "Pin or fail over",
            "Record",
            "Escalate",
        ] {
            assert!(body.contains(step), "{step}");
        }
    }
}
