//! molab: an interactive marimo notebook with a GPU, and nothing a router can call.
//!
//! ## Why this descriptor exists for a lane nothing can be built on
//!
//! Issue #13 proposed reaching a molab notebook through a reverse tunnel and treating it as an
//! OpenAI-compatible endpoint. Research against molab's own pages found that the tunnel is both
//! impossible (no token, no job endpoint, no inbound port) and forbidden (the restrictions page
//! names it as abuse), so this lane is [`LaneShape::Attended`], [`Inbound::None`] and
//! [`Submission::Manual`]. The descriptor exists so that answer is **enforced** rather than
//! remembered: [`crate::lane::plan_lane`] reads [`FACTS`] and returns `lane_is_attended` for
//! synchronous traffic, `lane_is_attended` again for batch traffic, and
//! `third_party_traffic_restricted` when the traffic is for someone else. A decision recorded only
//! in `docs/research/` cannot stop a future caller from wiring a tunnel; a refusal returned before
//! anything is configured can.
//!
//! ## Nothing to automate
//!
//! There is no API token to hold: molab's live Clerk configuration ([`CLERK_ENV`]) reports
//! `api_keys_settings.enabled = false`, with user and organisation API keys both disabled. There is
//! no documented job or run endpoint, and no URL serves a port bound inside a notebook:
//! `/notebooks/<id>` serves a read-only preview and `/notebooks/<id>/app` an app view — marimo's own
//! UI, not an inference endpoint. The GPU is attached by hand — "Toggle the GPU by clicking the
//! notebook specs button in the app header" ([`NOTEBOOK_DOCS`]) — with no API behind the toggle.
//! Sign-in is Google OAuth plus an email code; GitHub OAuth is present in the same configuration but
//! has `authenticatable: false`, so it is a notebook-sync integration, not a way in. Hence
//! `credentials` is [`Credential::None`]: the session lives in a browser profile, and there is
//! nothing portable to put in a vault.
//!
//! ## Why the tunnel from issue #13 is not implemented
//!
//! [`RESTRICTIONS`] (last updated 2026-06-24) says "Your account may be banned without notice if
//! you abuse molab", then lists the abuses. Four of them are exactly what a reverse tunnel plus an
//! OpenAI shim would do:
//!
//! * "connecting to remote proxies or otherwise exposing molab compute to outside traffic";
//! * "reselling compute to third parties";
//! * "remote control such as by SSH", one of the behaviours that "may have their notebooks
//!   terminated without warning";
//! * "using notebooks for non-interactive jobs", on the same list.
//!
//! So the tunnel is not an unattended-start gap to engineer around; it is a way to lose the
//! accounts the design depends on, one ban without notice at a time.
//!
//! ## The two numbers that rule out keep-warm
//!
//! [`NOTEBOOK_DOCS`] states both: "Notebooks can run for as long as 12 hours before molab shuts
//! them down. Notebooks that are idle for more than 90 minutes are automatically shut down." A
//! keep-warm lane needs a session that outlives the gaps between requests. molab closes it after 90
//! idle minutes in practice and 12 hours at the latest, and reopening it is a human clicking the
//! GPU toggle. The figures are load-bearing rather than trivia, which is why the tests pin them.
//!
//! ## Accounts: not addressed, not permitted
//!
//! [`AccountPolicy::Allowed`] means exactly what the variant documents: several accounts are not
//! prohibited. It must not be read as "permitted by the terms". No molab-specific terms of service
//! could be found. The only general Marimo terms ([`MARIMO_TERMS`]) are dated 2022-09-22, mention
//! molab nowhere in their body, and say nothing about how many accounts one person may hold; the
//! operative molab document is the restrictions page, which addresses geography and abuse but is
//! silent on account multiplicity. So several accounts are *not addressed* by any document we could
//! read, which is not the same as being *authorised* by one. `single_session_mode: true` in
//! [`CLERK_ENV`] does mean each extra account costs another browser profile and another Google
//! sign-in, but that is an operational cost, not a quota and not a prohibition.
//!
//! ## Quota: deliberately empty
//!
//! `quota` is `&[]` because nothing verifiable fills it. No primary page states a GPU-hour quota, a
//! concurrency cap or a fair-use number; the only stated limit is "as long as usage is reasonable"
//! ([`REINTRODUCTION`]), and an absence of documentation is not a documented "unlimited". GPU-hour
//! quota and concurrency are listed as unverified in the research file, so this descriptor claims
//! neither. A guessed number would be worse than a blank field, because the panel would render the
//! guess as fact.

