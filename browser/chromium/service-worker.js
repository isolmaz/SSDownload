"use strict";

// Chromium MV3 accepts one service worker entry point.  Keep discovery and the
// persistent browser-transfer relay as separate modules with separate channels. i18n.js
// comes first: every user-visible text of the worker goes through it.
importScripts("i18n.js", "codes.js", "events.js", "background.js", "relay-background.js");
