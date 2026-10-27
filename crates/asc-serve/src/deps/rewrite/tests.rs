//! Each rewrite changes the one version and nothing else, and what it writes still parses.

use super::*;
use crate::deps::parse::{Parsed, parse_file};

fn pin<'a>(eco: Ecosystem, name: &'a str, from: &'a str, to: &'a str) -> Pin<'a> {
    Pin {
        eco,
        name,
        from,
        to,
    }
}

fn run(format: Format, text: &str, p: &Pin<'_>, extra: &Extra) -> Done {
    rewrite(format, text, p, extra)
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("no change"))
}

fn one(format: Format, text: &str, p: &Pin<'_>) -> String {
    run(format, text, p, &Extra::default()).text
}

/// The lines that differ, as `(old, new)`.
fn changed_lines(old: &str, new: &str) -> Vec<(String, String)> {
    assert_eq!(old.lines().count(), new.lines().count(), "line count");
    old.lines()
        .zip(new.lines())
        .filter(|(a, b)| a != b)
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect()
}

fn reparse(path: &str, text: &str) -> Parsed {
    parse_file(path, text.as_bytes()).unwrap().unwrap()
}

fn has(p: &Parsed, name: &str, version: &str) -> bool {
    p.deps
        .iter()
        .any(|d| d.name == name && d.version == version)
}

