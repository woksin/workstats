//! Deterministic file-area and diff-shape classification.
//!
//! A category comes from the file path alone and a change shape from the
//! resulting per-category line counts. Nothing here opens a file or reads a
//! commit message, so every classification is reproducible from one
//! `git log --numstat` line and stays inside the privacy boundary.
//!
//! Categories are a runtime registry rather than a fixed enum: the built-in
//! defaults reproduce the historical six areas exactly, and a user can extend
//! or replace them from the JSON config. The registry is an ordered list and
//! that order IS the match priority — the first category whose rules match a
//! path wins, so `tests/fixtures/data.json` is a test rather than config.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// Bounds on a user-supplied registry. Classification runs once per changed
/// file in every commit, so the config must not be able to turn it into an
/// unbounded amount of work — the same reasoning as the bounded source-root
/// regex subset in `paths.rs`.
pub const MAX_CATEGORIES: usize = 32;
pub const MAX_RULES_PER_CATEGORY: usize = 128;
const MAX_NAME_BYTES: usize = 32;
const MAX_RULE_BYTES: usize = 128;
const MAX_GLOB_BYTES: usize = 256;

/// A category called `ignored` would emit `ignored_additions` in CSV, which is
/// already a column of its own.
const RESERVED_NAMES: &[&str] = &["ignored"];

/// One matching rule set, as written in the config file. Every list is
/// case-insensitive and compared against the lowercased path except
/// `cased_stem_suffixes` and `globs`, which see the path as Git reported it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryRules {
    /// Exact path component, anywhere above the file name.
    #[serde(default)]
    pub directories: Vec<String>,
    #[serde(default)]
    pub directory_prefixes: Vec<String>,
    #[serde(default)]
    pub directory_suffixes: Vec<String>,
    /// File extension, with or without the leading dot.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Exact file name including its extension.
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub name_prefixes: Vec<String>,
    #[serde(default)]
    pub name_suffixes: Vec<String>,
    #[serde(default)]
    pub name_contains: Vec<String>,
    /// File name with its final extension removed.
    #[serde(default)]
    pub stems: Vec<String>,
    #[serde(default)]
    pub stem_suffixes: Vec<String>,
    /// Stem suffix matched against the original casing, so `Latest.rs` is not
    /// read as a test while `UserTest.cs` is.
    #[serde(default)]
    pub cased_stem_suffixes: Vec<String>,
    #[serde(default)]
    pub globs: Vec<String>,
    /// Opt a category into the addition/deletion change shapes (`new code`,
    /// `revision`, `removal`) instead of being named directly.
    #[serde(default)]
    pub code_like: Option<bool>,
}

impl CategoryRules {
    fn rule_count(&self) -> usize {
        self.directories.len()
            + self.directory_prefixes.len()
            + self.directory_suffixes.len()
            + self.extensions.len()
            + self.names.len()
            + self.name_prefixes.len()
            + self.name_suffixes.len()
            + self.name_contains.len()
            + self.stems.len()
            + self.stem_suffixes.len()
            + self.cased_stem_suffixes.len()
            + self.globs.len()
    }
}

/// How a configured category relates to its built-in namesake.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CategoryMode {
    /// User rules are added to the built-in rules for that category.
    #[default]
    Extend,
    /// The named category's built-in rules are discarded.
    Replace,
}

impl CategoryMode {
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value {
            None | Some("extend") => Ok(CategoryMode::Extend),
            Some("replace") => Ok(CategoryMode::Replace),
            Some(other) => bail!("category_mode must be \"extend\" or \"replace\", not {other:?}"),
        }
    }
}

/// Which kind of rule matched, so `workstats classify` can say why.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleKind {
    Glob,
    Directory,
    DirectoryPrefix,
    DirectorySuffix,
    Name,
    NamePrefix,
    NameSuffix,
    NameContains,
    Stem,
    StemSuffix,
    CasedStemSuffix,
    Extension,
    Fallback,
}

impl RuleKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            RuleKind::Glob => "glob",
            RuleKind::Directory => "directory",
            RuleKind::DirectoryPrefix => "directory_prefix",
            RuleKind::DirectorySuffix => "directory_suffix",
            RuleKind::Name => "name",
            RuleKind::NamePrefix => "name_prefix",
            RuleKind::NameSuffix => "name_suffix",
            RuleKind::NameContains => "name_contains",
            RuleKind::Stem => "stem",
            RuleKind::StemSuffix => "stem_suffix",
            RuleKind::CasedStemSuffix => "cased_stem_suffix",
            RuleKind::Extension => "extension",
            RuleKind::Fallback => "fallback",
        }
    }
}

/// Why one path landed in one category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Classification {
    pub category: usize,
    pub rule: RuleKind,
    pub pattern: String,
}

