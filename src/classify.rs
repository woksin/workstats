//! Deterministic file-area and diff-shape classification.
//!
//! A category comes from the file path alone and a change shape from the
//! resulting per-category line counts. Nothing here opens a file or reads a
//! commit message, so every classification is reproducible from one
//! `git log --numstat` line and stays inside the privacy boundary.

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Category {
    Source,
    Test,
    Docs,
    Config,
    Assets,
    Other,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Source,
        Category::Test,
        Category::Docs,
        Category::Config,
        Category::Assets,
        Category::Other,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Category::Source => "source",
            Category::Test => "test",
            Category::Docs => "docs",
            Category::Config => "config",
            Category::Assets => "assets",
            Category::Other => "other",
        }
    }
}

pub const CATEGORY_COUNT: usize = Category::ALL.len();

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

pub type CategoryTally = [CategoryLines; CATEGORY_COUNT];

pub fn tally_add(target: &mut CategoryTally, category: Category, additions: u64, deletions: u64) {
    let slot = &mut target[category.index()];
    slot.additions = slot.additions.saturating_add(additions);
    slot.deletions = slot.deletions.saturating_add(deletions);
}

pub fn tally_merge(target: &mut CategoryTally, other: &CategoryTally) {
    for (slot, value) in target.iter_mut().zip(other) {
        slot.additions = slot.additions.saturating_add(value.additions);
        slot.deletions = slot.deletions.saturating_add(value.deletions);
    }
}

pub fn touched_lines(tally: &CategoryTally) -> u64 {
    tally
        .iter()
        .map(CategoryLines::touched)
        .fold(0, u64::saturating_add)
}

/// The observable shape of one commit's diff. These describe what the change
/// looks like, never what the author intended; the message is never read.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Shape {
    NewCode,
    Revision,
    Removal,
    Tests,
    Docs,
    Config,
    Assets,
    Mixed,
}

impl Shape {
    pub const ALL: [Shape; 8] = [
        Shape::NewCode,
        Shape::Revision,
        Shape::Removal,
        Shape::Tests,
        Shape::Docs,
        Shape::Config,
        Shape::Assets,
        Shape::Mixed,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Shape::NewCode => "new code",
            Shape::Revision => "revision",
            Shape::Removal => "removal",
            Shape::Tests => "tests",
            Shape::Docs => "docs",
            Shape::Config => "config",
            Shape::Assets => "assets",
            Shape::Mixed => "mixed",
        }
    }
}

pub const SHAPE_COUNT: usize = Shape::ALL.len();

pub type ShapeTally = [usize; SHAPE_COUNT];

/// A single category must hold this share of the changed lines before the
/// commit is described by that category rather than as mixed work.
const DOMINANT_SHARE: f64 = 0.6;

pub fn change_shape(tally: &CategoryTally) -> Option<Shape> {
    let total = touched_lines(tally);
    if total == 0 {
        return None;
    }
    let dominant = Category::ALL
        .into_iter()
        .max_by_key(|category| tally[category.index()].touched())?;
    if (tally[dominant.index()].touched() as f64) < total as f64 * DOMINANT_SHARE {
        return Some(Shape::Mixed);
    }
    Some(match dominant {
        Category::Test => Shape::Tests,
        Category::Docs => Shape::Docs,
        Category::Config => Shape::Config,
        Category::Assets => Shape::Assets,
        Category::Other => Shape::Mixed,
        Category::Source => {
            let lines = tally[Category::Source.index()];
            if lines.deletions > lines.additions.saturating_mul(2) {
                Shape::Removal
            } else if lines.deletions.saturating_mul(4) < lines.additions {
                Shape::NewCode
            } else {
                Shape::Revision
            }
        }
    })
}

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

pub fn classify(path: &str) -> Category {
    let lowered = path.to_ascii_lowercase();
    let mut components: Vec<&str> = lowered
        .split(['/', '\\'])
        .filter(|piece| !piece.is_empty() && *piece != "." && *piece != "..")
        .collect();
    let Some(name) = components.pop() else {
        return Category::Other;
    };
    let directories = components;
    let (stem, extension) = split_name(name);
    let original_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let original_stem = split_name(original_name).0;

    if directories.iter().any(|piece| is_test_directory(piece))
        || is_test_name(name, stem, original_stem)
    {
        return Category::Test;
    }
    if DOC_EXTENSIONS.contains(&extension)
        || DOC_STEMS.contains(&stem)
        || directories
            .iter()
            .any(|piece| DOC_DIRECTORIES.contains(piece))
    {
        return Category::Docs;
    }
    if CONFIG_EXTENSIONS.contains(&extension)
        || CONFIG_NAMES.contains(&name)
        || directories
            .iter()
            .any(|piece| CONFIG_DIRECTORIES.contains(piece))
    {
        return Category::Config;
    }
    if ASSET_EXTENSIONS.contains(&extension)
        || directories
            .iter()
            .any(|piece| ASSET_DIRECTORIES.contains(piece))
    {
        return Category::Assets;
    }
    if SOURCE_EXTENSIONS.contains(&extension) {
        return Category::Source;
    }
    Category::Other
}

fn is_test_directory(directory: &str) -> bool {
    TEST_DIRECTORIES.contains(&directory)
        || TEST_DIRECTORY_SUFFIXES
            .iter()
            .any(|suffix| directory.ends_with(suffix))
        || TEST_DIRECTORY_PREFIXES
            .iter()
            .any(|prefix| directory.starts_with(prefix))
}

fn is_test_name(name: &str, stem: &str, original_stem: &str) -> bool {
    TEST_STEMS.contains(&stem)
        || name.starts_with("test_")
        || name.starts_with("test-")
        // A BDD suite names the file after the behaviour under test.
        || name.starts_with("when_")
        || name.starts_with("given_")
        || name.contains(".test.")
        || name.contains(".spec.")
        || TEST_STEM_SUFFIXES
            .iter()
            .any(|suffix| stem.ends_with(suffix))
        || CAMEL_TEST_SUFFIXES
            .iter()
            .any(|suffix| original_stem.ends_with(suffix))
}

fn split_name(name: &str) -> (&str, &str) {
    name.rsplit_once('.').unwrap_or((name, ""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn category(path: &str) -> &'static str {
        classify(path).as_str()
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
    fn change_shapes_follow_the_dominant_area_and_line_balance() {
        let shape = |entries: &[(Category, u64, u64)]| {
            let mut tally = CategoryTally::default();
            for (category, additions, deletions) in entries {
                tally_add(&mut tally, *category, *additions, *deletions);
            }
            change_shape(&tally).map(Shape::as_str)
        };

        assert_eq!(Some("new code"), shape(&[(Category::Source, 200, 10)]));
        assert_eq!(Some("revision"), shape(&[(Category::Source, 100, 80)]));
        assert_eq!(Some("removal"), shape(&[(Category::Source, 10, 300)]));
        assert_eq!(Some("tests"), shape(&[(Category::Test, 120, 5)]));
        assert_eq!(Some("docs"), shape(&[(Category::Docs, 40, 2)]));
        assert_eq!(Some("config"), shape(&[(Category::Config, 30, 30)]));
        assert_eq!(
            Some("mixed"),
            shape(&[(Category::Source, 50, 0), (Category::Test, 50, 0)])
        );
        assert_eq!(None, shape(&[]));
    }
}
