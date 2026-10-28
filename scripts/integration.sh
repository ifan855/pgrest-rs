#!/bin/bash
# Integration battery for pgrest-rs against the seeded test database.
# Usage: BASE=http://127.0.0.1:3100 ./scripts/integration.sh
BASE="${BASE:-http://127.0.0.1:3100}"
if [ "${RESET_DB:-1}" = "1" ]; then bash "$(dirname "$0")/reset-db.sh" > /dev/null; fi
PASS=0; FAIL=0
check() { # name expected_substring actual
  local name="$1" want="$2" got="$3"
  if [[ "$got" == *"$want"* ]]; then PASS=$((PASS+1)); echo "PASS  $name";
  else FAIL=$((FAIL+1)); echo "FAIL  $name"; echo "      want: $want"; echo "      got:  ${got:0:300}"; fi
}

echo "== READS =="
check "all rows" '"Inception"' "$(curl -sg "$BASE/api/films?select=title&order=id")"
check "filter eq" '"Dune"' "$(curl -sg "$BASE/api/films?select=title&title=eq.Dune")"
check "filter neq" '"Inception"' "$(curl -sg "$BASE/api/films?select=title&title=neq.Dune&order=id&limit=1")"
check "filter gte+order desc" '2023' "$(curl -sg "$BASE/api/films?select=year&year=gte.2021&order=year.desc")"
check "filter in" '{"title":"Dune"},{"title":"Barbie"}' "$(curl -sg "$BASE/api/films?select=title&title=in.(Dune,Barbie)&order=id")"
check "filter is null" '[]' "$(curl -sg "$BASE/api/films?select=title&director_id=is.null")"
check "filter like" '"Inception"' "$(curl -sg "$BASE/api/films?select=title&title=like.*cept*")"
check "filter ilike" '"Dune"' "$(curl -sg "$BASE/api/films?select=title&title=ilike.dunE")"
check "or block" 'Interstellar' "$(curl -sg "$BASE/api/films?select=title&or=(year.eq.2014,title.eq.Dune)")"
check "and block" '[]' "$(curl -sg "$BASE/api/films?select=title&and=(year.gte.2010,year.lte.2012,rating.gt.9)")"
check "not eq" '{"title":"Inception"},{"title":"Interstellar"},{"title":"Barbie"}' "$(curl -sg "$BASE/api/films?select=title&title=not.eq.Dune&order=id")"
check "cs array contains" 'Inception' "$(curl -sg "$BASE/api/films?select=title&tags=cs.{scifi,thriller}")"
check "fts" 'Inception' "$(curl -sg "$BASE/api/films?select=title&search=fts.heist")"
check "fts english" 'Inception' "$(curl -sg "$BASE/api/films?select=title&search=fts(english).dream")"
check "websearch" 'Inception' "$(curl -sg "$BASE/api/films?select=title&search=websearch.dream")"
check "cast" '"8.8"' "$(curl -sg "$BASE/api/films?select=rating::text&title=eq.Inception")"
check "alias col" '{"NAME":"Villeneuve"}' "$(curl -sg "$BASE/api/directors?select=NAME:name&name=eq.Villeneuve")"

echo "== EMBEDS =="
check "o2m embed" '"reviews"' "$(curl -sg "$BASE/api/films?select=title,reviews(stars)&title=eq.Inception&reviews.order=stars.desc")"
check "o2m embed count" '"count":2' "$(curl -sg "$BASE/api/films?select=title,reviews(count)&title=eq.Inception")"
check "o2m embed sum" '"sum":8' "$(curl -sg "$BASE/api/films?select=title,reviews(sum(stars))&title=eq.Inception")"
check "m2o embed" '"director":{"name":"Villeneuve"}' "$(curl -sg "$BASE/api/films?select=title,director:directors!director_id(name)&title=eq.Dune")"
check "m2o filter lift" '"title":"Dune"' "$(curl -sg "$BASE/api/films?select=title,directors!director_id(name)&directors.name=eq.Villeneuve&order=id")"
check "m2m via join table" '"films":[{"title":"Inception"}]' "$(curl -sg "$BASE/api/actors?select=name,films(title)&name=eq.DiCaprio")"
check "embed filter" '"actors":[]' "$(curl -sg "$BASE/api/films?select=title,actors(name)&title=eq.Inception&actors.name=eq.Nobody")"
check "!inner embed" '[]' "$(curl -sg "$BASE/api/films?select=title,actors!inner(name)&actors.name=eq.Nobody")"
check "nested embed" '"reviews"' "$(curl -sg "$BASE/api/directors?select=name,films(title,reviews(stars))&name=eq.Nolan")"
check "embed limit" '"reviews"' "$(curl -sg "$BASE/api/films?select=title,reviews(stars)&title=eq.Inception&reviews.limit=1")"