/// One category and the rules that select it.
#[derive(Clone, Debug, Default)]
pub struct CategoryDef {
    pub name: String,
    /// Name used when this category dominates a diff. It differs from `name`
    /// only for the built-in `test` category, whose `tests` shape predates
    /// configurable categories and is kept so existing consumers keep working.
    pub shape_name: String,
    pub code_like: bool,
    pub directories: Vec<String>,
    pub directory_prefixes: Vec<String>,
    pub directory_suffixes: Vec<String>,
    pub extensions: Vec<String>,
    pub names: Vec<String>,
    pub name_prefixes: Vec<String>,
    pub name_suffixes: Vec<String>,
    pub name_contains: Vec<String>,
    pub stems: Vec<String>,
    pub stem_suffixes: Vec<String>,
    pub cased_stem_suffixes: Vec<String>,
    pub globs: Vec<String>,
    compiled_globs: Option<GlobSet>,
}

impl CategoryDef {
    fn extend_with(&mut self, other: CategoryDef) {
        self.directories.extend(other.directories);
        self.directory_prefixes.extend(other.directory_prefixes);
        self.directory_suffixes.extend(other.directory_suffixes);
        self.extensions.extend(other.extensions);
        self.names.extend(other.names);
        self.name_prefixes.extend(other.name_prefixes);
        self.name_suffixes.extend(other.name_suffixes);
        self.name_contains.extend(other.name_contains);
        self.stems.extend(other.stems);
        self.stem_suffixes.extend(other.stem_suffixes);
        self.cased_stem_suffixes.extend(other.cased_stem_suffixes);
        self.globs.extend(other.globs);
        self.code_like |= other.code_like;
    }

    fn compile_globs(&mut self) -> Result<()> {
        if self.globs.is_empty() {
            self.compiled_globs = None;
            return Ok(());
        }
        let mut builder = GlobSetBuilder::new();
        for glob in &self.globs {
            builder.add(
                GlobBuilder::new(glob)
                    .literal_separator(false)
                    .backslash_escape(true)
                    .build()
                    .with_context(|| format!("invalid glob {glob:?} in category {}", self.name))?,
            );
        }
        self.compiled_globs = Some(builder.build()?);
        Ok(())
    }

    /// The rule kinds are checked most-specific first so the reported reason is
    /// the useful one; within a category the result itself does not depend on
    /// the order, because every rule selects the same category.
    fn matches<'a>(&'a self, parts: &Parts<'_>) -> Option<(RuleKind, &'a str)> {
        if let Some(set) = &self.compiled_globs
            && set.is_match(parts.original)
        {
            let matched = set.matches(parts.original);
            let index = matched.first().copied().unwrap_or(0);
            return Some((RuleKind::Glob, self.globs[index].as_str()));
        }
        if let Some(rule) = first(&self.directories, |rule| parts.directories.contains(&rule)) {
            return Some((RuleKind::Directory, rule));
        }
        if let Some(rule) = first(&self.directory_prefixes, |rule| {
            parts
                .directories
                .iter()
                .any(|piece| piece.starts_with(rule))
        }) {
            return Some((RuleKind::DirectoryPrefix, rule));
        }
        if let Some(rule) = first(&self.directory_suffixes, |rule| {
            parts.directories.iter().any(|piece| piece.ends_with(rule))
        }) {
            return Some((RuleKind::DirectorySuffix, rule));
        }
        if let Some(rule) = first(&self.names, |rule| parts.name == rule) {
            return Some((RuleKind::Name, rule));
        }
        if let Some(rule) = first(&self.name_prefixes, |rule| parts.name.starts_with(rule)) {
            return Some((RuleKind::NamePrefix, rule));
        }
        if let Some(rule) = first(&self.name_suffixes, |rule| parts.name.ends_with(rule)) {
            return Some((RuleKind::NameSuffix, rule));
        }
        if let Some(rule) = first(&self.name_contains, |rule| parts.name.contains(rule)) {
            return Some((RuleKind::NameContains, rule));
        }
        if let Some(rule) = first(&self.stems, |rule| parts.stem == rule) {
            return Some((RuleKind::Stem, rule));
        }
        if let Some(rule) = first(&self.stem_suffixes, |rule| parts.stem.ends_with(rule)) {
            return Some((RuleKind::StemSuffix, rule));
        }
        if let Some(rule) = first(&self.cased_stem_suffixes, |rule| {
            parts.cased_stem.ends_with(rule)
        }) {
            return Some((RuleKind::CasedStemSuffix, rule));
        }
        if let Some(rule) = first(&self.extensions, |rule| parts.extension == rule) {
            return Some((RuleKind::Extension, rule));
        }
        None
    }
}

fn first(rules: &[String], mut predicate: impl FnMut(&str) -> bool) -> Option<&str> {
    rules
        .iter()
        .find(|rule| predicate(rule.as_str()))
        .map(String::as_str)
}

/// The pieces of a path every rule kind is compared against, computed once.
struct Parts<'a> {
    original: &'a str,
    directories: Vec<&'a str>,
    name: &'a str,
    stem: &'a str,
    extension: &'a str,
    cased_stem: &'a str,
}

/// An ordered set of categories. Position is match priority.
#[derive(Clone, Debug)]
pub struct CategoryRegistry {
    categories: Vec<CategoryDef>,
    fallback: usize,
}

