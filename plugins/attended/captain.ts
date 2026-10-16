// Loaded explicitly by `privateer start`; never installed in personal resources.
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { registerCaptain } from "./captain.mjs";
export default function captain(api: ExtensionAPI) { registerCaptain(api); }
