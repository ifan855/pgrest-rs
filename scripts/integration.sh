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

echo "== SELECT SPREAD + AGGREGATE GROUPING =="
STAR=$(curl -sg "$BASE/api/films?select=*&title=eq.Dune")
check "select=*" '"tags":["scifi","desert"]' "$STAR"
STARE=$(curl -sg "$BASE/api/films?select=*,reviews(count)&title=eq.Dune")
check "select=*,embed" '"reviews":[{"count":1}]' "$STARE"
GRP=$(curl -sg "$BASE/api/films?select=director_id,count()&order=director_id")
check "aggregate group by" '{"count":2,"director_id":1}' "$GRP"
ACAST=$(curl -sg "$BASE/api/films?select=total:count()::text")
check "aggregate cast" '"total":"4"' "$ACAST"
AMIX=$(curl -sg "$BASE/api/films?select=count(),avg(rating)")
check "multi aggregate" '"count":4' "$AMIX"
check "mix agg+embed rejected" '400' "$(curl -s -o /dev/null -w '%{http_code}' -g "$BASE/api/films?select=count(),reviews(stars)")"
check "mix agg in embed rejected" '400' "$(curl -s -o /dev/null -w '%{http_code}' -g "$BASE/api/films?select=title,reviews(count,stars)")"

echo "== PUT (upsert by pk) =="
PUTU=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$BASE/api/directors?id=eq.1" -H "Content-Type: application/json" -H "Prefer: return=representation" -d '{"id":1,"name":"Nolan"}')
check "put update 200" '200' "$PUTU"
PUTI=$(curl -s -w "|%{http_code}" -X PUT "$BASE/api/directors?id=eq.9" -H "Content-Type: application/json" -H "Prefer: return=representation" -d '{"id":9,"name":"Wachowski"}')
check "put insert 200" '[{"id":9,"name":"Wachowski"}]|200' "$PUTI"
check "put without pk filter 400" '400' "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "$BASE/api/directors" -H "Content-Type: application/json" -d '{"id":1,"name":"x"}')"
check "put without pk in body 400" '400' "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "$BASE/api/directors?id=eq.1" -H "Content-Type: application/json" -d '{"name":"x"}')"

echo "== MUTATION SELECT/EMBED RETURN =="
MSEL=$(curl -s -X POST "$BASE/api/reviews?select=id,stars" -H "Content-Type: application/json" -H "Prefer: return=representation" -d '{"film_id":1,"body":"msel","stars":3}')
check "mutation select projection" '"id"' "$MSEL"
check "mutation projection excludes others" '0' "$(echo "$MSEL" | grep -c body; true)"
MEMB=$(curl -s -X POST "$BASE/api/films?select=title,director:directors!director_id(name)" -H "Content-Type: application/json" -H "Prefer: return=representation" -d '{"title":"Tenet","director_id":1,"year":2020}')
check "mutation embed return" '"director":{"name":"Nolan"}' "$MEMB"
MDEL=$(curl -s -X DELETE "$BASE/api/films?select=title&title=eq.Tenet" -H "Prefer: return=representation")
check "delete embed return" '"title":"Tenet"' "$MDEL"

echo "== RPC: SHAPING, BULK, PROCEDURES, OVERLOADS =="
RSEL=$(curl -sg "$BASE/rpc/films_by_year?min_year=2021&select=title")
check "rpc setof select" '[{"title":"Dune"},{"title":"Barbie"}]' "$RSEL"
REMB=$(curl -sg "$BASE/rpc/films_by_year?min_year=2021&select=title,director:directors!director_id(name)")
check "rpc setof embed" '"director":{"name":"Villeneuve"}' "$REMB"
RFIL=$(curl -sg "$BASE/rpc/films_by_year?min_year=2010&title=eq.Dune&select=title")
check "rpc setof filter" '[{"title":"Dune"}]' "$RFIL"
RORD=$(curl -sg "$BASE/rpc/films_by_year?min_year=2010&order=year.desc&limit=1&select=title")
check "rpc setof order+limit" '[{"title":"Barbie"}]' "$RORD"
BULK=$(curl -s -X POST "$BASE/rpc/add" -H "Content-Type: application/json" -H "Prefer: params=multiple-objects" -d '[{"a":1,"b":2},{"a":3,"b":4}]')
check "bulk rpc multiple-objects" '[3,7]' "$BULK"
BULKT=$(curl -s -X POST "$BASE/rpc/hello" -H "Content-Type: application/json" -H "Prefer: params=multiple-objects" -d '[{"name":"a"},{"name":"b"}]')
check "bulk rpc scalar text" '["hello, a","hello, b"]' "$BULKT"
PROC=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/rpc/spin" -H "Content-Type: application/json" -d "{}")
check "procedure call 204" '204' "$PROC"
check "procedure get 405" '405' "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/rpc/spin")"
OV1=$(curl -s -X POST "$BASE/rpc/hello" -H "Content-Type: application/json" -d '{"name":"rust"}')
check "overload 1-arg" 'hello, rust' "$OV1"
OV2=$(curl -s -X POST "$BASE/rpc/hello" -H "Content-Type: application/json" -d '{"name":"rust","greeting":"yo"}')
check "overload 2-arg" 'yo, rust' "$OV2"
OVA=$(curl -sg "$BASE/rpc/hello?name=q&greeting=z")
check "overload via query" 'z, q' "$OVA"
RSET=$(curl -s -H "Accept: application/vnd.pgrst.object+json" -g "$BASE/rpc/films_by_year?min_year=2010&select=title" -o /dev/null -w "%{http_code}")
check "rpc setof object multi 406" '406' "$RSET"