echo "== AGGREGATES =="
check "top count" '"count":4' "$(curl -sg "$BASE/api/films?select=count()")"
check "top avg" '"avg"' "$(curl -sg "$BASE/api/films?select=avg(rating)")"
check "count col" '"count":4' "$(curl -sg "$BASE/api/films?select=count(year)")"

echo "== PAGINATION =="
check "limit/offset" '"Interstellar"' "$(curl -sg "$BASE/api/films?select=title&order=id&limit=1&offset=1")"
CR=$(curl -s -D- -o /dev/null "$BASE/api/films?select=title&order=id&limit=2" | grep -i content-range)
check "content-range" 'items 0-1/*' "$CR"
R206=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/api/films?select=title&order=id&limit=2&offset=2")
check "206 partial" '206' "$R206"
R416=$(curl -s -o /dev/null -w "%{http_code}" -H "Range: items=99-100" "$BASE/api/films?select=title")
check "416 range" '416' "$R416"
CNT=$(curl -s -D- -o /dev/null -H "Prefer: count=exact" "$BASE/api/films?select=title&limit=1" | grep -i content-range)
check "prefer count exact" '/4' "$CNT"

echo "== MUTATIONS =="
NEW=$(curl -s -X POST "$BASE/api/reviews" -H "Content-Type: application/json" -d '{"film_id":1,"body":"new rev","stars":4}' -H "Prefer: return=representation")
check "insert returning" '"stars":4' "$NEW"
LOC=$(curl -s -D- -o /dev/null -X POST "$BASE/api/reviews" -H "Content-Type: application/json" -d '{"film_id":1,"body":"loc test","stars":2}' | grep -i '^location:')
check "location header" 'location: /api/reviews?id=' "$LOC"
IDNEW=$(echo "$NEW" | grep -o '"id": *[0-9]*' | grep -o '[0-9]*' | head -1)
UPD=$(curl -s -X PATCH "$BASE/api/reviews?id=eq.$IDNEW" -H "Content-Type: application/json" -d '{"stars":5}' -H "Prefer: return=representation")
check "update returning" '"stars":5' "$UPD"
NOFILT=$(curl -s -o /dev/null -w "%{http_code}" -X PATCH "$BASE/api/reviews" -H "Content-Type: application/json" -d '{"stars":5}')
check "patch no filter 400" '400' "$NOFILT"
DEL=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$BASE/api/reviews?body=eq.loc%20test")
check "delete 204" '204' "$DEL"
BULK=$(curl -s -X POST "$BASE/api/reviews" -H "Content-Type: application/json" -d '[{"film_id":2,"body":"b1","stars":3},{"film_id":2,"body":"b2","stars":4}]' -H "Prefer: return=representation")
check "bulk insert" '"b1"' "$BULK"
UPS=$(curl -s -X POST "$BASE/api/directors?on_conflict=id" -H "Content-Type: application/json" -d '{"id":1,"name":"Christopher Nolan"}' -H "Prefer: resolution=merge-duplicates,return=representation")
check "upsert merge" 'Christopher Nolan' "$UPS"
CSV=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/reviews" -H "Content-Type: text/csv" -d 'film_id,body,stars
1,from csv,3')
check "csv insert 201" '201' "$CSV"
curl -s -X DELETE "$BASE/api/reviews?body=in.(\"new rev\",\"b1\",\"b2\",\"from csv\")" > /dev/null
DELRET=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$BASE/api/reviews?body=eq.zzz-none" -H "Prefer: return=representation")
check "delete returning 200" '200' "$DELRET"

