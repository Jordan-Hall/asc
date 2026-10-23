#!/usr/bin/env bash
# Runs every fuzz target for N seconds (default 60). Non-zero on a crash or a build failure.
# Usage: bash fuzz/run-all.sh [seconds] [target...]
# Needs cargo-fuzz and a nightly toolchain (FUZZ_TOOLCHAIN, default nightly).
set -euo pipefail
cd "$(dirname "$0")"
secs="${1:-60}"
shift || true
toolchain="${FUZZ_TOOLCHAIN:-nightly}"
# On Windows the sanitizer runtime is a DLL that only a Visual Studio developer shell has on PATH;
# without it every fuzz binary exits 0xc0000135 (STATUS_DLL_NOT_FOUND).
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    if ! command -v clang_rt.asan_dynamic-x86_64.dll > /dev/null 2>&1; then
      for dll in \
        "${VCToolsInstallDir:+$(cygpath -u "$VCToolsInstallDir")bin/Hostx64/x64/clang_rt.asan_dynamic-x86_64.dll}" \
        "$(cygpath -u "${ProgramFiles_x86_:-${PROGRAMFILES_X86:-C:\\Program Files (x86)}}")"/Microsoft\ Visual\ Studio/*/*/VC/Tools/MSVC/*/bin/Hostx64/x64/clang_rt.asan_dynamic-x86_64.dll \
        "$(cygpath -u "${PROGRAMFILES:-C:\\Program Files}")"/Microsoft\ Visual\ Studio/*/*/VC/Tools/MSVC/*/bin/Hostx64/x64/clang_rt.asan_dynamic-x86_64.dll; do
        if [ -f "$dll" ]; then
          export PATH="$(dirname "$dll"):$PATH"
        fi
      done
    fi
    ;;
esac
if [ "$#" -gt 0 ]; then
  targets=("$@")
else
  mapfile -t targets < <(sed -n 's/^name = "\(.*\)"$/\1/p' Cargo.toml | grep -v '^asc-fuzz$')
fi
fail=0
for t in "${targets[@]}"; do
  echo "== $t (${secs}s)"
  mkdir -p "corpus/$t" "regressions/$t"
  if ! cargo "+$toolchain" fuzz run "$t" "corpus/$t" "regressions/$t" -- \
      -max_total_time="$secs" -rss_limit_mb=2048 -max_len=65536; then
    echo "FAILED: $t (crash input is under fuzz/artifacts/$t; commit it to fuzz/regressions/$t/)"
    fail=1
  fi
done
exit "$fail"
