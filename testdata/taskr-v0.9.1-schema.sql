create table if not exists tasks (
  id            integer primary key,
  parent_id     integer references tasks(id),
  name          text not null,
  agent_name    text,
  role          text not null,
  status        text not null default 'open',
  workspace_id  text, tab_id text, pane_id text, cwd text,
  brief_path    text, report_path text,
  current_launch_id integer,
  acked_event_id    integer not null default 0,
  pending_event_id  integer,
  last_poll_at  text,
  waiting_until text,
  created_at text not null, updated_at text not null, closed_at text
);
create index if not exists tasks_parent on tasks(parent_id);

create table if not exists events (
  id                integer primary key,
  task_id           integer not null references tasks(id),
  recipient_task_id integer references tasks(id),
  launch_id         integer references launches(id),
  kind              text not null,
  summary           text,
  data              text,
  related_event_id  integer references events(id),
  answered_by       integer references events(id),
  event_key         text unique,
  created_at        text not null
);
create index if not exists events_task on events(task_id, id);
create index if not exists events_inbox on events(recipient_task_id, id);

create table if not exists launches (
  id           integer primary key,
  task_id      integer not null references tasks(id),
  provider     text, model text, effort text,
  account      text,
  native_home  text,
  session_ref  text, session_kind text, session_source text,
  herdr_scope  text,
  workspace_id text, tab_id text,
  pane_id      text,
  observed_status text, observed_seq integer, observed_version integer not null default 0,
  present      integer not null default 1, observed_at text,
  recorded_at  text not null
);

create table if not exists meta (
  key   text primary key,
  value text
);

-- The dashboard hub (v0.5): the latest snapshot each peer pushed, and owner
-- answers queued for a peer's ledger. Peers' ledgers are never merged here.
create table if not exists peers (
  node_id     text primary key,
  machine     text not null,
  login       text not null,
  snapshot    text not null,
  peer_now    text not null,
  received_at text not null
);

create table if not exists relay_answers (
  id           integer primary key,
  node_id      text not null,
  ask_id       integer not null,
  text         text not null,
  created_at   text not null,
  delivered_at text,
  applied_at   text,
  result       text
);
create unique index if not exists relay_answers_pending on relay_answers(node_id, ask_id) where applied_at is null;

-- A peer's record of each hub answer it applied: the idempotency key
-- relay:<hub node>:<relay id>, the hub that queued it, and the result it
-- reports back to that hub only.
create table if not exists relay_applied (
  key         text primary key,
  hub_node    text not null,
  relay_id    integer not null,
  ask_id      integer not null,
  answer_id   integer,
  result      text not null,
  applied_at  text not null,
  reported_at text
);