use super::{
    AccountPolicy, Credential, Inbound, LaneFacts, LaneKind, LaneShape, Restriction, Submission,
};

/// Where the 12-hour session cap, the 90-minute idle shutdown and the manual GPU toggle were read.
pub const NOTEBOOK_DOCS: &str = "https://docs.marimo.io/guides/molab/";
/// Where the abuse list, the two clauses quoted in `use_scope`, and the ban remedy were read.
pub const RESTRICTIONS: &str = "https://marimo.io/pages/molab/restrictions";
/// molab's live Clerk environment: no API keys, Google OAuth only, GitHub not authenticatable.
pub const CLERK_ENV: &str = "https://clerk.marimo.io/v1/environment";
/// The only general Marimo terms found: dated 2022-09-22, silent on molab compute and on accounts.
pub const MARIMO_TERMS: &str = "https://marimo.io/pages/legal/terms";
/// Where "for free, as long as usage is reasonable" — the only stated usage limit — was read.
pub const REINTRODUCTION: &str = "https://marimo.io/blog/reintroducing-molab";

pub static FACTS: LaneFacts = LaneFacts {
    kind: LaneKind::Molab,
    id: "molab",
    display: "molab",
    // A human opens the notebook, toggles the GPU and runs the cells. There is no provisioning API.
    shape: LaneShape::Attended,
    // No URL serves a port bound inside the notebook; the share URLs serve marimo's own UI.
    inbound: Inbound::None,
    // No job endpoint and no API key: every run begins with a person in a browser.
    submission: Submission::Manual,
    // The credential is the browser session; Clerk has user and organisation API keys disabled.
    credentials: &[Credential::None],
    // Not prohibited, but not addressed either: see "Accounts" above.
    accounts: AccountPolicy::Allowed,
    // "Notebooks can run for as long as 12 hours before molab shuts them down."
    max_session_secs: Some(12 * 60 * 60),
    // "Notebooks that are idle for more than 90 minutes are automatically shut down."
    idle_shutdown_secs: Some(90 * 60),
    // GPU-hour quota and concurrency are unverified, so neither is claimed. See "Quota" above.
    quota: &[],
    // The two clauses the tunnel design would break, verbatim from the page's list of abuses.
    use_scope: Some(Restriction {
        text: "connecting to remote proxies or otherwise exposing molab compute to outside \
               traffic; reselling compute to third parties",
        source: RESTRICTIONS,
    }),
    headline: "A person must be at the keyboard to attach the GPU and run cells; the notebook is \
               not a server, and tunnelling traffic through it is grounds for termination without \
               notice.",
    sources: &[
        NOTEBOOK_DOCS,
        RESTRICTIONS,
        CLERK_ENV,
        MARIMO_TERMS,
        REINTRODUCTION,
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_can_be_served_or_automated_and_no_quota_is_claimed() {
        assert_eq!(FACTS.shape, LaneShape::Attended);
        assert_eq!(FACTS.inbound, Inbound::None);
        assert_eq!(FACTS.submission, Submission::Manual);
        assert_eq!(FACTS.credentials.len(), 1);
        assert_eq!(FACTS.credentials[0], Credential::None);
        assert!(!FACTS.serves_traffic());
        assert!(!FACTS.automatable());
        assert!(
            FACTS.quota.is_empty(),
            "no GPU-hour or concurrency figure was verifiable"
        );
    }

    #[test]
    fn the_wall_clock_and_idle_caps_are_the_twelve_hours_and_ninety_minutes_the_page_states() {
        assert_eq!(FACTS.max_session_secs, Some(12 * 60 * 60));
        assert_eq!(FACTS.idle_shutdown_secs, Some(90 * 60));
        assert!(
            FACTS.idle_shutdown_secs.unwrap() < FACTS.max_session_secs.unwrap(),
            "an idle notebook is reclaimed long before the 12-hour ceiling"
        );
    }

    #[test]
    fn the_use_scope_quotes_both_abuse_clauses_and_cites_the_restrictions_page() {
        let scope = FACTS
            .use_scope
            .expect("molab restricts what its compute may be used for");
        assert!(scope
            .text
            .contains("exposing molab compute to outside traffic"));
        assert!(scope.text.contains("reselling compute to third parties"));
        assert_eq!(scope.source, "https://marimo.io/pages/molab/restrictions");
        assert!(scope.source.starts_with("https://"));
        assert!(FACTS.sources.contains(&scope.source));
    }
}
