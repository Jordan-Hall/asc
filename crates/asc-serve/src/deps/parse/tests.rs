//! A fixture per file format, the joins of `combine`, and the bounds.

use super::*;

fn read(path: &str, text: &str) -> Parsed {
    parse_file(path, text.as_bytes())
        .unwrap_or_else(|| panic!("{path} is not recognised"))
        .unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// `name version`, with a `*` when the dependency is direct and `~` when it is a requirement.
fn summary(p: &Parsed) -> Vec<String> {
    let mut v: Vec<String> = p
        .deps
        .iter()
        .map(|d| {
            format!(
                "{} {}{}{}",
                d.name,
                d.version,
                if d.direct { " *" } else { "" },
                if d.kind == DepKind::Manifest {
                    " ~"
                } else {
                    ""
                }
            )
        })
        .collect();
    v.sort();
    v
}

fn all(p: &Parsed, eco: Ecosystem) -> bool {
    p.deps
        .iter()
        .all(|d| d.ecosystem == eco && d.path == p.path)
}

#[test]
fn cargo_lock_pins_registry_crates_and_marks_what_the_workspace_names() {
    let p = read("Cargo.lock", include_str!("../fixtures/cargo/Cargo.lock"));
    assert_eq!(
        summary(&p),
        [
            "memchr 2.5.0",
            "regex 1.5.0 *",
            "regex 1.7.1 *",
            "serde 1.0.152 *",
            "tokio 1.25.0 *"
        ]
    );
    assert!(all(&p, Ecosystem::CratesIo));
    assert_eq!(p.notes.len(), 1);
    assert!(p.notes[0].starts_with("2 packages"), "{:?}", p.notes);
}

#[test]
fn cargo_toml_reads_requirements_and_skips_what_is_not_a_registry_crate() {
    let p = read(
        "crates/a/Cargo.toml",
        include_str!("../fixtures/cargo/Cargo.toml"),
    );
    assert_eq!(
        summary(&p),
        [
            "cc 1 * ~",
            "criterion 0.4 * ~",
            "real-name 2.1.0 * ~",
            "regex 1.7 * ~",
            "serde 1.0.152 * ~",
            "shared 0.9.1 * ~",
            "tokio 1.25 * ~",
            "winapi 0.3.9 * ~"
        ]
    );
    assert_eq!(
        p.deps
            .iter()
            .find(|d| d.name == "regex")
            .unwrap()
            .requirement,
        "~1.7"
    );
    assert!(p.notes[0].starts_with("4 dependencies"), "{:?}", p.notes);
}

#[test]
fn package_lock_v3_marks_root_and_workspace_dependencies_direct() {
    let p = read(
        "package-lock.json",
        include_str!("../fixtures/npm/package-lock.json"),
    );
    assert_eq!(
        summary(&p),
        [
            "@scope/pkg 2.1.0 *",
            "lodash 3.10.1",
            "lodash 4.17.20 *",
            "minimist 1.2.5 *",
            "mocha 10.2.0 *",
            "real-package 5.0.0"
        ]
    );
    assert!(p.notes[0].starts_with("1 packages"), "{:?}", p.notes);
}

#[test]
fn package_lock_v1_walks_the_nested_tree() {
    let p = read(
        "old/package-lock.json",
        include_str!("../fixtures/npm/package-lock-v1.json"),
    );
    assert_eq!(
        summary(&p),
        ["accepts 1.3.7", "debug 2.6.9", "express 4.17.1"]
    );
    assert!(p.notes[0].starts_with("1 packages"));
}

#[test]
fn yarn_v1_lock_reads_every_spec_of_an_entry_once() {
    let p = read("yarn.lock", include_str!("../fixtures/npm/yarn-v1.lock"));
    assert_eq!(
        summary(&p),
        ["@babel/core 7.1.2", "debug 3.2.6", "lodash 4.17.15"]
    );
    assert!(p.notes[0].starts_with("2 entries"), "{:?}", p.notes);
}

#[test]
fn yarn_berry_lock_keeps_registry_resolutions_only() {
    let p = read("yarn.lock", include_str!("../fixtures/npm/yarn-berry.lock"));
    assert_eq!(summary(&p), ["@types/node 18.11.9", "lodash 4.17.21"]);
    assert!(p.notes[0].starts_with("1 entries"), "{:?}", p.notes);
}

#[test]
fn pnpm_lock_v9_and_v5_read_their_key_forms() {
    let p = read(
        "pnpm-lock.yaml",
        include_str!("../fixtures/npm/pnpm-lock-v9.yaml"),
    );
    assert_eq!(
        summary(&p),
        [
            "@types/node 18.11.9 *",
            "lodash 4.17.21 *",
            "minimist 1.2.5 *",
            "react-dom 18.2.0"
        ]
    );
    assert!(p.notes[0].starts_with("1 packages"), "{:?}", p.notes);
    let p = read(
        "pnpm-lock.yaml",
        include_str!("../fixtures/npm/pnpm-lock-v5.yaml"),
    );
    assert_eq!(
        summary(&p),
        [
            "@babel/core 7.18.0",
            "@scope/name 3.0.1",
            "lodash 4.17.20 *",
            "react-dom 17.0.2 *"
        ]
    );
}

#[test]
fn package_json_reads_the_floor_of_each_requirement() {
    let p = read("package.json", include_str!("../fixtures/npm/package.json"));
    assert_eq!(
        summary(&p),
        [
            "@scope/pkg 2.0.0 * ~",
            "fsevents 2.3.2 * ~",
            "lodash 4.17.20 * ~",
            "mocha 10.2.0 * ~",
            "react 17.0.0 * ~"
        ]
    );
    assert!(p.notes[0].starts_with("5 dependencies"), "{:?}", p.notes);
}

#[test]
fn requirements_txt_pins_become_locks_and_ranges_become_floors() {
    let p = read(
        "requirements.txt",
        include_str!("../fixtures/python/requirements.txt"),
    );
    assert_eq!(
        summary(&p),
        [
            "Django 3.2.0 *",
            "Flask_Login 0.5 * ~",
            "numpy 1.0 * ~",
            "pyyaml 5.4.1 *",
            "requests 2.28.1 *"
        ]
    );
    assert!(all(&p, Ecosystem::PyPi));
    assert_eq!(p.notes.len(), 2, "{:?}", p.notes);
    assert!(p.notes[0].starts_with("2 requirements"));
    assert!(p.notes[1].starts_with("3 option lines"));
}

#[test]
fn a_pip_compile_file_says_which_pins_are_direct() {
    let p = read(
        "requirements.txt",
        include_str!("../fixtures/python/requirements-compiled.txt"),
    );
    assert_eq!(
        summary(&p),
        [
            "certifi 2022.12.7",
            "charset-normalizer 3.0.1",
            "requests 2.28.1 *",
            "urllib3 1.26.14 *"
        ]
    );
}

#[test]
fn poetry_uv_and_pipenv_locks_keep_registry_packages() {
    let p = read(
        "poetry.lock",
        include_str!("../fixtures/python/poetry.lock"),
    );
    assert_eq!(summary(&p), ["Django 3.2.0", "certifi 2022.12.7"]);
    let p = read("uv.lock", include_str!("../fixtures/python/uv.lock"));
    assert_eq!(
        summary(&p),
        ["certifi 2022.12.7", "pytest 7.2.0 *", "requests 2.28.1 *"]
    );
    let p = read(
        "Pipfile.lock",
        include_str!("../fixtures/python/Pipfile.lock"),
    );
    assert_eq!(summary(&p), ["pytest 7.2.0", "requests 2.28.1"]);
    assert!(p.notes[0].starts_with("3 packages"), "{:?}", p.notes);
}

#[test]
fn pyproject_and_pipfile_name_the_direct_dependencies() {
    let p = read(
        "pyproject.toml",
        include_str!("../fixtures/python/pyproject.toml"),
    );
    let with_version: Vec<String> = summary(&p)
        .into_iter()
        .filter(|s| !s.contains("  "))
        .collect();
    assert!(
        with_version.contains(&"requests 2.28 * ~".to_owned()),
        "{with_version:?}"
    );
    for want in [
        "Django 3.2.0 * ~",
        "pydantic 1.10 * ~",
        "pytest 7.2.0 * ~",
        "ruff 0.1.0 * ~",
        "httpx 0.23.0 * ~",
        "fastapi 0.95.0 * ~",
        "black 22.12.0 * ~",
    ] {
        assert!(
            with_version.contains(&want.to_owned()),
            "{want} in {with_version:?}"
        );
    }
    // `anyname` states no version but is still named.
    assert!(
        p.deps
            .iter()
            .any(|d| d.name == "anyname" && d.version.is_empty())
    );
    assert!(
        !p.deps
            .iter()
            .any(|d| d.name == "python" || d.name == "local")
    );
    let p = read("Pipfile", include_str!("../fixtures/python/Pipfile"));
    assert!(
        p.deps
            .iter()
            .any(|d| d.name == "requests" && d.version.is_empty())
    );
    let v: Vec<_> = p
        .deps
        .iter()
        .filter(|d| !d.version.is_empty())
        .map(|d| format!("{} {}", d.name, d.version))
        .collect();
    assert_eq!(v, ["django 3.2.0", "flask 2.0", "pytest 7.2"]);
}

#[test]
fn go_mod_applies_replacements_and_marks_indirect_requirements() {
    let p = read("go.mod", include_str!("../fixtures/go/go.mod"));
    assert_eq!(
        summary(&p),
        [
            "example.com/internal v0.0.0-20230101000000-abcdef123456 *",
            "example.com/replaced-fork v1.0.1 *",
            "github.com/gin-gonic/gin v1.9.0 *",
            "github.com/stretchr/testify v1.8.1 *",
            "golang.org/x/net v0.7.0",
            "golang.org/x/text v0.7.0",
            "quoted.example/mod v2.0.0+incompatible *"
        ]
    );
    assert!(p.notes[0].starts_with("1 requirements"), "{:?}", p.notes);
    let p = read("go.sum", include_str!("../fixtures/go/go.sum"));
    assert_eq!(
        summary(&p),
        [
            "github.com/gin-gonic/gin v1.8.0",
            "github.com/gin-gonic/gin v1.9.0",
            "golang.org/x/net v0.7.0"
        ]
    );
}

#[test]
fn pom_xml_resolves_properties_and_managed_versions() {
    let p = read("pom.xml", include_str!("../fixtures/maven/pom.xml"));
    assert_eq!(
        summary(&p),
        [
            "com.example:self 2.5.0 *",
            "com.fasterxml.jackson.core:jackson-databind 2.13.0 *",
            "junit:junit 4.12 * ~",
            "org.apache.commons:commons-lang3 3.12.0 *",
            "org.apache.logging.log4j:log4j-core 2.14.1 *"
        ]
    );
    assert!(p.notes[0].starts_with("2 dependencies"), "{:?}", p.notes);
    let p = read(
        "gradle.lockfile",
        include_str!("../fixtures/maven/gradle.lockfile"),
    );
    assert_eq!(
        summary(&p),
        [
            "com.google.guava:guava 31.1-jre",
            "junit:junit 4.13.2",
            "org.slf4j:slf4j-api 1.7.36"
        ]
    );
}

#[test]
fn nuget_lock_and_project_files_read_resolved_and_declared_versions() {
    let p = read(
        "src/packages.lock.json",
        include_str!("../fixtures/dotnet/packages.lock.json"),
    );
    assert_eq!(
        summary(&p),
        ["Newtonsoft.Json 13.0.1 *", "System.Text.Json 6.0.0"]
    );
    let p = read(
        "src/App.csproj",
        include_str!("../fixtures/dotnet/App.csproj"),
    );
    assert_eq!(
        summary(&p),
        [
            "Dapper 2.0.123 *",
            "Floating 1.0 * ~",
            "Newtonsoft.Json 12.0.3 *",
            "Serilog 2.12.0 *"
        ]
    );
    assert!(
        p.notes[0].starts_with("2 package references"),
        "{:?}",
        p.notes
    );
}

#[test]
fn composer_files_strip_the_v_and_skip_development_branches() {
    let p = read(
        "composer.lock",
        include_str!("../fixtures/php/composer.lock"),
    );
    assert_eq!(
        summary(&p),
        [
            "monolog/monolog 2.8.0",
            "phpunit/phpunit 9.5.0",
            "symfony/console 5.4.2"
        ]
    );
    let p = read(
        "composer.json",
        include_str!("../fixtures/php/composer.json"),
    );
    let v: Vec<String> = summary(&p)
        .into_iter()
        .filter(|s| !s.contains("  "))
        .collect();
    assert_eq!(
        v,
        [
            "monolog/monolog 2.8 * ~",
            "phpunit/phpunit 9.5 * ~",
            "symfony/console 5.4.0 * ~"
        ]
    );
    assert!(
        p.deps
            .iter()
            .any(|d| d.name == "acme/branch" && d.version.is_empty())
    );
    assert!(
        !p.deps
            .iter()
            .any(|d| d.name == "php" || d.name == "ext-json")
    );
}

#[test]
fn gemfile_lock_strips_platforms_and_reads_dependencies_as_direct() {
    let p = read(
        "Gemfile.lock",
        include_str!("../fixtures/ruby/Gemfile.lock"),
    );
    assert_eq!(
        summary(&p),
        [
            "actionpack 7.0.4",
            "nokogiri 1.13.10 *",
            "racc 1.6.2",
            "rack 2.2.4",
            "rails 7.0.4.pre *"
        ]
    );
    assert!(p.notes[0].starts_with("1 gems"), "{:?}", p.notes);
    let p = read("Gemfile", include_str!("../fixtures/ruby/Gemfile"));
    let mut names: Vec<_> = p.deps.iter().map(|d| d.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["forked", "nokogiri", "pry", "rails"]);
}

#[test]
fn mix_lock_reads_hex_entries_and_mix_exs_names_the_direct_ones() {
    let p = read("mix.lock", include_str!("../fixtures/elixir/mix.lock"));
    assert_eq!(
        summary(&p),
        ["cowboy 2.9.0", "cowlib 2.11.0", "plug 1.14.0"]
    );
    assert!(p.notes[0].starts_with("1 entries"), "{:?}", p.notes);
    let p = read("mix.exs", include_str!("../fixtures/elixir/mix.exs"));
    let mut names: Vec<_> = p.deps.iter().map(|d| d.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["cowboy", "my_fork", "plug"]);
}

#[test]
fn pubspec_lock_keeps_packages_hosted_on_pub_dev() {
    let p = read(
        "pubspec.lock",
        include_str!("../fixtures/dart/pubspec.lock"),
    );
    assert_eq!(
        summary(&p),
        ["http 0.13.5 *", "linted 2.1.0 *", "path 1.8.2"]
    );
    assert!(p.notes[0].starts_with("2 packages"), "{:?}", p.notes);
}

#[test]
fn package_resolved_names_swift_packages_by_repository_url() {
    let p = read(
        "Package.resolved",
        include_str!("../fixtures/swift/Package.resolved"),
    );
    assert_eq!(
        summary(&p),
        [
            "github.com/Alamofire/Alamofire 5.6.1",
            "github.com/apple/swift-nio 2.40.0"
        ]
    );
    assert!(p.notes[0].starts_with("2 pins"), "{:?}", p.notes);
    let p = read(
        "Package.resolved",
        include_str!("../fixtures/swift/Package-v1.resolved"),
    );
    assert_eq!(summary(&p), ["github.com/onevcat/Kingfisher 7.0.0"]);
    let p = read(
        "Package.swift",
        include_str!("../fixtures/swift/Package.swift"),
    );
    let mut names: Vec<_> = p.deps.iter().map(|d| d.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "github.com/Alamofire/Alamofire",
            "github.com/apple/swift-nio"
        ]
    );
    assert_eq!(
        super::swift::url_name("ssh://git@host.example:2222/org/repo.git/").as_deref(),
        Some("host.example/org/repo")
    );
    assert_eq!(super::swift::url_name("not a url"), None);
    assert_eq!(super::swift::url_name("https://host/../x"), None);
}

