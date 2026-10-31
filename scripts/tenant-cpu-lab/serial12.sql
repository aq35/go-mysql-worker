-- 1 ループ = 直列 12 クエリ（N+1 型: 親 1 + 子 1 + 孫 10）。各クエリは 1 往復
\set t random(1, 100)
SELECT id FROM a WHERE tenant_id = :t AND id = 1;
SELECT id FROM a WHERE tenant_id = :t AND id = 2;
SELECT id FROM a WHERE tenant_id = :t AND id = 3;
SELECT id FROM a WHERE tenant_id = :t AND id = 4;
SELECT id FROM a WHERE tenant_id = :t AND id = 5;
SELECT id FROM a WHERE tenant_id = :t AND id = 6;
SELECT id FROM a WHERE tenant_id = :t AND id = 7;
SELECT id FROM a WHERE tenant_id = :t AND id = 8;
SELECT id FROM a WHERE tenant_id = :t AND id = 9;
SELECT id FROM a WHERE tenant_id = :t AND id = 10;
SELECT id FROM a WHERE tenant_id = :t AND id = 11;
SELECT id FROM a WHERE tenant_id = :t AND id = 12;
