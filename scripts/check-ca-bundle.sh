#!/usr/bin/env bash
# RDS CA 번들과 `CA_BUNDLE_EARLIEST_EXPIRY_DAY` 상수의 일치를 확인한다.
#
# 만료 감시(07 §3.3)를 X.509 파서 없이 하는 방법이다. 파서를 바이너리에 넣는 대신
# 만료일을 상수로 박고, 이 스크립트가 CI 에서 번들과 상수를 대조한다.
# 번들만 갱신하고 상수를 잊으면 **만료 감시가 조용히 틀린 날짜를 보게 된다.**
set -euo pipefail
cd "$(dirname "$0")/.."

BUNDLE=crates/dbmon/assets/rds-global-bundle.pem
SRC=crates/dbmon/src/mysql/connect.rs

command -v openssl >/dev/null || { echo "건너뜀: openssl 이 없다"; exit 0; }
[ -s "$BUNDLE" ] || { echo "FAIL 번들이 없거나 비었다: $BUNDLE"; exit 1; }

COMPUTED=$(python3 - "$BUNDLE" <<'PY'
import re, subprocess, sys, datetime
pem = open(sys.argv[1]).read()
blocks = re.findall(r"-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----", pem, re.S)
if not blocks:
    print("NONE"); raise SystemExit
dates = []
for b in blocks:
    out = subprocess.run(["openssl", "x509", "-noout", "-enddate"],
                         input=b, capture_output=True, text=True)
    m = re.search(r"notAfter=(.*)", out.stdout)
    if m:
        dates.append(datetime.datetime.strptime(m.group(1).strip(), "%b %d %H:%M:%S %Y %Z"))
print(min(dates).date().isoformat() if dates else "NONE")
PY
)

DECLARED=$(grep -o 'CA_BUNDLE_EARLIEST_EXPIRY_DAY: &str = "[0-9-]*"' "$SRC" \
           | grep -o '[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}')

if [ "$COMPUTED" != "$DECLARED" ]; then
  echo "FAIL 번들의 최소 만료일과 상수가 다르다"
  echo "  번들 계산값: $COMPUTED"
  echo "  선언된 상수: $DECLARED"
  echo "  → $SRC 의 CA_BUNDLE_EARLIEST_EXPIRY_DAY 를 고친다"
  exit 1
fi

# 만료 임박도 여기서 알린다 — 상수가 맞더라도 날짜가 가까우면 갱신해야 한다.
DAYS=$(python3 -c "
import datetime,sys
d=datetime.date.fromisoformat('$COMPUTED')
print((d-datetime.date.today()).days)")
echo "OK  번들 최소 만료일 $COMPUTED (${DAYS}일 남음), 인증서 $(grep -c 'BEGIN CERTIFICATE' "$BUNDLE")개"
if [ "$DAYS" -lt 90 ]; then
  echo "WARN 90일 미만이다 — 번들을 갱신한다:"
  echo "  curl -o $BUNDLE https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem"
  [ "$DAYS" -lt 30 ] && { echo "CRITICAL 30일 미만"; exit 1; }
fi