#[test]
fn podfile_lock_joins_subspecs_and_leaves_out_external_sources() {
    let p = read(
        "Podfile.lock",
        include_str!("../fixtures/pods/Podfile.lock"),
    );
    assert_eq!(
        summary(&p),
        [
            "Alamofire 5.6.1 *",
            "Firebase 8.0.0 *",
            "FirebaseAnalytics 8.0.0"
        ]
    );
    assert!(all(&p, Ecosystem::CocoaPods));
    assert!(p.notes[0].starts_with("1 pods"), "{:?}", p.notes);
}

#[test]
fn every_format_is_recognised_by_name_only() {
    for (name, want) in [
        ("Cargo.lock", Some(Format::CargoLock)),
        ("requirements-dev.txt", Some(Format::Requirements)),
        ("App.csproj", Some(Format::Csproj)),
        ("Lib.fsproj", Some(Format::Csproj)),
        ("npm-shrinkwrap.json", Some(Format::PackageLock)),
        ("Cargo.lock.bak", None),
        ("cargo.lock", None),
        ("requirements.txt.orig", None),
        ("README.md", None),
    ] {
        assert_eq!(Format::of(name), want, "{name}");
    }
    assert!(parse_file("README.md", b"x").is_none());
    for f in [
        Format::CargoLock,
        Format::Csproj,
        Format::PodfileLock,
        Format::MixExs,
    ] {
        assert!(!f.name().is_empty());
        assert!(Ecosystem::ALL.contains(&f.ecosystem()));
    }
}

