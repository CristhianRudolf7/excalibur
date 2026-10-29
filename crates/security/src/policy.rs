use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Autonomy {
    Consultative,
    #[default]
    Supervised,
    Autonomous,
    Full,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum Network {
    #[default]
    None,
    Domains(BTreeSet<String>),
    Full,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Root {
    pub path: PathBuf,
    pub write: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityToken {
    pub roots: Vec<Root>,
    pub network: Network,
    pub processes: bool,
    pub sandbox_profile: Option<String>,
    pub secrets: BTreeSet<String>,
    pub external_actions: BTreeSet<String>,
    pub data_scopes: BTreeSet<String>,
    /// Maximum admitted tool calls for this execution; model tokens/cost/time
    /// retain their separate, more restrictive engine budget limits.
    pub budget: u64,
}
impl CapabilityToken {
    pub fn validate(&self) -> Result<(), String> {
        for root in &self.roots {
            if !root.path.is_absolute()
                || root.path.canonicalize().map_err(|e| e.to_string())? != root.path
            {
                return Err("capability roots must be existing canonical absolute paths".into());
            }
        }
        if self.processes && self.sandbox_profile.as_ref().is_none_or(|s| s.is_empty()) {
            return Err("process capability requires a sandbox profile".into());
        }
        if self
            .secrets
            .iter()
            .any(|s| !s.starts_with("secret://") || s.len() <= 9)
        {
            return Err("secret capabilities require references".into());
        }
        Ok(())
    }
    pub fn contains(&self, child: &Self) -> bool {
        child.roots.iter().all(|r| {
            self.roots
                .iter()
                .any(|p| r.path.starts_with(&p.path) && (!r.write || p.write))
        }) && network_contains(&self.network, &child.network)
            && (!child.processes || self.processes && child.sandbox_profile == self.sandbox_profile)
            && child.secrets.is_subset(&self.secrets)
            && child.external_actions.is_subset(&self.external_actions)
            && child.data_scopes.is_subset(&self.data_scopes)
            && child.budget <= self.budget
    }
    pub fn merge(&mut self, grant: &Self) {
        self.roots.extend(grant.roots.clone());
        self.processes |= grant.processes;
        if grant.processes {
            self.sandbox_profile = grant.sandbox_profile.clone();
        }
        self.network = match (&self.network, &grant.network) {
            (Network::Full, _) | (_, Network::Full) => Network::Full,
            (Network::Domains(a), Network::Domains(b)) => {
                Network::Domains(a.union(b).cloned().collect())
            }
            (Network::None, b) => b.clone(),
            (a, Network::None) => a.clone(),
        };
        self.secrets.extend(grant.secrets.clone());
        self.external_actions.extend(grant.external_actions.clone());
        self.data_scopes.extend(grant.data_scopes.clone());
        self.budget = self.budget.max(grant.budget);
    }
    pub fn permits(&self, action: &Action) -> bool {
        match action {
            Action::Read { path } => {
                canonical_target(path).is_ok_and(|p| p == *path)
                    && self.roots.iter().any(|r| path.starts_with(&r.path))
            }
            Action::Write { path } | Action::WriteExact { path, .. } => {
                canonical_target(path).is_ok_and(|p| p == *path)
                    && self
                        .roots
                        .iter()
                        .any(|r| r.write && path.starts_with(&r.path))
            }
            Action::Exec { .. } => self.processes && self.sandbox_profile.is_some(),
            Action::Network { domain } => domain_allowed(&self.network, domain),
            Action::UseSecret { reference } => self.secrets.contains(reference),
            Action::External { kind, .. } => self.external_actions.contains(kind),
            Action::Data { scope } => self.data_scopes.contains(scope),
            Action::SecuritySettings { .. } | Action::DisableSandbox | Action::ReadVault => false,
        }
    }
}
fn network_contains(parent: &Network, child: &Network) -> bool {
    match (parent, child) {
        (_, Network::None) | (Network::Full, _) => true,
        (Network::Domains(a), Network::Domains(b)) => b.is_subset(a),
        _ => false,
    }
}
pub fn domain_allowed(network: &Network, domain: &str) -> bool {
    match network {
        Network::None => false,
        Network::Full => true,
        Network::Domains(domains) => domains.contains(&domain.to_ascii_lowercase()),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Action {
    Read { path: PathBuf },
    Write { path: PathBuf },
    WriteExact { path: PathBuf, change: String },
    Exec { command: String },
    Network { domain: String },
    UseSecret { reference: String },
    External { kind: String, target: String },
    Data { scope: String },
    SecuritySettings { diff: String },
    DisableSandbox,
    ReadVault,
}
impl Action {
    pub fn kind(&self) -> &str {
        match self {
            Self::Read { .. } => "read",
            Self::Write { .. } | Self::WriteExact { .. } => "write",
            Self::Exec { .. } => "exec",
            Self::Network { .. } => "net",
            Self::UseSecret { .. } => "secret",
            Self::External { kind, .. } => kind,
            Self::Data { .. } => "data",
            Self::SecuritySettings { .. } => "settings",
            Self::DisableSandbox => "disable_sandbox",
            Self::ReadVault => "read_vault",
        }
    }
    pub fn target(&self) -> String {
        match self {
            Self::Read { path } | Self::Write { path } | Self::WriteExact { path, .. } => {
                path.to_string_lossy().into_owned()
            }
            Self::Exec { command } => command.clone(),
            Self::Network { domain } => domain.clone(),
            Self::UseSecret { reference } => reference.clone(),
            Self::External { target, .. } => target.clone(),
            Self::Data { scope } => scope.clone(),
            Self::SecuritySettings { diff } => diff.clone(),
            _ => String::new(),
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleEffect {
    Allow,
    Ask,
    Deny,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub action: String,
    pub pattern: String,
    pub effect: RuleEffect,
    pub run: Option<String>,
    pub project: Option<String>,
    pub expires_at: Option<i64>,
}
impl Rule {
    pub fn matches(&self, run: &str, project: &str, action: &Action, now: i64) -> bool {
        self.run.as_ref().is_none_or(|r| r == run)
            && self.project.as_ref().is_none_or(|p| p == project)
            && self.expires_at.is_none_or(|t| t > now)
            && self.action == action.kind()
            && globset::Glob::new(&self.pattern)
                .is_ok_and(|g| g.compile_matcher().is_match(action.target()))
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    Low,
    Medium,
    High,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskReport {
    pub risk: Risk,
    pub reasons: Vec<String>,
    pub network_command: bool,
    pub sensitive: bool,
}
pub fn sensitive(path: &Path) -> bool {
    let value = path.to_string_lossy().replace('\\', "/");
    [
        "/.git/hooks/",
        "/.git/config",
        "/.excalibur/",
        "/.agents/",
        "/.codex/",
        "/.ssh/",
        "/.aws/",
        "/.config/gcloud/",
        "/.github/workflows/",
    ]
    .iter()
    .any(|p| value.contains(p) || value.ends_with(p.trim_end_matches('/')))
        || value.ends_with("/.gitlab-ci.yml")
        || value.ends_with("/Jenkinsfile")
}
pub fn taints(path: &Path) -> bool {
    sensitive(path)
        || path
            .file_name()
            .is_some_and(|n| n == ".env" || n.to_string_lossy().starts_with(".env."))
}
pub fn classify(action: &Action) -> RiskReport {
    let mut report = RiskReport {
        risk: Risk::Low,
        reasons: Vec::new(),
        network_command: false,
        sensitive: false,
    };
    match action {
        Action::Write { path } | Action::WriteExact { path, .. } => {
            report.sensitive = taints(path);
            report.risk = if report.sensitive {
                Risk::High
            } else {
                Risk::Medium
            };
        }
        Action::Read { path } => report.sensitive = taints(path),
        Action::Network { .. } | Action::UseSecret { .. } => report.risk = Risk::Medium,
        Action::External { .. }
        | Action::SecuritySettings { .. }
        | Action::DisableSandbox
        | Action::ReadVault => report.risk = Risk::High,
        Action::Data { .. } => {}
        Action::Exec { command } => {
            let mut parser = tree_sitter::Parser::new();
            if parser
                .set_language(&tree_sitter_bash::LANGUAGE.into())
                .is_err()
            {
                report.risk = Risk::Unknown;
                report.reasons.push("shell parser unavailable".into());
                return report;
            }
            let Some(tree) = parser.parse(command, None) else {
                report.risk = Risk::Unknown;
                return report;
            };
            if tree.root_node().has_error() {
                report.risk = Risk::Unknown;
                report
                    .reasons
                    .push("shell syntax could not be classified".into());
            }
            fn nodes(node: tree_sitter::Node<'_>, source: &str, out: &mut Vec<String>) {
                if matches!(node.kind(), "command" | "pipeline" | "file_redirect") {
                    out.push(source[node.start_byte()..node.end_byte()].to_owned());
                }
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    nodes(child, source, out);
                }
            }
            fn computed(node: tree_sitter::Node<'_>) -> bool {
                if matches!(
                    node.kind(),
                    "command_substitution"
                        | "process_substitution"
                        | "expansion"
                        | "simple_expansion"
                        | "arithmetic_expansion"
                ) {
                    return true;
                }
                let mut cursor = node.walk();
                node.children(&mut cursor).any(computed)
            }
            let computed_shell = computed(tree.root_node());
            let mut parts = Vec::new();
            nodes(tree.root_node(), command, &mut parts);
            for part in parts {
                let words = match shell_words::split(&part) {
                    Ok(words) => words,
                    Err(_) => {
                        report.risk = report.risk.max(Risk::Unknown);
                        report.reasons.push("unresolved shell words".into());
                        continue;
                    }
                };
                let words = words
                    .into_iter()
                    .map(|w| w.to_ascii_lowercase())
                    .collect::<Vec<_>>();
                let normalized = words.join(" ");
                let program = words
                    .first()
                    .and_then(|w| Path::new(w).file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                let network = matches!(
                    program,
                    "curl" | "wget" | "ssh" | "scp" | "nc" | "ncat" | "ftp"
                ) || (program == "git"
                    && words
                        .iter()
                        .any(|w| matches!(w.as_str(), "push" | "fetch" | "clone" | "pull")));
                report.network_command |= network;
                report.sensitive |= normalized.contains(".env")
                    || normalized.contains(".ssh")
                    || normalized.contains(".aws")
                    || normalized.contains("gcloud");
                let destructive = (program == "rm"
                    && words
                        .iter()
                        .any(|w| w == "--recursive" || (w.starts_with('-') && w.contains('r'))))
                    || program == "git"
                        && ((words.iter().any(|w| w == "push")
                            && words.iter().any(|w| w == "-f" || w.starts_with("--force")))
                            || normalized.contains("reset --hard")
                            || words.iter().any(|w| {
                                w.starts_with("core.hooks") || w.starts_with("core.fsmonitor")
                            }))
                    || matches!(program, "dd" | "mkfs" | "eval" | "sendmail")
                    || program.starts_with("mkfs.")
                    || matches!(program, "chmod" | "chown")
                        && words.iter().any(|w| w == "-r" || w == "--recursive")
                    || (normalized.contains("curl")
                        && normalized.contains('|')
                        && (normalized.contains("sh") || normalized.contains("bash")))
                    || matches!(program, "npm" | "pnpm") && words.iter().any(|w| w == "publish")
                    || normalized.contains("deploy");
                if destructive {
                    report.risk = Risk::High;
                    report.reasons.push("high-risk shell syntax".into());
                }
                let known = matches!(
                    program,
                    "ls" | "cat"
                        | "pwd"
                        | "echo"
                        | "printf"
                        | "rg"
                        | "grep"
                        | "find"
                        | "cargo"
                        | "rustc"
                        | "npm"
                        | "pnpm"
                        | "git"
                        | "mkdir"
                        | "cp"
                        | "mv"
                        | "sed"
                        | "awk"
                        | "head"
                        | "tail"
                        | "wc"
                        | "true"
                        | "false"
                        | "test"
                        | "curl"
                        | "wget"
                        | "rm"
                        | "chmod"
                        | "chown"
                );
                if !known && !destructive
                    || program.contains('$')
                    || program.contains('`')
                    || program.is_empty()
                {
                    report.risk = Risk::Unknown;
                    report
                        .reasons
                        .push("unrecognized or computed command requires explicit approval".into());
                }
            }
            if computed_shell {
                report.risk = Risk::Unknown;
                report
                    .reasons
                    .push("shell expansions and substitutions require approval".into());
            }
        }
    }
    report
}

/// Canonicalize an absolute target, retaining a nonexistent leaf beneath its real ancestor.
pub fn canonical_target(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("target must be absolute without parent traversal".into());
    }
    let mut ancestor = path;
    let mut tail = Vec::new();
    while !ancestor.exists() {
        tail.push(
            ancestor
                .file_name()
                .ok_or("target has no parent")?
                .to_os_string(),
        );
        ancestor = ancestor.parent().ok_or("target has no parent")?;
    }
    let mut canonical = ancestor.canonicalize().map_err(|e| e.to_string())?;
    for part in tail.into_iter().rev() {
        canonical.push(part);
    }
    Ok(canonical)
}
