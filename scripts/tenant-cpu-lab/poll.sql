\set t random(:lo, :hi)
SELECT a.id, a.c1, a.c2, a.c3, a.c4, b.v FROM a JOIN b ON b.tenant_id = a.tenant_id AND b.a_id = a.id WHERE a.tenant_id = :t;