fn manifest(path: &str, eco: Ecosystem, name: &str, floor: &str) -> Dep {
    Dep::manifest(eco, name, &format!("^{floor}"), floor, path)
}

fn parsed(format: Format, path: &str, deps: Vec<Dep>) -> Parsed {
    Parsed {
        format,
        path: path.to_owned(),
        deps,
        notes: Vec::new(),
    }
}

#[test]
fn a_lockfile_replaces_the_requirement_it_pins_and_makes_the_package_direct() {
    let lock = parsed(
        Format::YarnLock,
        "yarn.lock",
        vec![
            Dep::lock(Ecosystem::Npm, "lodash", "4.17.21", "yarn.lock"),
            Dep::lock(Ecosystem::Npm, "debug", "3.2.6", "yarn.lock"),
        ],
    );
    let pkg = parsed(
        Format::PackageJson,
        "package.json",
        vec![manifest("package.json", Ecosystem::Npm, "lodash", "4.17.0")],
    );
    let c = combine(&[pkg, lock]);
    let got: Vec<_> = c
        .deps
        .iter()
        .map(|d| (d.name.as_str(), d.direct, d.kind))
        .collect();
    assert_eq!(
        got,
        [
            ("debug", false, DepKind::Lock),
            ("lodash", true, DepKind::Lock)
        ]
    );
}

