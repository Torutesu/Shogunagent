//! Retrieval evaluation for AI memory search (issue #940 B1; docs/specs/issue-940-ai-search.md).
//!
//! Compares (i) the current keyword search — the canonical `Db::search_request` pipeline, as the
//! AI keyword phase runs it — with (ii) expansion plus rerank (`Db::ai_search`), over one fixture
//! corpus in an in-memory database. The cases cover what the issue asks for: low-overlap
//! paraphrases, Japanese↔English questions, relative dates, older retained evidence, exact
//! identifiers / links / quoted phrases, and misleading near-match distractors.
//!
//! Two model modes:
//!
//! - **fake** (CI, always): [`ThesaurusModel`], a deterministic stand-in that reads the real
//!   prompts the pipeline builds and answers with the real JSON contract. Its "knowledge" is a
//!   small bilingual concept lexicon written next to this corpus, so its numbers show that the
//!   plumbing works and what a model that knows those synonyms would gain. They are NOT a
//!   measurement of any real model.
//! - **live** (`#[ignore]`, env-gated): the same cases through a real Agent-lane provider on the
//!   owner's machine. Skipped unless `SHOGUN_AI_SEARCH_EVAL_LIVE` is set; never needs a secret
//!   in CI. See `live_provider_eval` for the variables.
//!
//! `db` feature (the daemon adapter lives there). CI runs the fake mode in the
//! `cargo test -p shogun-core --features daemon-server` step. Print the table with
//! `cargo test -p shogun-core --features db --test ai_search_eval -- --nocapture`.
#![cfg(feature = "db")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use shogun_agents::entitlement::{entitlements, Entitlements, Plan};
use shogun_core::ai_search::dates::{
    civil_from_days, days_from_civil, local_date, local_midnight_ms, parse_iso_date, CivilDate,
};
use shogun_core::ai_search::eval::{render_table, score, Scores};
use shogun_core::ai_search::{
    AiSearchBounds, KeywordSearcher, ModelStep, SearchModel,
};
use shogun_core::capture::exclusion::ExclusionPolicy;
use shogun_core::daemon::Db;
use shogun_core::llm::LlmError;
use shogun_memory::event_log::NewEvent;

/// Monday 2026-10-05 12:00 UTC. The database clock, so relative dates are reproducible.
fn now_ms() -> i64 {
    local_midnight_ms(date(2026, 10, 5), 0) + 12 * 3_600_000
}

fn date(year: i32, month: u32, day: u32) -> CivilDate {
    CivilDate { year, month, day }
}

struct Fixture {
    label: &'static str,
    day: CivilDate,
    app: &'static str,
    content: &'static str,
}

fn fx(label: &'static str, (y, m, d): (i32, u32, u32), app: &'static str, content: &'static str) -> Fixture {
    Fixture { label, day: date(y, m, d), app, content }
}