impl CategoryRegistry {
    pub fn builtin() -> Self {
        Self::from_definitions(builtin_definitions())
    }

    /// Builds the registry described by the config. Unknown names create new
    /// categories, which are matched BEFORE the built-ins so a user's own rule
    /// wins over a built-in extension rule; among themselves they are ordered
    /// by name, because JSON object order is not preserved on the way in.
    pub fn from_config(
        categories: &BTreeMap<String, CategoryRules>,
        mode: CategoryMode,
    ) -> Result<Self> {
        if categories.len() > MAX_CATEGORIES {
            bail!("at most {MAX_CATEGORIES} configured categories are supported");
        }
        let mut definitions = builtin_definitions();
        let mut added: Vec<CategoryDef> = Vec::new();
        for (name, rules) in categories {
            validate_name(name)?;
            if rules.rule_count() > MAX_RULES_PER_CATEGORY {
                bail!("category {name:?} has more than {MAX_RULES_PER_CATEGORY} rules");
            }
            let configured = definition_from(name, rules)?;
            match definitions
                .iter_mut()
                .find(|existing| existing.name == *name)
            {
                Some(existing) => match mode {
                    CategoryMode::Extend => existing.extend_with(configured),
                    CategoryMode::Replace => {
                        let mut replacement = configured;
                        replacement.shape_name = existing.shape_name.clone();
                        // A replacement redefines the rules, not what kind of
                        // work the category is, unless it says so explicitly.
                        replacement.code_like = rules.code_like.unwrap_or(existing.code_like);
                        *existing = replacement;
                    }
                },
                None => added.push(configured),
            }
        }
        added.extend(definitions);
        if added.len() > MAX_CATEGORIES {
            bail!("at most {MAX_CATEGORIES} categories are supported");
        }
        for definition in &mut added {
            definition.compile_globs()?;
        }
        Ok(Self::from_definitions(added))
    }

    fn from_definitions(categories: Vec<CategoryDef>) -> Self {
        // `other` is the catch-all: a path matching nothing lands there, and a
        // diff dominated by it has no describable shape.
        let fallback = categories
            .iter()
            .position(|definition| definition.name == "other")
            .unwrap_or(categories.len().saturating_sub(1));
        Self {
            categories,
            fallback,
        }
    }

    pub fn len(&self) -> usize {
        self.categories.len()
    }

    pub fn name(&self, index: usize) -> &str {
        self.categories
            .get(index)
            .map_or("other", |definition| definition.name.as_str())
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.categories
            .iter()
            .map(|definition| definition.name.as_str())
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.categories
            .iter()
            .position(|definition| definition.name == name)
    }

    pub fn classify(&self, path: &str) -> usize {
        self.match_path(path)
            .map_or(self.fallback, |(index, _, _)| index)
    }

    pub fn explain(&self, path: &str) -> Classification {
        match self.match_path(path) {
            Some((category, rule, pattern)) => Classification {
                category,
                rule,
                pattern: pattern.to_string(),
            },
            None => Classification {
                category: self.fallback,
                rule: RuleKind::Fallback,
                pattern: String::new(),
            },
        }
    }

    fn match_path<'a>(&'a self, path: &str) -> Option<(usize, RuleKind, &'a str)> {
        let lowered = path.to_ascii_lowercase();
        let mut components: Vec<&str> = lowered
            .split(['/', '\\'])
            .filter(|piece| !piece.is_empty() && *piece != "." && *piece != "..")
            .collect();
        let name = components.pop()?;
        let (stem, extension) = split_name(name);
        let cased_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let parts = Parts {
            original: path,
            directories: components,
            name,
            stem,
            extension,
            cased_stem: split_name(cased_name).0,
        };
        self.categories
            .iter()
            .enumerate()
            .find_map(|(index, definition)| {
                definition
                    .matches(&parts)
                    .map(|(kind, rule)| (index, kind, rule))
            })
    }

    pub fn change_shape(&self, tally: &CategoryTally) -> Option<Shape> {
        let total = tally.touched();
        if total == 0 {
            return None;
        }
        let dominant =
            (0..self.categories.len()).max_by_key(|index| tally.get(*index).touched())?;
        let lines = tally.get(dominant);
        if (lines.touched() as f64) < total as f64 * DOMINANT_SHARE || dominant == self.fallback {
            return Some(Shape::Mixed);
        }
        if self.categories[dominant].code_like {
            return Some(if lines.deletions > lines.additions.saturating_mul(2) {
                Shape::Removal
            } else if lines.deletions.saturating_mul(4) < lines.additions {
                Shape::NewCode
            } else {
                Shape::Revision
            });
        }
        Some(Shape::Area(self.categories[dominant].shape_name.clone()))
    }
}

static ACTIVE: OnceLock<CategoryRegistry> = OnceLock::new();

/// The registry every classification runs against. It is configuration read
/// once at startup, not state, so it is shared rather than threaded through
/// every commit, row, and column; before `install` it is the built-in default.
pub fn active_registry() -> &'static CategoryRegistry {
    ACTIVE.get_or_init(CategoryRegistry::builtin)
}