#[test]
fn a_lockfile_above_a_workspace_member_pins_its_requirements() {
    let lock = parsed(
        Format::PackageLock,
        "package-lock.json",
        vec![Dep::lock(
            Ecosystem::Npm,
            "left-pad",
            "1.3.0",
            "package-lock.json",
        )],
    );
    let member = parsed(
        Format::PackageJson,
        "packages/a/package.json",
        vec![
            manifest(
                "packages/a/package.json",
                Ecosystem::Npm,
                "left-pad",
                "1.0.0",
            ),
            manifest(
                "packages/a/package.json",
                Ecosystem::Npm,
                "unlocked",
                "2.0.0",
            ),
        ],
    );
    let c = combine(&[lock, member]);
    let got: Vec<_> = c
        .deps
        .iter()
        .map(|d| (d.path.as_str(), d.name.as_str(), d.direct, d.kind))
        .collect();
    assert_eq!(
        got,
        [
            ("package-lock.json", "left-pad", true, DepKind::Lock),
            (
                "packages/a/package.json",
                "unlocked",
                true,
                DepKind::Manifest
            )
        ]
    );
}

#[test]
fn a_lockfile_below_a_manifest_does_not_pin_it_and_ecosystems_do_not_mix() {
    let lock = parsed(
        Format::PackageLock,
        "sub/package-lock.json",
        vec![Dep::lock(
            Ecosystem::Npm,
            "x",
            "1.0.0",
            "sub/package-lock.json",
        )],
    );
    let top = parsed(
        Format::PackageJson,
        "package.json",
        vec![manifest("package.json", Ecosystem::Npm, "x", "1.0.0")],
    );
    let other = parsed(
        Format::CargoToml,
        "Cargo.toml",
        vec![manifest("Cargo.toml", Ecosystem::CratesIo, "x", "1.0.0")],
    );
    let c = combine(&[lock, top, other]);
    assert_eq!(c.deps.len(), 3);
    let top_dep = c.deps.iter().find(|d| d.path == "package.json").unwrap();
    assert_eq!(top_dep.kind, DepKind::Manifest);
    let sub = c
        .deps
        .iter()
        .find(|d| d.path == "sub/package-lock.json")
        .unwrap();
    assert!(
        !sub.direct,
        "a manifest above a lockfile does not name its packages"
    );
}

