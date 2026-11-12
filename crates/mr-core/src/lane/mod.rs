//! What each GPU host can honestly be *for*, as data rather than as prose.
//!
//! ## Why this exists
//!
//! The rental work (issue #4) made Modal a real lane: a provider whose lifecycle Manarouter writes
//! and a relay that can reach it. Issues #10–#13 asked for four more hosts in the same shape, and
//! research against the platforms' own documents found that two of them cannot be built that way at
//! all — not for want of an API, but because their terms forbid the use:
//!
//! * Kaggle's Terms §3 forbids one person operating more than one account, so the "pool two accounts
//!   and rotate" design is a ban with extra steps. The submit-and-poll API itself works fine.
//! * molab names reverse tunnels and non-interactive use as abuse, with termination without notice.
//!   It also exposes no API token, no job endpoint and no inbound port, so there is nothing to
//!   automate even if the terms allowed it.
//!
//! Writing that in a document is necessary but not sufficient: a document does not stop a future
//! caller from wiring a tunnel. So the finding is encoded here as [`AccountPolicy::Prohibited`] and
//! [`LaneShape::Attended`], and [`plan_lane`] refuses the combination **before** anything is
//! configured. A refusal carries the clause and its URL, so the operator reads the platform's own
//! words rather than this module's summary of them.
//!
//! ## What this module is not
//!
//! It is not a network client and not a deployment mechanism. It holds the facts that decide whether
//! a lane may be used for a given traffic shape, and it answers that question with no I/O. The
//! per-platform detail — Modal's generated app, Cerebrium's `cerebrium.toml` — lives in the sibling
//! modules and in the conductor's `gpu_api`.
//!
//! | Question | Answer |
//! |---|---|
//! | Can traffic reach this lane synchronously? | [`LaneFacts::shape`], [`LaneFacts::inbound`] |
//! | How is a job started — API, CLI, or a human? | [`LaneFacts::submission`] |
//! | May the operator run two accounts? | [`LaneFacts::accounts`] |
//! | What stops working and when? | [`LaneFacts::max_session_secs`], [`LaneFacts::quota`] |
//! | Is this combination allowed at all? | [`plan_lane`] |
//!
//! ## The rule
//!
//! Every field that could be a guess is either `Option` or carries a `source`. "No free tier" and
//! "we did not find a free tier" are different answers, and only one of them may be printed as fact.

use std::fmt;

use serde::{Deserialize, Serialize};

pub mod cerebrium;
pub mod kaggle;
pub mod modal;
pub mod molab;
pub mod saturn;

/// A GPU host Manarouter knows how to talk about.
///
/// The discriminants are stable: they are what a snapshot or a config file would carry if a lane id
/// ever needs to be persisted, and renumbering them would silently reinterpret an old record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaneKind {
    /// Modal: proxy-token auth, a generated `modal_app.py`, `modal deploy` by hand.
    Modal = 0,
    /// Cerebrium: a REST deploy endpoint, path passthrough, OpenAI-shaped inference.
    Cerebrium = 1,
    /// Kaggle Notebooks: kernel push, poll, fetch files. No inbound socket.
    Kaggle = 2,
    /// Saturn Cloud: SSH-tunnelled Jupyter on the free plan, managed deployments on Hosted Pro.
    Saturn = 3,
    /// molab: an interactive marimo notebook on a GPU. Attended by design.
    Molab = 4,
}

impl LaneKind {
    /// Every lane, in discriminant order. Used by the catalogue route and by the tests that assert
    /// no lane is missing a descriptor.
    pub const ALL: [LaneKind; 5] = [
        LaneKind::Modal,
        LaneKind::Cerebrium,
        LaneKind::Kaggle,
        LaneKind::Saturn,
        LaneKind::Molab,
    ];

