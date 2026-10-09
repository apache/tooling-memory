//! A harness that would orchestrate subagents, told to file them on the task board
//! instead. Claude Code's `Agent` tool spawns one; its `Workflow` tool runs a script of
//! `agent()` calls; Codex's `spawn_agent` spawns one. Each becomes a task on the ledger,
//! born proposed, and the call itself is refused with the task ids, so the harness
//! carries on with its own part and the board runs the rest. The orchestration the
//! script would have done, the fan-out, the waits and the merges, is the board's: a
//! task's result comes back as a handoff, not as a return value.

use serde_json::Value;

/// One task the harness wanted a subagent for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    /// One line.
    pub title: String,
    /// The prompt the subagent would have been given, in Markdown.
    pub body: String,
}

/// Whether this tool is a harness spawning or orchestrating subagents: Claude Code's
/// `Agent`, `Task` and `Workflow`, Codex's `spawn_agent`, which its hooks name with
/// the `collaboration` prefix of its tool family.
#[must_use]
pub fn delegates(tool: &str) -> bool {
    matches!(tool, "Agent" | "Task" | "Workflow") || tool.ends_with("spawn_agent")
}

/// Whether a prompt is one the hook cannot read: Codex hands its hooks the subagent's
/// message encrypted, a token with no spaces where a prompt would have them.
#[must_use]
pub fn opaque(text: &str) -> bool {
    let t = text.trim();
    t.len() > 60 && !t.contains(char::is_whitespace)
}

/// The first line of a prompt, cut to a title's length.
fn title_of(text: &str) -> String {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let first = first.trim_start_matches('#').trim();
    if first.chars().count() <= 80 {
        return first.to_string();
    }
    let mut t: String = first.chars().take(77).collect();
    t.push_str("...");
    t
}

fn text_in<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| input.get(k).and_then(Value::as_str))
        .filter(|t| !t.trim().is_empty())
}

/// The tasks a tool call asks for. An `Agent` or `spawn_agent` call is one; a
/// `Workflow` script is one per `agent()` call whose prompt is a string literal, with
/// its `label` as the title when it has one, and the whole script as one task when no
/// prompt can be read out of it.
#[must_use]
pub fn units_of(tool: &str, input: &Value) -> Vec<Unit> {
    match tool {
        "Workflow" => {
            let script = text_in(input, &["script"]).unwrap_or("");
            let mut units = agents_in(script);
            if units.is_empty() && !script.trim().is_empty() {
                let name = text_in(input, &["name"]).unwrap_or("workflow");
                units.push(Unit {
                    title: format!("Run the workflow `{name}`"),
                    body: format!(
                        "The harness asked for this workflow to run. Its script:\n\n```js\n{}\n```",
                        script.trim()
                    ),
                });
            }
            units
        }
        _ => {
            let Some(prompt) = text_in(
                input,
                &["prompt", "message", "task", "instructions", "input"],
            ) else {
                return Vec::new();
            };
            if opaque(prompt) {
                return Vec::new();
            }
            let title = text_in(input, &["description", "label", "title", "task_name"])
                .map_or_else(|| title_of(prompt), str::to_string);
            vec![Unit {
                title,
                body: prompt.trim().to_string(),
            }]
        }
    }
}

/// Every `agent(<string literal>, {label: ...})` in a workflow script, in order.
fn agents_in(script: &str) -> Vec<Unit> {
    let mut out = Vec::new();
    let bytes = script.as_bytes();
    let mut i = 0;
    while let Some(at) = script[i..].find("agent(") {
        let start = i + at + "agent(".len();
        i = start;
        // not part of a longer identifier, such as `subagent(`
        if i > "agent(".len() {
            let before = bytes[start - "agent(".len() - 1];
            if before.is_ascii_alphanumeric() || before == b'_' || before == b'.' {
                continue;
            }
        }
        let rest = &script[start..];
        let rest_trim = rest.trim_start();
        let Some(quote) = rest_trim
            .chars()
            .next()
            .filter(|c| matches!(c, '`' | '\'' | '"'))
        else {
            continue;
        };
        let Some((literal, after)) = string_literal(&rest_trim[1..], quote) else {
            continue;
        };
        let label = label_in(after);
        let prompt = literal.trim();
        if prompt.is_empty() {
            continue;
        }
        out.push(Unit {
            title: label.unwrap_or_else(|| title_of(prompt)),
            body: prompt.to_string(),
        });
    }
    out
}