#[test]
fn cargo_toml_requirements_keep_their_shape() {
    let text = r#"[package]
name = "app"

[dependencies]
serde = { version = "1.0.152", features = ["derive"] }
tokio = "1.25"
renamed = { package = "regex", version = "~1.7" }
other = "2"
local = { path = "../x" }

[dev-dependencies]
tokio = "1.25"

[target.'cfg(unix)'.dependencies]
libc = "^0.2.100"

[dependencies.detail]
version = "0.3"
features = ["x"]
"#;
    let c = Ecosystem::CratesIo;
    let out = one(
        Format::CargoToml,
        text,
        &pin(c, "serde", "1.0.152", "2.0.1"),
    );
    assert_eq!(
        changed_lines(text, &out),
        [(
            r#"serde = { version = "1.0.152", features = ["derive"] }"#.to_owned(),
            r#"serde = { version = "2.0.1", features = ["derive"] }"#.to_owned()
        )]
    );
    // Both tables that name tokio change.
    let out = one(Format::CargoToml, text, &pin(c, "tokio", "1.25.0", "2.0.0"));
    assert_eq!(changed_lines(text, &out).len(), 2);
    assert!(out.contains("tokio = \"2.0\""));
    let out = one(Format::CargoToml, text, &pin(c, "regex", "1.7.1", "1.8.0"));
    assert!(out.contains(r#"renamed = { package = "regex", version = "~1.8" }"#));
    let out = one(Format::CargoToml, text, &pin(c, "libc", "0.2.150", "0.3.0"));
    assert!(out.contains(r#"libc = "^0.3.0""#));
    let out = one(Format::CargoToml, text, &pin(c, "detail", "0.3.1", "0.4.0"));
    assert!(out.contains("version = \"0.4\""));
    assert!(out.contains("features = [\"x\"]"));
    // A requirement that admits the new version is not touched; an absent package is not an edit.
    assert_eq!(
        rewrite(
            Format::CargoToml,
            text,
            &pin(c, "tokio", "1.25.0", "1.40.0"),
            &Extra::default()
        ),
        Ok(None)
    );
    assert_eq!(
        rewrite(
            Format::CargoToml,
            text,
            &pin(c, "absent", "1.0.0", "2.0.0"),
            &Extra::default()
        ),
        Ok(None)
    );
    assert_eq!(
        rewrite(
            Format::CargoToml,
            text,
            &pin(c, "local", "1.0.0", "2.0.0"),
            &Extra::default()
        ),
        Ok(None),
        "a path dependency has no version"
    );
    let done = run(
        Format::CargoToml,
        text,
        &pin(c, "serde", "1.0.152", "2.0.1"),
        &Extra::default(),
    );
    assert_eq!(done.requirement.as_deref(), Some("2.0.1"));
    let p = reparse("Cargo.toml", &done.text);
    assert!(has(&p, "serde", "2.0.1"));
}

#[test]
fn a_complex_cargo_requirement_is_an_error_not_a_guess() {
    let text = "[dependencies]\ncriterion = \">=0.4, <0.6\"\n";
    let e = rewrite(
        Format::CargoToml,
        text,
        &pin(Ecosystem::CratesIo, "criterion", "0.4.0", "0.7.0"),
        &Extra::default(),
    )
    .unwrap_err();
    assert!(e.contains("not a plain version"), "{e}");
}

const LOCK: &str = r#"version = 3

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "regex 1.5.0",
 "regex 1.7.1",
 "serde",
]

[[package]]
name = "regex"
version = "1.5.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aaaa"

[[package]]
name = "regex"
version = "1.7.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bbbb"
dependencies = [
 "memchr",
]

[[package]]
name = "serde"
version = "1.0.152"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "cccc"
"#;

#[test]
fn cargo_lock_moves_version_checksum_and_the_references_that_name_the_version() {
    let c = Ecosystem::CratesIo;
    let extra = Extra {
        checksum: Some("d".repeat(64)),
        ..Extra::default()
    };
    let out = run(
        Format::CargoLock,
        LOCK,
        &pin(c, "regex", "1.7.1", "1.9.0"),
        &extra,
    )
    .text;
    assert_eq!(
        changed_lines(LOCK, &out),
        [
            (
                " \"regex 1.7.1\",".to_owned(),
                " \"regex 1.9.0\",".to_owned()
            ),
            (
                "version = \"1.7.1\"".to_owned(),
                "version = \"1.9.0\"".to_owned()
            ),
            (
                "checksum = \"bbbb\"".to_owned(),
                format!("checksum = \"{}\"", "d".repeat(64))
            ),
        ]
    );
    let p = reparse("Cargo.lock", &out);
    assert!(has(&p, "regex", "1.9.0") && has(&p, "regex", "1.5.0") && !has(&p, "regex", "1.7.1"));
    // Without the checksum of the new version the lock is not touched.
    let e = rewrite(
        Format::CargoLock,
        LOCK,
        &pin(c, "serde", "1.0.152", "1.0.200"),
        &Extra::default(),
    )
    .unwrap_err();
    assert!(e.contains("checksum"), "{e}");
    // A version that is not locked, or a path package, is no edit.
    assert_eq!(
        rewrite(
            Format::CargoLock,
            LOCK,
            &pin(c, "regex", "1.6.0", "1.9.0"),
            &extra
        ),
        Ok(None)
    );
    assert_eq!(
        rewrite(
            Format::CargoLock,
            LOCK,
            &pin(c, "app", "0.1.0", "0.2.0"),
            &extra
        ),
        Ok(None)
    );
}

#[test]
fn package_json_changes_the_section_that_names_the_package() {
    let text = r#"{
  "name": "web",
  "dependencies": {
    "lodash": "^4.17.20",
    "@scope/pkg": "2.0.0"
  },
  "devDependencies": {
    "lodash": "^4.0.0",
    "mocha": "~10.2.0"
  },
  "scripts": { "lodash": "echo \"quoted\"" }
}
"#;
    let n = Ecosystem::Npm;
    let out = one(
        Format::PackageJson,
        text,
        &pin(n, "@scope/pkg", "2.0.0", "3.1.0"),
    );
    assert_eq!(
        changed_lines(text, &out),
        [(
            "    \"@scope/pkg\": \"2.0.0\"".to_owned(),
            "    \"@scope/pkg\": \"3.1.0\"".to_owned()
        )]
    );
    let out = one(
        Format::PackageJson,
        text,
        &pin(n, "lodash", "4.17.20", "5.0.0"),
    );
    assert_eq!(changed_lines(text, &out).len(), 2, "both sections");
    assert!(out.contains("\"lodash\": \"^5.0.0\""));
    assert!(
        out.contains("echo \\\"quoted\\\""),
        "strings elsewhere are untouched"
    );
    let done = run(
        Format::PackageJson,
        text,
        &pin(n, "mocha", "10.2.0", "10.4.0"),
        &Extra::default(),
    );
    assert_eq!(done.requirement.as_deref(), Some("~10.4.0"));
    assert!(has(&reparse("package.json", &done.text), "mocha", "10.4.0"));
    assert_eq!(
        rewrite(
            Format::PackageJson,
            text,
            &pin(n, "lodash", "4.17.20", "4.18.0"),
            &Extra::default()
        ),
        Ok(None)
    );
    assert!(
        rewrite(
            Format::PackageJson,
            "[1]",
            &pin(n, "x", "1.0.0", "2.0.0"),
            &Extra::default()
        )
        .is_err()
    );
}

const PKG_LOCK: &str = r#"{
  "name": "web",
  "lockfileVersion": 3,
  "packages": {
    "": {
      "name": "web",
      "dependencies": {
        "lodash": "^4.17.20"
      }
    },
    "node_modules/lodash": {
      "version": "4.17.20",
      "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.20.tgz",
      "integrity": "sha512-old"
    },
    "node_modules/other/node_modules/lodash": {
      "version": "4.17.20",
      "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.20.tgz",
      "integrity": "sha512-old"
    },
    "node_modules/alias": {
      "name": "lodash",
      "version": "9.9.9"
    },
    "node_modules/lodash-es": {
      "version": "4.17.20"
    }
  }
}
"#;

#[test]
fn package_lock_moves_every_copy_with_its_integrity_and_the_root_requirement() {
    let n = Ecosystem::Npm;
    let extra = Extra {
        integrity: Some("sha512-new".into()),
        tarball: Some("https://registry.npmjs.org/lodash/-/lodash-5.0.0.tgz".into()),
        requirement: Some("^5.0.0".into()),
        ..Extra::default()
    };
    let out = run(
        Format::PackageLock,
        PKG_LOCK,
        &pin(n, "lodash", "4.17.20", "5.0.0"),
        &extra,
    )
    .text;
    let diff = changed_lines(PKG_LOCK, &out);
    assert_eq!(diff.len(), 1 + 2 * 3, "{diff:#?}");
    assert!(out.contains("\"lodash\": \"^5.0.0\""));
    assert_eq!(out.matches("sha512-new").count(), 2);
    assert!(
        out.contains("\"version\": \"9.9.9\""),
        "an alias is another package"
    );
    assert!(
        out.contains("\"node_modules/lodash-es\": {\n      \"version\": \"4.17.20\""),
        "a different name"
    );
    let p = reparse("package-lock.json", &out);
    assert!(has(&p, "lodash", "5.0.0") && !has(&p, "lodash", "4.17.20"));
    // Without the integrity of the new version the lock is not touched.
    let e = rewrite(
        Format::PackageLock,
        PKG_LOCK,
        &pin(n, "lodash", "4.17.20", "5.0.0"),
        &Extra::default(),
    )
    .unwrap_err();
    assert!(e.contains("integrity") || e.contains("resolved"), "{e}");
    assert_eq!(
        rewrite(
            Format::PackageLock,
            PKG_LOCK,
            &pin(n, "lodash", "1.0.0", "5.0.0"),
            &extra
        ),
        Ok(None)
    );
}

#[test]
fn package_lock_v2_also_moves_the_legacy_tree() {
    let text = r#"{
  "lockfileVersion": 2,
  "packages": {
    "": { "dependencies": { "a": "1.0.0" } },
    "node_modules/a": { "version": "1.0.0", "resolved": "u1", "integrity": "i1" }
  },
  "dependencies": {
    "a": { "version": "1.0.0", "resolved": "u1", "integrity": "i1",
      "dependencies": { "a": { "version": "1.0.0", "resolved": "u1", "integrity": "i1" } } }
  }
}
"#;
    let extra = Extra {
        integrity: Some("sha512-i2".into()),
        tarball: Some("https://r.example/u2".into()),
        requirement: Some("2.0.0".into()),
        ..Extra::default()
    };
    let out = run(
        Format::PackageLock,
        text,
        &pin(Ecosystem::Npm, "a", "1.0.0", "2.0.0"),
        &extra,
    )
    .text;
    assert!(
        !out.contains("1.0.0\"") || out.matches("\"1.0.0\"").count() == 0,
        "{out}"
    );
    assert_eq!(out.matches("sha512-i2").count(), 3);
    assert_eq!(out.matches("https://r.example/u2").count(), 3);
    assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok());
}