/// Installs the configured registry. Must run before anything classifies a
/// path, which for the CLI means immediately after the config is read.
pub fn install(registry: CategoryRegistry) -> Result<()> {
    ACTIVE
        .set(registry)
        .map_err(|_| anyhow::anyhow!("the category registry was already in use"))
}

pub fn classify(path: &str) -> usize {
    active_registry().classify(path)
}

pub fn change_shape(tally: &CategoryTally) -> Option<Shape> {
    active_registry().change_shape(tally)
}

fn definition_from(name: &str, rules: &CategoryRules) -> Result<CategoryDef> {
    let mut definition = CategoryDef {
        name: name.to_string(),
        shape_name: name.to_string(),
        code_like: rules.code_like.unwrap_or(false),
        directories: normalized(name, &rules.directories, trim_directory)?,
        directory_prefixes: normalized(name, &rules.directory_prefixes, lowercase)?,
        directory_suffixes: normalized(name, &rules.directory_suffixes, lowercase)?,
        extensions: normalized(name, &rules.extensions, trim_extension)?,
        names: normalized(name, &rules.names, lowercase)?,
        name_prefixes: normalized(name, &rules.name_prefixes, lowercase)?,
        name_suffixes: normalized(name, &rules.name_suffixes, lowercase)?,
        name_contains: normalized(name, &rules.name_contains, lowercase)?,
        stems: normalized(name, &rules.stems, lowercase)?,
        stem_suffixes: normalized(name, &rules.stem_suffixes, lowercase)?,
        // Deliberately not lowercased: the point of this rule is the casing.
        cased_stem_suffixes: normalized(name, &rules.cased_stem_suffixes, str::to_string)?,
        globs: Vec::new(),
        compiled_globs: None,
    };
    for glob in &rules.globs {
        if glob.is_empty() || glob.len() > MAX_GLOB_BYTES || glob.chars().any(char::is_control) {
            bail!("category {name:?} has an empty, oversized, or control-character glob");
        }
        definition.globs.push(glob.clone());
    }
    Ok(definition)
}

/// Rules are compared against the lowercased path, so a rule written as
/// `CLAUDE.md` has to be lowercased here or it could never match.
fn normalized(
    category: &str,
    values: &[String],
    transform: impl Fn(&str) -> String,
) -> Result<Vec<String>> {
    let mut result = Vec::with_capacity(values.len());
    for value in values {
        if value.is_empty() || value.len() > MAX_RULE_BYTES || value.chars().any(char::is_control) {
            bail!("category {category:?} has an empty, oversized, or control-character rule");
        }
        let value = transform(value);
        if value.is_empty() {
            bail!("category {category:?} has a rule that normalizes to nothing");
        }
        result.push(value);
    }
    Ok(result)
}

fn lowercase(value: &str) -> String {
    value.to_ascii_lowercase()
}

fn trim_directory(value: &str) -> String {
    value.trim_matches(['/', '\\']).to_ascii_lowercase()
}

/// `".rs"` and `"rs"` mean the same thing to a reader, and only one of them
/// can match the extension Git reports.
fn trim_extension(value: &str) -> String {
    value.trim_start_matches('.').to_ascii_lowercase()
}

fn validate_name(name: &str) -> Result<()> {
    if name.chars().any(char::is_control) {
        bail!("category names must not contain control characters");
    }
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        bail!("category name must be 1 to {MAX_NAME_BYTES} characters: {name:?}");
    }
    if !name.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
    }) || !name.as_bytes()[0].is_ascii_lowercase()
    {
        bail!(
            "category name must start with a lowercase letter and use only lowercase letters, digits, '_' or '-': {name:?}"
        );
    }
    if RESERVED_NAMES.contains(&name) {
        bail!("category name {name:?} is reserved");
    }
    Ok(())
}

/// Changed lines attributed to one category.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CategoryLines {
    pub additions: u64,
    pub deletions: u64,
}

impl CategoryLines {
    pub fn touched(&self) -> u64 {
        self.additions.saturating_add(self.deletions)
    }
}

/// Changed lines per category index. It grows to the highest index it is
/// actually given rather than to the registry size, so merging stays a zip over
/// two short vectors and an empty tally costs nothing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CategoryTally {
    slots: Vec<CategoryLines>,
}

impl CategoryTally {
    pub fn add(&mut self, category: usize, additions: u64, deletions: u64) {
        let slot = self.slot(category);
        slot.additions = slot.additions.saturating_add(additions);
        slot.deletions = slot.deletions.saturating_add(deletions);
    }

    pub fn merge(&mut self, other: &CategoryTally) {
        if self.slots.len() < other.slots.len() {
            self.slots
                .resize(other.slots.len(), CategoryLines::default());
        }
        for (slot, value) in self.slots.iter_mut().zip(&other.slots) {
            slot.additions = slot.additions.saturating_add(value.additions);
            slot.deletions = slot.deletions.saturating_add(value.deletions);
        }
    }

