
drop trigger if exists search_events_insert;
drop table if exists search_fts;
create virtual table search_fts using fts5(body, src UNINDEXED, ref UNINDEXED,
  kind UNINDEXED, name UNINDEXED, root_id UNINDEXED, task_id UNINDEXED,
  at UNINDEXED, tokenize = 'unicode61 remove_diacritics 2');
insert into search_fts(search_fts, rank) values('secure-delete', 1);
CREATE TRIGGER search_events_insert after insert on events
when new.kind in ('decision', 'ask', 'answer', 'note', 'owner_answer')
begin
  insert into search_fts(body, src, ref, kind, name, root_id, task_id, at)
  values (coalesce(new.summary, ''), 'event', new.id, new.kind, '',
    (with recursive ancestors(id, parent_id) as (
      select id, parent_id from tasks where id = new.task_id
      union all select t.id, t.parent_id from tasks t join ancestors a on t.id = a.parent_id
    ) select id from ancestors where parent_id is null), new.task_id, new.created_at);
end;
insert into search_fts(body, src, ref, kind, name, root_id, task_id, at)
select b.body, 'doc', d.id, d.kind, d.name, d.root_id, d.task_id, d.created_at
from documents d join doc_blobs b on b.sha256 = d.sha256
where d.captured = 1 and d.version = (
  select max(version) from documents v where v.task_id = d.task_id
  and v.kind = d.kind and v.name = d.name and v.captured = 1);
insert into search_fts(body, src, ref, kind, name, root_id, task_id, at)
with recursive tree(id, root_id) as (
  select id, id from tasks where parent_id is null
  union all select t.id, tree.root_id from tasks t join tree on t.parent_id = tree.id
)
select coalesce(e.summary, ''), 'event', e.id, e.kind, '', tree.root_id, e.task_id, e.created_at
from events e join tree on tree.id = e.task_id
where e.kind in ('decision', 'ask', 'answer', 'note', 'owner_answer');