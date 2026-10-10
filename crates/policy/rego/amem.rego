# The checkable perimeter, as the paper bounds it (§5, §10, §12): who may file a task
# and whether it is admitted as filed, and the gates on writes whose blast radius is
# not routine. Judgement is never encoded here; what is here is what a machine can
# check. The same rules run through regorus on a laptop and through OPA at scale, with
# the grants file as `data.grants` and one request as `input`.
#
# input:
#   action      file_task | write | erase | read | promote
#   principal   {id, class}           class: owner | human | agent | tap | system
#   domain      the domain acted on
#   filing      own | held | session   (file_task) the system on the owner's behalf, a
#                                      session filing from a task it holds, a session
#                                      filing on its own account
#   task        {requirements, origin} (file_task)
#   parent      {requirements}         (file_task, filing == held) the task held
#   write       {supersedes, supersedes_tier, supersedes_author, session_supersedes}
#   promote     {phase, to, author, proposer}  phase: direct | propose | approve
#
# decision: {allow, admit, review, reason}
#   review: none | human | approver    who has to look before this goes further

package amem

import rego.v1

class := input.principal.class

domain_granted if data.grants.domains[input.domain]

admits_own if data.grants.domains[input.domain].admit_own

gates := object.union(
	{"max_supersedes_per_session": 20, "verified_needs_approver": true},
	object.get(data.grants, "gates", {}),
)

# ---- filing and admission: creation is cheap and ungated, admission is not ----

needs_human if "human" in input.task.requirements

# A task with requirements its filer could not itself hold is targeting, not work:
# a principal may not attach requirements it was not granted, and the legitimate
# exception, a residual for capabilities the filer lacks, is triage's to admit.
inflated if {
	input.filing == "held"
	some r in input.task.requirements
	not r in input.parent.requirements
}

auto_admit if {
	input.filing == "own"
	admits_own
}

auto_admit if {
	input.filing == "held"
	admits_own
	not inflated
}

review_of := "human" if needs_human

review_of := "none" if not needs_human

file_decision := {"allow": false, "admit": false, "review": "none", "reason": sprintf("domain %v is not granted", [input.domain])} if {
	not domain_granted
}

file_decision := {"allow": true, "admit": false, "review": "human", "reason": "requirements the filer does not hold itself; left proposed for triage"} if {
	domain_granted
	inflated
}

file_decision := {"allow": true, "admit": true, "review": review_of, "reason": "admitted as filed: the domain admits its own"} if {
	domain_granted
	not inflated
	auto_admit
}

file_decision := {"allow": true, "admit": false, "review": review_of, "reason": "born proposed; the owner admits"} if {
	domain_granted
	not inflated
	not auto_admit
}

# ---- writes: the gates scale with the blast radius ----

# Verified is the review gate's tier; a proposer never approves its own, so an entry
# that would replace a verified one is a proposal for an approver, not a write.
blocked_verified if {
	gates.verified_needs_approver
	input.write.supersedes_tier == "verified"
	class != "owner"
}

# A session that keeps superseding is a curation pass with a blast radius; past the
# gate the rest is a task for the owner, and everything before it stays reversible.
blocked_blast if {
	class == "agent"
	input.write.supersedes != null
	input.write.session_supersedes >= gates.max_supersedes_per_session
}

# What the scan found in the body is not knowledge: instructions to the reader, a
# broadcast to agents, tool syntax, hidden text, a payload, a secret. From anyone but
# the owner it is quarantined for the owner to look at, never served.
blocked_injection if {
	class != "owner"
	count(input.write.findings) > 0
}

write_decision := {"allow": false, "admit": false, "review": "human", "reason": sprintf("quarantined: the body carries %v, which is not knowledge; the owner decides whether it enters the record", [concat(", ", input.write.findings)])} if {
	blocked_injection
}

write_decision := {"allow": false, "admit": false, "review": "approver", "reason": sprintf("`%v` is verified; replacing it needs an approver who is not the proposer. File it as a task instead", [input.write.supersedes])} if {
	not blocked_injection
	blocked_verified
}

write_decision := {"allow": false, "admit": false, "review": "human", "reason": sprintf("blast radius: this session has superseded %v entries and the gate is %v; the rest is the owner's to admit", [input.write.session_supersedes, gates.max_supersedes_per_session])} if {
	not blocked_injection
	not blocked_verified
	blocked_blast
}

write_decision := {"allow": true, "admit": false, "review": "none", "reason": "within the gates"} if {
	not blocked_injection
	not blocked_verified
	not blocked_blast
}

# ---- erasure: irreversible, the owner only ----

erase_decision := {"allow": true, "admit": false, "review": "none", "reason": "the owner erases"} if class == "owner"

erase_decision := {"allow": false, "admit": false, "review": "human", "reason": "erasure is irreversible and the owner's alone"} if class != "owner"

# ---- reads: the session's filter, evaluated once and pushed into the index ----

