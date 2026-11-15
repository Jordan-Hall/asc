// Webview script of the signing page. It sets text with textContent only and sends the fixed
// messages the host allowlists. It never holds key content.
const vscode = acquireVsCodeApi();
const $ = (id) => document.getElementById(id);
const send = (m) => vscode.postMessage(m);

$("pickKey").addEventListener("click", () => send({ type: "pickKey" }));
$("test").addEventListener("click", () => send({ type: "test" }));
$("scope").addEventListener("change", (e) => send({ type: "setScope", value: e.target.value }));
$("format").addEventListener("change", (e) => send({ type: "setFormat", value: e.target.value }));
$("commitSign").addEventListener("change", (e) =>
  send({ type: "setCommitSign", value: e.target.checked }),
);
$("tagSign").addEventListener("change", (e) =>
  send({ type: "setTagSign", value: e.target.checked }),
);

window.addEventListener("message", (ev) => {
  const m = ev.data;
  if (m && m.type === "status") {
    const v = m.view;
    $("headline").textContent = v.headline;
    const dl = $("rows");
    dl.replaceChildren();
    for (const r of v.rows) {
      const dt = document.createElement("dt");
      dt.textContent = r.label;
      const dd = document.createElement("dd");
      dd.textContent = r.value;
      dl.append(dt, dd);
    }
    $("scope").value = v.scope;
    $("format").value = v.format;
    $("commitSign").checked = v.commitSign;
    $("tagSign").checked = v.tagSign;
  } else if (m && m.type === "test") {
    $("testResult").textContent = m.result.text;
  }
});
send({ type: "ready" });