    pub fn get(&self, category: usize) -> CategoryLines {
        self.slots.get(category).copied().unwrap_or_default()
    }

    pub fn touched(&self) -> u64 {
        self.slots
            .iter()
            .map(CategoryLines::touched)
            .fold(0, u64::saturating_add)
    }

    fn slot(&mut self, category: usize) -> &mut CategoryLines {
        if self.slots.len() <= category {
            self.slots.resize(category + 1, CategoryLines::default());
        }
        &mut self.slots[category]
    }
}

/// The observable shape of one commit's diff. These describe what the change
/// looks like, never what the author intended; the message is never read.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Shape {
    NewCode,
    Revision,
    Removal,
    /// Dominated by a category that is not code, named after it.
    Area(String),
    Mixed,
}

impl Shape {
    pub fn as_str(&self) -> &str {
        match self {
            Shape::NewCode => "new code",
            Shape::Revision => "revision",
            Shape::Removal => "removal",
            Shape::Area(name) => name.as_str(),
            Shape::Mixed => "mixed",
        }
    }
}

/// Commits counted per shape. Shapes are open-ended once categories are, so
/// this is a map rather than a fixed-size array.
#[derive(Clone, Debug, Default)]
pub struct ShapeTally {
    counts: BTreeMap<Shape, usize>,
}

impl ShapeTally {
    pub fn add(&mut self, shape: Shape) {
        *self.counts.entry(shape).or_insert(0) += 1;
    }

    pub fn total(&self) -> usize {
        self.counts.values().copied().sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Shape, usize)> {
        self.counts.iter().map(|(shape, count)| (shape, *count))
    }
}

/// A single category must hold this share of the changed lines before the
/// commit is described by that category rather than as mixed work.
const DOMINANT_SHARE: f64 = 0.6;

const TEST_DIRECTORIES: &[&str] = &[
    "test",
    "tests",
    "spec",
    "specs",
    "testing",
    "__tests__",
    "__mocks__",
    "testdata",
    "test_data",
    "fixtures",
    "e2e",
    "benches",
    "benchmarks",
    "given",
];

/// Test-project directory suffixes. .NET splits tests into a sibling project
/// such as `Arc.Core.Specs/`, so an exact directory name is not enough.
const TEST_DIRECTORY_SUFFIXES: &[&str] = &[".specs", ".tests", ".test", "-tests", "-specs"];

/// BDD directory prefixes. Cratis-style suites nest subjects and behaviours as
/// `for_TheSubject/when_something_happens/and_a_detail.cs`, where no path
/// component is literally named "test".
const TEST_DIRECTORY_PREFIXES: &[&str] = &["for_", "when_", "given_"];

const TEST_STEMS: &[&str] = &["test", "tests", "spec", "specs", "conftest", "given"];

/// A BDD suite names the file after the behaviour under test.
const TEST_NAME_PREFIXES: &[&str] = &["test_", "test-", "when_", "given_"];

const TEST_NAME_CONTAINS: &[&str] = &[".test.", ".spec."];

const TEST_STEM_SUFFIXES: &[&str] = &[
    "_test", "_tests", "-test", "-tests", "_spec", "_specs", "-spec", "-specs",
];

/// Checked against the original casing so `Latest.rs` is not read as a test.
const CAMEL_TEST_SUFFIXES: &[&str] = &["Test", "Tests", "TestCase", "Spec", "Specs"];

const DOC_DIRECTORIES: &[&str] = &["doc", "docs", "documentation", "man"];

const DOC_EXTENSIONS: &[&str] = &[
    "md", "markdown", "mdx", "rst", "adoc", "asciidoc", "org", "texi", "pod",
];

const DOC_STEMS: &[&str] = &[
    "readme",
    "license",
    "licence",
    "copying",
    "notice",
    "authors",
    "contributors",
    "contributing",
    "changelog",
    "changes",
    "history",
    "code_of_conduct",
];

const CONFIG_DIRECTORIES: &[&str] = &[
    ".github",
    ".gitlab",
    ".circleci",
    ".azure",
    ".vscode",
    ".idea",
    ".husky",
    ".devcontainer",
    "ci",
];

const CONFIG_EXTENSIONS: &[&str] = &[
    "json",
    "jsonc",
    "json5",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "properties",
    "plist",
    "xml",
    "env",
    "editorconfig",
    "gitignore",
    "gitattributes",
    "gitmodules",
    "dockerignore",
    "npmrc",
    "nvmrc",
    "tf",
    "tfvars",
    "hcl",
    "gradle",
    "bazel",
    "bzl",
    "cmake",
    "mk",
    "csproj",
    "fsproj",
    "vbproj",
    "vcxproj",
    "sln",
    "props",
    "targets",
    "nuspec",
    "podspec",
    "gemspec",
];