/// The literal up to the closing `quote`, escapes kept as written, and what follows it.
fn string_literal(s: &str, quote: char) -> Option<(&str, &str)> {
    let mut escaped = false;
    for (idx, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
        } else if c == quote {
            return Some((&s[..idx], &s[idx + c.len_utf8()..]));
        }
    }
    None
}

/// A `label: <string literal>` in the options that follow the prompt, before the call's
/// closing parenthesis at this nesting depth.
fn label_in(after: &str) -> Option<String> {
    let mut depth = 0i32;
    let mut end = after.len();
    for (idx, c) in after.char_indices() {
        match c {
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => {
                if depth == 0 {
                    end = idx;
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let options = &after[..end];
    let at = options.find("label")?;
    let rest = options[at + "label".len()..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let quote = rest
        .chars()
        .next()
        .filter(|c| matches!(c, '`' | '\'' | '"'))?;
    let (literal, _) = string_literal(&rest[1..], quote)?;
    let label = literal.trim();
    (!label.is_empty()).then(|| label.to_string())
}

/// What the harness is told instead of a spawned agent.
#[must_use]
pub fn refusal(ids: &[String]) -> String {
    let list = ids.join(", ");
    format!(
        "Filed on the task board as {list}. Subagents are not run from here: the board claims each task into a session of its own and its result comes back as a handoff. Carry on with your own part of the work, and leave a handoff when it depends on theirs."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_call_is_one_task_titled_by_its_description() {
        let u = units_of(
            "Agent",
            &serde_json::json!({"description": "Audit the egress", "prompt": "Read crates/egress and list every place a header is copied.", "subagent_type": "general-purpose"}),
        );
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].title, "Audit the egress");
        assert!(u[0].body.starts_with("Read crates/egress"));
        assert!(units_of("Agent", &serde_json::json!({"description": "x"})).is_empty());
        assert!(delegates("Workflow") && delegates("spawn_agent") && !delegates("Read"));
        assert!(
            delegates("collaborationspawn_agent"),
            "Codex's hooks name it so"
        );
        assert!(!delegates("collaborationwait_agent"));
        // Codex's hook payload carries the message encrypted: nothing to file from it
        assert!(units_of(
            "collaborationspawn_agent",
            &serde_json::json!({"task_name": "count", "message": "gAAAAABqyWPr0akHx_T6lZyV61yB0ak14LVbK2jYfWPjcPcrgzxsFr987qcjZQjoqOawv0vPYnVdseTnPfT7Dk6R8YYiJDtCrM"})
        )
        .is_empty());
        assert!(!opaque("List the crates and count them."));
    }

    #[test]
    fn a_workflow_script_is_one_task_per_agent_call() {
        let script = r#"export const meta = { name: 'review', description: 'd' }
const DIMENSIONS = [{key: 'bugs'}]
const results = await pipeline(
  DIMENSIONS,
  d => agent(`Review ${d.key} in crates/egress`, {label: `review:${d.key}`, phase: 'Review'}),
  review => parallel(review.findings.map(f => () =>
    agent("Adversarially verify: is it real?", {label: "verify", schema: S})))
)
const other = subagent('not this one')
return results"#;
        let u = units_of(
            "Workflow",
            &serde_json::json!({"script": script, "name": "review"}),
        );
        assert_eq!(u.len(), 2, "{u:?}");
        assert_eq!(u[0].title, "review:${d.key}");
        assert_eq!(u[0].body, "Review ${d.key} in crates/egress");
        assert_eq!(u[1].title, "verify");
        assert_eq!(u[1].body, "Adversarially verify: is it real?");
    }

    #[test]
    fn a_script_with_no_readable_prompt_is_one_task_holding_the_script() {
        let u = units_of(
            "Workflow",
            &serde_json::json!({"script": "const p = prompts[0]\nawait agent(p)", "name": "wf"}),
        );
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].title, "Run the workflow `wf`");
        assert!(u[0].body.contains("await agent(p)"));
        assert!(units_of("Workflow", &serde_json::json!({})).is_empty());
    }

    #[test]
    fn a_long_first_line_is_cut_to_a_title() {
        let long = "word ".repeat(30);
        let u = units_of("spawn_agent", &serde_json::json!({"message": long}));
        assert_eq!(u[0].title.chars().count(), 80);
        assert!(u[0].title.ends_with("..."));
        assert!(refusal(&["task-1".into(), "task-2".into()]).contains("task-1, task-2"));
    }
}
