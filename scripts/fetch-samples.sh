#!/usr/bin/env bash
# Fetch a small real slice of one IEX DEEP session per year.
#
# These are baked into the server image so a visitor can verify the decoder
# against REAL exchange data rather than the synthetic generator. Each slice is
# the first ~2 MB of that day's capture, which is roughly twenty thousand real
# messages -- enough to prove the decoder is right, small enough to ship.
#
# HTTP range requests make this cheap: a full day is 0.7 MB to 12 GB, and we
# take the first megabyte of the gzip stream, which decompresses to several MB
# of pcap before it runs out. A truncated gzip stream decompresses fine up to
# the truncation point, and the capture readers already stop cleanly on a
# partial final record because an interrupted capture looks exactly the same.
#
# DATA: Provided for free by IEX. By accessing or using IEX Historical Data you
# agree to the IEX Historical Data Terms of Use.  https://iextrading.com
set -euo pipefail
OUT="crates/server/samples"
mkdir -p "$OUT"

curl -s -m 120 "https://iextrading.com/api/1.0/hist" -o /tmp/hist.json

python3 - <<'PY' > /tmp/days.txt
import json, random
d = json.load(open('/tmp/hist.json'))
by_year = {}
for day in sorted(d):
    for f in d[day]:
        if f['feed'] == 'DEEP':
            by_year.setdefault(day[:4], []).append((day, f['link']))
# One day per year, chosen with a fixed seed so the set is reproducible rather
# than whatever happened to be picked the day this was run.
random.seed(20260827)
for year in sorted(by_year):
    day, link = random.choice(by_year[year])
    print(f"{day}\t{link}")
PY

while IFS=$'\t' read -r day link; do
  gz="/tmp/${day}.gz"
  out="$OUT/${day}.pcap"
  [ -f "$out" ] && { echo "  $day  (have)"; continue; }
  curl -s -m 180 -H "Range: bytes=0-1048575" "$link" -o "$gz"
  # Decompress what arrived and keep the first 2 MB. `gunzip` exits non-zero on
  # a truncated stream, which is expected here, so the failure is tolerated.
  gunzip -c "$gz" 2>/dev/null | head -c 2097152 > "$out" || true
  printf "  %s  %s bytes\n" "$day" "$(wc -c < "$out" | tr -d ' ')"
  rm -f "$gz"
done < /tmp/days.txt

echo
echo "  total: $(du -sh "$OUT" | cut -f1)"