const CONFIG_NAMES: &[&str] = &[
    "dockerfile",
    "containerfile",
    "makefile",
    "gnumakefile",
    "cmakelists.txt",
    "build",
    "workspace",
    "procfile",
    "vagrantfile",
    "rakefile",
    "gemfile",
    "pipfile",
    "justfile",
    "brewfile",
    "codeowners",
    "go.mod",
    "requirements.txt",
    "constraints.txt",
];

const ASSET_DIRECTORIES: &[&str] = &["assets", "images", "img", "fonts", "media"];

const ASSET_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "webp", "ico", "icns", "bmp", "tiff", "avif", "mp3", "mp4",
    "wav", "ogg", "webm", "mov", "woff", "woff2", "ttf", "otf", "eot", "pdf", "psd", "sketch",
    "fig", "zip", "tar", "gz", "bz2", "xz", "7z", "jar", "wasm", "bin", "dat", "db", "sqlite",
    "sqlite3",
];

const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "go", "py", "pyi", "js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts", "java", "kt",
    "kts", "scala", "swift", "m", "mm", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "cs", "fs",
    "fsx", "vb", "rb", "php", "pl", "pm", "sh", "bash", "zsh", "fish", "ps1", "psm1", "lua", "r",
    "jl", "dart", "ex", "exs", "erl", "hrl", "hs", "elm", "clj", "cljs", "cljc", "nim", "zig", "v",
    "sql", "graphql", "gql", "proto", "vue", "svelte", "astro", "css", "scss", "sass", "less",
    "styl", "html", "htm", "hbs", "ejs", "pug", "erb", "haml", "slim", "jinja", "j2", "tmpl",
    "tpl", "razor", "cshtml", "vim", "el", "asm", "s", "bat", "cmd",
];

/// The default registry, in match order: a test fixture is a test before it is
/// config, and a source extension only wins once nothing more specific has.
fn builtin_definitions() -> Vec<CategoryDef> {
    vec![
        CategoryDef {
            name: "test".to_string(),
            shape_name: "tests".to_string(),
            directories: owned(TEST_DIRECTORIES),
            directory_prefixes: owned(TEST_DIRECTORY_PREFIXES),
            directory_suffixes: owned(TEST_DIRECTORY_SUFFIXES),
            name_prefixes: owned(TEST_NAME_PREFIXES),
            name_contains: owned(TEST_NAME_CONTAINS),
            stems: owned(TEST_STEMS),
            stem_suffixes: owned(TEST_STEM_SUFFIXES),
            cased_stem_suffixes: owned(CAMEL_TEST_SUFFIXES),
            ..CategoryDef::default()
        },
        CategoryDef {
            name: "docs".to_string(),
            shape_name: "docs".to_string(),
            directories: owned(DOC_DIRECTORIES),
            extensions: owned(DOC_EXTENSIONS),
            stems: owned(DOC_STEMS),
            ..CategoryDef::default()
        },
        CategoryDef {
            name: "config".to_string(),
            shape_name: "config".to_string(),
            directories: owned(CONFIG_DIRECTORIES),
            extensions: owned(CONFIG_EXTENSIONS),
            names: owned(CONFIG_NAMES),
            ..CategoryDef::default()
        },
        CategoryDef {
            name: "assets".to_string(),
            shape_name: "assets".to_string(),
            directories: owned(ASSET_DIRECTORIES),
            extensions: owned(ASSET_EXTENSIONS),
            ..CategoryDef::default()
        },
        CategoryDef {
            name: "source".to_string(),
            shape_name: "source".to_string(),
            code_like: true,
            extensions: owned(SOURCE_EXTENSIONS),
            ..CategoryDef::default()
        },
        CategoryDef {
            name: "other".to_string(),
            shape_name: "other".to_string(),
            ..CategoryDef::default()
        },
    ]
}

