#!/usr/bin/env bash
# Black-box checks of the server-rendered viewer against a running Worker (`wrangler dev`).
#
#   ./smoke.sh [base-url] [sample.docx] [fixtures-dir]
#
# Defaults: http://127.0.0.1:8787 and ../../../../dist/web/samples/welcome.docx. With a fixtures
# directory holding `long.docx` and `locked.docx` (password `s3cret`), written by
# `FIXTURES=dir cargo test -- --ignored write_fixtures`, it also checks a protected upload and
# the inline page cap. Prints PASS or
# FAIL per check and the sizes a reader downloads; exits non-zero when any check fails. Needs
# curl, head, od and mktemp.
set -u

base="${1:-http://127.0.0.1:8787}"
here="$(cd "$(dirname "$0")" && pwd)"
sample="${2:-$here/../../../../dist/web/samples/welcome.docx}"
fixtures="${3:-}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
failures=0

check() { # check <name> <command...>: PASS when the command succeeds
  local name="$1"
  shift
  if "$@"; then
    printf 'PASS  %s\n' "$name"
  else
    printf 'FAIL  %s\n' "$name"
    failures=$((failures + 1))
  fi
}

lacks() { ! grep -qi "$1" "$2"; }       # lacks <pattern> <file>
is_4xx() { case "$1" in 4??) return 0 ;; *) return 1 ;; esac; }

# fetch <name> <curl args...>: body to $work/<name>.body, headers to $work/<name>.head; prints
# the status code.
fetch() {
  local name="$1"
  shift
  curl -sS -o "$work/$name.body" -D "$work/$name.head" -w '%{http_code}' "$@"
}

header() { # header <name> <header>: the header's value, lower-cased name match
  grep -i "^$2:" "$work/$1.head" | head -n 1 | cut -d: -f2- | tr -d '\r' | sed 's/^ *//'
}

no_store() { header "$1" Cache-Control | grep -q 'no-store'; }
has_type() { header "$1" Content-Type | grep -q "$2"; } # has_type <name> <pattern>

[ -f "$sample" ] || { echo "no sample at $sample (build dist/web first)"; exit 2; }

# 1. The viewer page for the deployed sample.
status=$(fetch view "$base/view?doc=samples/welcome.docx&page=1")
view_bytes=$(wc -c < "$work/view.body" | tr -d ' ')
check "viewer page is 200 (got $status)" [ "$status" = 200 ]
check "viewer page is HTML" has_type view '^text/html'
check "viewer page is under 2000 bytes ($view_bytes)" [ "$view_bytes" -lt 2000 ]
check "viewer page has no <script" lacks '<script' "$work/view.body"

# 2. Its image.
img=$(grep -o 'src="/view/page.png?[^"]*"' "$work/view.body" | head -n 1 | sed -e 's/^src="//' -e 's/"$//' -e 's/&amp;/\&/g')
status=$(fetch png "$base$img")
png_bytes=$(wc -c < "$work/png.body" | tr -d ' ')
signature=$(head -c 8 "$work/png.body" | od -An -tx1 | tr -d ' \n')
check "page image is 200 (got $status)" [ "$status" = 200 ]
check "page image is image/png" has_type png '^image/png'
check "page image has the PNG signature" [ "$signature" = 89504e470d0a1a0a ]

# 2b. HEAD: the GET's status and headers, no body, and no Content-Length the body wouldn't have.
head_of() { # head_of <name> <url>: headers to $work/<name>.head; prints the status code
  curl -sS -I -o /dev/null -D "$work/$1.head" -w '%{http_code}' "$2"
}
length_absent_or() { # length_absent_or <name> <bytes>
  local len
  len=$(header "$1" Content-Length)
  [ -z "$len" ] || [ "$len" = "$2" ]
}
status=$(head_of headpng "$base$img")
check "HEAD on the page image is 200 (got $status)" [ "$status" = 200 ]
check "HEAD on the page image is image/png" has_type headpng '^image/png'
check "HEAD on the page image has the GET's Cache-Control" [ "$(header headpng Cache-Control)" = "$(header png Cache-Control)" ]
check "HEAD on the page image has no wrong Content-Length ($(header headpng Content-Length))" length_absent_or headpng "$png_bytes"
status=$(head_of headview "$base/view?doc=samples/welcome.docx&page=1")
check "HEAD on the viewer page is 200 (got $status)" [ "$status" = 200 ]
check "HEAD on the viewer page is HTML" has_type headview '^text/html'
check "HEAD on the viewer page has no wrong Content-Length ($(header headview Content-Length))" length_absent_or headview "$view_bytes"
status=$(head_of headp999 "$base/view/page.png?doc=samples/welcome.docx&page=999")
check "HEAD on page 999's image is 404 (got $status)" [ "$status" = 404 ]
big="$base/view/page.png?doc=samples/welcome.docx&page=1&scale=2"
get_time=$(curl -sS -o /dev/null -w '%{time_total}' "$big")
head_time=$(curl -sS -I -o /dev/null -w '%{time_total}' "$big")

