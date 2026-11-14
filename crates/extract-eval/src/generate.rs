//! The deterministic SynthDay generator (FR-EV-02, model-free slice).
//!
//! `generate(spec)` is a pure function of the spec: a seed picks templates and pool names, fills
//! the template slots, spaces the events through the persona's morning and afternoon, and emits
//! the gold rows the templates declare. People and projects are derived from the text: every pool
//! name the day's events mention becomes a gold row evidenced by exactly those events.
//!
//! No model, no clock, no file read, no environment: the only inputs are the spec and the
//! constants in [`crate::templates`]. That is the structural guarantee that real data cannot enter
//! a golden day. The LLM-written SynthDay the issue describes (Batch lane, 10 personas) is a later
//! slice and will need its own guard — see docs/extract-eval.md §5.

use std::collections::{BTreeMap, BTreeSet};

use shogun_memory::due::{civil_from_days, days_from_civil, CivilDate};

use crate::golden::{BandLabel, Event, GeneratorInfo, GoldItem, Persona, SynthDay, Table, FORMAT};
use crate::rng::SplitMix64;
use crate::templates::{
    GoldTpl, Lang, PersonaDef, Template, EN_WEEKDAYS, JA_WEEKDAYS, PERSONAS, TEMPLATES,
    TEMPLATE_SET,
};

/// What a golden day is generated from.
#[derive(Debug, Clone, Copy)]
pub struct DaySpec {
    pub id: &'static str,
    pub seed: u64,
    /// Index into [`PERSONAS`].
    pub persona: usize,
    pub date: CivilDate,
    /// Minimum number of events (a multi-event template may overshoot by one).
    pub min_events: usize,
    /// First event id; events are numbered upward from here.
    pub id_base: i64,
    /// Templates that must appear (traps, bug repros), before seeded picks fill the rest.
    pub required: &'static [&'static str],
}

/// The checked-in golden days. Adding a day: append a spec, run
/// `cargo run -p extract-eval -- regen`, and commit the new file.
pub const GOLDEN_SPECS: &[DaySpec] = &[
    DaySpec {
        id: "day-01-eng-lead",
        seed: 317_001,
        persona: 0,
        date: CivilDate { year: 2026, month: 3, day: 2 },
        min_events: 16,
        id_base: 1_001,
        required: &["restated_commit", "commit_theirs_chat", "trap_quoted_example"],
    },
    DaySpec {
        id: "day-02-ops-traps",
        seed: 317_002,
        persona: 1,
        date: CivilDate { year: 2026, month: 3, day: 3 },
        min_events: 16,
        id_base: 2_001,
        required: &[
            "zwsp_commit",
            "trap_job_ad",
            "trap_chrome",
            "trap_let_me_know",
            "trap_hedge",
            "commit_mine_meeting",
            "loop_followup_note",
        ],
    },
    DaySpec {
        id: "day-03-ja-pm",
        seed: 317_003,
        persona: 2,
        date: CivilDate { year: 2026, month: 3, day: 4 },
        min_events: 12,
        id_base: 3_001,
        required: &["ja_review", "ja_trap_quoted", "ja_commit_theirs", "ja_decision", "ja_reply", "ja_commit_mine"],
    },
    DaySpec {
        id: "day-04-mixed",
        seed: 317_004,
        persona: 0,
        date: CivilDate { year: 2026, month: 3, day: 5 },
        min_events: 0,
        id_base: 4_001,
        required: &["mix_commit_mine", "mix_commit_theirs", "mix_waiting", "mix_trap_quoted"],
    },
    // The dashboard noise reported on 2026-10-13 (`dash_` templates), one day per language.
    DaySpec {
        id: "day-05-ja-dashboard-noise",
        seed: 317_005,
        persona: 2,
        date: CivilDate { year: 2026, month: 3, day: 10 },
        min_events: 0,
        id_base: 5_001,
        required: &[
            "dash_trap_ja_greeting",
            "dash_trap_ja_request_closing",
            "dash_trap_ja_marketing",
            "dash_trap_ja_pr_rows",
            "dash_trap_ja_agent_rows",
            "dash_ja_commit_fix_mention",
            "dash_ja_commit_humble",
            "dash_ja_review_cc",
            "dash_ja_reply_offer",
        ],
    },
    DaySpec {
        id: "day-06-en-dashboard-noise",
        seed: 317_006,
        persona: 0,
        date: CivilDate { year: 2026, month: 3, day: 11 },
        min_events: 0,
        id_base: 6_001,
        required: &[
            "dash_trap_announcement",
            "dash_trap_pr_rows",
            "dash_trap_agent_rows",
            "dash_commit_theirs_to_you",
        ],
    },
];

