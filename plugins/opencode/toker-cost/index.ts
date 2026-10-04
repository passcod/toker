import { Plugin } from "@opencode/plugin"

// Server entrypoint: this plugin is TUI-only, but a combined package keeps a
// no-op server entry so the server-side discovery and the TUI agree on one
// plugin identity.
export default Plugin.define({
  id: "passcod.toker-cost",
  setup() {},
})