    /// The wire id. Matches `#[serde(rename_all = "kebab-case")]`, and is what a URL or a config file
    /// carries. Kept as an explicit function because it is also used in error messages and in the
    /// panel, where serde is not in the picture.
    pub fn as_str(self) -> &'static str {
        match self {
            LaneKind::Modal => "modal",
            LaneKind::Cerebrium => "cerebrium",
            LaneKind::Kaggle => "kaggle",
            LaneKind::Saturn => "saturn",
            LaneKind::Molab => "molab",
        }
    }

    /// Parse a wire id. Case-sensitive: an id is a key, not a sentence.
    pub fn parse(value: &str) -> Option<LaneKind> {
        LaneKind::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// From a persisted discriminant. Returns `None` rather than panicking on an unknown number, so
    /// a newer writer's record is rejected instead of being read as some other lane.
    pub fn from_u8(value: u8) -> Option<LaneKind> {
        LaneKind::ALL.into_iter().find(|kind| *kind as u8 == value)
    }

    /// The descriptor. Total by construction: every variant has one.
    pub fn facts(self) -> &'static LaneFacts {
        match self {
            LaneKind::Modal => &modal::FACTS,
            LaneKind::Cerebrium => &cerebrium::FACTS,
            LaneKind::Kaggle => &kaggle::FACTS,
            LaneKind::Saturn => &saturn::FACTS,
            LaneKind::Molab => &molab::FACTS,
        }
    }
}

impl fmt::Display for LaneKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How traffic can reach a lane. This is the field that decides whether a lane can be a provider at
/// all, and it is deliberately three-valued: collapsing `Batch` into `Attended` would hide that
/// Kaggle can run unattended jobs, and collapsing `Batch` into `Serving` would promise a request
/// path that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaneShape {
    /// An inbound HTTP endpoint answers a request while it is open.
    Serving,
    /// Work is submitted, runs to completion unattended, and is collected as files.
    Batch,
    /// A human must open the session; nothing can be submitted to it programmatically.
    Attended,
}

impl LaneShape {
    pub fn as_str(self) -> &'static str {
        match self {
            LaneShape::Serving => "serving",
            LaneShape::Batch => "batch",
            LaneShape::Attended => "attended",
        }
    }

    /// Whether a synchronous chat completion could ever be answered here.
    pub fn answers_requests(self) -> bool {
        matches!(self, LaneShape::Serving)
    }
}

/// Where a running session can be reached from. Separate from [`LaneShape`] because a serving lane
/// can still be owner-only: SatoSaturn's free plan reaches Jupyter over SSH, which serves requests
/// but not to the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Inbound {
    /// A public HTTPS endpoint with path passthrough: the relay can call it directly.
    PublicEndpoint,
    /// Reachable only through a tunnel or an owner-authenticated session. Usable by a person at the
    /// keyboard, not by a router.
    OwnerSessionOnly,
    /// No inbound socket at all. Results come back as files.
    None,
}

impl Inbound {
    pub fn as_str(self) -> &'static str {
        match self {
            Inbound::PublicEndpoint => "public-endpoint",
            Inbound::OwnerSessionOnly => "owner-session-only",
            Inbound::None => "none",
        }
    }
}

/// How a run is started. `Manual` is not a defect: for molab it is the only true answer, and saying
/// so is what keeps the router from pretending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Submission {
    /// A documented HTTP API that creates and updates the deployment.
    RestApi,
    /// A CLI that a process can drive unattended with a token in the environment.
    Cli,
    /// A notebook/kernel push that runs server-side.
    KernelPush,
    /// A person, in a browser. No API token exists to replace them.
    Manual,
}

impl Submission {
    pub fn as_str(self) -> &'static str {
        match self {
            Submission::RestApi => "rest-api",
            Submission::Cli => "cli",
            Submission::KernelPush => "kernel-push",
            Submission::Manual => "manual",
        }
    }

    /// Whether a process could start work without a human.
    pub fn is_automatable(self) -> bool {
        !matches!(self, Submission::Manual)
    }
}

/// The credential a lane's control plane wants. Named because the same platform often wants two
/// different ones — Cerebrium's service-account token deploys, its project API key infers — and
/// confusing them produces a 401 that looks like a wrong key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Credential {
    /// Two-part proxy token, sent as platform-specific headers.
    ProxyTokenPair,
    /// One bearer token for the control plane.
    ServiceAccountToken,
    /// One bearer token for inference.
    ApiKey,
    /// A personal API token, bearer.
    ApiToken,
    /// Username and key, HTTP Basic.
    BasicPair,
    /// Nothing portable: the session lives in a browser profile.
    None,
}

impl Credential {
    pub fn as_str(self) -> &'static str {
        match self {
            Credential::ProxyTokenPair => "proxy-token-pair",
            Credential::ServiceAccountToken => "service-account-token",
            Credential::ApiKey => "api-key",
            Credential::ApiToken => "api-token",
            Credential::BasicPair => "basic-pair",
            Credential::None => "none",
        }
    }
}

