//! The `cerebrium.toml` a Cerebrium app needs to run `vllm serve`, generated from the same facts the
//! Modal bundle is generated from.
//!
//! Issue #10 asks for a second GPU host beside Modal. Cerebrium's deployment unit is an *app* whose
//! behaviour is described entirely by one TOML file in the project root, and — this is the part that
//! makes the lane worth building — the file is the deployment. There is no printed endpoint to copy:
//! a custom runtime passes every path after `/{app}/` through to the container's `port`, so
//! [`Artefact::base_url`] can be computed before the first deploy.
//!
//! Three facts drive the emitter, all from `docs/research/issue-10-platform-facts.md`:
//!
//! * **An omitted key is reset to its default**, because a deploy replaces the whole app record
//!   rather than patching it. So every key whose default is wrong for vLLM is written out
//!   explicitly, and the ones whose defaults are harmless are left alone on purpose.
//! * **The defaults are wrong for vLLM in five places**: `disable_auth` is `true` (a generated app
//!   would be public), `replica_concurrency` is `1` on GPU compute (one in-flight request per
//!   container, so batching never happens and every caller buys a container), `cooldown` is `10 s`,
//!   `response_grace_period` is `900 s` and doubles as the per-request timeout, and
//!   `deployment_initialization_timeout` is `600 s`, which is shorter than a cold start that has to
//!   pull a multi-gigabyte checkpoint.
//! * **`compute` ids are platform-specific** and not Modal's. `HOPPER_H100` is a Cerebrium compute
//!   id; `H100` is Modal's. Passing one to the other is a deploy-time error that names neither.
//!
//! This module is pure: it takes a request, returns text or a problem. The conductor owns the HTTP
//! route, the vault and the config commit, as it does for every other GPU-rental surface.

use std::fmt;

/// One accepted `compute` id, with the facts this module needs to validate a request against it.
///
/// Prices are the published **per-second** rates; the per-hour figure is derived (×3600) and is only
/// ever printed as such. `max_gpu_count` is the smallest cap the platform documents — 8 for every GPU
/// type, with the schema's `gpuCount` maximum of 16 reachable only on some accounts, which is one of
/// the unresolved conflicts in the facts file; the conservative number is the one that can be
/// honoured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComputeType {
    /// The id to put in `compute`.
    pub id: &'static str,
    /// The card, as a person would name it.
    pub label: &'static str,
    /// VRAM in GB, when the platform publishes it.
    pub vram_gb: Option<u32>,
    /// The published per-second price, when there is one.
    pub usd_per_second: Option<f64>,
    /// The largest `gpu_count` this module will emit.
    pub max_gpu_count: u32,
    /// The cheapest plan that can deploy this compute type.
    pub plan: &'static str,
}

/// Every accepted `compute` id, from `https://cerebrium.ai/pricing` and
/// `https://cerebrium.ai/docs/hardware/using-gpus`.
///
/// `BLACKWELL_B300` is deliberately absent: it appears in the published GPU table but not in the
/// accepted set, and Cerebrium's own hardware reference says not to use it.
pub const COMPUTE_TYPES: &[ComputeType] = &[
    ComputeType {
        id: "BLACKWELL_B200",
        label: "B200",
        vram_gb: Some(180),
        usd_per_second: Some(0.00167),
        max_gpu_count: 8,
        plan: "standard",
    },
    ComputeType {
        id: "HOPPER_H200",
        label: "H200",
        vram_gb: Some(141),
        usd_per_second: Some(0.001166),
        max_gpu_count: 8,
        plan: "standard",
    },
    ComputeType {
        id: "HOPPER_H100",
        label: "H100",
        vram_gb: Some(80),
        usd_per_second: Some(0.000944),
        max_gpu_count: 8,
        plan: "standard",
    },
    ComputeType {
        id: "BLACKWELL_RTX6000",
        label: "RTX PRO 6000",
        vram_gb: Some(96),
        usd_per_second: Some(0.000694),
        max_gpu_count: 8,
        plan: "standard",
    },
    ComputeType {
        id: "AMPERE_A100_80GB",
        label: "A100 80 GB",
        vram_gb: Some(80),
        usd_per_second: Some(0.000583),
        max_gpu_count: 8,
        plan: "standard",
    },
    ComputeType {
        id: "AMPERE_A100_40GB",
        label: "A100 40 GB",
        vram_gb: Some(40),
        usd_per_second: Some(0.000555),
        max_gpu_count: 8,
        plan: "standard",
    },
    ComputeType {
        id: "ADA_L40",
        label: "L40s",
        vram_gb: Some(48),
        usd_per_second: Some(0.000542),
        max_gpu_count: 8,
        plan: "hobby",
    },
    ComputeType {
        id: "AMPERE_A10",
        label: "A10",
        vram_gb: Some(24),
        usd_per_second: Some(0.000306),
        max_gpu_count: 8,
        plan: "hobby",
    },
    ComputeType {
        id: "ADA_L4",
        label: "L4",
        vram_gb: Some(24),
        usd_per_second: Some(0.000222),
        max_gpu_count: 8,
        plan: "hobby",
    },
    ComputeType {
        id: "TURING_T4",
        label: "T4",
        vram_gb: Some(16),
        usd_per_second: Some(0.000164),
        max_gpu_count: 8,
        plan: "hobby",
    },
    ComputeType {
        id: "INF2",
        label: "Inferentia2",
        vram_gb: Some(32),
        usd_per_second: None,
        max_gpu_count: 8,
        plan: "hobby",
    },
    ComputeType {
        id: "TRN1",
        label: "Trainium",
        vram_gb: Some(32),
        usd_per_second: None,
        max_gpu_count: 8,
        plan: "enterprise",
    },
    ComputeType {
        id: "CPU",
        label: "CPU",
        vram_gb: None,
        usd_per_second: Some(0.00000655),
        max_gpu_count: 0,
        plan: "hobby",
    },
];