#[test]
fn requirements_txt_moves_exact_pins_and_refuses_hashed_ones() {
    let text = "# deps\nDjango==3.2.0  # web\nrequests[security]===2.28.1 ; python_version >= \"3.7\"\nFlask_Login>=0.5\nhashed==1.0 \\\n    --hash=sha256:aaaa\nother-lib==1.0.0\n";
    let p = Ecosystem::PyPi;
    let out = one(
        Format::Requirements,
        text,
        &pin(p, "django", "3.2.0", "4.2.1"),
    );
    assert_eq!(
        changed_lines(text, &out),
        [(
            "Django==3.2.0  # web".to_owned(),
            "Django==4.2.1  # web".to_owned()
        )]
    );
    let out = one(
        Format::Requirements,
        text,
        &pin(p, "requests", "2.28.1", "2.31.0"),
    );
    assert!(out.contains("requests[security]===2.31.0 ; python_version"));
    let e = rewrite(
        Format::Requirements,
        text,
        &pin(p, "hashed", "1.0", "2.0"),
        &Extra::default(),
    )
    .unwrap_err();
    assert!(e.contains("--hash"), "{e}");
    assert_eq!(
        rewrite(
            Format::Requirements,
            text,
            &pin(p, "other", "1.0.0", "2.0.0"),
            &Extra::default()
        ),
        Ok(None),
        "a longer name is another package"
    );
    assert!(has(
        &reparse(
            "requirements.txt",
            &one(
                Format::Requirements,
                text,
                &pin(p, "other_lib", "1.0.0", "1.1.0")
            )
        ),
        "other-lib",
        "1.1.0"
    ));
}

