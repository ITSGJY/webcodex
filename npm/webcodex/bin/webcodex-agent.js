#!/usr/bin/env node
"use strict";

// Deprecated alias. The binary is `webcodex-runner`; this entry point exists so
// that an existing install, systemd unit, or script keeps working across the
// rename instead of failing with "command not found".
const { deprecatedAlias } = require("./wrapper");

deprecatedAlias("webcodex-agent", "webcodex-runner");