/// Look a `compute` id up, case-sensitively: the platform compares it exactly.
pub fn compute_by_id(id: &str) -> Option<&'static ComputeType> {
    COMPUTE_TYPES.iter().find(|compute| compute.id == id)
}

/// The documented bounds of `[cerebrium.scaling] cooldown`, in seconds.
///
/// Unlike Modal's `scaledown_window` there is no separate "default" constant because the default is
/// the same as the floor; see [`COMPUTE_TYPES`] for what a [`Tuning`] leaves out.
pub const COOLDOWN_RANGE: (u32, u32) = (0, 3600);

/// The `[cerebrium.runtime.custom] port` the generated app listens on.
///
/// 8000 is vLLM's own default and the port the platform's examples use. It is a *reference* port
/// here: any port works as long as `entrypoint` names the same one, which is why the emitter writes
/// both from this constant rather than from two literals.
pub const PORT: u16 = 8000;

/// What to serve and how, as the caller asked for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tuning {
    /// The app name. Determines the `base_url`, so it has to be stable across deploys.
    pub app: String,
    /// The project id, when the caller knows it. Only used to render `base_url`.
    pub project: Option<String>,
    /// The HuggingFace repository to serve.
    pub repo: String,
    /// A specific revision, when the caller pinned one.
    pub revision: Option<String>,
    /// The `compute` id. Validated against [`COMPUTE_TYPES`].
    pub compute: String,
    /// How many of that card. `1` for a single-GPU deployment.
    pub gpu_count: u32,
    /// CPU cores for `[cerebrium.hardware] cpu`. Required by the platform.
    pub cpu: f64,
    /// RAM in GB for `[cerebrium.hardware] memory`. Required by the platform, and **not** VRAM.
    pub memory_gb: f64,
    /// `--max-model-len`, when the caller chose one.
    pub max_model_len: Option<u64>,
    /// `--max-num-seqs`, when the caller chose one. Also becomes `replica_concurrency`.
    pub max_num_seqs: Option<u32>,
    /// `--kv-cache-dtype`, when it is something other than vLLM's `auto`.
    pub kv_cache_dtype: String,
    /// `--quantization`, when the checkpoint is not natively loadable. `none` omits the flag.
    pub weight_quant: String,
    /// `[cerebrium.scaling] cooldown`, in seconds.
    pub cooldown_secs: u32,
    /// `[cerebrium.scaling] max_replicas`.
    pub max_replicas: u32,
    /// The vLLM distribution spec, e.g. `vllm==0.11.0`. `None` leaves it unpinned.
    pub vllm_pin: Option<String>,
}

/// A generated deployment artefact: one file, the command that deploys it, and where it will answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Artefact {
    /// The file name, always `cerebrium.toml`.
    pub filename: &'static str,
    /// The file's contents.
    pub text: String,
    /// The command a person runs to deploy it.
    pub command: String,
    /// The OpenAI-compatible base URL, when the project id was known.
    pub base_url: Option<String>,
    /// Everything the generated file cannot decide, stated rather than left to be discovered.
    pub warnings: Vec<String>,
}

