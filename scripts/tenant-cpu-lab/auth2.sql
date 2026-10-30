\set u random(1, 50000)
SELECT s.user_id, s.tenant_id, r.role, r.can_write FROM sessions s JOIN user_roles r ON r.user_id = s.user_id AND r.tenant_id = s.tenant_id WHERE s.token = 'tok' || :u AND s.expires_at > now();
