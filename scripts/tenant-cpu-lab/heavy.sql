-- 重いテナント: 50 人分のデータをまとめて JOIN・集計するバッチ（5,000 行 × 5,000 行）
SELECT count(*), sum(b.v), max(a.c3) FROM a JOIN b ON b.tenant_id = a.tenant_id AND b.a_id = a.id WHERE a.tenant_id BETWEEN 1 AND 50;
