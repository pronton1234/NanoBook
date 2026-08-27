#!/usr/bin/env bash
# Rust vs C++ over byte-identical input, runs INTERLEAVED.
#
# Alternating the two binaries matters. Running all of one and then all of the
# other lets machine load drift land entirely on whichever went first, and that
# mistake has already produced a fake 1.38x speedup elsewhere in this project.
# Taking the minimum does not rescue it: the minimum over a busy window is still
# worse than the minimum over a quiet one.
set -euo pipefail
CAP="${1:-/tmp/ll-bench.pcap}"
ROUNDS="${2:-6}"

rust_best=99999; rust_worst=0
cpp_best=99999;  cpp_worst=0

for i in $(seq 1 "$ROUNDS"); do
  r=$(./target/release/examples/xlang "$CAP" 1 | awk '/tick-to-book/ {print $3}')
  c=$(./cpp/build/ll_bench   "$CAP" 1 | awk '/tick-to-book/ {print $3}')
  rust_best=$(python3 -c "print(min($rust_best,$r))")
  rust_worst=$(python3 -c "print(max($rust_worst,$r))")
  cpp_best=$(python3 -c "print(min($cpp_best,$c))")
  cpp_worst=$(python3 -c "print(max($cpp_worst,$c))")
  printf "  round %d   rust %8.2f   c++ %8.2f\n" "$i" "$r" "$c"
done

python3 - "$rust_best" "$rust_worst" "$cpp_best" "$cpp_worst" <<'PY'
import sys
rb, rw, cb, cw = map(float, sys.argv[1:5])
spread = max(rw - rb, cw - cb)
gap = cb - rb
print()
print(f"  {'implementation':<22}{'ns/packet':>12}{'spread':>10}")
print(f"  {'Rust':<22}{rb:>9.2f} ns{rw-rb:>7.2f} ns")
print(f"  {'C++':<22}{cb:>9.2f} ns{cw-cb:>7.2f} ns")
print()
if abs(gap) < spread:
    print(f"  INDISTINGUISHABLE. The {abs(gap):.1f} ns gap is inside the {spread:.1f} ns spread.")
    print("  Two implementations of the same data structures land in the same place.")
elif gap > 0:
    print(f"  Rust is faster by {gap:.1f} ns/packet ({gap/rb*100:.0f}%), beyond the {spread:.1f} ns spread.")
else:
    print(f"  C++ is faster by {-gap:.1f} ns/packet ({-gap/cb*100:.0f}%), beyond the {spread:.1f} ns spread.")
print()
print("  Both decode the SAME FILE and must agree on every count. This is one")
print("  workload on one machine with two compilers -- it says these two")
print("  implementations land where they land, not that either language wins.")
PY