fn corpus() -> Vec<Fixture> {
    vec![
        // Paraphrase: "delay the launch".
        fx("launch_delay", (2026, 9, 18), "com.tinyspeck.slackmacgap", "Agreed in standup: push the Orchid release to next month so QA can finish."),
        fx("launch_party", (2026, 10, 2), "com.apple.mail", "Launch party catering order confirmed for the Orchid launch."),
        fx("logo_decided", (2026, 9, 25), "com.figma.Desktop", "We decided on the new logo colors: indigo and sand."),
        fx("hiring_conversation", (2026, 9, 22), "com.apple.Notes", "Conversation notes: hiring plan for Q4, two backend roles."),
        fx("release_notes", (2026, 9, 29), "com.github.GitHubClient", "Release notes for v2.3 published on the changelog."),
        // Paraphrase / JA: "cut the marketing budget".
        fx("budget_cut", (2026, 9, 9), "com.apple.mail", "Finance sync: ad spend reduced by 20% for Q4, approved by Ken."),
        fx("marketing_lunch", (2026, 9, 30), "com.apple.iCal", "Marketing team lunch scheduled for Friday."),
        fx("budget_template", (2026, 9, 15), "com.microsoft.Excel", "Budget template spreadsheet shared with the team."),
        // Paraphrase: "hire for the designer role".
        fx("designer_hired", (2026, 9, 26), "com.apple.mail", "Offer accepted by Dana Kim — she starts in the UI/UX position on Nov 3."),
        fx("designer_jd", (2026, 9, 1), "com.google.Chrome", "Designer role job description posted on the careers page."),
        fx("hire_form", (2026, 9, 20), "com.apple.Safari", "Hire date field missing in the HR onboarding form."),
        // Paraphrase: "outage on the payments service".
        fx("payments_outage", (2026, 9, 24), "com.tinyspeck.slackmacgap", "Incident: checkout API returned 502 for 40 minutes; root cause was an expired TLS cert."),
        fx("payments_roadmap", (2026, 9, 27), "com.apple.Keynote", "Payments service roadmap review with the platform team."),
        // EN question, JA evidence.
        fx("acme_renewal_ja", (2026, 9, 16), "com.apple.Notes", "Acme社との契約更新は12月まで保留、法務確認待ち。"),
        fx("acme_demo", (2026, 9, 28), "com.apple.mail", "Acme demo feedback: they liked the dashboard."),
        // Relative date: "the design review last week".
        fx("design_review_lastweek", (2026, 9, 30), "com.apple.Notes", "Design review: go with the dark sidebar, drop the floating toolbar."),
        fx("design_review_moved", (2026, 9, 10), "com.apple.iCal", "Design review moved to Thursday."),
        fx("design_review_aug", (2026, 8, 10), "com.apple.Notes", "Design review: keep the light theme for now."),
        fx("design_review_template", (2026, 7, 1), "com.apple.Pages", "Design review notes template."),
        // Relative date in JA, EN evidence.
        fx("invoice_sep", (2026, 9, 12), "com.apple.mail", "Invoice INV-2026-0042 from Northwind paid on Sep 12."),
        fx("invoice_aug", (2026, 8, 12), "com.apple.mail", "Invoice INV-2026-0031 from Northwind paid on Aug 12."),
        // Older retained evidence (beyond the 30-day Warm window).
        fx("hosting_migration", (2026, 3, 14), "com.apple.Notes", "Migrated off Linode to Fly.io because of repeated noisy-neighbor latency spikes."),
        fx("hosting_costs", (2026, 9, 20), "com.google.Chrome", "Hosting costs dashboard reviewed, within plan."),
        fx("mobile_idea", (2026, 2, 2), "com.apple.Notes", "Brainstorm: an iPhone companion for quick capture — revisit after v1."),
        fx("mobile_plan", (2026, 9, 5), "com.apple.mail", "Mobile data plan renewal due next week."),
        // Exact identifiers, links, quoted phrases.
        fx("pr_941", (2026, 10, 1), "com.github.GitHubClient", "Review requested: https://github.com/acme/orchid/pull/941 (search rerank)."),
        fx("pr_94", (2026, 9, 2), "com.github.GitHubClient", "Merged https://github.com/acme/orchid/pull/94 after review."),
        fx("ssot", (2026, 6, 3), "com.apple.Pages", "Decision doc: the event log is the single source of truth; state tables are derived."),
        fx("truth_table", (2026, 9, 21), "com.microsoft.VSCode", "Source code truth table for the parser."),
        fx("ops_1182", (2026, 9, 19), "com.atlassian.jira", "OPS-1182: rotate the staging database credentials by Friday."),
        fx("ops_1182_reopened", (2026, 10, 3), "com.atlassian.jira", "OPS-1182 reopened: the rotation script failed on the replica."),
        fx("ops_118", (2026, 9, 23), "com.atlassian.jira", "OPS-118 closed as duplicate."),
        // Near-match distractors.
        fx("offsite_cancelled", (2026, 9, 29), "com.tinyspeck.slackmacgap", "Tokyo offsite called off — the venue fell through; refund requested."),
        fx("offsite_agenda", (2026, 9, 17), "com.apple.Pages", "Tokyo offsite agenda draft v2."),
        fx("cancel_subscription", (2026, 9, 26), "com.apple.Safari", "Cancel subscription to the old analytics tool."),
        fx("maya_phone", (2026, 9, 14), "com.apple.MobileSMS", "Maya's contact changed: mobile 090-1234-5678."),
        fx("maya_phone_case", (2026, 9, 28), "com.apple.Safari", "New phone case ordered for Maya's birthday."),
        // A meaning the fake lexicon does not know: shows the method's limit, not a win.
        fx("pricing", (2026, 7, 20), "com.apple.Pages", "Pricing decision: Standard stays at $49 a year and Pro at $99."),
        // Background noise.
        fx("noise_sync", (2026, 9, 29), "com.apple.Notes", "Weekly sync notes: nothing blocking, demo on Thursday."),
        fx("noise_lunch", (2026, 10, 1), "com.apple.iCal", "Team lunch at the ramen place."),
        fx("noise_docs", (2026, 9, 8), "com.google.Chrome", "Read the SQLite FTS5 trigram tokenizer documentation."),
        fx("noise_gym", (2026, 9, 19), "com.apple.iCal", "Gym session moved to 7am."),
        fx("noise_travel", (2026, 8, 30), "com.apple.mail", "Flight to Osaka booked for the partner visit."),
    ]
}

