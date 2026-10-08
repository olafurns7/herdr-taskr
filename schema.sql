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
  machine       text, -- the task's host; NULL is the server host (v0.10)
  -- A root's pane agent as its host last listed it (v0.15); lead_observed_at is the last change.
  lead_status   text, lead_present integer, lead_observed_at text,
  created_at text not null, updated_at text not null, closed_at text
);
create index if not exists tasks_parent on tasks(parent_id);

create trigger if not exists tasks_lead_rebind after update of pane_id, machine on tasks
  when old.pane_id is not new.pane_id or old.machine is not new.machine
  begin update tasks set lead_status = null, lead_present = null, lead_observed_at = null where id = new.id; end;

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
  session_ref  text, session_kind text, session_source text, transcript_path text,
  herdr_scope  text,
  workspace_id text, tab_id text,
  pane_id      text,
  observed_status text, observed_seq integer, observed_version integer not null default 0,
  present      integer not null default 1, observed_at text,
  machine      text, -- the launch's host; NULL is the server host (v0.10)
  recorded_at  text not null
);

-- One row per stored RPC request (v0.10): a retry with the same key gets
-- the stored result instead of running the command again.
create table if not exists requests (
  key        text primary key,
  machine    text,
  argv_sha   text not null,
  state      text not null,
  exit       integer,
  stdout     text,
  stderr     text,
  upload     text,
  created_at text not null
);

create table if not exists meta (
  key   text primary key,
  value text
);

-- Legacy (v0.5 to v0.16): the latest snapshot each peer pushed to the hub,
-- and owner answers queued for a peer's ledger. Nothing reads or writes these
-- tables now; they stay so old ledgers open unchanged.
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

create table if not exists doc_blobs (
  sha256 text primary key,
  bytes integer not null,
  body text not null
);
create table if not exists documents (
  id integer primary key autoincrement,
  root_id integer not null references tasks(id),
  task_id integer not null references tasks(id),
  kind text not null,
  name text not null default '',
  version integer not null,
  sha256 text,
  bytes integer,
  format text,
  captured integer not null,
  reason text,
  source_path text,
  source_host text,
  event_id integer references events(id),
  backfill integer not null default 0,
  created_at text not null
);
create index if not exists documents_task on documents(task_id, kind, name, version);
create index if not exists documents_root on documents(root_id, kind);