/// Why a [`Tuning`] cannot become an [`Artefact`].
///
/// Each variant is a `code` a client can branch on and a sentence a person can act on, matching the
/// shape the rest of the GPU surface uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The app name was empty or contained a character that would break the TOML or the URL.
    BadAppName(String),
    /// The repository was empty or malformed.
    BadRepo(String),
    /// The `compute` id is not one the platform accepts.
    UnknownCompute(String),
    /// `gpu_count` is zero on a GPU compute type, or above the documented cap.
    BadGpuCount {
        /// The compute id.
        compute: String,
        /// What was asked for.
        requested: u32,
        /// The largest value this module will emit.
        max: u32,
    },
    /// `cpu` or `memory_gb` was not a positive number.
    BadHardware(String),
    /// `cooldown_secs` was outside [`COOLDOWN_RANGE`].
    BadCooldown(u32),
    /// `max_replicas` was zero.
    BadReplicas(u32),
    /// `max_num_seqs` was zero.
    BadConcurrency(u32),
}

impl Problem {
    /// The stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Problem::BadAppName(_) => "bad_app_name",
            Problem::BadRepo(_) => "bad_repo",
            Problem::UnknownCompute(_) => "unknown_compute",
            Problem::BadGpuCount { .. } => "bad_gpu_count",
            Problem::BadHardware(_) => "bad_hardware",
            Problem::BadCooldown(_) => "bad_cooldown",
            Problem::BadReplicas(_) => "bad_replicas",
            Problem::BadConcurrency(_) => "bad_concurrency",
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::BadAppName(app) => write!(
                f,
                "`{app}` cannot be an app name: use letters, digits, dashes and underscores, and no slash"
            ),
            Problem::BadRepo(repo) => write!(
                f,
                "`{repo}` is not a HuggingFace repository id; it must look like `owner/name`"
            ),
            Problem::UnknownCompute(id) => {
                let known: Vec<&str> = COMPUTE_TYPES.iter().map(|compute| compute.id).collect();
                write!(
                    f,
                    "`{id}` is not a Cerebrium `compute` id; the accepted ids are {}",
                    known.join(", ")
                )
            }
            Problem::BadGpuCount {
                compute,
                requested,
                max,
            } => write!(
                f,
                "`gpu_count` is {requested} for `{compute}`, which this generator supports from 1 to {max}"
            ),
            Problem::BadHardware(what) => {
                write!(f, "`{what}` must be a positive number of GB or cores")
            }
            Problem::BadCooldown(value) => write!(
                f,
                "`cooldown` is {value} s; the platform accepts {} to {} s",
                COOLDOWN_RANGE.0, COOLDOWN_RANGE.1
            ),
            Problem::BadReplicas(value) => {
                write!(f, "`max_replicas` is {value}; it must be at least 1")
            }
            Problem::BadConcurrency(value) => {
                write!(f, "`max_num_seqs` is {value}; it must be at least 1")
            }
        }
    }
}