echo "== COMPUTED RELATIONSHIPS =="
COMP=$(curl -sg "$BASE/api/directors?select=name,films_of(title)&id=eq.1")
check "computed embed" '"films_of":[{"title":"Inception"},{"title":"Interstellar"}]' "$COMP"
COMPF=$(curl -sg "$BASE/api/directors?select=name,films_of(title)&id=eq.1&films_of.year=eq.2014")
check "computed embed filter" '"films_of":[{"title":"Interstellar"}]' "$COMPF"

echo "== PROFILES =="
PROF=$(curl -sg "$BASE/api/films?select=title" -H "Accept-Profile: api2")
check "accept-profile" '[{"title":"Alt Film"}]' "$PROF"
PROFW=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/api/films?select=id" -H "Content-Profile: api2" -H "Content-Type: application/json" -d '{"id":2,"title":"Second"}')
check "content-profile write" '201' "$PROFW"
PROFBAD=$(curl -s -o /dev/null -w "%{http_code}" -g "$BASE/api/films?select=title" -H "Accept-Profile: nosuch")
check "bad profile 404" '404' "$PROFBAD"
check "content-profile delete" '204' "$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api2/films?id=eq.2" -H 'Content-Profile: api2')"

echo "== GRAMMAR VALIDATION + COUNTS + RANGE =="
check "order garbage 400 PGRST100" 'PGRST100' "$(curl -sg "$BASE/api/films?order=id.bogus")"
check "limit negative 400" '400' "$(curl -s -o /dev/null -w '%{http_code}' -g "$BASE/api/films?limit=-1")"
check "count(*)" '"count":4' "$(curl -sg "$BASE/api/films?select=count(*)")"
PLN=$(curl -s -D- -o /dev/null -H "Prefer: count=planned" "$BASE/api/films?select=title&limit=1" | grep -i content-range)
check "planned count" 'items 0-0/' "$PLN"
RU=$(curl -s -D- -o /dev/null -H "Range: 0-1" -H "Range-Unit: items" "$BASE/api/films?select=title" | grep -i "^content-range")
check "range-unit form" 'items 0-1' "$RU"
CL=$(curl -s -D- -o /dev/null -g "$BASE/api/films?select=title&order=id&limit=2" | grep -i "^content-location")
check "content-location full" 'select=title&order=id&limit=2' "$CL"
OBJ0=$(curl -s -o /dev/null -w "%{http_code}" -H "Accept: application/vnd.pgrst.object+json" "$BASE/api/films?select=title&title=eq.NOPE")
check "object zero rows 406" '406' "$OBJ0"
LEFT=$(curl -sg "$BASE/api/films?select=title,actors!left(name)&title=eq.Dune")
check "!left hint" '"actors":[{"name":"Chalamet"}]' "$LEFT"
NEST=$(curl -sg "$BASE/api/films?select=title,reviews(stars)&reviews.order=stars.desc&reviews.limit=1&title=eq.Inception")
check "nested embed params" '"stars":5' "$NEST"
ALIASP=$(curl -sg "$BASE/api/films?select=title,r:directors!director_id(name)&title=eq.Dune&r.name=eq.Villeneuve")
check "embed alias param" '"r":{"name":"Villeneuve"}' "$ALIASP"

echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