struct Case {
    category: &'static str,
    question: &'static str,
    relevant: &'static [&'static str],
}

fn cases() -> Vec<Case> {
    let c = |category, question, relevant| Case { category, question, relevant };
    vec![
        c("paraphrase", "Find the conversation where we decided to delay the launch", &["launch_delay"]),
        c("paraphrase", "When did we agree to cut the marketing budget?", &["budget_cut"]),
        c("paraphrase", "Who did we decide to hire for the designer role?", &["designer_hired"]),
        c("paraphrase", "the outage on the payments service", &["payments_outage"]),
        c("ja_en", "ローンチを延期した件", &["launch_delay"]),
        c("ja_en", "Where did we note the contract renewal with Acme?", &["acme_renewal_ja"]),
        c("ja_en", "予算削減の決定", &["budget_cut"]),
        c("ja_en", "東京オフサイトは中止になった？", &["offsite_cancelled"]),
        c("relative_date", "what was decided in the design review last week", &["design_review_lastweek"]),
        c("relative_date", "先月の請求書の件", &["invoice_sep"]),
        c("older_evidence", "the reason we moved away from the old hosting provider", &["hosting_migration"]),
        c("older_evidence", "when did we first talk about the mobile app idea", &["mobile_idea"]),
        c("exact", "INV-2026-0042", &["invoice_sep"]),
        c("exact", "https://github.com/acme/orchid/pull/941", &["pr_941"]),
        c("exact", "what did the doc say about \"single source of truth\"", &["ssot"]),
        c("exact", "why was OPS-1182 reopened", &["ops_1182_reopened"]),
        c("distractor", "Did we cancel the Tokyo offsite?", &["offsite_cancelled"]),
        c("distractor", "what is Maya's new phone number", &["maya_phone"]),
        // Expansion can only add words the model knows; this one the fake does not, so neither
        // mode finds the note. A real model may; the live run says.
        c("lexicon_miss", "how much do the paid tiers charge", &["pricing"]),
    ]
}

/// Capture the corpus; return the database and `event:<id>` → fixture label.
fn seeded_db() -> (Db, HashMap<String, &'static str>) {
    let now = now_ms();
    let db = Db::open_in_memory(Arc::new(move || now)).unwrap();
    let mut labels = HashMap::new();
    for (i, f) in corpus().into_iter().enumerate() {
        let hash = format!("ai-search-eval-{i}");
        let (id, _) = db
            .capture(&NewEvent {
                ts: local_midnight_ms(f.day, 0) + 10 * 3_600_000,
                source: "capture",
                kind: "text",
                app_bundle_id: Some(f.app),
                window_title: Some("Evidence"),
                content: f.content,
                content_hash: &hash,
                dwell_ms: 5_000,
                display_id: None,
                window_bounds: None,
            })
            .unwrap();
        labels.insert(format!("event:{id}"), f.label);
    }
    (db, labels)
}

