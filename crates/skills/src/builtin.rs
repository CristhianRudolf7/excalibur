//! Integrated skills shipped and versioned with the harness.

macro_rules! builtin {
    ($($name:literal),* $(,)?) => {
        /// (folder name, SKILL.md content) for every integrated skill.
        pub const BUILTIN_SKILLS: &[(&str, &str)] = &[
            $(($name, include_str!(concat!("../builtin/", $name, "/SKILL.md")))),*
        ];
    };
}
builtin!(
    "request-interview",
    "idea-refinement",
    "spec-writing",
    "quality-criteria",
    "task-breakdown",
    "plan-stress-test",
    "flow-simulation",
    "task-verification",
    "common-pitfalls",
    "review-feedback",
    "user-preferences",
    "report-formatting",
    "progress-updates",
    "voice-responses",
    "subagent-delegation",
    "task-skill-matching",
    "memory-graph-modeling",
    "context-provisioning",
    "memory-curation",
    "root-cause-first",
    "systematic-debugging",
    "fault-isolation",
    "test-driven-development",
    "test-planning",
);
