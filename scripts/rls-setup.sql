-- RLS setup: prove role impersonation + row level security end-to-end.
-- create role authenticated; (exists)
grant authenticated to authenticator;
grant usage on schema api to authenticated;
grant all on all tables in schema api to authenticated;
grant all on all sequences in schema api to authenticated;

-- personal reviews: a user (user_id int via jwt claim) sees only own rows
alter table api.reviews enable row level security;
drop policy if exists own_reviews on api.reviews;
create policy own_reviews on api.reviews
  using (user_id = coalesce(nullif(current_setting('request.jwt.claim.user_id', true), '')::int, -1))
  with check (user_id = coalesce(nullif(current_setting('request.jwt.claim.user_id', true), '')::int, -1));

-- give some reviews owners
update api.reviews set user_id = 42 where id % 2 = 1;
update api.reviews set user_id = 7 where id % 2 = 0 and user_id is null;

-- authenticated can read films (anon already could), but reviews are RLS-scoped
grant select on api.reviews to authenticated, anon;
grant select on api.films to authenticated, anon;

-- anon keeps full access (battery runs as anon); authenticated is RLS-scoped
drop policy if exists anon_all_reviews on api.reviews;
create policy anon_all_reviews on api.reviews to anon using (true) with check (true);