echo "== RPC =="
check "rpc scalar" 'hello, rust' "$(curl -s -X POST "$BASE/rpc/hello" -H "Content-Type: application/json" -d '{"name":"rust"}')"
check "rpc get" 'hello, world' "$(curl -sg "$BASE/rpc/hello")"
check "rpc setof" 'Inception' "$(curl -s -X POST "$BASE/rpc/films_by_year" -H "Content-Type: application/json" -d '{"min_year":2010}' | head -c 200)"
check "rpc table fn" 'avg_rating' "$(curl -s -X POST "$BASE/rpc/stats" -H "Content-Type: application/json" -d '{}')"
check "rpc void 204" '204' "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/rpc/voidfn" -H "Content-Type: application/json" -d '{}')"
check "rpc unknown 404" '404' "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/rpc/nope" -H "Content-Type: application/json" -d '{}')"

echo "== MEDIA =="
check "csv output" 'title' "$(curl -sg "$BASE/api/films?select=title&limit=1" -H "Accept: text/csv")"
OBJ=$(curl -sg "$BASE/api/films?select=title&title=eq.Dune" -H "Accept: application/vnd.pgrst.object+json")
check "object media" '"title":"Dune"' "$OBJ"
MULTI=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/api/films?select=title" -H "Accept: application/vnd.pgrst.object+json")
check "object multi 406" '406' "$MULTI"

echo "== AUTH =="
UOK=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/api/films?select=title" -H "Authorization: Bearer garbage")
check "invalid jwt rejected with secret configured" '401' "$UOK"

echo "== ERRORS =="
check "unknown table 404" '404' "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/nothing")"
check "unknown table code" 'PGRST118' "$(curl -sg "$BASE/api/nothing")"
check "fk violation 409" '409' "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/reviews" -H "Content-Type: application/json" -d '{"film_id":999,"body":"x","stars":3}')"
check "check violation 400" '400' "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/reviews" -H "Content-Type: application/json" -d '{"film_id":1,"body":"x","stars":99}')"
check "not found rpc" 'PGRST202' "$(curl -s -X POST "$BASE/rpc/ghost")"

echo "== OPENAPI + ADMIN =="
OA=$(curl -sg "$BASE/")
check "openapi at root" '"swagger":"2.0"' "$OA"
check "openapi has films" '"/api/films"' "$OA"
check "openapi rpc path" '"/rpc/hello"' "$OA"
check "admin live" '200' "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/admin/live")"
check "admin ready" '200' "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/admin/ready")"
MET=$(curl -sg "$BASE/admin/metrics")
check "metrics requests" 'pgrst_requests_total' "$MET"
check "metrics rss" 'pgrst_process_resident_memory_bytes' "$MET"

echo "== PREFS =="
PA=$(curl -s -D- -o /dev/null -H "Prefer: count=exact" "$BASE/api/films?select=title&limit=1" | grep -i preference-applied)
check "preference-applied" 'count=exact' "$PA"
curl -s -X POST "$BASE/api/reviews" -H "Content-Type: application/json" -H "Prefer: tx=rollback,return=representation" -d '{"film_id":1,"body":"should roll back","stars":3}' > /dev/null
LEFT=$(curl -sg "$BASE/api/reviews?select=count()&body=eq.should%20roll%20back")
check "tx=rollback discarded" '"count":0' "$LEFT"
PLAN=$(curl -s -D- -o /dev/null -H "Prefer: planning" "$BASE/api/films?select=title&limit=1" | grep -i "postgrest-plan")
check "plan header" 'PostgREST-Plan' "$PLAN"

echo "== CORS + OPTIONS =="
OPT=$(curl -s -D- -o /dev/null -X OPTIONS "$BASE/api/films" | grep -i access-control-allow-origin)
check "options cors" '*' "$OPT"

echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
