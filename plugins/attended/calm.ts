// Pi supplies this module, including its real exported presentation classes.
// Helpers stay importable in bounded, token-free Node behavior tests.
import * as host from "@earendil-works/pi-coding-agent";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { registerCalm } from "./calm.mjs";
export default function calm(api: ExtensionAPI) { return registerCalm(api, host); }
