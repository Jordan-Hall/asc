use super::*;

fn names(items: &[Item], kind: ItemKind) -> Vec<String> {
    items.iter().filter(|i| i.kind == kind).map(|i| i.name.clone()).collect()
}

const COMPONENT: &str = "export function Greeting({ name }: { name?: string }) @{\n  const message = name ? `Hello, ${name}` : 'Hello';\n  <>\n    <div class=\"g\"><p>{message}</p></div>\n  </>\n}\nexport const after = 1;\n";

#[test]
fn the_mask_keeps_length_rows_and_typescript() {
    let a = analyze(COMPONENT.as_bytes());
    assert_eq!(a.masked.len(), COMPONENT.len());
    let m = String::from_utf8(a.masked.clone()).unwrap();
    assert_eq!(m.matches('\n').count(), COMPONENT.matches('\n').count());
    assert!(m.contains("function Greeting({ name }: { name?: string })  {"), "{m}");
    assert!(m.contains("const message = name ? `Hello, ${name}` : 'Hello';"));
    assert!(!m.contains("<div"), "{m}");
    assert!(m.contains("export const after = 1;"));
    assert_eq!(a.containers.len(), 1);
    assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
}

#[test]
fn control_flow_defines_and_uses_names() {
    let src = "function L({ users }: P) @{\n  <ul>\n    @for (const user of users; key user.id index i) {\n      const bio = user.bio.slice(0, 1);\n      @if (show && user.bio) {\n        <p>{bio}{i}</p>\n      } @else {\n        <p>none</p>\n      }\n    } @empty {\n      <li>x</li>\n    }\n  </ul>\n}\n";
    let a = analyze(src.as_bytes());
    let labels: Vec<&str> = a.scopes.iter().map(|s| s.label.as_str()).collect();
    assert!(labels.contains(&"@for (const user of users; key user.id index i)"), "{labels:?}");
    assert!(labels.contains(&"@if (show && user.bio)"), "{labels:?}");
    assert!(labels.contains(&"@else"), "{labels:?}");
    assert!(labels.contains(&"@empty"), "{labels:?}");
    assert_eq!(names(&a.defs, ItemKind::LoopVar), ["user", "i"]);
    assert_eq!(names(&a.defs, ItemKind::Local), ["bio"]);
    let uses = names(&a.uses, ItemKind::ValueUse);
    for want in ["users", "show", "bio", "i"] {
        assert!(uses.iter().any(|u| u == want), "{want} in {uses:?}");
    }
    assert!(!uses.iter().any(|u| u == "slice" || u == "id"), "members are not uses: {uses:?}");
    assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
}

#[test]
fn switch_try_and_dynamic_tags() {
    let src = "function S() @{\n  <section>\n    @switch (kind) {\n      @case 'a': { <A.B x={1} /> }\n      @default: { <{as} className=\"p\">t</{as}> }\n    }\n    @try { <Slow /> } @pending { <p>wait</p> } @catch (err) { <p>{err.message}</p> }\n  </section>\n}\n";
    let a = analyze(src.as_bytes());
    let kinds: Vec<&str> = a.scopes.iter().map(|s| s.kind.keyword()).collect();
    for k in ["switch", "case", "default", "try", "pending", "catch"] {
        assert!(kinds.contains(&k), "{k} in {kinds:?}");
    }
    assert_eq!(names(&a.defs, ItemKind::LoopVar), ["err"]);
    let comps = names(&a.uses, ItemKind::ComponentUse);
    assert!(comps.contains(&"A".to_owned()) && comps.contains(&"Slow".to_owned()), "{comps:?}");
    let uses = names(&a.uses, ItemKind::ValueUse);
    assert!(uses.contains(&"kind".to_owned()) && !uses.contains(&"as".to_owned()));
    assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
}

#[test]
fn style_classes_are_definitions_and_declarations_are_not() {
    let src = "export function F() @{\n  <div class=\"a\">x</div>\n  <style>\n    .card { padding: 1rem; background: url(x.png); }\n    @media (min-width: 1px) { .wide .x { margin: 0.5rem; } }\n  </style>\n}\n";
    let a = analyze(src.as_bytes());
    assert_eq!(names(&a.defs, ItemKind::StyleClass), ["card", "wide", "x"]);
}

#[test]
fn text_is_text_and_comments_are_comments() {
    let src = "function T() @{\n  <p>https://example.com {x} // not code\n  </p>\n}\n";
    let a = analyze(src.as_bytes());
    assert_eq!(names(&a.uses, ItemKind::ValueUse), ["x"]);
    assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
}

#[test]
fn strings_braces_and_regexes_in_setup_do_not_confuse_the_scan() {
    let src = "function T() @{\n  const s = '}' + \"{\" + `}${1}`;\n  const r = /[}]/g;\n  <p>{s}</p>\n}\nexport const z = 2;\n";
    let a = analyze(src.as_bytes());
    assert!(a.diagnostics.is_empty(), "{:?}", a.diagnostics);
    let m = String::from_utf8(a.masked).unwrap();
    assert!(m.contains("export const z = 2;"));
    assert!(!m.contains("<p>"));
}

#[test]
fn hostile_input_ends_with_diagnostics_not_a_panic() {
    for src in [
        "function T() @{ <div".to_owned(),
        "function T() @{ @for (".to_owned(),
        "@{".repeat(5_000),
        "<".repeat(5_000),
        format!("function T() @{{ {} }}", "<a>".repeat(5_000)),
        "function T() @{ <style> .a { ".to_owned(),
        "\u{0}\u{ff}\u{fe}".to_owned(),
    ] {
        let a = analyze(src.as_bytes());
        assert_eq!(a.masked.len(), src.len());
    }
}

#[test]
fn a_file_without_tsrx_is_unchanged() {
    let src = b"export const a = <div>{b}</div>;\nfunction f() { return 1 < 2; }\n";
    let a = analyze(src);
    assert_eq!(a.masked, src);
    assert!(a.containers.is_empty() && a.scopes.is_empty());
}

#[test]
fn imports_are_read_like_typescript_and_templates_do_not_count() {
    let src = "import a from './a';
import { b } from \"./b.tsrx\";
import './side';
export * from './c';
export { d } from './d';
const e = require('./e');
const f = import('./f');
export function F() @{
  <p>import x from './not-an-import'</p>
}
export const g = 1;
";
    let a = analyze(src.as_bytes());
    assert_eq!(
        import_specifiers(&a),
        ["./a", "./b.tsrx", "./side", "./c", "./d", "./e", "./f"]
    );
}

#[test]
fn the_scope_chain_names_the_component_and_the_scopes_around_a_line() {
    let src = "function L({ xs }: P) @{
  <ul>
    @for (const x of xs) {
      @if (x) {
        <li>{x}</li>
      }
    }
  </ul>
}
";
    let a = analyze(src.as_bytes());
    let at = src.find("<li>").unwrap();
    let comp = [("L".to_owned(), Span { start: 0, end: src.len() })];
    assert_eq!(
        scope_chain(&a, &comp, at),
        ["L", "@for (const x of xs)", "@if (x)"]
    );
    assert_eq!(scope_chain(&a, &comp, 2), ["L"]);
}

#[test]
fn import_evidence_works_for_tsrx_modules() {
    assert!(crate::symbols::imports_path(
        b"import Card from './Card.tsrx';
",
        b"src/Card.tsrx"
    ));
}
