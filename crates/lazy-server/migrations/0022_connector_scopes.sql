-- Connector scopes hold Sentry project slugs too (llm-docs/DECISIONS.md, D37): `-` and up to 50
-- characters, still stored upper case.
ALTER TABLE connector_subscriptions DROP CONSTRAINT connector_subscriptions_scope_check;
ALTER TABLE connector_subscriptions ADD CONSTRAINT connector_subscriptions_scope_check
    CHECK (scope = upper(scope) AND scope ~ '^[A-Z0-9_-]{0,64}$');