#[test]
fn pyproject_pep_508_strings_change_in_place() {
    let text = "[project]\ndependencies = [\n    \"Django==3.2.0\",\n    \"requests >= 2.28\",\n    \"pydantic[email]~=1.10.2\",\n    \"ranged>=1.0,<2\",\n]\n";
    let p = Ecosystem::PyPi;
    let out = one(Format::PyProject, text, &pin(p, "django", "3.2.0", "4.2.1"));
    assert!(out.contains("\"Django==4.2.1\","));
    let out = one(
        Format::PyProject,
        text,
        &pin(p, "pydantic", "1.10.2", "2.0.0"),
    );
    assert!(out.contains("\"pydantic[email]~=2.0.0\""));
    assert!(
        rewrite(
            Format::PyProject,
            text,
            &pin(p, "ranged", "1.0", "3.0"),
            &Extra::default()
        )
        .is_err()
    );
    assert_eq!(
        rewrite(
            Format::PyProject,
            text,
            &pin(p, "requests", "2.28.0", "2.31.0"),
            &Extra::default()
        ),
        Ok(None),
        ">= admits it"
    );
}

#[test]
fn go_mod_changes_requires_and_leaves_replaces_to_a_person() {
    let text = "module example.com/app\n\ngo 1.21\n\nrequire github.com/gin-gonic/gin v1.9.0\n\nrequire (\n\tgolang.org/x/net v0.7.0 // indirect\n\tgithub.com/x/y v1.0.0\n\texample.com/replaced v1.0.0\n)\n\nreplace example.com/replaced => example.com/fork v1.0.1\n";
    let g = Ecosystem::Go;
    let out = one(
        Format::GoMod,
        text,
        &pin(g, "github.com/gin-gonic/gin", "v1.9.0", "v1.10.0"),
    );
    assert_eq!(
        changed_lines(text, &out),
        [(
            "require github.com/gin-gonic/gin v1.9.0".to_owned(),
            "require github.com/gin-gonic/gin v1.10.0".to_owned()
        )]
    );
    let out = one(
        Format::GoMod,
        text,
        &pin(g, "golang.org/x/net", "v0.7.0", "v0.17.0"),
    );
    assert!(out.contains("\tgolang.org/x/net v0.17.0 // indirect"));
    let e = rewrite(
        Format::GoMod,
        text,
        &pin(g, "example.com/replaced", "v1.0.0", "v1.1.0"),
        &Extra::default(),
    )
    .unwrap_err();
    assert!(e.contains("replaced"), "{e}");
    assert!(has(
        &reparse(
            "go.mod",
            &one(
                Format::GoMod,
                text,
                &pin(g, "github.com/x/y", "v1.0.0", "v1.2.0")
            )
        ),
        "github.com/x/y",
        "v1.2.0"
    ));
}