#[test]
fn go_sum_is_ignored_next_to_go_mod_and_used_alone() {
    let both = [
        parsed(
            Format::GoMod,
            "go.mod",
            vec![Dep::lock(Ecosystem::Go, "a.example/m", "v1.1.0", "go.mod")],
        ),
        parsed(
            Format::GoSum,
            "go.sum",
            vec![
                Dep::lock(Ecosystem::Go, "a.example/m", "v1.0.0", "go.sum"),
                Dep::lock(Ecosystem::Go, "a.example/m", "v1.1.0", "go.sum"),
            ],
        ),
    ];
    let c = combine(&both);
    assert_eq!(c.deps.len(), 1);
    assert_eq!(c.deps[0].path, "go.mod");
    let alone = combine(&both[1..]);
    assert_eq!(alone.deps.len(), 2);
}

#[test]
fn names_that_differ_in_spelling_but_not_in_meaning_are_one_package() {
    let lock = parsed(
        Format::PoetryLock,
        "poetry.lock",
        vec![Dep::lock(
            Ecosystem::PyPi,
            "Django-Rest.Framework",
            "3.14.0",
            "poetry.lock",
        )],
    );
    let req = parsed(
        Format::PyProject,
        "pyproject.toml",
        vec![Dep::manifest(
            Ecosystem::PyPi,
            "django_rest_framework",
            ">=3",
            "3",
            "pyproject.toml",
        )],
    );
    let c = combine(&[lock, req]);
    assert_eq!(c.deps.len(), 1);
    assert!(c.deps[0].direct);
}

