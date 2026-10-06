"use strict";

// Chromium MV3 accepts one service worker entry point.  Keep discovery and the
// persistent browser-transfer relay as separate modules with separate channels.
importScripts("codes.js", "events.js", "background.js", "relay-background.js");