#[test]
fn pom_xml_edits_the_version_the_property_or_the_managed_entry() {
    let text = r#"<project>
  <properties>
    <log4j.version>2.14.1</log4j.version>
    <shared.version>1.0</shared.version>
  </properties>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>org.apache.commons</groupId>
        <artifactId>commons-lang3</artifactId>
        <version>3.12.0</version>
      </dependency>
    </dependencies>
  </dependencyManagement>
  <dependencies>
    <dependency>
      <groupId>org.apache.logging.log4j</groupId>
      <artifactId>log4j-core</artifactId>
      <version>${log4j.version}</version>
    </dependency>
    <dependency>
      <groupId>org.apache.commons</groupId>
      <artifactId>commons-lang3</artifactId>
    </dependency>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>direct</artifactId>
      <version>5.0.0</version>
    </dependency>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>one</artifactId>
      <version>${shared.version}</version>
    </dependency>
    <dependency>
      <groupId>com.example</groupId>
      <artifactId>two</artifactId>
      <version>${shared.version}</version>
    </dependency>
  </dependencies>
</project>
"#;
    let m = Ecosystem::Maven;
    let out = one(
        Format::PomXml,
        text,
        &pin(m, "com.example:direct", "5.0.0", "6.0.0"),
    );
    assert_eq!(changed_lines(text, &out).len(), 1);
    assert!(out.contains("<version>6.0.0</version>"));
    let out = one(
        Format::PomXml,
        text,
        &pin(m, "org.apache.logging.log4j:log4j-core", "2.14.1", "2.17.1"),
    );
    assert!(out.contains("<log4j.version>2.17.1</log4j.version>"));
    assert!(out.contains("<version>${log4j.version}</version>"));
    let out = one(
        Format::PomXml,
        text,
        &pin(m, "org.apache.commons:commons-lang3", "3.12.0", "3.14.0"),
    );
    assert!(out.contains("<version>3.14.0</version>"));
    let e = rewrite(
        Format::PomXml,
        text,
        &pin(m, "com.example:one", "1.0", "2.0"),
        &Extra::default(),
    )
    .unwrap_err();
    assert!(e.contains("shared"), "{e}");
    assert!(has(
        &reparse(
            "pom.xml",
            &one(
                Format::PomXml,
                text,
                &pin(m, "com.example:direct", "5.0.0", "6.0.0")
            )
        ),
        "com.example:direct",
        "6.0.0"
    ));
}

