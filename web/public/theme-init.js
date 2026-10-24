// Sets the theme class before first paint. External (not inline) so the CSP needs no
// 'unsafe-inline'. Keep in step with src/lib/theme.ts (key "asc-theme": light, dark or system).
(function () {
  var pref = "system";
  try {
    pref = localStorage.getItem("asc-theme") || "system";
  } catch {}
  var dark =
    pref === "dark" ||
    (pref !== "light" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  var root = document.documentElement;
  if (dark) root.classList.add("dark");
  root.style.colorScheme = dark ? "dark" : "light";
})();
