//! How long the brief of a recall is, and how much the recall reads for it. The caller
//! says what kind of answer it needs and how many tokens its context already holds, and
//! [`plan_brief`] gives the plan: the length of the brief and the body tokens of the pack.
//! The plan is a function of those two values and of fixed constants. It reads no entry
//! and calls no model. A recall that names neither value has no plan and is served as a
//! recall was before a plan existed.
//!
//! The plan binds the part of the recall that goes into the caller's context. With a
//! deputy, that part is the brief: the deputy reads the pack under the full budget, and
//! the prompt tells it the kind and the length. With no deputy, that part is the pack:
//! the body tokens of the pack stay under the plan.

use crate::recall::Budget;
use serde::{Deserialize, Serialize};

/// The brief of a fact, in tokens.
pub const FACT_BRIEF_TOKENS: usize = 150;
/// The brief of a list, in tokens.
pub const LIST_BRIEF_TOKENS: usize = 400;
/// The brief of an overview, in tokens.
pub const OVERVIEW_BRIEF_TOKENS: usize = 900;
/// The body tokens of the pack that a fact is answered from.
pub const FACT_PACK_TOKENS: usize = 1_500;
/// The body tokens of the pack that a list is answered from.
pub const LIST_PACK_TOKENS: usize = 3_000;
/// The body tokens of the pack that an overview is answered from.
pub const OVERVIEW_PACK_TOKENS: usize = 6_000;
/// A context that holds more tokens than this gets half the brief and half the pack.
pub const CONTEXT_FULL: usize = 50_000;
/// A context that holds more tokens than this gets a quarter of the brief and of the pack.
pub const CONTEXT_VERY_FULL: usize = 150_000;

/// The kind of answer that the caller of a recall needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerKind {
    /// One fact: one or two sentences.
    Fact,
    /// A short list: one line for each item.
    List,
    /// An overview: paragraphs.
    Overview,
}

impl AnswerKind {
    /// The form of the brief, in words for the one that writes it.
    #[must_use]
    pub fn form(self) -> &'static str {
        match self {
            AnswerKind::Fact => "one or two sentences",
            AnswerKind::List => "a short list, one line for each item",
            AnswerKind::Overview => "paragraphs",
        }
    }

    /// The kind with its article, for a sentence.
    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            AnswerKind::Fact => "a fact",
            AnswerKind::List => "a list",
            AnswerKind::Overview => "an overview",
        }
    }

    /// The brief tokens and the pack tokens of this kind, for a context that is not full.
    fn sizes(self) -> (usize, usize) {
        match self {
            AnswerKind::Fact => (FACT_BRIEF_TOKENS, FACT_PACK_TOKENS),
            AnswerKind::List => (LIST_BRIEF_TOKENS, LIST_PACK_TOKENS),
            AnswerKind::Overview => (OVERVIEW_BRIEF_TOKENS, OVERVIEW_PACK_TOKENS),
        }
    }
}

impl std::str::FromStr for AnswerKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fact" => Ok(AnswerKind::Fact),
            "list" => Ok(AnswerKind::List),
            "overview" => Ok(AnswerKind::Overview),
            other => Err(format!(
                "answer kind {other:?} is not fact, list or overview"
            )),
        }
    }
}

impl std::fmt::Display for AnswerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AnswerKind::Fact => "fact",
            AnswerKind::List => "list",
            AnswerKind::Overview => "overview",
        })
    }
}

/// What the caller of a recall says about itself. Each value is optional.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ask {
    /// The tokens that the context of the caller holds now, approximately.
    pub context_tokens: Option<usize>,
    /// The kind of answer that the caller needs.
    pub answer_kind: Option<AnswerKind>,
}

/// The size of one recall: what [`plan_brief`] chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefPlan {
    /// The kind of answer: the one that the caller named, or an overview.
    pub answer_kind: AnswerKind,
    /// The tokens that the context of the caller holds, when the caller said so.
    pub context_tokens: Option<usize>,
    /// The brief stays under this many tokens.
    pub brief_tokens: usize,
    /// The bodies of the pack stay under this many tokens when the pack goes to the caller.
    pub pack_tokens: usize,
}

impl BriefPlan {
    /// The plan as one word of the read log:
    /// `<kind>/context=<n or none>/brief=<n>/pack=<n>`.
    #[must_use]
    pub fn choice(&self) -> String {
        let context = self
            .context_tokens
            .map_or_else(|| "none".to_string(), |n| n.to_string());
        format!(
            "{}/context={context}/brief={}/pack={}",
            self.answer_kind, self.brief_tokens, self.pack_tokens
        )
    }

    /// One sentence for the one that writes the brief: the form and the length.
    #[must_use]
    pub fn sentence(&self) -> String {
        format!(
            "The caller needs {}: write the brief as {}. Keep the brief under {} tokens.",
            self.answer_kind.noun(),
            self.answer_kind.form(),
            self.brief_tokens
        )
    }
}

