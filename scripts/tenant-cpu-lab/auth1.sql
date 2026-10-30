\set u random(1, 50000)
SELECT s.user_id, s.tenant_id, s.expires_at FROM sessions s WHERE s.token = 'tok' || :u AND s.expires_at > now();