fn active_plan() -> Entitlements {
    let now = now_ms() as u64;
    entitlements(Plan::Trial { started_at_ms: Some(now) }, now)
}

/// A bilingual concept lexicon. Each group is one meaning; any member matches it.
const CONCEPTS: &[&[&str]] = &[
    &["delay", "postpone", "push", "slip", "defer", "延期", "遅らせ", "後ろ倒し"],
    &["launch", "release", "go-live", "rollout", "ローンチ", "リリース", "公開"],
    &["decide", "decision", "agreed", "agree", "approved", "決定", "決め", "合意"],
    &["budget", "spend", "cost", "予算", "費用"],
    &["cut", "reduce", "reduced", "削減", "減ら"],
    &["hire", "hiring", "offer accepted", "joins", "採用", "入社"],
    &["designer", "ui/ux", "デザイナー"],
    &["outage", "incident", "502", "障害", "停止"],
    &["payments", "payment", "checkout", "決済"],
    &["contract", "renewal", "契約", "更新"],
    &["acme"],
    &["design review", "デザインレビュー"],
    &["invoice", "請求書", "請求"],
    &["hosting", "linode", "fly.io", "server", "サーバー", "ホスティング"],
    &["migrate", "migrated", "moved away", "move away", "移行"],
    &["mobile app", "iphone", "companion app", "モバイルアプリ", "アプリ"],
    &["idea", "brainstorm", "アイデア", "構想"],
    &["cancel", "called off", "scrapped", "中止", "キャンセル"],
    &["offsite", "オフサイト", "合宿"],
    &["tokyo", "東京"],
    &["phone number", "contact changed", "電話番号", "携帯"],
    &["maya", "マヤ"],
];