#[test]
fn hostile_and_broken_files_are_errors_never_partial_lists() {
    let big = vec![b' '; MAX_FILE_BYTES + 1];
    assert!(
        parse_file("Cargo.lock", &big)
            .unwrap()
            .unwrap_err()
            .contains("larger")
    );
    assert!(
        parse_file("Cargo.lock", b"\xff\xfe")
            .unwrap()
            .unwrap_err()
            .contains("UTF-8")
    );
    // A byte order mark is not content.
    let bom = b"\xEF\xBB\xBF[[package]]\nname = \"a\"\nversion = \"1.0.0\"\nsource = \"sparse+https://index.crates.io/\"\n";
    assert_eq!(
        parse_file("Cargo.lock", bom).unwrap().unwrap().deps.len(),
        1
    );
    for (name, text) in [
        ("Cargo.lock", "[[package]\nname"),
        ("Cargo.lock", "[[package]]\nname = 1\n"),
        ("package-lock.json", "{"),
        ("package-lock.json", "[]"),
        ("yarn.lock", "this is not a lockfile\n"),
        ("pnpm-lock.yaml", "- a\n- b\n"),
        ("pnpm-lock.yaml", "a: &x 1\n"),
        ("go.mod", "require x v1.0.0\n"),
        ("go.sum", "only two\n"),
        ("pom.xml", "<project><dependencies></project>"),
        ("pom.xml", "<other/>"),
        ("App.csproj", "<Project>"),
        ("composer.lock", "{}"),
        ("Gemfile.lock", "just text without sections"),
        ("Gemfile.lock", " leading space only\n"),
        ("mix.lock", "garbage\n"),
        ("pubspec.lock", "- a\n"),
        ("Package.resolved", "{\"version\": 2}"),
        ("Podfile.lock", "- a\n"),
        ("gradle.lockfile", "not:enough\n"),
        ("packages.lock.json", "{}"),
        ("Pipfile.lock", "["),
        ("poetry.lock", "[[package]]\nname = \"a\"\n"),
        ("uv.lock", "[[package]]\n"),
    ] {
        assert!(
            parse_file(name, text.as_bytes()).unwrap().is_err(),
            "{name}: {text:?}"
        );
    }
    // JSON nested past the reader's limit.
    let deep = format!(
        "{}{}",
        "{\"a\":".repeat(500),
        "1".to_owned() + &"}".repeat(500)
    );
    assert!(
        parse_file("package.json", deep.as_bytes())
            .unwrap()
            .is_err()
    );
    // A document type declaration cannot make the XML reader fetch or expand anything.
    let xxe = "<!DOCTYPE project [<!ENTITY x SYSTEM \"file:///etc/passwd\">]><project><artifactId>&x;</artifactId></project>";
    assert!(parse_file("pom.xml", xxe.as_bytes()).unwrap().is_err());
}

