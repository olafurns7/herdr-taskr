CREATE TRIGGER search_events_insert after insert on events
when new.kind in ('decision', 'ask', 'answer', 'note', 'owner_answer')
begin
  insert into search_fts(body, src, ref, kind, name, root_id, task_id, at)
  values (coalesce(new.summary, ''), 'event', new.id, new.kind, '',
    (with recursive ancestors(id, parent_id) as (
      select id, parent_id from tasks where id = new.task_id
      union all select t.id, t.parent_id from tasks t join ancestors a on t.id = a.parent_id
    ) select id from ancestors where parent_id is null), new.task_id, new.created_at);
end