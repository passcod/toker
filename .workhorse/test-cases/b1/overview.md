# Live routing configuration

## Control boundary

- [x] A routing snapshot is available only with its matching control gate. Verifies spec: RCTL
- [x] Each routing mutation rejects a missing or wrong gate, non-JSON content type, unknown fields, and oversized body. Verifies spec: RCTL
- [ ] A routing control request never reaches an upstream provider. Verifies spec: RCTL

## Apply and restore

- [x] Applying a valid configuration creates and activates a new immutable revision without restarting the server. Verifies spec: RCTL
- [x] A configuration compilation failure leaves the active revision and runtime snapshot unchanged. Verifies spec: RCTL
- [x] Applying a configuration records publication desire at the captured inventory generation. Verifies spec: RCTL
- [x] Restoring a historical configuration creates a new active revision without modifying either historical revision. Verifies spec: RCTL
- [ ] An already captured request or pinned lane continues using its prior routing revision after an apply. Verifies spec: RCTL

## Binding and publication operations

- [ ] Promotion requires a binding ID and updates only that binding’s learned model state. Verifies spec: RCTL
- [ ] A publication acknowledgement with stale desired revision or inventory generation does not overwrite the current publication row. Verifies spec: RCTL

## Terminal workflow

- [x] Tab switches between the operational dashboard and routing page. Verifies spec: RCTL
- [x] The routing page shows active configuration and revision history, applies a saved `$EDITOR` edit immediately, and requires a second Enter before restoring a historical revision. Verifies spec: RCTL