#[test]
fn a_file_with_too_many_dependencies_is_refused_whole() {
    let mut text = String::new();
    for i in 0..=MAX_DEPS {
        text.push_str(&format!("p{i}==1.0\n"));
    }
    let err = parse_file("requirements.txt", text.as_bytes())
        .unwrap()
        .unwrap_err();
    assert!(err.contains("more than"), "{err}");
    let ok: String = (0..1000).map(|i| format!("p{i}==1.0\n")).collect();
    assert_eq!(
        parse_file("requirements.txt", ok.as_bytes())
            .unwrap()
            .unwrap()
            .deps
            .len(),
        1000
    );
}

#[test]
fn property_bombs_in_a_pom_and_a_project_file_leave_the_dependency_unresolved() {
    // Ten levels of properties that each name the next one ten times: a billion bytes if expanded.
    let levels = |open: &str, close: &str| -> String {
        let mut props = String::new();
        for i in 0..9 {
            props.push_str(&format!(
                "<l{i}>{}</l{i}>",
                format!("{open}l{}{close}", i + 1).repeat(10)
            ));
        }
        props.push_str("<l9>lol</l9>");
        props
    };
    let pom = format!(
        "<project><properties>{}<self>${{self}}</self></properties><dependencies>         <dependency><groupId>g</groupId><artifactId>bomb</artifactId><version>${{l0}}</version></dependency>         <dependency><groupId>g</groupId><artifactId>cycle</artifactId><version>${{self}}</version></dependency>         <dependency><groupId>g</groupId><artifactId>fine</artifactId><version>1.0</version></dependency>         </dependencies></project>",
        levels("${", "}")
    );
    assert!(pom.len() < 4096);
    let p = read("pom.xml", &pom);
    assert_eq!(summary(&p), ["g:fine 1.0 *"]);
    assert!(p.notes[0].starts_with("2 dependencies"), "{:?}", p.notes);

    let proj = format!(
        "<Project><PropertyGroup>{}<Self>$(Self)</Self></PropertyGroup><ItemGroup>         <PackageReference Include=\"Bomb\" Version=\"$(l0)\" />         <PackageReference Include=\"Cycle\" Version=\"$(Self)\" />         <PackageReference Include=\"Fine\" Version=\"1.0.0\" />         </ItemGroup></Project>",
        levels("$(", ")")
    );
    let p = read("App.csproj", &proj);
    assert_eq!(summary(&p), ["Fine 1.0.0 *"]);
    assert!(
        p.notes[0].starts_with("2 package references"),
        "{:?}",
        p.notes
    );
}
