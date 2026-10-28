#!/bin/bash
# Reset the integration database to a pristine seeded state.
PSQL="/home/z/my-project/tools/pgsql/bin/psql"
$PSQL -h 127.0.0.1 -p 5433 -U postgres -d testdb -q -v ON_ERROR_STOP=1 <<'EOF'
truncate api.reviews, api.performances, api.actors, api.films, api.directors restart identity cascade;
insert into api.directors(id,name) values (1,'Nolan'),(2,'Villeneuve'),(3,'Greta Gerwig');
insert into api.films(id,title,year,director_id,rating,tags,search) values
 (1,'Inception',2010,1,8.8,'{scifi,thriller}',to_tsvector('Inception dream heist')),
 (2,'Interstellar',2014,1,8.6,'{scifi,drama}',to_tsvector('Interstellar space wormhole')),
 (3,'Dune',2021,2,8.0,'{scifi,desert}',to_tsvector('Dune sand arrakis')),
 (4,'Barbie',2023,3,6.8,'{comedy}',to_tsvector('Barbie doll comedy'));
insert into api.actors(id,name) values (1,'DiCaprio'),(2,'Chalamet'),(3,'Robbie');
insert into api.performances values (1,1,5000),(3,2,9000),(4,3,7000);
insert into api.reviews(film_id,body,stars) values (1,'mIND BEND',5),(1,'fine',3),(3,'epic',5),(4,'fun',4);
EOF
echo "db reset ok"
/home/z/my-project/tools/pgsql/bin/psql -h 127.0.0.1 -p 5433 -U postgres -d testdb -q <<'EOF2'
update api.reviews set user_id = 42 where id % 2 = 1;
update api.reviews set user_id = 7 where id % 2 = 0 and user_id is null;
EOF2
