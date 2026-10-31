-- アプリがトランザクションを開いたまま外部処理を待つ想定（50ms）
\set t random(:lo, :hi)
BEGIN;
SELECT a.id, a.c1, a.c2, a.c3, a.c4, b.v FROM a JOIN b ON b.tenant_id = a.tenant_id AND b.a_id = a.id WHERE a.tenant_id = :t;
\sleep 50 ms
COMMIT;
