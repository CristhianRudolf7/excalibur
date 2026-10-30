//! Skill groups and the deterministic rules that select the relevant ones.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GroupOrigin {
    /// Shipped and versioned with the harness.
    Integrated,
    /// Written or obtained externally; active only after audit and evaluation.
    Curated,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupDefinition {
    pub id: String,
    pub name_en: String,
    pub name_es: String,
    pub description: String,
    pub origin: GroupOrigin,
}
const GROUPS: [(&str, &str, &str, &str, bool); 22] = [
    (
        "planning",
        "Planning",
        "Planificación",
        "Define requirements and design before implementing.",
        true,
    ),
    (
        "verification",
        "Completion and requirement verification",
        "Finalización y verificación de requisitos",
        "Check the state of the work before declaring it done.",
        true,
    ),
    (
        "communication",
        "Communication and feedback",
        "Comunicación y retroalimentación",
        "Give the user useful, well-founded answers.",
        true,
    ),
    (
        "delegation",
        "Subagents",
        "Subagentes",
        "Delegate and coordinate work between specialized agents.",
        true,
    ),
    (
        "memory",
        "Memory and relationships",
        "Memoria y relaciones",
        "Organize and retrieve the context of the user and their interactions.",
        true,
    ),
    (
        "debugging",
        "Debugging",
        "Depuración",
        "Investigate errors and find their causes before fixing them.",
        true,
    ),
    (
        "testing",
        "Testing",
        "Pruebas",
        "Drive implementation through tests of the expected behavior.",
        true,
    ),
    (
        "software-engineering",
        "Software engineering",
        "Ingeniería de software",
        "Analyze code and work with development and deployment platforms.",
        false,
    ),
    (
        "design",
        "Interface design",
        "Diseño de interfaces",
        "Apply good design practice to web, app, presentation and image interfaces.",
        false,
    ),
    (
        "security",
        "Security",
        "Seguridad",
        "Audit code and systems to find and verify vulnerabilities.",
        false,
    ),
    (
        "google-cloud",
        "Google Cloud",
        "Google Cloud",
        "Work with Google products and technologies.",
        false,
    ),
    (
        "personal-assistant",
        "Personal assistant and productivity",
        "Asistente personal y productividad",
        "Manage the user's communications, calendar and errands with their authorization.",
        false,
    ),
    (
        "research",
        "Research and analysis",
        "Investigación y análisis",
        "Research, cross-check, monitor and analyze information.",
        false,
    ),
    (
        "browser",
        "Browser and sources",
        "Navegador y fuentes",
        "Operate the browser and consult public sources.",
        false,
    ),
    (
        "documents",
        "Documents and formats",
        "Documentos y formatos",
        "Work with documents, spreadsheets, presentations, PDF and office suites.",
        false,
    ),
    (
        "diagrams",
        "Diagrams, charts and images",
        "Diagramas, gráficas e imágenes",
        "Create diagrams and visual artifacts.",
        false,
    ),
    (
        "automation",
        "Automation",
        "Automatización",
        "Create durable automations and triggers.",
        false,
    ),
    (
        "creative-tools",
        "Creative and technical applications",
        "Aplicaciones creativas y técnicas",
        "Operate applications with their own interface through their tools.",
        false,
    ),
    (
        "science",
        "Scientific research",
        "Investigación científica",
        "Support research in different scientific fields.",
        false,
    ),
    (
        "marketing",
        "Marketing",
        "Marketing",
        "Plan and run marketing work for a product, its audience and positioning.",
        false,
    ),
    (
        "compliance",
        "Regulatory compliance",
        "Cumplimiento normativo",
        "Check legal and contractual obligations without replacing professional advice.",
        false,
    ),
    (
        "finance",
        "Finance",
        "Finanzas",
        "Organize and analyze the user's finances without replacing professional advice.",
        false,
    ),
];
pub fn builtin_groups() -> Vec<GroupDefinition> {
    GROUPS
        .iter()
        .map(|(id, en, es, description, integrated)| GroupDefinition {
            id: (*id).into(),
            name_en: (*en).into(),
            name_es: (*es).into(),
            description: (*description).into(),
            origin: if *integrated {
                GroupOrigin::Integrated
            } else {
                GroupOrigin::Curated
            },
        })
        .collect()
}
#[derive(Clone, Debug)]
pub struct GroupRegistry {
    groups: BTreeMap<String, GroupDefinition>,
}
impl Default for GroupRegistry {
    fn default() -> Self {
        Self {
            groups: builtin_groups()
                .into_iter()
                .map(|g| (g.id.clone(), g))
                .collect(),
        }
    }
}
impl GroupRegistry {
    /// Add a new group (for learned or installed skills) with its description.
    pub fn register(&mut self, group: GroupDefinition) -> Result<(), String> {
        if !crate::format::valid_name(&group.id)
            || group.name_en.trim().is_empty()
            || group.name_es.trim().is_empty()
            || group.description.trim().is_empty()
            || group.description.chars().count() > 256
        {
            return Err(
                "group needs a valid id, both names and a 1-256 character description".into(),
            );
        }
        if self.groups.contains_key(&group.id) {
            return Err(format!("group {} already exists", group.id));
        }
        self.groups.insert(group.id.clone(), group);
        Ok(())
    }
    pub fn get(&self, id: &str) -> Option<&GroupDefinition> {
        self.groups.get(id)
    }
    pub fn all(&self) -> impl Iterator<Item = &GroupDefinition> {
        self.groups.values()
    }
}

/// Kind of work, decided by deterministic triage before the catalog is built.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Programming,
    Conversation,
    Planning,
    Research,
    /// Triage could not decide; the decider may resolve relevant groups.
    Unknown,
}
/// Inputs of the deterministic relevance rules.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogScope {
    pub task: TaskKind,
    /// Role profile (`coordinator`, `implementer`, `verifier`...).
    pub role: String,
    /// Groups enabled by the project settings.
    #[serde(default)]
    pub project_groups: Vec<String>,
    /// Model executing the session, for per-model evidence.
    #[serde(default)]
    pub model: Option<String>,
}
impl CatalogScope {
    pub fn new(task: TaskKind, role: &str) -> Self {
        Self {
            task,
            role: role.into(),
            project_groups: Vec::new(),
            model: None,
        }
    }
}
/// Groups listed in full for this scope.
pub fn relevant_groups(scope: &CatalogScope) -> BTreeSet<String> {
    let mut groups: BTreeSet<String> = match scope.task {
        TaskKind::Programming => vec![
            "planning",
            "software-engineering",
            "debugging",
            "testing",
            "verification",
        ],
        TaskKind::Conversation => vec!["communication", "memory", "personal-assistant"],
        TaskKind::Planning => vec!["planning", "verification", "communication"],
        TaskKind::Research => vec!["research", "browser", "documents", "communication"],
        TaskKind::Unknown => vec![],
    }
    .into_iter()
    .map(String::from)
    .collect();
    let role: &[&str] = match scope.role.as_str() {
        "coordinator" | "master" | "interlocutor" => &["delegation", "planning", "communication"],
        "planner" => &["planning"],
        "implementer" | "integrator" => &["software-engineering", "testing", "debugging"],
        "verifier" | "reviewer" => &["verification", "testing"],
        "researcher" => &["research"],
        "security-auditor" => &["security"],
        "browser-operator" => &["browser"],
        _ => &[],
    };
    groups.extend(role.iter().map(|g| g.to_string()));
    groups.extend(scope.project_groups.iter().cloned());
    groups
}