/// Whether the platform permits more than one account for one operator.
///
/// This is the field that carries the two stop signs found in research. It is not an opinion about
/// convenience: `Prohibited` quotes the clause, and [`plan_lane`] turns it into a refusal.
///
/// This is the field that carries the two stop signs found in research. It is not an opinion about
/// convenience: `Prohibited` quotes the clause, and [`plan_lane`] turns it into a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "verdict")]
pub enum AccountPolicy {
    /// Several accounts are permitted, or at least not addressed by the terms.
    Allowed,
    /// More than one account is forbidden, in the platform's own words.
    Prohibited {
        /// The clause, quoted rather than paraphrased, because a paraphrase is arguable.
        clause: &'static str,
        /// Where the clause was read.
        source: &'static str,
    },
}

impl AccountPolicy {
    pub fn is_prohibited(self) -> bool {
        matches!(self, AccountPolicy::Prohibited { .. })
    }

    /// How many accounts this policy permits. A refusal is expressed as a ceiling of one rather than
    /// as a boolean so a caller cannot "allow" it by ignoring a flag.
    pub fn ceiling(self) -> usize {
        match self {
            AccountPolicy::Allowed => usize::MAX,
            AccountPolicy::Prohibited { .. } => 1,
        }
    }
}

/// A quota that stops work at a known boundary. The limit is a string because platforms state rules,
/// not numbers: "30 hours or sometimes higher depending on demand" is the fact, and parsing it into
/// `30.0` would invent precision the platform did not offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct QuotaRule {
    /// What is rationed: `gpu-hours`, `sessions`, `apps`.
    pub what: &'static str,
    /// The rule in the platform's own terms.
    pub limit: &'static str,
    /// Whether the counter resets, and how often.
    pub reset: Option<&'static str>,
    pub source: &'static str,
}

/// A restriction on what the lane's compute may be used for. Distinct from [`AccountPolicy`]: one
/// bounds how many identities, the other bounds whose traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Restriction {
    pub text: &'static str,
    pub source: &'static str,
}

/// Everything this crate knows about one platform, with provenance.
///
/// Serialised as-is by the conductor's `GET /api/lanes`, so the panel renders the same facts the
/// refusal logic uses instead of a second copy that can drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LaneFacts {
    pub kind: LaneKind,
    /// The wire id, repeated here so a serialised descriptor is self-describing.
    pub id: &'static str,
    /// What a person calls it.
    pub display: &'static str,
    pub shape: LaneShape,
    pub inbound: Inbound,
    pub submission: Submission,
    /// Every credential the lane needs, in the order a setup flow should ask for them.
    pub credentials: &'static [Credential],
    pub accounts: AccountPolicy,
    /// Wall-clock ceiling on one session, where the platform states one.
    pub max_session_secs: Option<u64>,
    /// Idle shutdown, where the platform states one. This is the number that decides whether a
    /// keep-warm strategy is possible at all.
    pub idle_shutdown_secs: Option<u64>,
    pub quota: &'static [QuotaRule],
    pub use_scope: Option<Restriction>,
    /// One sentence a person needs before choosing this lane.
    pub headline: &'static str,
    /// Every page the facts above were read from. Never empty: an uncited descriptor is a rumour.
    pub sources: &'static [&'static str],
}

impl LaneFacts {
    /// Whether a synchronous request could be answered here by a router with no human in the loop.
    pub fn serves_traffic(&self) -> bool {
        self.shape.answers_requests() && matches!(self.inbound, Inbound::PublicEndpoint)
    }

    /// Whether a process can start work without a person.
    pub fn automatable(&self) -> bool {
        self.submission.is_automatable()
    }
}

/// The traffic a caller intends to send. Asked of the caller because the same lane can be acceptable
/// for batch work and unacceptable for a chat request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Traffic {
    /// Request in, tokens out, while the caller waits. What the relay's `/v1/chat/completions` does.
    Synchronous,
    /// Submit, wait minutes, collect files. What the batch lane does.
    Batch,
}

impl Traffic {
    pub fn as_str(self) -> &'static str {
        match self {
            Traffic::Synchronous => "synchronous",
            Traffic::Batch => "batch",
        }
    }
}