/// `term` occurs in `text` (lowercased) at a word start for ASCII terms, anywhere for CJK.
fn has_term(text: &str, term: &str) -> bool {
    if !term.is_ascii() {
        return text.contains(term);
    }
    text.match_indices(term).any(|(at, _)| {
        text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}

fn concepts_in(text: &str) -> Vec<usize> {
    let lower = text.to_lowercase();
    (0..CONCEPTS.len())
        .filter(|&i| CONCEPTS[i].iter().any(|term| has_term(&lower, term)))
        .collect()
}

/// The local-date window a question names, if any.
fn date_phrase(question: &str, today: CivilDate) -> Option<(CivilDate, CivilDate)> {
    let lower = question.to_lowercase();
    let today_days = days_from_civil(today);
    if lower.contains("last week") || lower.contains("先週") {
        // Monday-based weeks: 1970-01-05 was a Monday.
        let monday = today_days - (today_days - 4).rem_euclid(7);
        return Some((civil_from_days(monday - 7), civil_from_days(monday - 1)));
    }
    if lower.contains("last month") || lower.contains("先月") {
        let (year, month) = if today.month == 1 { (today.year - 1, 12) } else { (today.year, today.month - 1) };
        let first = CivilDate { year, month, day: 1 };
        let next_first = CivilDate { year: today.year, month: today.month, day: 1 };
        return Some((first, civil_from_days(days_from_civil(next_first) - 1)));
    }
    if lower.contains("yesterday") || lower.contains("昨日") {
        return Some((civil_from_days(today_days - 1), civil_from_days(today_days - 1)));
    }
    None
}

fn today_from(system: &str) -> CivilDate {
    let at = system.find("Today is ").unwrap() + "Today is ".len();
    parse_iso_date(&system[at..at + 10]).unwrap()
}

/// Deterministic stand-in for the user's model: reads the prompts the pipeline built and answers
/// in the contracted JSON. Expansion = the synonyms of every concept in the question (+ a date
/// range for "last week" / 「先月」 …); rerank = excerpts covering all but at most one of the
/// question's concepts, ordered by coverage, then by whether their date falls in the named window.
struct ThesaurusModel;

impl SearchModel for ThesaurusModel {
    fn complete(&self, step: ModelStep, system: &str, user: &str, _deadline: Instant) -> Result<String, LlmError> {
        let today = today_from(system);
        let question = user.lines().next().unwrap().strip_prefix("Question: ").unwrap();
        let concepts = concepts_in(question);
        let window = date_phrase(question, today);
        match step {
            ModelStep::Expand => {
                let queries: Vec<String> = concepts.iter().map(|&i| CONCEPTS[i].join(" ")).take(6).collect();
                let date_range = window.map(|(from, to)| {
                    serde_json::json!({
                        "from": shogun_core::ai_search::dates::format_date(from),
                        "to": shogun_core::ai_search::dates::format_date(to),
                    })
                });
                Ok(serde_json::json!({"queries": queries, "date_range": date_range}).to_string())
            }
            ModelStep::Rerank => {
                let mut scored: Vec<(usize, bool, usize, String)> = Vec::new();
                for (position, line) in user.lines().filter(|l| l.starts_with("[c")).enumerate() {
                    let id = line[1..line.find(']').unwrap()].to_string();
                    let matched = concepts_in(line).iter().filter(|c| concepts.contains(c)).count();
                    let relevant = !concepts.is_empty() && matched >= 1 && matched + 1 >= concepts.len();
                    if !relevant {
                        continue;
                    }
                    let in_window = window.is_some_and(|(from, to)| {
                        let at = line.find("] ").unwrap() + 2;
                        parse_iso_date(&line[at..at + 10]).is_some_and(|d| d >= from && d <= to)
                    });
                    scored.push((matched, in_window, position, id));
                }
                scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
                let ranked: Vec<String> = scored.into_iter().map(|s| s.3).collect();
                Ok(serde_json::json!({"ranked": ranked}).to_string())
            }
        }
    }
}

struct Outcome {
    keyword: Vec<Vec<String>>,
    ai: Vec<Vec<String>>,
    statuses: Vec<&'static str>,
    model_calls: usize,
    candidates_sent: usize,
    elapsed_ms: Vec<u64>,
}

fn evaluate<M: SearchModel + ?Sized>(model: &M, bounds: &AiSearchBounds) -> Outcome {
    let (db, labels) = seeded_db();
    let plan = active_plan();
    let policy = ExclusionPolicy::new();
    let label = |key: &str| labels.get(key).map_or_else(|| key.to_string(), |l| l.to_string());
    let mut out = Outcome {
        keyword: Vec::new(),
        ai: Vec::new(),
        statuses: Vec::new(),
        model_calls: 0,
        candidates_sent: 0,
        elapsed_ms: Vec::new(),
    };
    for case in cases() {
        // (i) today's keyword search, exactly as the AI keyword phase runs it.
        let searcher = db.ai_keyword_searcher(&policy);
        let keyword = searcher.search(case.question, None, bounds.keyword_limit).unwrap();
        out.keyword.push(keyword.iter().map(|h| label(&h.key)).collect());
        // (ii) expansion + rerank.
        let response = db.ai_search(&plan, &policy, Some(model), case.question, bounds).unwrap();
        out.ai.push(response.hits.iter().map(|h| label(&h.hit.key)).collect());
        out.statuses.push(response.status.code());
        out.model_calls += response.model_calls;
        out.candidates_sent += response.candidates_sent;
        out.elapsed_ms.push(response.elapsed_ms);
    }
    out
}

fn table(outcome: &Outcome) -> (String, Vec<(String, Scores, Scores)>) {
    let cases = cases();
    let relevant: Vec<Vec<String>> =
        cases.iter().map(|c| c.relevant.iter().map(|s| s.to_string()).collect()).collect();
    let mut categories: Vec<&str> = Vec::new();
    for case in &cases {
        if !categories.contains(&case.category) {
            categories.push(case.category);
        }
    }
    let scores_for = |runs: &[Vec<String>], filter: &dyn Fn(&Case) -> bool| {
        score(
            cases
                .iter()
                .enumerate()
                .filter(|(_, c)| filter(c))
                .map(|(i, _)| (runs[i].as_slice(), relevant[i].as_slice())),
        )
    };
    let mut rows = Vec::new();
    for category in &categories {
        let f = |c: &Case| c.category == *category;
        rows.push((category.to_string(), scores_for(&outcome.keyword, &f), scores_for(&outcome.ai, &f)));
    }
    let all = |_: &Case| true;
    rows.push(("**all**".to_string(), scores_for(&outcome.keyword, &all), scores_for(&outcome.ai, &all)));
    (render_table(&rows), rows)
}

fn per_case_report(outcome: &Outcome) -> String {
    let mut out = String::new();
    for (i, case) in cases().iter().enumerate() {
        let rank = |run: &[String]| {
            run.iter()
                .position(|k| case.relevant.contains(&k.as_str()))
                .map_or("-".to_string(), |p| (p + 1).to_string())
        };
        out.push_str(&format!(
            "  {:<15} keyword rank {:>2}  ai rank {:>2}  [{}]\n",
            case.category,
            rank(&outcome.keyword[i]),
            rank(&outcome.ai[i]),
            outcome.statuses[i],
        ));
    }
    out
}

#[test]
fn fake_model_eval_improves_retrieval_without_regressing_exact_lookups() {
    let bounds = AiSearchBounds::default();
    let outcome = evaluate(&ThesaurusModel, &bounds);
    let (rendered, rows) = table(&outcome);
    println!("AI search eval — FAKE deterministic model (plumbing proof, not a model measurement)\n{rendered}");
    println!("{}", per_case_report(&outcome));
    println!(
        "model calls {} for {} cases; mean candidates sent {:.1}",
        outcome.model_calls,
        outcome.ai.len(),
        outcome.candidates_sent as f64 / outcome.ai.len() as f64
    );

    let (_, keyword_all, ai_all) = rows.last().unwrap();
    assert!(ai_all.recall_at_5 > keyword_all.recall_at_5, "AI search must retrieve more at 5");
    assert!(ai_all.mrr > keyword_all.mrr, "AI search must rank relevant evidence higher");
    for (category, keyword, ai) in &rows {
        assert!(ai.recall_at_10 >= keyword.recall_at_10, "{category}: recall@10 regressed");
        if category == "exact" {
            assert!(ai.recall_at_5 >= keyword.recall_at_5 && ai.mrr >= keyword.mrr, "exact lookups regressed");
        }
    }
    // Exact-only questions never reached the model.
    for (case, status) in cases().iter().zip(&outcome.statuses) {
        if case.question == "INV-2026-0042" || case.question.starts_with("https://") {
            assert_eq!(*status, "exact_query");
        }
    }
}

/// The same cases through a real provider. Opt-in only:
///
/// - `SHOGUN_AI_SEARCH_EVAL_LIVE=anthropic` with `SHOGUN_AI_SEARCH_EVAL_ANTHROPIC_KEY` (a key the
///   owner provides for this run; deliberately not `ANTHROPIC_API_KEY`, so an ambient key is
///   never picked up) and optionally `SHOGUN_AI_SEARCH_EVAL_MODEL` (default: the catalog's budget
///   model). Needs the `net` feature.
/// - `SHOGUN_AI_SEARCH_EVAL_LIVE=claude-code` / `codex` / `gemini-cli`: the signed-in vendor CLI
///   on the user's own subscription (the delegation lane).
///
/// `cargo test -p shogun-core --features exec --test ai_search_eval -- --ignored --nocapture`
#[test]
#[ignore = "live provider eval: set SHOGUN_AI_SEARCH_EVAL_LIVE (docs/specs/issue-940-ai-search.md)"]
fn live_provider_eval() {
    let Ok(mode) = std::env::var("SHOGUN_AI_SEARCH_EVAL_LIVE") else {
        println!("live AI search eval skipped: SHOGUN_AI_SEARCH_EVAL_LIVE is not set");
        return;
    };
    let bounds = AiSearchBounds::default();
    let outcome = match live::model(&mode) {
        Some(model) => evaluate(model.as_ref(), &bounds),
        None => {
            println!("live AI search eval skipped: mode `{mode}` unavailable in this build or missing its key");
            return;
        }
    };
    let (rendered, _) = table(&outcome);
    println!("AI search eval — LIVE provider `{mode}`\n{rendered}");
    println!("{}", per_case_report(&outcome));
    let mut elapsed = outcome.elapsed_ms.clone();
    elapsed.sort_unstable();
    let pct = |p: f64| elapsed[((elapsed.len() as f64 - 1.0) * p).round() as usize];
    println!("refine latency ms: p50 {} p95 {} (model calls {})", pct(0.5), pct(0.95), outcome.model_calls);
}

mod live {
    use shogun_core::ai_search::{AgentSearchModel, SearchModel, TRACE_PURPOSE};
    use shogun_core::llm::subscription::{Delegate, ProcessRunner, SubscriptionAgentClient};
    use shogun_core::llm::traceability::RecordingSink;

    pub fn model(mode: &str) -> Option<Box<dyn SearchModel>> {
        if let Some(delegate) = Delegate::parse(mode) {
            let client = || {
                SubscriptionAgentClient::new(ProcessRunner, RecordingSink::new(), delegate)
                    .with_purpose(TRACE_PURPOSE)
            };
            return Some(Box::new(AgentSearchModel { expand: client(), rerank: client() }));
        }
        if mode == "anthropic" {
            return anthropic();
        }
        None
    }

    #[cfg(feature = "net")]
    fn anthropic() -> Option<Box<dyn SearchModel>> {
        use shogun_core::llm::anthropic::{AnthropicAgentClient, AnthropicConfig};
        use shogun_core::llm::transport::ReqwestTransport;
        use shogun_core::llm::{AgentClient, ByokKey, LlmError, Secret};

        struct Blocking {
            rt: tokio::runtime::Runtime,
            client: AnthropicAgentClient<ReqwestTransport, RecordingSink>,
        }
        impl AgentClient for Blocking {
            fn complete(&self, prompt: &str) -> Result<String, LlmError> {
                self.rt.block_on(self.client.complete(prompt))
            }
            fn complete_split(&self, system: &str, user: &str) -> Result<String, LlmError> {
                self.rt.block_on(self.client.complete_split(system, user))
            }
        }

        let key = std::env::var("SHOGUN_AI_SEARCH_EVAL_ANTHROPIC_KEY").ok()?;
        let model = std::env::var("SHOGUN_AI_SEARCH_EVAL_MODEL")
            .unwrap_or_else(|_| shogun_core::model_catalog::budget_model("anthropic").to_string());
        let client = || -> Option<Blocking> {
            Some(Blocking {
                rt: tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?,
                client: AnthropicAgentClient::new(
                    ReqwestTransport::new().ok()?,
                    RecordingSink::new(),
                    ByokKey::new(Secret::new(key.clone())),
                    AnthropicConfig::new(model.clone()).with_max_tokens(512),
                )
                .with_purpose(TRACE_PURPOSE),
            })
        };
        Some(Box::new(AgentSearchModel { expand: client()?, rerank: client()? }))
    }

    #[cfg(not(feature = "net"))]
    fn anthropic() -> Option<Box<dyn SearchModel>> {
        None
    }
}

#[test]
fn the_fake_model_is_deterministic() {
    let bounds = AiSearchBounds::default();
    let a = evaluate(&ThesaurusModel, &bounds);
    let b = evaluate(&ThesaurusModel, &bounds);
    assert_eq!(a.ai, b.ai);
    assert_eq!(a.keyword, b.keyword);
    // Dates are resolved against the database clock, not the machine's.
    assert_eq!(local_date(now_ms(), 0), date(2026, 10, 5));
}