#[test]
fn gradle_lockfile_csproj_composer_and_the_ruby_and_elixir_files() {
    let m = Ecosystem::Maven;
    let gl = "# c\ncom.google.guava:guava:31.1-jre=compileClasspath,runtimeClasspath\norg.slf4j:slf4j-api:1.7.36=compileClasspath\n";
    let out = one(
        Format::GradleLock,
        gl,
        &pin(m, "com.google.guava:guava", "31.1-jre", "32.0.0-jre"),
    );
    assert!(out.contains("com.google.guava:guava:32.0.0-jre=compileClasspath,runtimeClasspath"));
    assert_eq!(changed_lines(gl, &out).len(), 1);

    let cs = "<Project>\n  <ItemGroup>\n    <PackageReference Include=\"Newtonsoft.Json\" Version=\"12.0.3\" />\n    <PackageReference Include=\"Dapper\">\n      <Version>2.0.123</Version>\n    </PackageReference>\n  </ItemGroup>\n</Project>\n";
    let n = Ecosystem::NuGet;
    let out = one(
        Format::Csproj,
        cs,
        &pin(n, "Newtonsoft.Json", "12.0.3", "13.0.1"),
    );
    assert!(out.contains("Version=\"13.0.1\" />"));
    let out = one(Format::Csproj, cs, &pin(n, "Dapper", "2.0.123", "2.1.0"));
    assert!(out.contains("<Version>2.1.0</Version>"));
    assert!(has(&reparse("A.csproj", &out), "Dapper", "2.1.0"));

    let cj = "{\n    \"require\": {\n        \"php\": \">=7.4\",\n        \"monolog/monolog\": \"^2.8\",\n        \"symfony/console\": \"~5.4.0\"\n    }\n}\n";
    let p = Ecosystem::Packagist;
    let out = one(
        Format::ComposerJson,
        cj,
        &pin(p, "monolog/monolog", "2.8.0", "3.5.0"),
    );
    assert!(out.contains("\"monolog/monolog\": \"^3.5\""));
    assert!(out.contains("\"php\": \">=7.4\""));

    let gf = "source \"https://rubygems.org\"\ngem \"rails\", \"~> 7.0\"\ngem 'nokogiri'\ngem \"rack\", \">= 2.0\", \"< 3\"\n";
    let r = Ecosystem::RubyGems;
    let done = run(
        Format::Gemfile,
        gf,
        &pin(r, "rails", "7.0.4", "8.0.1"),
        &Extra::default(),
    );
    assert!(done.text.contains("gem \"rails\", \"~> 8.0\""));
    assert_eq!(done.requirement.as_deref(), Some("~> 8.0"));
    assert!(
        rewrite(
            Format::Gemfile,
            gf,
            &pin(r, "rack", "2.2.4", "4.0.0"),
            &Extra::default()
        )
        .is_err()
    );
    assert_eq!(
        rewrite(
            Format::Gemfile,
            gf,
            &pin(r, "nokogiri", "1.0.0", "2.0.0"),
            &Extra::default()
        ),
        Ok(None)
    );

    let gl = "GEM\n  remote: https://rubygems.org/\n  specs:\n    rails (7.0.4)\n      rack (~> 2.0)\n    nokogiri (1.13.10-x86_64-linux)\n\nDEPENDENCIES\n  nokogiri\n  rails (~> 7.0)\n";
    let extra = Extra {
        requirement: Some("~> 8.0".into()),
        ..Extra::default()
    };
    let out = run(
        Format::GemfileLock,
        gl,
        &pin(r, "rails", "7.0.4", "8.0.1"),
        &extra,
    )
    .text;
    assert!(out.contains("    rails (8.0.1)\n"));
    assert!(out.contains("  rails (~> 8.0)\n"));
    assert!(out.contains("      rack (~> 2.0)\n"));
    let out = one(
        Format::GemfileLock,
        gl,
        &pin(r, "nokogiri", "1.13.10", "1.16.0"),
    );
    assert!(out.contains("nokogiri (1.16.0-x86_64-linux)"));

    let mx = "defp deps do\n  [\n    {:plug, \"~> 1.14\"},\n    {:cowboy, \"2.9.0\", only: :prod},\n    {:fork, git: \"x\"}\n  ]\nend\n";
    let h = Ecosystem::Hex;
    let out = one(Format::MixExs, mx, &pin(h, "plug", "1.14.0", "2.0.0"));
    assert!(out.contains("{:plug, \"~> 2.0\"}"));
    let out = one(Format::MixExs, mx, &pin(h, "cowboy", "2.9.0", "2.10.0"));
    assert!(out.contains("{:cowboy, \"2.10.0\", only: :prod}"));
}

