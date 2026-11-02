// Sets the theme before first paint. External (not inline) so the CSP needs no 'unsafe-inline'.
// Keep in step with src/lib/theme.ts (key "asc-theme": asc-light, asc-dark or asc-auto).
(function () {
  var pref = "asc-auto";
  try {
    var raw = localStorage.getItem("asc-theme");
    if (raw === "asc-light" || raw === "light") pref = "asc-light";
    else if (raw === "asc-dark" || raw === "dark") pref = "asc-dark";
  } catch {}
  var dark =
    pref === "asc-dark" ||
    (pref === "asc-auto" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  var root = document.documentElement;
  root.setAttribute("data-theme", pref);
  if (dark) root.classList.add("dark");
  root.style.colorScheme = dark ? "dark" : "light";
})();
