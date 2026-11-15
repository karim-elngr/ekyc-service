# Decision webhook relay

The relay delivers committed `ekyc.case.decided.v1` events to one operator-configured endpoint. Set `DATABASE_URL`, `WEBHOOK_URL` and `WEBHOOK_TOKEN`. Each POST carries the persisted JSON event, a bearer token and its stable event ID as the `Idempotency-Key`. The receiver should return a 2xx response after it durably accepts the event or recognizes an identical duplicate.

Workers claim one row with a 30-second lease. They call the receiver outside the database transaction with a 5-second request timeout. If a worker loses its connection after the receiver accepts an event, a later claim can deliver the same event again. Updates are fenced by claim count. Failures back off exponentially up to five minutes and honor longer receiver cooldowns. The `decision_outbox` table exposes the attempt count, next scheduled attempt and delivery timestamp for each event.

The Compose receiver is a local development inbox. One statically configured endpoint receives every case in the demo. For a multi-tenant deployment, route through authorized per-tenant subscriptions, authenticate receivers over TLS, store credentials in a secret manager, and retain delivery state per event and subscription. Keep the read model’s tenant authorization on any follow-up query.
