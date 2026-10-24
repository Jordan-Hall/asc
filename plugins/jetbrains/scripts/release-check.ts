// Dry-run checker for the JetBrains Marketplace release (SPEC 24.9, P43b). It proves everything except the
// token step: workflow schema, the Gradle signing and publishing wiring, plugin metadata, version agreement,
// and (with --artefact, after buildPlugin and signPlugin) the shape of the built zip. It reads no secret.
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const pluginDir = resolve(here, "..");
const repoRoot = resolve(pluginDir, "..", "..");

export type Finding = { level: "error" | "warn"; message: string };

type Step = {
  name?: string;
  uses?: string;
  run?: string;
  if?: string;
  env?: Record<string, string>;
};

type Doc = {
  on?: { push?: { tags?: string[]; branches?: unknown } };
  true?: Doc["on"];
  permissions?: { contents?: string };
  jobs?: { release?: { "runs-on"?: string; env?: Record<string, string>; steps?: Step[] } };
};

const read = (p: string): string => readFileSync(p, "utf8");
const SHA_PIN = /@[0-9a-f]{40}(\s|$)/;
const SECRET_IN_SCRIPT = /\$\{\{\s*secrets\./;
const TOKEN_ARG = /(\s-P|\s--)\w*(token|password|private[-_]?key|certificate)\w*[=\s]/i;
const ECHO_SECRET = /\becho\b.*\$\{?(PUBLISH_TOKEN|PRIVATE_KEY|CERTIFICATE_CHAIN)/;

export function checkWorkflow(text: string): Finding[] {
  const out: Finding[] = [];
  const err = (message: string) => out.push({ level: "error", message: `workflow: ${message}` });
  let doc: Doc;
  try {
    doc = Bun.YAML.parse(text) as Doc;
  } catch (e) {
    return [{ level: "error", message: `workflow: not valid YAML (${(e as Error).message})` }];
  }
  const on = doc?.on ?? doc?.true;
  const tags: string[] = on?.push?.tags ?? [];
  if (!tags.some((t) => t === "jetbrains-v[0-9]+.[0-9]+.[0-9]+")) {
    err("push must trigger on jetbrains-v<x.y.z> tags");
  }
  if (on?.push?.branches) err("push must not trigger on branches");
  if (doc?.permissions?.contents !== "read") err("permissions.contents must be read");
  const job = doc?.jobs?.release;
  if (!job) return [...out, { level: "error", message: "workflow: job `release` missing" }];
  if (!String(job["runs-on"] ?? "").includes("trusted")) err("must run on a trusted runner");
  const steps = job.steps ?? [];
  for (const s of steps) {
    const label = s.name ?? s.uses ?? s.run?.slice(0, 40) ?? "step";
    if (s.uses && !SHA_PIN.test(`${s.uses} `)) {
      // `uses` keeps the pinned SHA before the trailing comment, which the YAML parser strips.
      err(`action not pinned to a full SHA: ${s.uses}`);
    }
    if (s.run && SECRET_IN_SCRIPT.test(s.run)) err(`secret interpolated into a script: ${label}`);
    if (s.run && TOKEN_ARG.test(s.run)) err(`credential-looking command-line argument: ${label}`);
    if (s.run && (/\bset -x\b/.test(s.run) || ECHO_SECRET.test(s.run))) {
      err(`step may echo a secret: ${label}`);
    }
  }
  const idx = (needle: string) => steps.findIndex((s) => s.run?.includes(needle));
  const build = idx("buildPlugin");
  const verify = idx("verifyPlugin");
  const sign = idx("signPlugin");
  const publish = idx("publishPlugin");
  const named: [string, number][] = [
    ["buildPlugin", build],
    ["verifyPlugin", verify],
    ["signPlugin", sign],
    ["publishPlugin", publish],
  ];
  for (const [n, i] of named) if (i < 0) err(`no step runs ${n}`);
  if (named.every(([, i]) => i >= 0) && !(Math.max(build, verify) < sign && sign < publish)) {
    err("order must be buildPlugin and verifyPlugin, then signPlugin, then publishPlugin");
  }
  const pub = steps[publish];
  if (pub) {
    if (!/github\.event_name\s*==\s*'push'/.test(pub.if ?? "")) {
      err("publish step must run only for a tag push");
    }
    if (!/secrets\.\w+/.test(pub.env?.PUBLISH_TOKEN ?? ""))
      err("PUBLISH_TOKEN must come from secrets via env");
  }
  const sig = steps[sign];
  if (sig) {
    if (!sig.env?.CERTIFICATE_CHAIN) err("sign step needs CERTIFICATE_CHAIN env");
    if (!sig.env?.PRIVATE_KEY) err("sign step needs PRIVATE_KEY env");
  }
  for (const [k, v] of Object.entries(job.env ?? {})) {
    if (/token|key|password|certificate/i.test(k) || /secrets\./.test(String(v))) {
      err(`job-level env exposes ${k} to every step`);
    }
  }
  if (!steps.some((s) => s.run?.includes("release-check.ts")))
    err("release-check.ts must run in the workflow");
  if (!steps.some((s) => /pluginVersion/.test(s.run ?? "")))
    err("version must be passed to Gradle as pluginVersion");
  return out;
}

export function checkGradle(text: string): Finding[] {
  const out: Finding[] = [];
  const err = (m: string) => out.push({ level: "error", message: `build.gradle.kts: ${m}` });
  const envs: [string, string][] = [
    ["certificateChain", "CERTIFICATE_CHAIN"],
    ["privateKey", "PRIVATE_KEY"],
    ["password", "PRIVATE_KEY_PASSWORD"],
    ["token", "PUBLISH_TOKEN"],
  ];
  for (const [key, env] of envs) {
    if (!new RegExp(`${key}\\s*=\\s*providers\\.environmentVariable\\("${env}"\\)`).test(text)) {
      err(`${key} must be read from env ${env}`);
    }
  }
  if (/BEGIN (RSA |EC |ENCRYPTED )?(PRIVATE KEY|CERTIFICATE)/.test(text))
    err("key material in source");
  if (!/sinceBuild\s*=\s*"261"/.test(text)) err("sinceBuild must be 261");
  if (/untilBuild\s*=\s*"/.test(text)) err("untilBuild must not be set");
  if (!text.includes("zipSigner()")) err("zipSigner() dependency missing");
  if (!text.includes("pluginVerifier()")) err("pluginVerifier() dependency missing");
  for (const t of [
    "IntellijIdeaUltimate",
    "Rider",
    "RustRover",
    "PyCharmProfessional",
    "WebStorm",
  ]) {
    if (!text.includes(`IntelliJPlatformType.${t}`)) err(`verifier does not cover ${t}`);
  }
  for (const v of ["2026.1", "2026.2"])
    if (!text.includes(`"${v}"`)) err(`verifier does not cover ${v}`);
  return out;
}

export function checkMetadata(pluginXml: string, gradleProps: string, pkgJson: string): Finding[] {
  const out: Finding[] = [];
  const err = (m: string) => out.push({ level: "error", message: m });
  if (!/<id>asc\.asc<\/id>/.test(pluginXml)) err("plugin.xml: id must be asc.asc");
  for (const tag of ["name", "vendor", "description"]) {
    if (!new RegExp(`<${tag}[^>]*>[^<]+</${tag}>`).test(pluginXml))
      err(`plugin.xml: <${tag}> missing`);
  }
  if (!pluginXml.includes("<depends>com.intellij.modules.vcs</depends>")) {
    err("plugin.xml: must depend on com.intellij.modules.vcs");
  }
  const gv = /^pluginVersion=(.+)$/m.exec(gradleProps)?.[1]?.trim();
  const pv = (JSON.parse(pkgJson) as { version?: string }).version;
  if (!gv) err("gradle.properties: pluginVersion missing");
  else if (!/^\d+\.\d+\.\d+$/.test(gv)) err(`gradle.properties: pluginVersion ${gv} is not x.y.z`);
  if (gv && pv && gv !== pv) err(`version mismatch: gradle.properties ${gv} vs package.json ${pv}`);
  return out;
}

/** Reads the zip central directory with no dependency and returns the entry names. */
export function zipEntries(buf: Buffer): string[] {
  let eocd = -1;
  for (let i = buf.length - 22; i >= Math.max(0, buf.length - 65557); i--) {
    if (buf.readUInt32LE(i) === 0x06054b50) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) return [];
  const count = buf.readUInt16LE(eocd + 10);
  let p = buf.readUInt32LE(eocd + 16);
  const names: string[] = [];
  for (let n = 0; n < count && buf.readUInt32LE(p) === 0x02014b50; n++) {
    const nl = buf.readUInt16LE(p + 28);
    const el = buf.readUInt16LE(p + 30);
    const cl = buf.readUInt16LE(p + 32);
    names.push(buf.toString("utf8", p + 46, p + 46 + nl));
    p += 46 + nl + el + cl;
  }
  return names;
}

export function checkZipEntries(names: string[]): Finding[] {
  const out: Finding[] = [];
  const err = (m: string) => out.push({ level: "error", message: `artefact: ${m}` });
  if (!names.some((n) => /^[^/]+\/lib\/[^/]+\.jar$/.test(n))) err("no lib/*.jar");
  if (names.some((n) => /\.(pem|key|p12|jks)$/i.test(n) || /(^|\/)\.env/.test(n))) {
    err("key or env file packaged in the zip");
  }
  return out;
}

export function checkArtefact(dir = join(pluginDir, "build", "distributions")): Finding[] {
  if (!existsSync(dir)) {
    return [
      {
        level: "error",
        message: `artefact: ${dir} missing (run buildPlugin and signPlugin first)`,
      },
    ];
  }
  const zips = readdirSync(dir).filter((f) => f.endsWith(".zip"));
  const out: Finding[] = [];
  if (!zips.some((f) => f.endsWith("-signed.zip"))) {
    out.push({ level: "error", message: "artefact: no *-signed.zip (signPlugin did not run)" });
  }
  const plain = zips.find((f) => !f.endsWith("-signed.zip"));
  if (!plain) out.push({ level: "error", message: "artefact: no plain plugin zip" });
  else out.push(...checkZipEntries(zipEntries(readFileSync(join(dir, plain)))));
  return out;
}

export function runAll(withArtefact: boolean): Finding[] {
  const f: Finding[] = [];
  f.push(...checkWorkflow(read(join(repoRoot, ".forgejo", "workflows", "jetbrains-release.yml"))));
  f.push(...checkGradle(read(join(pluginDir, "build.gradle.kts"))));
  f.push(
    ...checkMetadata(
      read(join(pluginDir, "src", "main", "resources", "META-INF", "plugin.xml")),
      read(join(pluginDir, "gradle.properties")),
      read(join(pluginDir, "package.json")),
    ),
  );
  if (!existsSync(join(pluginDir, "gradlew"))) {
    f.push({
      level: "warn",
      message:
        "plugins/jetbrains/gradlew is not checked in; the workflow's ./gradlew calls need the Gradle wrapper",
    });
  }
  if (withArtefact) f.push(...checkArtefact());
  return f;
}

if (import.meta.main) {
  const findings = runAll(process.argv.includes("--artefact"));
  for (const f of findings)
    process.stdout.write(`${f.level}: ${f.message}
`);
  const errors = findings.filter((f) => f.level === "error").length;
  process.stdout.write(
    `${errors === 0 ? "release-check: ok (token step not exercised; watch the first tag by hand)" : `release-check: ${errors} error(s)`}
`,
  );
  process.exit(errors === 0 ? 0 : 1);
}