/// The plan of a recall. `None` when the caller named no kind and no context: that recall
/// has no plan. A caller that names only its context gets an overview. A context that
/// holds more than [`CONTEXT_FULL`] tokens gets half the brief and half the pack, and one
/// that holds more than [`CONTEXT_VERY_FULL`] gets a quarter. The pack is never above
/// `budget.tokens`.
#[must_use]
pub fn plan_brief(ask: Ask, budget: Budget) -> Option<BriefPlan> {
    if ask == Ask::default() {
        return None;
    }
    let answer_kind = ask.answer_kind.unwrap_or(AnswerKind::Overview);
    let (brief, pack) = answer_kind.sizes();
    let part = match ask.context_tokens {
        Some(n) if n > CONTEXT_VERY_FULL => 4,
        Some(n) if n > CONTEXT_FULL => 2,
        _ => 1,
    };
    Some(BriefPlan {
        answer_kind,
        context_tokens: ask.context_tokens,
        brief_tokens: brief / part,
        pack_tokens: (pack / part).min(budget.tokens),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::{prompt_for, to_json, Depth, Deputy, Recall, Synthesis};
    use crate::{reads, Memory, WriteRequest};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    const KINDS: [AnswerKind; 3] = [AnswerKind::Fact, AnswerKind::List, AnswerKind::Overview];
    /// Contexts from empty to far above [`CONTEXT_VERY_FULL`], in rising order.
    const CONTEXTS: [usize; 9] = [
        0,
        1_000,
        CONTEXT_FULL,
        CONTEXT_FULL + 1,
        100_000,
        CONTEXT_VERY_FULL,
        CONTEXT_VERY_FULL + 1,
        400_000,
        usize::MAX,
    ];
    const QUESTION: &str = "how does the import job run";

    fn ask(kind: Option<AnswerKind>, context: Option<usize>) -> Ask {
        Ask {
            context_tokens: context,
            answer_kind: kind,
        }
    }

    fn plan(kind: AnswerKind, context: Option<usize>) -> BriefPlan {
        plan_brief(ask(Some(kind), context), Budget::default()).unwrap()
    }

    #[test]
    fn a_recall_that_names_no_kind_and_no_context_has_no_plan() {
        assert_eq!(plan_brief(Ask::default(), Budget::default()), None);
        let only_context = plan_brief(ask(None, Some(10)), Budget::default()).unwrap();
        assert_eq!(only_context.answer_kind, AnswerKind::Overview);
        assert_eq!(only_context.context_tokens, Some(10));
        let only_kind = plan_brief(ask(Some(AnswerKind::List), None), Budget::default()).unwrap();
        assert_eq!(
            (only_kind.answer_kind, only_kind.context_tokens),
            (AnswerKind::List, None)
        );
    }

    #[test]
    fn a_fuller_context_gets_no_longer_brief_and_no_larger_pack() {
        for kind in KINDS {
            let mut last = plan(kind, None);
            for context in CONTEXTS {
                let p = plan(kind, Some(context));
                assert!(
                    p.brief_tokens <= last.brief_tokens && p.pack_tokens <= last.pack_tokens,
                    "{kind} at {context}: {p:?} after {last:?}"
                );
                last = p;
            }
        }
        // the steps, for one kind: whole, half above the first bound, a quarter above
        // the second
        let sizes = |context| {
            let p = plan(AnswerKind::List, Some(context));
            (p.brief_tokens, p.pack_tokens)
        };
        assert_eq!(sizes(CONTEXT_FULL), (400, 3_000));
        assert_eq!(sizes(CONTEXT_FULL + 1), (200, 1_500));
        assert_eq!(sizes(CONTEXT_VERY_FULL), (200, 1_500));
        assert_eq!(sizes(CONTEXT_VERY_FULL + 1), (100, 750));
    }

    #[test]
    fn a_fact_gets_no_more_than_a_list_and_a_list_no_more_than_an_overview() {
        for context in [None].into_iter().chain(CONTEXTS.map(Some)) {
            let [fact, list, overview] = KINDS.map(|k| plan(k, context));
            assert!(
                fact.brief_tokens <= list.brief_tokens
                    && list.brief_tokens <= overview.brief_tokens,
                "{context:?}"
            );
            assert!(
                fact.pack_tokens <= list.pack_tokens && list.pack_tokens <= overview.pack_tokens,
                "{context:?}"
            );
        }
        let fact = plan(AnswerKind::Fact, None);
        let overview = plan(AnswerKind::Overview, None);
        assert_eq!((fact.brief_tokens, fact.pack_tokens), (150, 1_500));
        assert_eq!((overview.brief_tokens, overview.pack_tokens), (900, 6_000));
    }

    #[test]
    fn the_pack_of_a_plan_is_never_above_the_budget() {
        for tokens in [0, 1, 700, 1_500, 5_999, 6_000, 100_000] {
            let budget = Budget {
                tokens,
                ..Budget::default()
            };
            for kind in KINDS {
                for context in [None].into_iter().chain(CONTEXTS.map(Some)) {
                    let p = plan_brief(ask(Some(kind), context), budget).unwrap();
                    assert!(p.pack_tokens <= tokens, "{p:?} under {tokens}");
                }
            }
        }
    }

    #[test]
    fn an_unknown_answer_kind_does_not_parse() {
        for kind in KINDS {
            assert_eq!(kind.to_string().parse::<AnswerKind>(), Ok(kind));
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{kind}\""));
        }
        for unknown in ["essay", "Fact", "", "facts"] {
            assert_eq!(
                unknown.parse::<AnswerKind>(),
                Err(format!(
                    "answer kind {unknown:?} is not fact, list or overview"
                )),
            );
        }
        assert!(serde_json::from_str::<AnswerKind>("\"essay\"").is_err());
        assert_eq!(
            plan(AnswerKind::Fact, Some(60_000)).choice(),
            "fact/context=60000/brief=75/pack=750"
        );
        assert_eq!(
            plan(AnswerKind::Overview, None).choice(),
            "overview/context=none/brief=900/pack=6000"
        );
    }

    /// A deputy that gives one answer and keeps each prompt.
    struct Keeps {
        prompts: Mutex<Vec<String>>,
    }

    impl Deputy for Keeps {
        fn model(&self) -> String {
            "keeps-1".into()
        }

        fn complete<'a>(
            &'a self,
            prompt: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.prompts.lock().unwrap().push(prompt.to_string());
                Ok("{\"brief\": \"It runs at night.\", \"citations\": [], \"missing\": []}".into())
            })
        }
    }

    /// A root with six entries that [`QUESTION`] finds. Each body has more than 600
    /// tokens, so six bodies are more than the pack of a fact and less than the default
    /// budget.
    async fn shelf(dir: &std::path::Path) -> Memory {
        let m = Memory::init(dir, "human:test:me", "dom-me").unwrap();
        for n in 0..6 {
            let filler = "the import job run takes a step ".repeat(70);
            m.write(WriteRequest {
                title: Some(format!("Import job run, part {n}")),
                body: format!("{filler}\n"),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        }
        m
    }

    fn with_deputy(m: &Memory) -> (Arc<Keeps>, Arc<dyn Deputy>) {
        let keeps = Arc::new(Keeps {
            prompts: Mutex::new(Vec::new()),
        });
        let strong: Arc<dyn Deputy> = keeps.clone();
        m.set_deputy(&strong);
        (keeps, strong)
    }

    fn recall_lines(dir: &std::path::Path) -> Vec<reads::Read> {
        let all = reads::all(dir).unwrap();
        all.into_iter().filter(|r| r.tool == "recall").collect()
    }

    async fn recall_with(m: &Memory, ask: Ask) -> Recall {
        m.recall_for(QUESTION, None, None, Budget::default(), ask)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_recall_owed_for_a_fact_carries_no_more_bodies_than_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let m = shelf(dir.path()).await;
        let plain = m
            .recall(QUESTION, None, None, Budget::default())
            .await
            .unwrap();
        assert!(
            plain.pack.body_tokens() > FACT_PACK_TOKENS,
            "the shelf is larger than the pack of a fact: {}",
            plain.pack.body_tokens()
        );
        assert_eq!(plain.plan, None);
        let fact = recall_with(&m, ask(Some(AnswerKind::Fact), None)).await;
        assert_eq!(fact.synthesis, Synthesis::Owed);
        assert_eq!(fact.plan, Some(plan(AnswerKind::Fact, None)));
        let carried = fact.pack.body_tokens();
        assert!(
            carried > 0 && carried <= FACT_PACK_TOKENS,
            "{carried} body tokens"
        );
        assert!(fact.pack.truncated, "the pack says that bodies are missing");
        assert_eq!(fact.pack.ids(), plain.pack.ids(), "the same hits");
        let next = to_json(&fact)["next"].as_str().unwrap().to_string();
        assert!(
            next.ends_with(
                "The caller needs a fact. Write one or two sentences, under 150 tokens."
            ),
            "{next}"
        );
        assert_eq!(to_json(&fact)["plan"]["answer_kind"], "fact");
        let lines = recall_lines(dir.path());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].via.as_deref(), Some("pack:sections"));
        assert_eq!(
            lines[1].choice.as_deref(),
            Some("fact/context=none/brief=150/pack=1500")
        );
        assert!(
            lines[1].tokens < lines[0].tokens,
            "a smaller pack was served"
        );
    }

    #[tokio::test]
    async fn a_recall_without_an_ask_is_served_and_logged_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let m = shelf(dir.path()).await;
        let plain = m
            .recall(QUESTION, None, None, Budget::default())
            .await
            .unwrap();
        let empty = recall_with(&m, Ask::default()).await;
        assert_eq!(plain.plan, None);
        assert_eq!(empty.pack, plain.pack, "the same pack");
        let v = to_json(&plain);
        assert!(v.get("plan").is_none(), "{v}");
        assert!(v["next"].as_str().unwrap().ends_with("warrants it."));
        // the read-log line has the keys that it had, and no choice
        let text = std::fs::read_to_string(reads::path(dir.path())).unwrap();
        let line = text.lines().find(|l| l.contains("\"recall\"")).unwrap();
        assert!(!line.contains("choice"), "{line}");
        let lines = recall_lines(dir.path());
        assert_eq!(lines.len(), 2);
        for l in &lines {
            assert_eq!(l.via.as_deref(), Some("pack:sections"));
            assert_eq!(l.choice, None);
            assert_eq!(
                (l.hits, l.tokens),
                (plain.pack.pieces.len(), plain.pack.tokens)
            );
        }

        // with a deputy: the prompt is the prompt of a pack with no plan
        let pack = m
            .pack(
                &[QUESTION.to_string()],
                None,
                Depth::Sections,
                Budget::default(),
            )
            .await
            .unwrap();
        let (keeps, _strong) = with_deputy(&m);
        let brief = m
            .recall(QUESTION, None, None, Budget::default())
            .await
            .unwrap();
        assert_eq!(brief.plan, None);
        let prompts = keeps.prompts.lock().unwrap().clone();
        assert_eq!(prompts, [prompt_for(QUESTION, &pack, None)]);
        let before = format!(
            "{}\n\n## Question\n\n{QUESTION}\n\n## Entries\n",
            crate::recall::INSTRUCTIONS
        );
        assert!(prompts[0].starts_with(&before), "{}", &prompts[0][..900]);
        let last = recall_lines(dir.path()).pop().unwrap();
        assert_eq!((last.via.as_deref(), last.choice), (Some("brief"), None));
        assert!(to_json(&brief).get("plan").is_none());
    }

    #[tokio::test]
    async fn a_deputy_reads_the_whole_pack_and_its_prompt_holds_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let m = shelf(dir.path()).await;
        let whole = m
            .pack(
                &[QUESTION.to_string()],
                None,
                Depth::Sections,
                Budget::default(),
            )
            .await
            .unwrap();
        let (keeps, _strong) = with_deputy(&m);
        let r = recall_with(&m, ask(Some(AnswerKind::Fact), Some(60_000))).await;
        let planned = plan(AnswerKind::Fact, Some(60_000));
        assert_eq!(r.plan, Some(planned));
        assert_eq!(
            r.synthesis,
            Synthesis::Server {
                model: "keeps-1".into()
            }
        );
        let prompts = keeps.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 1);
        let prompt = &prompts[0];
        assert!(prompt.contains("write the brief as one or two sentences"));
        assert!(prompt.contains("Keep the brief under 75 tokens."));
        let instructions_end = prompt.find("## Question").unwrap();
        assert!(
            prompt.find("Keep the brief under").unwrap() < instructions_end,
            "the sentence follows the instructions"
        );
        assert!(
            whole.body_tokens() > planned.pack_tokens,
            "the whole pack is larger than the pack of the plan: {}",
            whole.body_tokens()
        );
        assert!(
            *prompt == prompt_for(QUESTION, &whole, Some(&planned)),
            "the deputy read the pack of the full budget, not one cut to {} tokens",
            planned.pack_tokens
        );
        let line = recall_lines(dir.path()).pop().unwrap();
        assert_eq!(line.via.as_deref(), Some("brief"));
        assert_eq!(
            line.choice.as_deref(),
            Some("fact/context=60000/brief=75/pack=750")
        );
        let next = to_json(&r)["next"].as_str().unwrap().to_string();
        assert!(
            !next.contains("under 75 tokens"),
            "the brief is written: {next}"
        );

        // each kind has its own form in the prompt
        for (kind, form) in [
            (AnswerKind::List, "a short list, one line for each item"),
            (AnswerKind::Overview, "paragraphs"),
        ] {
            recall_with(&m, ask(Some(kind), None)).await;
            let prompts = keeps.prompts.lock().unwrap();
            let prompt = prompts.last().unwrap();
            let p = plan(kind, None);
            assert!(prompt.contains(&format!("write the brief as {form}.")));
            assert!(prompt.contains(&format!("under {} tokens.", p.brief_tokens)));
        }
    }
}