# 3. Out of range and hostile parameters.
status=$(fetch p999 "$base/view?doc=samples/welcome.docx&page=999")
check "page 999 is 404 (got $status)" [ "$status" = 404 ]
status=$(fetch trav "$base/view?doc=../../etc/passwd")
check "doc=../../etc/passwd is 400 (got $status)" [ "$status" = 400 ]
status=$(fetch url "$base/view?doc=https://example.com/x.docx")
check "doc=https://example.com/x.docx is 400 (got $status)" [ "$status" = 400 ]
status=$(fetch pngtrav "$base/view/page.png?doc=samples%2F..%2F..%2Fetc%2Fpasswd.docx&page=1")
check "encoded traversal on the image route is 400 (got $status)" [ "$status" = 400 ]
status=$(fetch scale "$base/view/page.png?doc=samples/welcome.docx&page=1&scale=NaN")
check "scale=NaN is 400 (got $status)" [ "$status" = 400 ]

# 4. Uploads.
status=$(fetch up -F "file=@$sample;type=application/vnd.openxmlformats-officedocument.wordprocessingml.document" "$base/view")
check "upload of the sample is 200 (got $status)" [ "$status" = 200 ]
check "upload result has a data:image/png" grep -q 'data:image/png;base64,' "$work/up.body"
check "upload result is no-store" no_store up

head -c $((5 * 1024 * 1024)) /dev/zero > "$work/zeros.docx"
status=$(fetch big -F "file=@$work/zeros.docx" "$base/view")
check "upload of 5 MiB of zeros is 413 (got $status)" [ "$status" = 413 ]
check "413 answer is no-store" no_store big

printf 'Just some text, not a Word document.\n' > "$work/note.txt"
status=$(fetch txt -F "file=@$work/note.txt;type=text/plain" "$base/view")
check "upload of a text file is 4xx (got $status)" is_4xx "$status"
check "text-file answer is no-store" no_store txt

status=$(fetch notform --data-binary "@$sample" -H 'Content-Type: application/octet-stream' "$base/view")
check "a non-form upload is 4xx (got $status)" is_4xx "$status"
check "non-form answer is no-store" no_store notform

# 5. Optional: a protected document and a long one.
if [ -n "$fixtures" ]; then
  status=$(fetch lock0 -F "file=@$fixtures/locked.docx" "$base/view")
  check "protected upload without a password is 422 (got $status)" [ "$status" = 422 ]
  check "it asks for the password again" grep -q 'type="password"' "$work/lock0.body"
  status=$(fetch lock1 -F "file=@$fixtures/locked.docx" -F 'password=guess' "$base/view")
  check "a wrong password is 422 (got $status)" [ "$status" = 422 ]
  check "the wrong password is not echoed" lacks 'guess' "$work/lock1.body"
  status=$(fetch lock2 -F "file=@$fixtures/locked.docx" -F 'password=s3cret' "$base/view")
  check "the right password is 200 (got $status)" [ "$status" = 200 ]
  check "the protected document's page is inline" grep -q 'data:image/png;base64,' "$work/lock2.body"
  check "the password is not echoed" lacks 's3cret' "$work/lock2.body"
  check "protected answers are no-store" no_store lock2
  status=$(fetch long -F "file=@$fixtures/long.docx" "$base/view")
  inline=$(grep -o 'data:image/png' "$work/long.body" | wc -l | tr -d ' ')
  check "a long upload is 200 (got $status)" [ "$status" = 200 ]
  check "a long upload shows at most 12 pages inline ($inline)" [ "$inline" -le 12 ]
  check "it says pages were left out" grep -q 'more pages are not shown' "$work/long.body"
fi

# 6. What a reader downloads to see page 1 of the sample.
echo
echo "viewer HTML: $view_bytes bytes; page 1 PNG: $png_bytes bytes; total: $((view_bytes + png_bytes)) bytes"
echo "upload result for the sample: $(wc -c < "$work/up.body" | tr -d ' ') bytes"
echo "page 1 at scale 2: GET ${get_time}s, HEAD ${head_time}s"

[ "$failures" -eq 0 ] || { echo "$failures check(s) failed"; exit 1; }
echo "all checks passed"
