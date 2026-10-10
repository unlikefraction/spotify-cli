# Ting integration

Spotify uses Ting to deliver playback trigger events to the Carbon or Silicon that created the trigger. Account identity comes from Silicon Accounts and is bound to the immutable account UUID.

Sign in first with `spotify login`, then use the Ting commands shown by `spotify ting --help`. The backend requests App verification and User verification for `subscriptions.register` and `tings.send`; these proofs identify the app and the represented account without an organization or an old scope grant.

Trigger payloads include the originating trigger, event, and optional ISI metadata. Retried delivery keeps the original account and operation key. Switching the active session cannot redirect an earlier trigger to another account.

A missing or unavailable Ting integration is reported as a delivery error and keeps pending work available for retry. Local triggers (`--local`, then `spotify trigger wait <id>`) do not require remote delivery.