fn owned(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn split_name(name: &str) -> (&str, &str) {
    name.rsplit_once('.').unwrap_or((name, ""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn category(path: &str) -> &'static str {
        let registry = active_registry();
        registry.name(registry.classify(path))
    }

    fn rules(json: &str) -> BTreeMap<String, CategoryRules> {
        serde_json::from_str(json).expect("test rules parse")
    }

    #[test]
    fn paths_are_classified_by_area_with_tests_taking_priority() {
        assert_eq!("source", category("src/main.rs"));
        assert_eq!("source", category("web/src/components/Button.tsx"));
        assert_eq!("source", category("install.sh"));
        assert_eq!("test", category("tests/rust_cli.rs"));
        assert_eq!("test", category("src/components/Button.test.tsx"));
        assert_eq!("test", category("spec/models/user_spec.rb"));
        assert_eq!("test", category("api/UserServiceTest.java"));
        assert_eq!("test", category("tests/fixtures/payload.json"));
        assert_eq!("docs", category("README.md"));
        assert_eq!("docs", category("LICENSE"));
        assert_eq!("docs", category("docs/architecture.txt"));
        assert_eq!("config", category("Cargo.toml"));
        assert_eq!("config", category(".github/workflows/ci.yml"));
        assert_eq!("config", category("schema/events-v1.schema.json"));
        assert_eq!("assets", category("assets/banner.svg"));
        assert_eq!("other", category("Makefile.custom"));
    }

    #[test]
    fn dotnet_spec_projects_and_bdd_folders_count_as_tests() {
        // Tests living in a sibling project rather than a `tests/` directory.
        assert_eq!(
            "test",
            category("Source/Arc.Core.Specs/for_Thing/when_x.cs")
        );
        assert_eq!("test", category("Source/DotNET.Specs/Helpers/Builder.cs"));
        assert_eq!("test", category("Source/Fundamentals.Tests/Runner.cs"));

        // Behaviour folders that contain no component literally named "test".
        assert_eq!(
            "test",
            category("Integration/Api/for_EventTypes/TestEvent.cs")
        );
        assert_eq!(
            "test",
            category("Specs/for_Rule/when_analyzing/and_void_is_used.cs")
        );
        assert_eq!("test", category("Specs/for_Rule/given/a_rule.cs"));
        assert_eq!("test", category("Source/Api/when_handling_a_command.cs"));

        // Production code beside them stays source.
        assert_eq!("source", category("Source/Api/Controller.cs"));
        assert_eq!("source", category("Source/Arc.Core/Engine.cs"));
    }

    #[test]
    fn specification_lookalikes_stay_out_of_the_test_bucket() {
        // A directory merely containing the word, or a product named for it.
        assert_eq!("source", category("Source/Forecast/Model.cs"));
        assert_eq!("source", category("Source/Formatting/Engine.cs"));
        assert_eq!("source", category("Source/Whenever/Scheduler.cs"));
        assert_eq!("source", category("src/specular/render.rs"));
        assert_eq!("source", category("Source/OpenApi.Specification/Doc.cs"));
    }

    #[test]
    fn test_lookalike_names_are_not_misread_as_tests() {
        assert_eq!("source", category("src/latest.rs"));
        assert_eq!("source", category("src/protest/handler.go"));
        assert_eq!("source", category("src/contest.py"));
    }

    #[test]
    fn windows_separators_and_bare_dotfiles_classify_the_same_way() {
        assert_eq!("source", category(r"src\model\user.cs"));
        assert_eq!("test", category(r"tests\unit\user.cs"));
        assert_eq!("config", category(".gitignore"));
        assert_eq!("config", category(".github/dependabot.yml"));
    }

    #[test]
    fn the_builtin_registry_keeps_the_historical_six_areas_in_match_order() {
        let registry = CategoryRegistry::builtin();
        assert_eq!(
            vec!["test", "docs", "config", "assets", "source", "other"],
            registry.names().collect::<Vec<_>>()
        );
        // `other` is the catch-all rather than a category with rules of its own.
        assert_eq!(
            registry.index_of("other"),
            Some(registry.classify("Makefile.custom"))
        );
    }

    #[test]
    fn classification_reports_the_rule_that_matched() {
        let registry = CategoryRegistry::builtin();
        let explained = |path: &str| {
            let result = registry.explain(path);
            (
                registry.name(result.category).to_string(),
                result.rule.as_str(),
                result.pattern,
            )
        };
        assert_eq!(
            ("source".into(), "extension", "rs".into()),
            explained("src/main.rs")
        );
        assert_eq!(
            ("test".into(), "directory", "tests".into()),
            explained("tests/lib.rs")
        );
        assert_eq!(
            ("test".into(), "directory_suffix", ".specs".into()),
            explained("Source/Arc.Core.Specs/Runner.cs")
        );
        assert_eq!(
            ("test".into(), "cased_stem_suffix", "Test".into()),
            explained("api/UserServiceTest.java")
        );
        assert_eq!(
            ("other".into(), "fallback", String::new()),
            explained("Makefile.custom")
        );
    }

    #[test]
    fn configured_categories_extend_the_builtins_by_default() {
        let registry = CategoryRegistry::from_config(
            &rules(r#"{"test": {"directory_prefixes": ["it_"]}}"#),
            CategoryMode::Extend,
        )
        .unwrap();
        let name = |path: &str| registry.name(registry.classify(path));
        assert_eq!("test", name("src/it_login/flow.rs"));
        // The built-in rules are still there.
        assert_eq!("test", name("tests/lib.rs"));
        assert_eq!("source", name("src/main.rs"));
    }

    #[test]
    fn replace_mode_discards_the_builtin_rules_for_that_category() {
        let registry = CategoryRegistry::from_config(
            &rules(r#"{"test": {"directories": ["checks"]}}"#),
            CategoryMode::Replace,
        )
        .unwrap();
        let name = |path: &str| registry.name(registry.classify(path));
        assert_eq!("test", name("checks/login.rs"));
        assert_eq!("source", name("tests/lib.rs"));
        // Replacing rules does not change what kind of work a category is.
        assert_eq!(
            Some("tests"),
            registry
                .change_shape(&{
                    let mut tally = CategoryTally::default();
                    tally.add(registry.index_of("test").unwrap(), 10, 0);
                    tally
                })
                .as_ref()
                .map(Shape::as_str)
        );
    }

    #[test]
    fn a_new_category_is_matched_before_the_builtins_and_gets_its_own_shape() {
        let registry = CategoryRegistry::from_config(
            &rules(
                r#"{"ai": {"directories": [".ai", ".claude"], "names": ["CLAUDE.md"]},
                    "planning": {"globs": ["planning/**"]}}"#,
            ),
            CategoryMode::Extend,
        )
        .unwrap();
        let name = |path: &str| registry.name(registry.classify(path));
        // A rule written in the user's casing still matches a lowercased path.
        assert_eq!("ai", name("CLAUDE.md"));
        // The new category wins over the built-in config extension rule.
        assert_eq!("ai", name(".claude/settings.json"));
        assert_eq!("planning", name("planning/roadmap.md"));
        assert_eq!("docs", name("README.md"));

        let mut tally = CategoryTally::default();
        tally.add(registry.index_of("ai").unwrap(), 100, 0);
        assert_eq!(
            Some("ai"),
            registry.change_shape(&tally).as_ref().map(Shape::as_str)
        );
    }

    #[test]
    fn a_custom_category_can_opt_into_the_code_shapes() {
        let registry = CategoryRegistry::from_config(
            &rules(r#"{"corpus": {"directories": ["corpus"], "code_like": true}}"#),
            CategoryMode::Extend,
        )
        .unwrap();
        let mut tally = CategoryTally::default();
        tally.add(registry.index_of("corpus").unwrap(), 200, 10);
        assert_eq!(
            Some("new code"),
            registry.change_shape(&tally).as_ref().map(Shape::as_str)
        );
    }

    #[test]
    fn out_of_bounds_configuration_is_rejected() {
        let many: BTreeMap<String, CategoryRules> = (0..MAX_CATEGORIES + 1)
            .map(|index| (format!("c{index}"), CategoryRules::default()))
            .collect();
        assert!(CategoryRegistry::from_config(&many, CategoryMode::Extend).is_err());

        let too_many_rules = rules(&format!(
            r#"{{"ai": {{"directories": {:?}}}}}"#,
            (0..MAX_RULES_PER_CATEGORY + 1)
                .map(|index| format!("d{index}"))
                .collect::<Vec<_>>()
        ));
        assert!(CategoryRegistry::from_config(&too_many_rules, CategoryMode::Extend).is_err());

        for bad in [
            r#"{"Ai": {}}"#,
            r#"{"9ai": {}}"#,
            r#"{"a i": {}}"#,
            r#"{"ignored": {}}"#,
            r#"{"": {}}"#,
            r#"{"ai": {"directories": [""]}}"#,
            r#"{"ai": {"globs": ["["]}}"#,
        ] {
            assert!(
                CategoryRegistry::from_config(&rules(bad), CategoryMode::Extend).is_err(),
                "{bad} was accepted"
            );
        }
        // A misspelled rule set is a loud error rather than a silent no-op.
        assert!(
            serde_json::from_str::<BTreeMap<String, CategoryRules>>(
                r#"{"ai": {"directory_prefix": ["x"]}}"#
            )
            .is_err()
        );
        assert!(CategoryMode::parse(Some("merge")).is_err());
        assert_eq!(CategoryMode::Extend, CategoryMode::parse(None).unwrap());
    }

    #[test]
    fn change_shapes_follow_the_dominant_area_and_line_balance() {
        let registry = CategoryRegistry::builtin();
        let index = |name: &str| registry.index_of(name).unwrap();
        let shape = |entries: &[(&str, u64, u64)]| {
            let mut tally = CategoryTally::default();
            for (category, additions, deletions) in entries {
                tally.add(index(category), *additions, *deletions);
            }
            registry
                .change_shape(&tally)
                .as_ref()
                .map(Shape::as_str)
                .map(str::to_string)
        };

        assert_eq!(Some("new code".into()), shape(&[("source", 200, 10)]));
        assert_eq!(Some("revision".into()), shape(&[("source", 100, 80)]));
        assert_eq!(Some("removal".into()), shape(&[("source", 10, 300)]));
        assert_eq!(Some("tests".into()), shape(&[("test", 120, 5)]));
        assert_eq!(Some("docs".into()), shape(&[("docs", 40, 2)]));
        assert_eq!(Some("config".into()), shape(&[("config", 30, 30)]));
        assert_eq!(Some("mixed".into()), shape(&[("other", 40, 2)]));
        assert_eq!(
            Some("mixed".into()),
            shape(&[("source", 50, 0), ("test", 50, 0)])
        );
        assert_eq!(None, shape(&[]));
    }

    #[test]
    fn tallies_merge_across_different_lengths() {
        let mut left = CategoryTally::default();
        left.add(0, 5, 1);
        let mut right = CategoryTally::default();
        right.add(3, 7, 2);
        left.merge(&right);
        assert_eq!(5, left.get(0).additions);
        assert_eq!(7, left.get(3).additions);
        assert_eq!(15, left.touched());
        // An index the tally never saw reads as zero rather than panicking.
        assert_eq!(CategoryLines::default(), left.get(31));
    }
}