#[test]
fn lockfiles_that_hold_hashes_of_a_graph_are_never_rewritten() {
    for f in [
        Format::YarnLock,
        Format::PnpmLock,
        Format::PoetryLock,
        Format::UvLock,
        Format::PipfileLock,
        Format::NugetLock,
        Format::ComposerLock,
        Format::MixLock,
        Format::PubspecLock,
        Format::PackageResolved,
        Format::PodfileLock,
        Format::GoSum,
    ] {
        let e = rewrite(
            f,
            "",
            &pin(Ecosystem::Npm, "x", "1", "2"),
            &Extra::default(),
        )
        .unwrap_err();
        assert!(!e.is_empty());
        assert!(unsupported(f).is_some(), "{f:?}");
    }
    for f in [
        Format::CargoToml,
        Format::PackageLock,
        Format::GoMod,
        Format::PomXml,
    ] {
        assert!(unsupported(f).is_none(), "{f:?}");
    }
}

#[test]
fn json_scanning_survives_strings_that_look_like_structure() {
    let text =
        "{\"a\": \"}{\\\"\", \"dependencies\": {\"x\": \"1.0.0\", \"y\": [\"{\"]}, \"z\": 1}";
    let out = rewrite(
        Format::PackageJson,
        text,
        &pin(Ecosystem::Npm, "x", "1.0.0", "2.0.0"),
        &Extra::default(),
    )
    .unwrap()
    .unwrap()
    .text;
    assert!(out.contains("\"x\": \"2.0.0\""));
    assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok());
}

/// A sample file of every format that is rewritten, each naming `x` at `1.0.0`.
fn samples() -> Vec<(Format, Ecosystem, &'static str)> {
    vec![
        (
            Format::CargoToml,
            Ecosystem::CratesIo,
            "[dependencies]\nx = \"1.0.0\"\n",
        ),
        (
            Format::CargoLock,
            Ecosystem::CratesIo,
            "[[package]]\nname = \"x\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"aaaa\"\n",
        ),
        (
            Format::PackageJson,
            Ecosystem::Npm,
            "{\"dependencies\": {\"x\": \"1.0.0\"}}",
        ),
        (
            Format::PackageLock,
            Ecosystem::Npm,
            "{\"lockfileVersion\": 3, \"packages\": {\"node_modules/x\": {\"version\": \"1.0.0\", \"resolved\": \"u\", \"integrity\": \"i\"}}}",
        ),
        (Format::Requirements, Ecosystem::PyPi, "x==1.0.0\n"),
        (
            Format::PyProject,
            Ecosystem::PyPi,
            "dependencies = [\"x==1.0.0\"]\n",
        ),
        (
            Format::GoMod,
            Ecosystem::Go,
            "module m\n\nrequire x v1.0.0\n",
        ),
        (
            Format::GradleLock,
            Ecosystem::Maven,
            "g:x:1.0.0=compileClasspath\n",
        ),
        (
            Format::PomXml,
            Ecosystem::Maven,
            "<project><dependencies><dependency><groupId>g</groupId><artifactId>x</artifactId><version>1.0.0</version></dependency></dependencies></project>",
        ),
        (
            Format::Csproj,
            Ecosystem::NuGet,
            "<Project><ItemGroup><PackageReference Include=\"x\" Version=\"1.0.0\" /></ItemGroup></Project>",
        ),
        (
            Format::ComposerJson,
            Ecosystem::Packagist,
            "{\"require\": {\"x\": \"1.0.0\"}}",
        ),
        (
            Format::Gemfile,
            Ecosystem::RubyGems,
            "gem \"x\", \"~> 1.0\"\n",
        ),
        (
            Format::GemfileLock,
            Ecosystem::RubyGems,
            "GEM\n  specs:\n    x (1.0.0)\n",
        ),
        (Format::MixExs, Ecosystem::Hex, "{:x, \"~> 1.0\"}\n"),
    ]
}