# What a session may read, as the conditions on an entry's attributes: its domain, its
# classification at most `handling`, its trust tier at least `min_tier`. The server
# evaluates this once per session and the index applies it inside every query, so a
# denied entry is never scored and counts are right; this is the residual OPA's partial
# evaluation would compile, written out, since regorus does not partially evaluate. It
# can only narrow the grants: the server intersects it with them. The built-in rule
# leaves them as they are; a root's own policy narrows, for example agents to public:
#
#   read_filter := object.union(base_filter, {"handling": "public"}) if class == "agent"
base_filter := {
	"domains": [d | some d, _ in data.grants.domains],
	"handling": data.grants.handling,
	"min_tier": "raw",
}

default read_filter := {"domains": [], "handling": "public", "min_tier": "verified"}

read_filter := base_filter

# ---- promotion: raw to curated to verified ----

# Curated means a librarian has summarised, merged or checked an entry: anyone but its
# author may say so, and the owner always may. Verified is the review gate: it is
# proposed, which files a task for a person, and approved by a person who did not
# propose it. An agent never approves. This is the paper's defence against an exploit
# found once becoming fleet-wide best practice: a librarian can propose but never
# approve its own promotion.
promotes_own if input.promote.author == input.principal.id

promote_decision := {"allow": true, "admit": false, "review": "none", "reason": "curated by someone other than its author"} if {
	input.promote.phase == "direct"
	input.promote.to == "curated"
	not promotes_own
}

promote_decision := {"allow": true, "admit": false, "review": "none", "reason": "curated by the owner"} if {
	input.promote.phase == "direct"
	input.promote.to == "curated"
	promotes_own
	class == "owner"
}

promote_decision := {"allow": false, "admit": false, "review": "none", "reason": "an author does not curate its own entry; a librarian or the owner does"} if {
	input.promote.phase == "direct"
	input.promote.to == "curated"
	promotes_own
	class != "owner"
}

promote_decision := {"allow": false, "admit": false, "review": "approver", "reason": "verified is proposed and approved, never set directly: use phase propose"} if {
	input.promote.phase == "direct"
	input.promote.to == "verified"
}

promote_decision := {"allow": true, "admit": true, "review": "approver", "reason": "proposed for verified; a person who did not propose it approves"} if {
	input.promote.phase == "propose"
	input.promote.to == "verified"
}

promote_decision := {"allow": false, "admit": false, "review": "approver", "reason": "the proposer does not approve its own promotion"} if {
	input.promote.phase == "approve"
	input.promote.proposer == input.principal.id
}

promote_decision := {"allow": false, "admit": false, "review": "approver", "reason": "an agent does not approve a promotion to verified; a person does"} if {
	input.promote.phase == "approve"
	input.promote.proposer != input.principal.id
	not class in {"owner", "human"}
}

promote_decision := {"allow": true, "admit": false, "review": "none", "reason": "approved by a person who did not propose it"} if {
	input.promote.phase == "approve"
	input.promote.proposer != input.principal.id
	class in {"owner", "human"}
}

default promote_decision := {"allow": false, "admit": false, "review": "none", "reason": "no promotion rule for this request"}

default base_decision := {"allow": false, "admit": false, "review": "none", "reason": "no rule for this action"}

base_decision := promote_decision if input.action == "promote"

base_decision := file_decision if input.action == "file_task"

base_decision := write_decision if input.action == "write"

base_decision := erase_decision if input.action == "erase"

# ---- the record's rules: modules from verified policy entries ----

# A verified policy entry's module lives in `package amem.rules` and contributes
# `deny contains {"policy": <id>, "reason": <why>}` for a request. A denial wins over
# the decision above unless an exception in force covers it: an exception entry,
# verified and unexpired, that breaches that policy and is scoped to the task this
# session holds. An L0 rule takes no exception. Every decision says which policies it
# cites, the denials or the exceptions used: the paper's citation and override rates.
core_policy(pid) if {
	some p in data.policies
	p.id == pid
	p.layer == 0
}

excepted(pid) if {
	not core_policy(pid)
	some e in data.exceptions
	e.breaches == pid
	e.scope_task == input.held_task
}

rule_denials contains d if {
	some d in data.amem.rules.deny
	not excepted(d.policy)
}

exceptions_used contains x if {
	some d in data.amem.rules.deny
	excepted(d.policy)
	some ex in data.exceptions
	ex.breaches == d.policy
	ex.scope_task == input.held_task
	x := ex.id
}

used_list := [x | some x in exceptions_used]

denied_list := [p | some d in rule_denials; p := d.policy]

denied_reasons := [r | some d in rule_denials; r := sprintf("%v: %v", [d.policy, d.reason])]

decision := object.union(base_decision, {"cites": used_list}) if {
	count(rule_denials) == 0
}

decision := {
	"allow": false,
	"admit": false,
	"review": "human",
	"reason": concat("; ", denied_reasons),
	"cites": denied_list,
} if {
	count(rule_denials) > 0
}