/// Render the app's `cerebrium.toml`.
///
/// The file is written complete for every key whose platform default is wrong for a vLLM server, and
/// deliberately silent about the ones whose defaults are harmless — `include`, `exclude`, `use_uv`,
/// `shell_commands`, `pre_build_commands`, `docker_base_image_url`, `python_version`, the two health
/// checks and the load-balancing knobs. Silence there is a real choice: those keys fall back to
/// values this generator would have written anyway, and writing them would only freeze a platform
/// default that may improve.
pub fn artefact(tuning: &Tuning) -> Result<Artefact, Problem> {
    let app = tuning.app.trim();
    if !app_name_is_safe(app) {
        return Err(Problem::BadAppName(tuning.app.clone()));
    }
    let repo = tuning.repo.trim();
    if !repo_is_safe(repo) {
        return Err(Problem::BadRepo(tuning.repo.clone()));
    }
    let Some(compute) = compute_by_id(tuning.compute.trim()) else {
        return Err(Problem::UnknownCompute(tuning.compute.clone()));
    };
    let cap = compute.max_gpu_count;
    if tuning.gpu_count > cap {
        return Err(Problem::BadGpuCount {
            compute: compute.id.to_owned(),
            requested: tuning.gpu_count,
            max: cap,
        });
    }
    if cap > 0 && tuning.gpu_count == 0 {
        return Err(Problem::BadGpuCount {
            compute: compute.id.to_owned(),
            requested: 0,
            max: cap,
        });
    }
    if !tuning.cpu.is_finite() || tuning.cpu <= 0.0 {
        return Err(Problem::BadHardware("cpu".to_owned()));
    }
    if !tuning.memory_gb.is_finite() || tuning.memory_gb <= 0.0 {
        return Err(Problem::BadHardware("memory".to_owned()));
    }
    if tuning.cooldown_secs < COOLDOWN_RANGE.0 || tuning.cooldown_secs > COOLDOWN_RANGE.1 {
        return Err(Problem::BadCooldown(tuning.cooldown_secs));
    }
    if tuning.max_replicas == 0 {
        return Err(Problem::BadReplicas(0));
    }
    if let Some(seqs) = tuning.max_num_seqs {
        if seqs == 0 {
            return Err(Problem::BadConcurrency(0));
        }
    }

    // `replica_concurrency` is what the platform routes to one container. It must be the server's
    // own admission limit, or the platform holds traffic back that vLLM would have batched — the
    // exact mistake the platform's default of 1 makes. Without a `--max-num-seqs` there is no
    // admission limit to copy, so the documented example value (32) is used and said out loud.
    let (concurrency, concurrency_basis) = match tuning.max_num_seqs {
        Some(seqs) => (seqs, "the server's --max-num-seqs".to_owned()),
        None => (32, "the platform's own vLLM example, because no --max-num-seqs was chosen".to_owned()),
    };

    let mut entrypoint: Vec<String> = vec!["vllm".to_owned(), "serve".to_owned(), repo.to_owned()];
    if let Some(revision) = tuning.revision.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        entrypoint.push("--revision".to_owned());
        entrypoint.push(revision.to_owned());
    }
    entrypoint.push("--host".to_owned());
    entrypoint.push("0.0.0.0".to_owned());
    entrypoint.push("--port".to_owned());
    entrypoint.push(PORT.to_string());
    entrypoint.push("--served-model-name".to_owned());
    entrypoint.push(repo.to_owned());
    if let Some(len) = tuning.max_model_len {
        entrypoint.push("--max-model-len".to_owned());
        entrypoint.push(len.to_string());
    }
    if let Some(seqs) = tuning.max_num_seqs {
        entrypoint.push("--max-num-seqs".to_owned());
        entrypoint.push(seqs.to_string());
    }
    let kv = tuning.kv_cache_dtype.trim();
    if !kv.is_empty() && kv != "auto" {
        entrypoint.push("--kv-cache-dtype".to_owned());
        entrypoint.push(kv.to_owned());
    }
    let quant = tuning.weight_quant.trim();
    if !quant.is_empty() && quant != "none" {
        entrypoint.push("--quantization".to_owned());
        entrypoint.push(quant.to_owned());
    }

    let entrypoint_toml = entrypoint
        .iter()
        .map(|arg| toml_string(arg))
        .collect::<Vec<String>>()
        .join(", ");

    let dependencies = match tuning.vllm_pin.as_deref().map(str::trim) {
        Some(pin) if !pin.is_empty() => format!("pip = [{}]", toml_string(pin)),
        _ => "pip = [\"vllm\"]".to_owned(),
    };

    let mut warnings: Vec<String> = Vec::new();
    match compute.usd_per_second {
        Some(rate) => warnings.push(format!(
            "`{}` bills ${:.5}/s, which is ${:.4}/hour (derived, ×3600); a single replica left warm for \
             an hour costs that whether or not it answers.",
            compute.id,
            rate,
            rate * 3600.0
        )),
        None => warnings.push(format!(
            "`{}` has no published per-second price, so this deployment's cost cannot be computed here.",
            compute.id
        )),
    }
    warnings.push(format!(
        "`replica_concurrency` is set to {concurrency} from {concurrency_basis}. The platform default is \
         1 on GPU compute, which would let one request occupy a whole card."
    ));
    warnings.push(
        "`response_grace_period` is set to 900 s, the platform default, because it also bounds a single \
         request: a longer generation is cut off at that point, not at the model's context limit."
            .to_owned(),
    );
    warnings.push(format!(
        "`cooldown` is set to {} s. The platform default is 10 s, so a container is gone before most \
         interactive follow-ups arrive; every request after a cooldown pays a cold start, and no \
         published figure for that exists (see `issue-10-platform-facts.md`).",
        tuning.cooldown_secs
    ));
    if tuning.vllm_pin.is_none() {
        warnings.push(
            "the vLLM version is unpinned, so the image is whatever `pip install vllm` resolves to on the \
             build. Pin it once a version is known to match the card's CUDA driver."
                .to_owned(),
        );
    }
    if tuning.gpu_count > 1 {
        warnings.push(format!(
            "`gpu_count = {}` is tensor-parallel: vLLM shards one model across the cards, so a single \
             request is faster but the deployment is not {} independent servers.",
            tuning.gpu_count, tuning.gpu_count
        ));
    }

    let app_toml = toml_string(app);
    let compute_toml = toml_string(compute.id);
    let memory = format_float(tuning.memory_gb);
    let cpu = format_float(tuning.cpu);

    let text = format!(
        r#"# Generated by Manarouter for issue #10. Deploy it with:
#     cerebrium deploy -y --detach
#
# Every key written here is written because the platform default is wrong for a vLLM server: a
# deploy replaces the whole app record, so an omitted key is reset to its default rather than
# preserved. Keys left out are left out on purpose (see `gpu::cerebrium` in `mr-core`).

[cerebrium.deployment]
name = {app_toml}
# The platform default is true, which would publish an app that runs a 30 GB model to anyone who
# finds the URL. Set false, then put a project API key in front of it.
disable_auth = false
# Default 600 s. A cold start has to pull the checkpoint before vLLM binds its port, and 600 s is
# inside the range that has been seen to time out on a large repository. Range 60-7200.
deployment_initialization_timeout = 1800

[cerebrium.runtime.custom]
port = {port}
# The command the container runs. Everything after /{{app_name}}/ on the inference host is proxied
# to this port, which is why the base URL below is knowable before the first deploy.
entrypoint = [{entrypoint}]

[cerebrium.hardware]
# RAM and cores, not VRAM. vLLM keeps the checkpoint on disk and streams it; the memory figure has
# to cover the loader's own copy of the tensors.
cpu = {cpu}
memory = {memory}
compute = {compute_toml}
gpu_count = {gpu_count}

[cerebrium.scaling]
min_replicas = 0
max_replicas = {max_replicas}
# Default is 1 on GPU compute. Requests per container, not containers.
replica_concurrency = {concurrency}
cooldown = {cooldown}
# Doubles as the per-request timeout; it is written out because 900 is a bound, not a preference.
response_grace_period = 900
# interruptible is the base rate; protected is billed at 2x and cannot be relocated mid-request.
compute_tier = "interruptible"

[cerebrium.dependencies]
{dependencies}
"#,
        app_toml = app_toml,
        port = PORT,
        entrypoint = entrypoint_toml,
        cpu = cpu,
        memory = memory,
        compute_toml = compute_toml,
        gpu_count = tuning.gpu_count,
        max_replicas = tuning.max_replicas,
        concurrency = concurrency,
        cooldown = tuning.cooldown_secs,
        dependencies = dependencies,
    );

    let base_url = tuning
        .project
        .as_deref()
        .map(str::trim)
        .filter(|project| !project.is_empty())
        .map(|project| {
            format!(
                "https://api.cerebrium.ai/v4/{}/{}/v1",
                project.trim_matches('/'),
                app
            )
        });

    Ok(Artefact {
        filename: "cerebrium.toml",
        text,
        command: "cerebrium deploy -y --detach".to_owned(),
        base_url,
        warnings,
    })
}

