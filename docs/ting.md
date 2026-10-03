# Ting integration

spotify-cli is a proactive app: it notifies Silicons through Ting, which handles delivery,
retries and muting. It never registers interpreter webhooks itself.

## Types

| Type | Sent when |
| --- | --- |
| `spotify.trigger.fired` | a trigger reached its checkpoint (`data.outcome: "fired"`), or `spotify trigger test` (`"test"`) |
| `spotify.trigger.expired` | a one-shot (`current`) trigger's song ended before its checkpoint |

Payload: `spotify docs triggers`. Keys are `<recipient>/<trigger>/<play>/<outcome>`, stable across
retries, so Ting never delivers one firing twice. Consumers should dedupe by the Ting `id`.

## How a notification travels

```text
spotify-daemon ──(Silicon's own oat_ token)──▶ backend POST /api/v1/tings
backend: introspect token → build exact body {org_id, type, for: <you>, key, data, metadata}
       → refresh the separately approved tings.send access token when needed
       → Ting POST /v1/tings (X-IAM-OBO-Access-Token: <reusable access token>)
Ting: verify token for registered endpoint, method and path; enforce selected-context ACLs
Ting ──▶ ting-daemon on the Silicon's machine ──▶ Stemcell /events ──▶ your flow
```

The backend binds the action to the verified login account and organization and uses the
provider account and organization selected during consent. The caller cannot substitute a
recipient or organization in the request. Start with `spotify ting authorize`, review in IAM,
and redeem the one-use code with `spotify ting complete REQUEST_ID --code-file FILE`. Ordinary
login does not grant OBO. The backend encrypts the root access/refresh pairs, serializes token
rotation, and preserves the original mutation key after an uncertain response. Reusable access
tokens do not deduplicate writes: notification keys remain stable across retries.

`spotify ting register` explicitly retries recipient registration after approval. Revoked or
changed grants require fresh feature consent; ordinary logout preserves durable consent.

## Routing in your flow

Stemcell runs your YAML `flow` with `request.tings`. Unknown types fall into the default flow's
"other" branch and reach `intuit`. To route spotify-cli notifications to the ISI that set the
trigger, branch on the type and use `metadata.isi` (verify syntax with `silicon compile`):

```yaml
- if:
    condition: '{request.tings.filter(t, t.type.startsWith("spotify.trigger.")).size() > 0}'
    then:
      - send:
          isi: planner          # or the ISI in metadata.isi
          message: '{request.tings.filter(t, t.type.startsWith("spotify.trigger.")).map(t, make_readable(t)).join("\n\n")}'
```

## Operator setup (once per Ting context)

The types must be registered for the `spotify` app in each Ting context (production and every
testing environment) by an owner/admin of `unlikefraction` (the org that owns the app): `deploy/ting-types.sh`. The app's Honeycomb
config must declare `app_scope.external` for `ting/subscriptions.register` and `ting/tings.send`
and `self.identity.read` (`deploy/honeycomb-application.json`).
