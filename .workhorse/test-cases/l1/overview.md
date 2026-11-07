# Moving configuration into the store

Scenarios that verify configuration lives in the store, is edited live from the dashboard, and that a machine with a file is forced across exactly once.

## Resolving what is not stored

- [x] The store path defaults under the user data directory, and `TOKER_DB` overrides it (verifies spec: DCONF)
- [x] The listener port defaults, and `PORT` overrides it (verifies spec: DCONF)
- [ ] The service, a command and the dashboard resolve the same port and store path from the same environment (verifies spec: DCONF)
- [ ] A command that cannot reach the service names the port it tried (verifies spec: DCONF)

## Reading configuration from the store

- [x] A store with no configuration rows yields a usable config with every provider disabled (verifies spec: DCONF)
- [x] A provider row round-trips its enablement, endpoint and credential source (verifies spec: DCONF)
- [x] Disabling a provider keeps its endpoint and credential source (verifies spec: DCONF)
- [x] Gate settings, notice styles, header names, the sleep lock and transcript roots round-trip (verifies spec: DCONF)
- [x] Opening the store makes its directory owner-only, including a directory that was not (verifies spec: DCONF)

## Configuration never refuses a start

- [x] A provider whose credential does not resolve is treated as not configured, and a request reaching it is answered as such (verifies spec: DCONF)
- [ ] A machine with no provider enabled starts and serves, answering that nothing is configured (verifies spec: DCONF)
- [x] A stored setting the service cannot use disables only what it describes (verifies spec: DCONF)

## The file gate

- [ ] Every command except setup refuses to run while a configuration file exists, naming setup (verifies spec: UCLI, DCONF)
- [ ] The service refuses to start while a configuration file exists (verifies spec: DLIFE)
- [ ] The refusal exits so the supervisor does not retry it (verifies spec: DLIFE)
- [x] The file is looked for where the environment names it, and otherwise in the user configuration directory (verifies spec: DCONF)

## Importing a file

- [x] Setup imports a file's settings as rows, and activates revision one from its routing fields, in one transaction (verifies spec: RMIG, USETUP)
- [x] The file is removed only after that transaction commits (verifies spec: RMIG)
- [ ] An import interrupted before the removal is repaired by running setup again, to the same rows (verifies spec: RMIG, DCONF)
- [ ] An import whose routing fields do not validate writes nothing and removes nothing (verifies spec: RMIG)
- [x] The file's routing fields are not stored as configuration (verifies spec: RMIG)
- [ ] Setup reports what it read from the file before importing it (verifies spec: USETUP)

## Editing live

- [x] An edit through the control surface changes the row and the service's running state together, with no restart (verifies spec: DCONF, DCTRL)
- [x] A refused value leaves the stored value as it was and names the setting (verifies spec: DCTRL)
- [x] A notice style naming a profile outside the permitted character set is refused when set (verifies spec: FPROF)
- [x] A credential value reaches Toker only through the credential operation (verifies spec: DCTRL)
- [x] No control operation returns a credential value (verifies spec: DCTRL, SEC)
- [ ] Enabling a provider takes effect for the next request without a restart (verifies spec: DCONF)

## Setup on a fresh machine

- [ ] Setup asks for one provider and its credential and nothing further (verifies spec: USETUP)
- [x] Setup writes rows and no configuration file (verifies spec: USETUP)
- [x] Setup ends by replacing itself with the dashboard (verifies spec: USETUP)
- [x] Setup on an owned machine reports routing is owned by the store and does not ask for a protocol default (verifies spec: RMIG)

## Discovery instead of configured paths

- [x] The Claude and codex directories are found the way those programs find them (verifies spec: DCONF)
- [ ] The service resolves them the same way a command in the operator's shell does (verifies spec: DCONF)
- [ ] Setup adds the discovered directory to the paths the service may write, and nothing else (verifies spec: DCONF)

## The dashboard pages

- [x] The providers page shows every provider, enabled or not, with endpoint and credential source (verifies spec: UTUI)
- [x] A credential is shown as set or not set, never as its value (verifies spec: UTUI, SEC)
- [x] A provider whose credential does not resolve shows as not configured, naming the source (verifies spec: UTUI)
- [x] The settings page shows the store path and port without offering to edit them (verifies spec: UTUI)
- [x] Both pages report the service not answering rather than offering values to edit (verifies spec: UTUI)

## Credentials

- [x] A literal is stored and never displayed afterwards (verifies spec: SEC, DCONF)
- [ ] The resolution order is the environment, then the keyring, then the literal, then the Claude login file (verifies spec: WCRED)
- [ ] Exporting the ledger carries no credential (verifies spec: DCONF)
- [ ] A debug rendering of a credential-bearing value redacts it (verifies spec: SEC)