/// Why a lane may not be used as asked. Coded so a client can branch on it, and quoted so a person
/// can check it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    /// Stable machine code: `lane_cannot_serve`, `lane_is_attended`, `multiple_accounts_forbidden`,
    /// `lane_is_batch_only`, `third_party_traffic_restricted`.
    pub code: &'static str,
    /// One sentence, naming the lane.
    pub message: String,
    /// The platform's own words, where a clause is the reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clause: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
}

/// The answer to "may I use this lane this way?".
///
/// A list of refusals rather than a `Result`: reporting every problem at once is what lets a panel
/// show the whole objection instead of one line at a time, and an empty list is the success case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LanePlan {
    pub lane: LaneKind,
    pub id: &'static str,
    pub traffic: Traffic,
    /// How many accounts were asked for, echoed so a stored plan is readable on its own.
    pub accounts: usize,
    pub usable: bool,
    pub refusals: Vec<Refusal>,
}

/// Decide whether a lane may carry the intended traffic with the intended number of accounts.
///
/// This is the function that makes the research binding. It refuses:
///
/// * synchronous traffic on a lane with no public inbound endpoint (batch or attended);
/// * batch traffic on a lane where even that is impossible (attended);
/// * more than one account where the platform forbids it, quoting the clause;
/// * third-party serving where the platform restricts use to the operator's own work — reported as
///   a refusal only when the caller says the traffic is for someone else, because a single operator
///   routing their own requests is inside Kaggle's licence.
///
/// It does not check quotas against live usage: that needs a clock and a counter, and this module
/// has neither. A quota-exhausted lane fails at submission, which is the honest place for it.
pub fn plan_lane(lane: LaneKind, traffic: Traffic, accounts: usize, for_third_parties: bool) -> LanePlan {
    let facts = lane.facts();
    let mut refusals = Vec::new();

    if accounts == 0 {
        refusals.push(Refusal {
            code: "no_account",
            message: format!("{} needs at least one account before it can run anything", facts.display),
            clause: None,
            source: None,
        });
    }

    match traffic {
        Traffic::Synchronous if !facts.serves_traffic() => {
            refusals.push(match facts.shape {
                LaneShape::Batch => Refusal {
                    code: "lane_is_batch_only",
                    message: format!(
                        "{} runs submitted jobs and returns files; it has no endpoint a chat request \
                         can be answered from",
                        facts.display
                    ),
                    clause: None,
                    source: Some(facts.sources[0]),
                },
                LaneShape::Attended => Refusal {
                    code: "lane_is_attended",
                    message: format!(
                        "{} needs a person to open the session, so it cannot answer a request that \
                         arrives while nobody is watching",
                        facts.display
                    ),
                    clause: None,
                    source: Some(facts.sources[0]),
                },
                LaneShape::Serving => Refusal {
                    code: "lane_is_owner_only",
                    message: format!(
                        "{} is reachable only from your own machine or a tunnel you open by hand",
                        facts.display
                    ),
                    clause: None,
                    source: Some(facts.sources[0]),
                },
            });
        }
        Traffic::Batch if facts.shape == LaneShape::Attended => {
            refusals.push(Refusal {
                code: "lane_is_attended",
                message: format!(
                    "{} has no job API: work is started by hand and returns files, so it cannot be \
                     queued",
                    facts.display
                ),
                clause: None,
                source: Some(facts.sources[0]),
            });
        }
        _ => {}
    }

    if accounts > facts.accounts.ceiling() {
        if let AccountPolicy::Prohibited { clause, source } = facts.accounts {
            refusals.push(Refusal {
                code: "multiple_accounts_forbidden",
                message: format!(
                    "{} allows one account per person; {} accounts would break the platform's terms",
                    facts.display, accounts
                ),
                clause: Some(clause),
                source: Some(source),
            });
        }
    }

    if for_third_parties {
        if let Some(restriction) = facts.use_scope {
            refusals.push(Refusal {
                code: "third_party_traffic_restricted",
                message: format!(
                    "{} limits its compute to your own work, so serving other people's requests \
                     through it is outside the licence",
                    facts.display
                ),
                clause: Some(restriction.text),
                source: Some(restriction.source),
            });
        }
    }

    LanePlan {
        lane,
        id: facts.id,
        traffic,
        accounts,
        usable: refusals.is_empty(),
        refusals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_lane_has_a_descriptor_that_names_itself_and_cites_a_source() {
        for kind in LaneKind::ALL {
            let facts = kind.facts();
            assert_eq!(facts.kind, kind, "{kind} descriptor is wired to the wrong variant");
            assert_eq!(facts.id, kind.as_str());
            assert!(!facts.display.is_empty(), "{kind} has no label");
            assert!(
                !facts.sources.is_empty(),
                "{kind} cites nothing, so its facts cannot be checked"
            );
            for source in facts.sources {
                assert!(source.starts_with("https://"), "{kind} cites a non-URL: {source}");
            }
            assert!(!facts.headline.is_empty(), "{kind} has no headline");
        }
    }

    #[test]
    fn the_wire_ids_round_trip_and_an_unknown_number_is_refused() {
        for kind in LaneKind::ALL {
            assert_eq!(LaneKind::parse(kind.as_str()), Some(kind));
            assert_eq!(LaneKind::from_u8(kind as u8), Some(kind));
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(kind.as_str()));
        }
        assert_eq!(LaneKind::parse("Modal"), None, "ids are keys, not prose");
        assert_eq!(LaneKind::parse("vast"), None);
        assert_eq!(LaneKind::from_u8(5), None, "a newer writer's lane is rejected, not guessed");
    }

    #[test]
    fn only_a_serving_lane_with_a_public_endpoint_can_answer_a_request() {
        for kind in LaneKind::ALL {
            let facts = kind.facts();
            let expected = facts.shape == LaneShape::Serving
                && facts.inbound == Inbound::PublicEndpoint;
            assert_eq!(facts.serves_traffic(), expected, "{kind}");
        }
    }

    #[test]
    fn a_lane_without_a_public_endpoint_refuses_synchronous_traffic_and_says_which_kind_it_is() {
        for kind in LaneKind::ALL {
            let facts = kind.facts();
            let plan = plan_lane(kind, Traffic::Synchronous, 1, false);
            if facts.serves_traffic() {
                assert!(plan.usable, "{kind} serves traffic but was refused: {:?}", plan.refusals);
                continue;
            }
            assert!(!plan.usable, "{kind} cannot serve but was allowed");
            let code = plan.refusals[0].code;
            let expected = match facts.shape {
                LaneShape::Batch => "lane_is_batch_only",
                LaneShape::Attended => "lane_is_attended",
                LaneShape::Serving => "lane_is_owner_only",
            };
            assert_eq!(code, expected, "{kind}");
            assert!(plan.refusals[0].source.is_some(), "{kind} refused without a citation");
        }
    }

    #[test]
    fn an_attended_lane_cannot_be_given_batch_work_either() {
        for kind in LaneKind::ALL {
            if kind.facts().shape != LaneShape::Attended {
                continue;
            }
            let plan = plan_lane(kind, Traffic::Batch, 1, false);
            assert!(!plan.usable, "{kind} is attended, so even batch work cannot be queued");
            assert_eq!(plan.refusals[0].code, "lane_is_attended");
        }
    }

    #[test]
    fn a_lane_that_forbids_second_accounts_returns_the_clause_the_platform_wrote() {
        let mut saw_one = false;
        for kind in LaneKind::ALL {
            let facts = kind.facts();
            let plan = plan_lane(kind, facts.shape_to_traffic(), 2, false);
            if facts.accounts.is_prohibited() {
                saw_one = true;
                let refusal = plan
                    .refusals
                    .iter()
                    .find(|r| r.code == "multiple_accounts_forbidden")
                    .unwrap_or_else(|| panic!("{kind} forbids two accounts but allowed them"));
                assert!(refusal.clause.unwrap().len() > 40, "{kind}'s clause is a summary");
                assert!(refusal.source.unwrap().starts_with("https://"));
            } else {
                assert!(
                    plan.refusals.iter().all(|r| r.code != "multiple_accounts_forbidden"),
                    "{kind} allows several accounts"
                );
            }
        }
        assert!(saw_one, "no lane reports an account prohibition, so the check proved nothing");
    }

    #[test]
    fn third_party_traffic_is_refused_only_where_the_platform_restricts_it() {
        for kind in LaneKind::ALL {
            let facts = kind.facts();
            let traffic = facts.shape_to_traffic();
            let own = plan_lane(kind, traffic, 1, false);
            let others = plan_lane(kind, traffic, 1, true);
            if facts.use_scope.is_some() {
                assert!(
                    others.refusals.iter().any(|r| r.code == "third_party_traffic_restricted"),
                    "{kind} restricts use but allowed other people's traffic"
                );
            } else {
                assert!(own.usable, "{kind} should be usable for the operator's own work");
            }
            assert!(
                own.refusals.len() <= others.refusals.len(),
                "{kind} became more permissive when more people were involved"
            );
        }
    }

    #[test]
    fn asking_for_no_accounts_is_refused_rather_than_defaulted() {
        for kind in LaneKind::ALL {
            let traffic = kind.facts().shape_to_traffic();
            let plan = plan_lane(kind, traffic, 0, false);
            assert!(!plan.usable, "{kind} accepted zero accounts");
            assert_eq!(plan.refusals[0].code, "no_account");
        }
    }

    #[test]
    fn a_plan_serialises_its_refusals_without_inventing_empty_fields() {
        let plan = plan_lane(LaneKind::Kaggle, Traffic::Synchronous, 2, true);
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["usable"], serde_json::json!(false));
        assert_eq!(json["id"], serde_json::json!("kaggle"));
        assert_eq!(json["traffic"], serde_json::json!("synchronous"));
        let refusals = json["refusals"].as_array().unwrap();
        assert!(refusals.len() >= 3, "batch-only plus multi-account plus scope: got {refusals:?}");
        let batch = refusals.iter().find(|r| r["code"] == "lane_is_batch_only").unwrap();
        assert!(batch.get("clause").is_none(), "a shape refusal quotes no clause");
        let accounts = refusals
            .iter()
            .find(|r| r["code"] == "multiple_accounts_forbidden")
            .expect("Kaggle Terms §3 forbids a second account");
        assert!(accounts["clause"].as_str().unwrap().contains("Kaggle"));
    }

    #[test]
    fn a_descriptor_serialises_the_fields_a_panel_needs() {
        let facts = LaneKind::Kaggle.facts();
        let json = serde_json::to_value(facts).unwrap();
        assert_eq!(json["shape"], serde_json::json!("batch"));
        assert_eq!(json["inbound"], serde_json::json!("none"));
        assert_eq!(json["submission"], serde_json::json!("kernel-push"));
        assert_eq!(json["accounts"]["verdict"], serde_json::json!("prohibited"));
        assert!(json["accounts"]["clause"].as_str().unwrap().len() > 40);
        assert!(json["quota"].as_array().unwrap().iter().any(|q| q["what"] == "gpu-hours"));
    }

    /// A representative traffic shape for a lane, used by the tests that only care about accounts or
    /// scope. Synchronous where it is possible, batch otherwise.
    impl LaneFacts {
        fn shape_to_traffic(&self) -> Traffic {
            if self.serves_traffic() {
                Traffic::Synchronous
            } else {
                Traffic::Batch
            }
        }
    }

    /// Every lane × traffic combination must yield a verdict, never a silent gap.
    #[test]
    fn every_lane_and_traffic_has_a_usable_or_refused_verdict() {
        let known = [
            (LaneKind::Modal, true, true),
            (LaneKind::Cerebrium, true, true),
            (LaneKind::Saturn, true, true),
            (LaneKind::Kaggle, false, true),
            (LaneKind::Molab, false, false),
        ];
        for (lane, sync_ok, batch_ok) in known {
            let sync = plan_lane(lane, Traffic::Synchronous, 1, false);
            assert_eq!(sync.usable, sync_ok, "{lane} sync: expected usable={sync_ok}");
            if sync.usable {
                assert!(sync.refusals.is_empty(), "{lane} sync is usable but has refusals");
            } else {
                assert!(!sync.refusals.is_empty(), "{lane} sync refused with no reason");
            }
            let batch = plan_lane(lane, Traffic::Batch, 1, false);
            assert_eq!(batch.usable, batch_ok, "{lane} batch: expected usable={batch_ok}");
            if batch.usable {
                assert!(batch.refusals.is_empty(), "{lane} batch is usable but has refusals");
            } else {
                assert!(!batch.refusals.is_empty(), "{lane} batch refused with no reason");
            }
        }
    }
}