#[test]
fn a_version_with_quotes_newlines_or_markup_is_written_into_no_format() {
    let evil = [
        "2.0.0\"\nmalicious = true",
        "2.0.0'; rm -rf /",
        "2.0.0</version><version>9",
        "2.0.0\n[patch]\nx = 1",
        "2.0.0\\",
        "2.0.0 || 9",
        "2.0.0\"}, \"y\": {\"version\": \"9",
        "2.0.0&amp;",
        "${evil}",
    ];
    for (format, eco, text) in samples() {
        // The clean version is written, so the sample is a fair target.
        let extra = Extra {
            checksum: Some("a".repeat(64)),
            integrity: Some("sha512-abc==".into()),
            tarball: Some("https://r.example/x.tgz".into()),
            ..Extra::default()
        };
        let name = if eco == Ecosystem::Maven { "g:x" } else { "x" };
        let (from, to) = if format == Format::GoMod {
            ("v1.0.0", "v9.0.0")
        } else {
            ("1.0.0", "9.0.0")
        };
        let good = rewrite(format, text, &pin(eco, name, from, to), &extra);
        assert!(matches!(&good, Ok(Some(_))), "{format:?}: {good:?}");
        for bad in evil {
            let got = rewrite(format, text, &pin(eco, name, from, bad), &extra);
            assert!(
                got.as_ref().is_err_and(|e| e.contains("plain literal")),
                "{format:?} {bad:?}: {got:?}"
            );
            let got = rewrite(format, text, &pin(eco, name, bad, to), &extra);
            assert!(got.is_err(), "{format:?} from {bad:?}");
        }
    }
}

#[test]
fn what_a_registry_adds_to_a_lockfile_is_checked_too() {
    let p = pin(Ecosystem::Npm, "x", "1.0.0", "2.0.0");
    let lock = samples()
        .into_iter()
        .find(|s| s.0 == Format::PackageLock)
        .unwrap()
        .2;
    for extra in [
        Extra {
            integrity: Some("sha512-abc\",\"x\":\"".into()),
            tarball: Some("https://r/x.tgz".into()),
            ..Extra::default()
        },
        Extra {
            integrity: Some("md5-abc".into()),
            tarball: Some("https://r/x.tgz".into()),
            ..Extra::default()
        },
        Extra {
            integrity: Some("sha512-abc".into()),
            tarball: Some("http://r/x.tgz".into()),
            ..Extra::default()
        },
        Extra {
            integrity: Some("sha512-abc".into()),
            tarball: Some("https://r/x.tgz\"y".into()),
            ..Extra::default()
        },
        Extra {
            integrity: Some("sha512-abc".into()),
            tarball: Some("https://r/x y.tgz".into()),
            ..Extra::default()
        },
        Extra {
            requirement: Some("^2.0.0\", \"x".into()),
            integrity: Some("sha512-abc".into()),
            tarball: Some("https://r/x.tgz".into()),
            ..Extra::default()
        },
    ] {
        assert!(
            rewrite(Format::PackageLock, lock, &p, &extra).is_err(),
            "{extra:?}"
        );
    }
    let c = pin(Ecosystem::CratesIo, "x", "1.0.0", "2.0.0");
    let cargo = samples()
        .into_iter()
        .find(|s| s.0 == Format::CargoLock)
        .unwrap()
        .2;
    for sum in [
        "abc",
        &"G".repeat(64),
        &format!("{}\"", "a".repeat(63)),
        &"A".repeat(64),
    ] {
        let extra = Extra {
            checksum: Some(sum.to_owned()),
            ..Extra::default()
        };
        assert!(
            rewrite(Format::CargoLock, cargo, &c, &extra).is_err(),
            "{sum}"
        );
    }
}

#[test]
fn json_strings_are_escaped_whatever_they_hold() {
    assert_eq!(
        json_escape("a\"b\\c\nd\re\tf\u{1}g\u{7f}"),
        "a\\\"b\\\\c\\nd\\re\\tf\\u0001g\\u007f"
    );
    assert_eq!(json_escape("plain-1.0.0+build"), "plain-1.0.0+build");
}