/// Why a spec could not be generated. Only a malformed spec can fail — never the seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenError {
    UnknownPersona(usize),
    UnknownTemplate(String),
    WrongLanguage(String),
    EmptyPool,
}

impl std::fmt::Display for GenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenError::UnknownPersona(i) => write!(f, "no persona at index {i}"),
            GenError::UnknownTemplate(k) => write!(f, "no template named {k}"),
            GenError::WrongLanguage(k) => write!(f, "template {k} is not in the persona's language"),
            GenError::EmptyPool => write!(f, "persona has an empty pool"),
        }
    }
}

impl std::error::Error for GenError {}

/// Weekday of a civil date, Monday = 0.
fn weekday(days: i64) -> usize {
    // 1970-01-01 (day 0) was a Thursday (index 3).
    (days + 3).rem_euclid(7) as usize
}

fn fmt_date(d: CivilDate) -> String {
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

/// Slot values for one template instance.
struct Slots {
    values: BTreeMap<&'static str, String>,
}

impl Slots {
    fn render(&self, pattern: &str) -> String {
        let mut out = pattern.to_string();
        for (k, v) in &self.values {
            out = out.replace(&format!("{{{k}}}"), v);
        }
        out
    }
}

fn pick_slots(
    rng: &mut SplitMix64,
    persona: &PersonaDef,
    today: i64,
) -> Result<Slots, GenError> {
    let n_people = persona.people.len();
    if n_people < 2 || persona.projects.is_empty() || persona.artifacts.is_empty() {
        return Err(GenError::EmptyPool);
    }
    let i = rng.below(n_people);
    let j = (i + 1 + rng.below(n_people - 1)) % n_people;
    let (p, p2) = (persona.people[i], persona.people[j]);
    let project = rng.pick(persona.projects).copied().unwrap_or_default();
    let artifact = rng.pick(persona.artifacts).copied().unwrap_or_default();
    let channel = rng.pick(persona.channels).copied().unwrap_or("general");
    // A weekday strictly later in the same working week, so "by <weekday>" has one reading.
    let wd_today = weekday(today);
    let ahead = if wd_today >= 4 { 1 } else { rng.range(1, (4 - wd_today) as i64) };
    let due_days = today + ahead;
    let wd_name = match persona.lang {
        Lang::En => EN_WEEKDAYS[weekday(due_days)],
        Lang::Ja => JA_WEEKDAYS[weekday(due_days)],
    };
    let mut values = BTreeMap::new();
    values.insert("person", p.name.to_string());
    values.insert("first", p.short.to_string());
    values.insert("email", p.email.to_string());
    // Derived from the email, not drawn: adding the slot leaves every existing day byte-identical.
    values.insert("handle", p.email.split('@').next().unwrap_or_default().to_string());
    values.insert("person2", p2.name.to_string());
    values.insert("me", persona.short.to_string());
    values.insert("org", persona.org.to_string());
    values.insert("project", project.to_string());
    values.insert("artifact", artifact.to_string());
    values.insert("channel", channel.to_string());
    values.insert("pr", rng.range(120, 980).to_string());
    values.insert("weekday", wd_name.to_string());
    values.insert("due", fmt_date(civil_from_days(due_days)));
    values.insert("due_tomorrow", fmt_date(civil_from_days(today + 1)));
    Ok(Slots { values })
}

/// Generate one golden day from its spec. Pure and deterministic.
pub fn generate(spec: &DaySpec) -> Result<SynthDay, GenError> {
    let persona = PERSONAS.get(spec.persona).ok_or(GenError::UnknownPersona(spec.persona))?;
    let today = days_from_civil(spec.date);
    let mut rng = SplitMix64::new(spec.seed);

    // 1. Which templates: the required ones, then seeded picks (each template at most twice).
    let lang_pool: Vec<&'static Template> =
        TEMPLATES.iter().filter(|t| t.lang == persona.lang && !t.required_only()).collect();
    let mut chosen: Vec<&'static Template> = Vec::new();
    for key in spec.required {
        let t = crate::templates::template(key)
            .ok_or_else(|| GenError::UnknownTemplate((*key).to_string()))?;
        if t.lang != persona.lang {
            return Err(GenError::WrongLanguage((*key).to_string()));
        }
        chosen.push(t);
    }
    let count = |chosen: &[&Template]| chosen.iter().map(|t| t.events.len()).sum::<usize>();
    let mut guard = 0;
    while count(&chosen) < spec.min_events && guard < 1_000 {
        guard += 1;
        let Some(t) = rng.pick(&lang_pool).copied() else { return Err(GenError::EmptyPool) };
        if chosen.iter().filter(|c| c.key == t.key).count() >= 2 {
            continue;
        }
        chosen.push(t);
    }
    rng.shuffle(&mut chosen);

    // 2. Fill slots. Re-roll an instance whose gold description repeats an earlier one, so two
    //    gold rows never share a key.
    let mut instances: Vec<(&'static Template, Slots)> = Vec::new();
    let mut seen_keys: BTreeSet<String> = BTreeSet::new();
    for t in chosen {
        let mut slots = pick_slots(&mut rng, persona, today)?;
        for _ in 0..16 {
            let keys: Vec<String> = t
                .gold
                .iter()
                .map(|g| match g {
                    GoldTpl::Commitment { description, .. } | GoldTpl::OpenLoop { description, .. } => {
                        slots.render(description)
                    }
                })
                .collect();
            if keys.iter().all(|k| !seen_keys.contains(k)) {
                seen_keys.extend(keys);
                break;
            }
            slots = pick_slots(&mut rng, persona, today)?;
        }
        instances.push((t, slots));
    }

    // 3. Emit events (spaced 7–38 minutes apart from 09:00 local) and template gold rows.
    let day_start_utc_ms = today * 86_400_000 - i64::from(persona.utc_offset_secs) * 1_000;
    let mut minute: i64 = 9 * 60;
    let mut next_id = spec.id_base;
    let mut events = Vec::new();
    let mut gold: Vec<GoldItem> = Vec::new();
    let row = |table, fields, expected_band, evidence| GoldItem {
        id: String::new(),
        table,
        fields,
        expected_band,
        evidence,
    };
    for (t, slots) in &instances {
        let mut ids = Vec::new();
        for e in t.events {
            minute += rng.range(7, 38);
            events.push(Event {
                id: next_id,
                ts_ms: day_start_utc_ms + minute * 60_000,
                local_time: format!("{:02}:{:02}", minute / 60, minute % 60),
                source: e.source.to_string(),
                app: slots.render(e.app),
                window: slots.render(e.window),
                text: slots.render(e.text),
                template: t.key.to_string(),
            });
            ids.push(next_id);
            next_id += 1;
        }
        // A fact evidenced by more than one event is the High band's case (FR-ST-20/21).
        let band = if ids.len() > 1 { BandLabel::High } else { BandLabel::Medium };
        for g in t.gold {
            let (table, fields) = match g {
                GoldTpl::Commitment { direction, description, due } => {
                    let mut f = BTreeMap::new();
                    f.insert("direction".to_string(), (*direction).to_string());
                    f.insert("description".to_string(), slots.render(description));
                    if let Some(d) = due {
                        f.insert("due".to_string(), slots.render(d));
                    }
                    (Table::Commitments, f)
                }
                GoldTpl::OpenLoop { kind, description } => {
                    let mut f = BTreeMap::new();
                    f.insert("kind".to_string(), (*kind).to_string());
                    f.insert("description".to_string(), slots.render(description));
                    (Table::OpenLoops, f)
                }
            };
            gold.push(row(table, fields, band, ids.clone()));
        }
    }

    // 4. People and projects: every pool name the day mentions, evidenced by those events.
    let mentions = |needles: &[&str]| -> Vec<i64> {
        events
            .iter()
            .filter(|e| needles.iter().any(|n| mentions_name(&e.text, n)))
            .map(|e| e.id)
            .collect()
    };
    for p in persona.people {
        let ids = mentions(&[p.name, p.short]);
        if !ids.is_empty() {
            let band = if ids.len() > 1 { BandLabel::High } else { BandLabel::Medium };
            let mut f = BTreeMap::new();
            f.insert("name".to_string(), p.name.to_string());
            gold.push(row(Table::People, f, band, ids));
        }
    }
    for project in persona.projects {
        let ids = mentions(&[project]);
        if !ids.is_empty() {
            let band = if ids.len() > 1 { BandLabel::High } else { BandLabel::Medium };
            let mut f = BTreeMap::new();
            f.insert("name".to_string(), (*project).to_string());
            gold.push(row(Table::Projects, f, band, ids));
        }
    }

    for (i, g) in gold.iter_mut().enumerate() {
        g.id = format!("g{:02}", i + 1);
    }

    Ok(SynthDay {
        format: FORMAT.to_string(),
        id: spec.id.to_string(),
        generator: GeneratorInfo {
            seed: spec.seed,
            template_set: TEMPLATE_SET.to_string(),
            required_templates: spec.required.iter().map(|k| (*k).to_string()).collect(),
        },
        persona: Persona {
            id: persona.id.to_string(),
            name: persona.name.to_string(),
            role: persona.role.to_string(),
            org: persona.org.to_string(),
            lang: persona.lang.as_str().to_string(),
            utc_offset_secs: persona.utc_offset_secs,
        },
        date: fmt_date(spec.date),
        events,
        gold,
    })
}

/// Whether `text` mentions `name`: for ASCII names, as a whole word (so "Oren" does not match
/// "Lorenzo"); for Japanese names, as a substring (no word boundaries to respect).
pub fn mentions_name(text: &str, name: &str) -> bool {
    if !name.is_ascii() {
        return text.contains(name);
    }
    let mut start = 0;
    while let Some(pos) = text[start..].find(name) {
        let at = start + pos;
        let end = at + name.len();
        let before = text[..at].chars().next_back();
        let after = text[end..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
        if boundary(before) && boundary(after) {
            return true;
        }
        start = at + name.len().max(1);
    }
    false
}

/// Generate every golden day.
pub fn generate_all() -> Result<Vec<SynthDay>, GenError> {
    GOLDEN_SPECS.iter().map(generate).collect()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn weekday_of_known_dates() {
        // 2026-03-02 is a Monday.
        assert_eq!(weekday(days_from_civil(CivilDate { year: 2026, month: 3, day: 2 })), 0);
        assert_eq!(weekday(0), 3);
    }

    #[test]
    fn generation_is_deterministic() {
        for spec in GOLDEN_SPECS {
            assert_eq!(generate(spec), generate(spec));
        }
    }

    #[test]
    fn different_seeds_differ() {
        let a = generate(&GOLDEN_SPECS[0]).expect("day");
        let b = generate(&DaySpec { seed: 1, ..GOLDEN_SPECS[0] }).expect("day");
        assert_ne!(a.events, b.events);
    }

    #[test]
    fn whole_word_mentions() {
        assert!(mentions_name("Hi Oren,", "Oren"));
        assert!(!mentions_name("Lorenzo", "Oren"));
        assert!(mentions_name("佐伯さん", "佐伯"));
    }

    #[test]
    fn unknown_required_template_is_an_error() {
        let spec = DaySpec { required: &["no_such_template"], ..GOLDEN_SPECS[0] };
        assert_eq!(generate(&spec), Err(GenError::UnknownTemplate("no_such_template".into())));
    }
}