/// Whether `app` can go into the TOML and the URL unchanged.
///
/// This is a check on *our* safety, not a re-statement of the platform's rules: the platform's name
/// pattern is not published, so the only thing asserted here is that the string cannot break out of
/// a TOML basic string or a URL path. Upper case is allowed, because rejecting it would be inventing
/// a rule.
pub fn app_name_is_safe(app: &str) -> bool {
    !app.is_empty()
        && app.len() <= 64
        && app
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// Whether `repo` is shaped like a HuggingFace repository id.
pub fn repo_is_safe(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !owner.is_empty()
        && !name.is_empty()
        && repo.len() <= 120
        && repo.chars().all(|ch| {
            ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/')
        })
}

/// A TOML basic string for `value`.
///
/// Quotes, backslashes and control characters are escaped, so a repository id or an app name cannot
/// end the string early. `\u{…}` escapes are valid TOML and are what TOML itself requires for
/// control characters.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 || ch as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// A `[cerebrium.hardware]` figure, always with a decimal point.
///
/// TOML distinguishes integer `64` from float `64.0`, and the platform's reference types both `cpu`
/// and `memory` as floats. Emitting the integer spelling would rely on the reader coercing it, which
/// is a deploy-time type error if it does not — so the decimal point is written even when the value
/// is whole, and scientific notation is never used because TOML accepts it and every reader of this
/// file would misread it.
fn format_float(value: f64) -> String {
    format!("{:.1}", (value * 10.0).round() / 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuning() -> Tuning {
        Tuning {
            app: "manarouter-demo".to_owned(),
            project: Some("p-abc123".to_owned()),
            repo: "Qwen/Qwen3.8-27B".to_owned(),
            revision: Some("b32584cc".to_owned()),
            compute: "BLACKWELL_RTX6000".to_owned(),
            gpu_count: 1,
            cpu: 8.0,
            memory_gb: 64.0,
            max_model_len: Some(32768),
            max_num_seqs: Some(64),
            kv_cache_dtype: "fp8".to_owned(),
            weight_quant: "none".to_owned(),
            cooldown_secs: 120,
            max_replicas: 1,
            vllm_pin: Some("vllm==0.11.0".to_owned()),
        }
    }

    #[test]
    fn the_file_sets_every_default_that_is_wrong_for_vllm() {
        let artefact = artefact(&tuning()).expect("a valid tuning renders");
        assert_eq!(artefact.filename, "cerebrium.toml");
        assert_eq!(artefact.command, "cerebrium deploy -y --detach");
        for line in [
            // disable_auth defaults to true: an app that omits this is public.
            "disable_auth = false",
            // replica_concurrency defaults to 1 on GPU compute.
            "replica_concurrency = 64",
            // cooldown defaults to 10 s.
            "cooldown = 120",
            // response_grace_period defaults to 900 but doubles as the request timeout.
            "response_grace_period = 900",
            // The 600 s default is inside cold-start territory for a 31 GB checkpoint.
            "deployment_initialization_timeout = 1800",
            "min_replicas = 0",
            "max_replicas = 1",
            "compute_tier = \"interruptible\"",
            "compute = \"BLACKWELL_RTX6000\"",
            "gpu_count = 1",
        ] {
            assert!(
                artefact.text.contains(line),
                "the generated file is missing `{line}`:\n{}",
                artefact.text
            );
        }
        assert!(!artefact.text.contains("disable_auth = true"));
    }

    #[test]
    fn the_entrypoint_is_the_serve_command_and_names_the_same_port_as_the_runtime() {
        let artefact = artefact(&tuning()).expect("a valid tuning renders");
        let port_line = format!("port = {PORT}");
        assert!(artefact.text.contains(&port_line));
        assert!(artefact.text.contains(&format!("\"--port\", \"{PORT}\"")));
        assert!(artefact.text.contains(
            "entrypoint = [\"vllm\", \"serve\", \"Qwen/Qwen3.8-27B\", \"--revision\", \"b32584cc\", \
             \"--host\", \"0.0.0.0\", \"--port\", \"8000\", \"--served-model-name\", \
             \"Qwen/Qwen3.8-27B\", \"--max-model-len\", \"32768\", \"--max-num-seqs\", \"64\", \
             \"--kv-cache-dtype\", \"fp8\"]"
        ));
        // `--quantization` must never be derived from anything but an explicit choice.
        assert!(!artefact.text.contains("--quantization"));
    }

    #[test]
    fn the_base_url_is_computable_before_the_first_deploy() {
        let rendered = artefact(&tuning()).expect("a valid tuning renders");
        assert_eq!(
            rendered.base_url.as_deref(),
            Some("https://api.cerebrium.ai/v4/p-abc123/manarouter-demo/v1")
        );

        let mut anonymous = tuning();
        anonymous.project = None;
        let anonymous_artefact =
            artefact(&anonymous).expect("a valid tuning renders without a project");
        assert_eq!(anonymous_artefact.base_url, None, "no project id, no invented URL");
    }

    #[test]
    fn an_unknown_compute_id_lists_the_ones_the_platform_accepts() {
        let mut request = tuning();
        request.compute = "H100".to_owned(); // Modal's id, not Cerebrium's
        let problem = artefact(&request).expect_err("H100 is not a Cerebrium compute id");
        assert_eq!(problem.code(), "unknown_compute");
        let message = problem.to_string();
        assert!(message.contains("HOPPER_H100"), "got: {message}");
        assert!(message.contains("BLACKWELL_RTX6000"), "got: {message}");
        // The id that is published but not accepted must not be offered.
        assert!(!message.contains("BLACKWELL_B300"), "got: {message}");
    }

    #[test]
    fn a_gpu_count_above_the_documented_cap_is_refused_rather_than_clamped() {
        let mut request = tuning();
        request.gpu_count = 9;
        let problem = artefact(&request).expect_err("nine cards is above the cap");
        assert_eq!(problem.code(), "bad_gpu_count");
        assert!(problem.to_string().contains("1 to 8"), "got: {problem}");

        request.gpu_count = 0;
        let problem = artefact(&request).expect_err("zero cards is not a GPU deployment");
        assert_eq!(problem.code(), "bad_gpu_count");
    }

    #[test]
    fn a_cooldown_outside_the_published_range_is_refused_with_the_range() {
        let mut request = tuning();
        request.cooldown_secs = 3601;
        let problem = artefact(&request).expect_err("3601 s is out of range");
        assert_eq!(problem.code(), "bad_cooldown");
        assert!(problem.to_string().contains("0 to 3600"), "got: {problem}");

        request.cooldown_secs = 0;
        assert!(artefact(&request).is_ok(), "zero is the documented floor");
    }

    #[test]
    fn a_name_or_repo_that_would_break_the_file_is_refused_before_it_is_rendered() {
        let mut request = tuning();
        request.app = "manarouter\"; disable_auth = true; x \"".to_owned();
        assert_eq!(
            artefact(&request).expect_err("quotes are not an app name").code(),
            "bad_app_name"
        );

        request = tuning();
        request.repo = "Qwen/../../etc/passwd".to_owned();
        assert_eq!(artefact(&request).expect_err("dots are fine, slashes are not").code(), "bad_repo");

        request.repo = "Qwen".to_owned();
        assert_eq!(artefact(&request).expect_err("a bare owner is not a repo").code(), "bad_repo");
    }

    #[test]
    fn the_warnings_state_the_cost_and_the_cold_start_rather_than_leaving_them_to_be_found() {
        let rendered = artefact(&tuning()).expect("a valid tuning renders");
        let warnings = rendered.warnings.join("\n");
        assert!(warnings.contains("$2.4984/hour"), "the derived hourly rate: {warnings}");
        assert!(warnings.contains("×3600"), "the derivation is named: {warnings}");
        assert!(warnings.contains("cold start"), "the cold start is named: {warnings}");
        assert!(
            warnings.contains("response_grace_period"),
            "the request timeout is named: {warnings}"
        );

        // Unpinned vLLM is a warning, not an error: the caller may not know a version yet.
        let mut unpinned = tuning();
        unpinned.vllm_pin = None;
        let unpinned_artefact = artefact(&unpinned).expect("an unpinned vLLM still renders");
        assert!(
            unpinned_artefact.warnings.iter().any(|warning| warning.contains("unpinned")),
            "got: {:?}",
            unpinned_artefact.warnings
        );
        assert!(unpinned_artefact.text.contains("pip = [\"vllm\"]"));
    }

    #[test]
    fn a_compute_type_without_a_published_price_says_so_instead_of_guessing() {
        let mut request = tuning();
        request.compute = "TRN1".to_owned();
        let artefact = artefact(&request).expect("Trainium is an accepted compute id");
        assert!(artefact.warnings[0].contains("no published per-second price"));
        assert!(artefact.text.contains("compute = \"TRN1\""));
    }

    #[test]
    fn a_cpu_only_app_needs_no_gpu_count() {
        let mut request = tuning();
        request.compute = "CPU".to_owned();
        request.gpu_count = 0;
        request.cooldown_secs = 10;
        let artefact = artefact(&request).expect("a CPU app is valid with zero GPUs");
        assert!(artefact.text.contains("gpu_count = 0"));
    }

    #[test]
    fn every_documented_compute_id_renders() {
        // A table that can be looked up is only useful if every row round-trips; a typo in one id
        // would otherwise surface as a deploy-time rejection that names no field.
        for compute in COMPUTE_TYPES {
            let mut request = tuning();
            request.compute = compute.id.to_owned();
            request.gpu_count = compute.max_gpu_count.min(1);
            let artefact = artefact(&request)
                .unwrap_or_else(|problem| panic!("{} does not render: {problem}", compute.id));
            assert!(artefact.text.contains(&format!("compute = \"{}\"", compute.id)));
        }
        assert_eq!(COMPUTE_TYPES.len(), 13, "the platform publishes 13 accepted ids");
    }

    #[test]
    fn hardware_figures_keep_their_decimal_point_because_the_platform_types_them_as_floats() {
        // TOML `64` is an integer and TOML `64.0` is a float; the reference types both hardware
        // figures as floats, so the integer spelling is never written.
        assert_eq!(format_float(64.0), "64.0");
        assert_eq!(format_float(64.5), "64.5");
        assert_eq!(format_float(8.04), "8.0");

        let mut request = tuning();
        request.cpu = 0.0;
        assert_eq!(artefact(&request).expect_err("zero cores is not a machine").code(), "bad_hardware");
        request.cpu = 4.0;
        request.memory_gb = -1.0;
        assert_eq!(artefact(&request).expect_err("negative RAM").code(), "bad_hardware");
    }

    #[test]
    fn app_injection_backtick_is_rejected() {
        // Backticks can inject TOML sections; app_name_is_safe must reject them.
        let mut request = tuning();
        request.app = "`[broken]".to_owned();
        assert_eq!(
            artefact(&request).expect_err("backtick injection").code(),
            "bad_app_name"
        );
    }

    #[test]
    fn app_injection_space_is_rejected() {
        // Spaces in app names break TOML basic strings and URL path segments.
        let mut request = tuning();
        request.app = "my app".to_owned();
        assert_eq!(
            artefact(&request).expect_err("space in app name").code(),
            "bad_app_name"
        );
    }

    #[test]
    fn app_injection_tab_is_rejected() {
        // A tab character in the app name must not be accepted.
        let mut request = tuning();
        request.app = "app\tname".to_owned();
        assert_eq!(
            artefact(&request).expect_err("tab in app name").code(),
            "bad_app_name"
        );
    }

    #[test]
    fn repo_injection_quote_is_rejected() {
        // A quote in the repo would not break TOML (toml_string escapes it),
        // but it is not a valid HuggingFace repo id, so repo_is_safe rejects it.
        let mut request = tuning();
        request.repo = "a\"b/c".to_owned();
        assert_eq!(
            artefact(&request).expect_err("repo with quote").code(),
            "bad_repo"
        );
    }

    #[test]
    fn cooldown_secs_3600_accepted() {
        // 3600 is the explicit upper bound; it must be accepted.
        let mut request = tuning();
        request.cooldown_secs = 3600;
        let artefact = artefact(&request).expect("3600 s is within COOLDOWN_RANGE");
        assert!(artefact.text.contains("cooldown = 3600"), "got: {}", artefact.text);
    }

    #[test]
    fn warnings_always_non_empty() {
        // Even with a compute that has no price, at least the cold-start and
        // concurrency warnings must appear.
        let mut request = tuning();
        request.compute = "INF2".to_owned(); // no published price
        request.vllm_pin = None;
        let artefact = artefact(&request).expect("compute without price still renders");
        assert!(!artefact.warnings.is_empty(), "warnings must never be empty");
        // Price warning for unpriced compute.
        assert!(
            artefact.warnings.iter().any(|w| w.contains("no published per-second price")),
            "got: {:?}",
            artefact.warnings
        );
        // Cold-start warning must be present.
        assert!(
            artefact.warnings.iter().any(|w| w.contains("cold start")),
            "got: {:?}",
            artefact.warnings
        );
        // Concurrency warning must be present.
        assert!(
            artefact.warnings.iter().any(|w| w.contains("replica_concurrency")),
            "got: {:?}",
            artefact.warnings
        );
    }

    #[test]
    fn entrypoint_without_revision_omits_revision_flag() {
        // Without a revision the entrypoint must not include --revision.
        let mut request = tuning();
        request.revision = None;
        let artefact = artefact(&request).expect("no revision still renders");
        assert!(
            !artefact.text.contains("--revision"),
            "entrypoint must not contain --revision when revision is unset: {}",
            artefact.text
        );
    }

    #[test]
    fn base_url_empty_project_is_none() {
        // An empty-string project is treated the same as None for base_url.
        let mut request = tuning();
        request.project = Some("".to_owned());
        let artefact = artefact(&request).expect("empty project string still renders");
        assert!(
            artefact.base_url.is_none(),
            "base_url should be None when project is empty"
        );
    }
}
